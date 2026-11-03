//! The receive thread: ONE UDP socket per local port, demuxed by `(source IP, stream name)`
//! (PLAN.md §5.2, CLAUDE.md law 3).
//!
//! ```text
//!   recv_from (blocking, 5 ms timeout)         control thread
//!        |                                          |  add/remove stream, set policy
//!        |  audio  --> find stream --> StreamRx::on_audio --> ring (the strip's input port)
//!        |  text (peers only) / service (anyone) --> ControlHandler (service.rs)
//!        |  anything else ------------------------> counted as malformed
//!        v
//!   every loop: StreamRx::tick() for every stream (give up on holes), publish packet age,
//!               ControlHandler::tick (the 2 s discovery beacon is sent from here)
//! ```
//!
//! # Rules this file obeys
//! * **Blocking `recv` with a timeout**, never a busy-polled non-blocking socket. The timeout
//!   is 5 ms, or 1 ms while some stream is waiting for a missing packet (see
//!   [`StreamRx::is_blocked`]).
//! * The control thread talks to this thread through a channel; every reply is awaited, so
//!   `add_stream` / `remove_stream` are synchronous for the caller and the ring's producer
//!   half always has exactly one owner.
//! * **Security (PLAN.md §5.2):** VBAN has no authentication. By default a stream whose
//!   configured source is "any IP" only accepts packets from addresses in the allow list
//!   ([`RxPolicy`]); "accept any" is an explicit, expert switch. A stream configured with one
//!   specific source IP trusts exactly that IP.
//! * **Control packets split in two.** VBAN-TEXT carries *commands* (mute, gain), so it obeys
//!   the same peer policy as audio. VBAN-SERVICE pings are how a peer becomes "discovered" in
//!   the first place, so they reach the handler from anyone; the handler is responsible for
//!   not being abusable (LAN-only sources, rate limits; see `service.rs`).

use crate::packet::{PacketError, parse_audio_packet};
use crate::stream::StreamRx;
use audio_types::{FixedStr, LanRxConfig, MAX_LAN_STREAMS, VbanParseError, VbanSubProtocol};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Socket timeout while every stream is in order (CLAUDE.md law 3: 5 ms).
pub const RECV_TIMEOUT: Duration = Duration::from_millis(5);
/// Socket timeout while a stream waits for a missing packet, so the give-up rule is checked
/// often enough (see `StreamRx::is_blocked`).
pub const BLOCKED_TIMEOUT: Duration = Duration::from_millis(1);
/// How long the control thread waits for the RX thread to answer a command.
const REPLY_TIMEOUT: Duration = Duration::from_millis(1000);
/// Telemetry value for "no packet received yet".
pub const NEVER: u32 = u32::MAX;
/// Senders remembered for the "unassigned streams" list of the UI.
const SEEN_CAPACITY: usize = 16;

// SECTION: Keys and policy
/// Which packets belong to a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamKey {
    /// `None` = any source IP (subject to the [`RxPolicy`]).
    pub source: Option<Ipv4Addr>,
    /// An empty name matches any stream name.
    pub name: FixedStr<16>,
}

impl StreamKey {
    pub fn from_config(cfg: &LanRxConfig) -> Self {
        let ip = Ipv4Addr::from(cfg.source_ipv4);
        Self {
            source: (!ip.is_unspecified()).then_some(ip),
            name: cfg.stream_name,
        }
    }

    fn name_matches(&self, name: &FixedStr<16>) -> bool {
        self.name.as_str().is_empty() || self.name == *name
    }
}

/// Who may send to a stream that did not name a specific source IP.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RxPolicy {
    /// Expert switch: accept from any address.
    pub accept_any: bool,
    /// Discovered / paired peers.
    pub allowed: Vec<Ipv4Addr>,
}

impl RxPolicy {
    pub fn permits(&self, ip: Ipv4Addr) -> bool {
        self.accept_any || self.allowed.contains(&ip)
    }
}
// !SECTION: Keys and policy

// SECTION: Statistics
/// Datagram-level counters of one socket. Written by the RX thread, read by anyone.
#[derive(Default)]
pub struct RxStats {
    pub datagrams: AtomicU64,
    pub bytes: AtomicU64,
    /// Audio packets delivered to a stream.
    pub audio: AtomicU32,
    /// Not VBAN, truncated, or an audio format we cannot read.
    pub malformed: AtomicU32,
    /// Valid audio, but no configured stream wants it.
    pub unmatched: AtomicU32,
    /// A stream wants it, but the sender is not allowed by the [`RxPolicy`].
    pub rejected: AtomicU32,
    pub text: AtomicU32,
    pub service: AtomicU32,
    pub socket_errors: AtomicU32,
    seen: Mutex<Vec<SeenSender>>,
}

/// A sender whose audio no stream accepted: what the UI offers as "incoming, not assigned".
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SeenSender {
    pub ip: Ipv4Addr,
    pub name: FixedStr<16>,
    pub channels: u16,
    pub sample_rate: u32,
    pub packets: u32,
    /// Filled in by [`RxStats::seen_senders`].
    pub ms_ago: u32,
    last: Option<Instant>,
}

impl PartialEq for RxStats {
    fn eq(&self, _: &Self) -> bool {
        false // counters are identities, not values
    }
}

impl RxStats {
    pub fn get(c: &AtomicU32) -> u32 {
        c.load(Relaxed)
    }

    /// Senders heard but not routed, newest last. Entries older than 30 s are dropped.
    pub fn seen_senders(&self) -> Vec<SeenSender> {
        let now = Instant::now();
        let mut list = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        list.retain(|s| {
            s.last
                .is_some_and(|t| now.duration_since(t) < Duration::from_secs(30))
        });
        list.iter()
            .map(|s| SeenSender {
                ms_ago: s.last.map_or(NEVER, |t| {
                    now.duration_since(t).as_millis().min(u128::from(NEVER - 1)) as u32
                }),
                ..*s
            })
            .collect()
    }

    fn note_seen(&self, ip: Ipv4Addr, name: FixedStr<16>, channels: u16, sample_rate: u32) {
        let mut list = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        let now = Some(Instant::now());
        if let Some(s) = list.iter_mut().find(|s| s.ip == ip && s.name == name) {
            s.packets = s.packets.saturating_add(1);
            s.channels = channels;
            s.sample_rate = sample_rate;
            s.last = now;
            return;
        }
        if list.len() >= SEEN_CAPACITY {
            list.remove(0);
        }
        list.push(SeenSender {
            ip,
            name,
            channels,
            sample_rate,
            packets: 1,
            ms_ago: 0,
            last: now,
        });
    }
}
// !SECTION: Statistics

// SECTION: Control-plane hook
/// Receives VBAN-TEXT and VBAN-SERVICE datagrams (they share the audio port). Implemented by
/// `service.rs`. Runs on the RX thread: keep it short and never block.
pub trait ControlHandler: Send + 'static {
    /// `socket` is the RX socket itself, so replies leave from the port the peer wrote to.
    /// VBAN-TEXT only arrives here from addresses the [`RxPolicy`] permits; VBAN-SERVICE
    /// arrives from anyone.
    fn on_packet(
        &mut self,
        sub: VbanSubProtocol,
        data: &[u8],
        from: SocketAddrV4,
        socket: &UdpSocket,
    );

    /// Called after every receive or timeout (at least every 5 ms) so the handler can do
    /// periodic work from the RX thread, e.g. the discovery beacon, which must leave from the
    /// RX socket so that replies come back to it.
    fn tick(&mut self, _now: Instant, _socket: &UdpSocket) {}
}
// !SECTION: Control-plane hook

enum Cmd {
    Add {
        key: StreamKey,
        stream: Box<StreamRx>,
        reply: Sender<Result<(), Box<StreamRx>>>,
    },
    Remove {
        key: StreamKey,
        reply: Sender<Option<Box<StreamRx>>>,
    },
    SetPolicy(RxPolicy),
}

#[derive(Debug)]
pub enum RxError {
    Io(io::Error),
    /// The RX thread is gone (it panicked or the socket was shut down).
    ThreadGone,
    /// All `MAX_LAN_STREAMS` slots are used.
    Full,
}

impl From<io::Error> for RxError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl core::fmt::Display for RxError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "socket error: {e}"),
            Self::ThreadGone => f.write_str("receive thread is not running"),
            Self::Full => f.write_str("too many LAN streams on this port"),
        }
    }
}

impl std::error::Error for RxError {}

/// The control-plane handle of one receive socket. Dropping it stops the thread.
pub struct RxSocket {
    local: SocketAddrV4,
    cmd: Sender<Cmd>,
    stop: Arc<AtomicBool>,
    stats: Arc<RxStats>,
    thread: Option<JoinHandle<()>>,
}

impl RxSocket {
    /// Open the socket and start the thread. `addr` port 0 picks a free port (tests).
    pub fn bind(
        addr: SocketAddrV4,
        policy: RxPolicy,
        handler: Option<Box<dyn ControlHandler>>,
    ) -> Result<Self, RxError> {
        // ? No SO_REUSEADDR on purpose: on Linux that lets a second socket share a UDP port
        // ? and unicast then goes to only one of them. A busy port should be an error.
        let socket = UdpSocket::bind(addr)?;
        socket.set_read_timeout(Some(RECV_TIMEOUT))?;
        let local = match socket.local_addr()? {
            SocketAddr::V4(a) => a,
            SocketAddr::V6(_) => return Err(RxError::Io(io::ErrorKind::Unsupported.into())),
        };
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(RxStats::default());
        let worker = Worker {
            socket,
            cmds: rx,
            stop: stop.clone(),
            stats: stats.clone(),
            policy,
            handler,
            streams: Vec::with_capacity(MAX_LAN_STREAMS),
            timeout: RECV_TIMEOUT,
        };
        let thread = std::thread::Builder::new()
            .name(format!("vban-rx-{}", local.port()))
            .spawn(move || worker.run())
            .map_err(RxError::Io)?;
        Ok(Self {
            local,
            cmd: tx,
            stop,
            stats,
            thread: Some(thread),
        })
    }

    pub fn local_addr(&self) -> SocketAddrV4 {
        self.local
    }

    pub fn stats(&self) -> &Arc<RxStats> {
        &self.stats
    }

    /// Start routing `key` to `stream`. If it is refused, the stream (and with it the ring's
    /// producer) comes back so the port can be reused.
    // ? The stream is handed back still in its `Box`: it is ~450 bytes and travels through a
    // ? channel anyway, and a large `Err` payload makes every `Result` of this function large.
    pub fn add_stream(
        &self,
        key: StreamKey,
        stream: StreamRx,
    ) -> Result<(), (RxError, Option<Box<StreamRx>>)> {
        let (reply, answer) = mpsc::channel();
        self.cmd
            .send(Cmd::Add {
                key,
                stream: Box::new(stream),
                reply,
            })
            .map_err(|mpsc::SendError(cmd)| {
                let back = match cmd {
                    Cmd::Add { stream, .. } => Some(stream),
                    _ => None,
                };
                (RxError::ThreadGone, back)
            })?;
        match answer.recv_timeout(REPLY_TIMEOUT) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(back)) => Err((RxError::Full, Some(back))),
            Err(_) => Err((RxError::ThreadGone, None)),
        }
    }

    /// Stop routing `key` and take the stream (and with it the ring's producer) back.
    pub fn remove_stream(&self, key: StreamKey) -> Result<Option<StreamRx>, RxError> {
        let (reply, answer) = mpsc::channel();
        self.cmd
            .send(Cmd::Remove { key, reply })
            .map_err(|_| RxError::ThreadGone)?;
        answer
            .recv_timeout(REPLY_TIMEOUT)
            .map(|s| s.map(|b| *b))
            .map_err(|_| RxError::ThreadGone)
    }

    pub fn set_policy(&self, policy: RxPolicy) -> Result<(), RxError> {
        self.cmd
            .send(Cmd::SetPolicy(policy))
            .map_err(|_| RxError::ThreadGone)
    }
}

impl Drop for RxSocket {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            // ? The thread notices within one socket timeout (5 ms).
            let _ = t.join();
        }
    }
}

struct Entry {
    key: StreamKey,
    rx: Box<StreamRx>,
    last_rx: Option<Instant>,
}

enum Route {
    Stream(usize),
    /// A stream would take it, but the policy says no.
    Rejected,
    NoStream,
}

struct Worker {
    socket: UdpSocket,
    cmds: mpsc::Receiver<Cmd>,
    stop: Arc<AtomicBool>,
    stats: Arc<RxStats>,
    policy: RxPolicy,
    handler: Option<Box<dyn ControlHandler>>,
    streams: Vec<Entry>,
    /// The timeout currently set on the socket (changing it is a syscall).
    timeout: Duration,
}

impl Worker {
    fn run(mut self) {
        // ? Stack buffer, large enough for any UDP payload we could accept (1464 B max).
        let mut buf = [0u8; 2048];
        while !self.stop.load(Relaxed) {
            self.drain_commands();
            self.adjust_timeout();
            match self.socket.recv_from(&mut buf) {
                Ok((n, SocketAddr::V4(src))) => self.on_datagram(&buf[..n], src),
                Ok(_) => {} // IPv6 can't happen on a v4 socket
                Err(e) if is_timeout(&e) => {}
                // ? On Windows a UDP socket reports an earlier ICMP "port unreachable" as
                // ? `ConnectionReset` on the next recv. It carries no packet and is harmless.
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionRefused
                    ) => {}
                Err(_) => {
                    self.stats.socket_errors.fetch_add(1, Relaxed);
                    // A persistent error must not turn into a hot loop.
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            self.housekeeping();
        }
    }

    fn adjust_timeout(&mut self) {
        let want = if self.streams.iter().any(|e| e.rx.is_blocked()) {
            BLOCKED_TIMEOUT
        } else {
            RECV_TIMEOUT
        };
        if want != self.timeout {
            // ! Never `None` (that would block forever and the thread could not be stopped).
            if self.socket.set_read_timeout(Some(want)).is_ok() {
                self.timeout = want;
            }
        }
    }

    fn drain_commands(&mut self) {
        while let Ok(cmd) = self.cmds.try_recv() {
            match cmd {
                Cmd::Add { key, stream, reply } => {
                    let dup = self.streams.iter().any(|e| e.key == key);
                    if dup || self.streams.len() >= MAX_LAN_STREAMS {
                        let _ = reply.send(Err(stream));
                    } else {
                        self.streams.push(Entry {
                            key,
                            rx: stream,
                            last_rx: None,
                        });
                        let _ = reply.send(Ok(()));
                    }
                }
                Cmd::Remove { key, reply } => {
                    let found = self
                        .streams
                        .iter()
                        .position(|e| e.key == key)
                        .map(|i| self.streams.remove(i).rx);
                    let _ = reply.send(found);
                }
                Cmd::SetPolicy(p) => self.policy = p,
            }
        }
    }

    /// Per loop: let every stream run its give-up rule and publish how stale it is.
    fn housekeeping(&mut self) {
        let now = Instant::now();
        for e in &mut self.streams {
            e.rx.tick();
            let age = e.last_rx.map_or(NEVER, |t| {
                now.duration_since(t).as_millis().min(u128::from(NEVER - 1)) as u32
            });
            e.rx.publish_age_ms(age);
        }
        if let Some(h) = self.handler.as_mut() {
            h.tick(now, &self.socket);
        }
    }

    fn on_datagram(&mut self, data: &[u8], src: SocketAddrV4) {
        self.stats.datagrams.fetch_add(1, Relaxed);
        self.stats.bytes.fetch_add(data.len() as u64, Relaxed);
        match parse_audio_packet(data) {
            Ok(parsed) => {
                let h = parsed.header;
                match self.route(*src.ip(), &h.stream_name) {
                    Route::Stream(i) => {
                        let e = &mut self.streams[i];
                        e.last_rx = Some(Instant::now());
                        e.rx.on_audio(&parsed);
                        self.stats.audio.fetch_add(1, Relaxed);
                    }
                    Route::Rejected => {
                        self.stats.rejected.fetch_add(1, Relaxed);
                        self.stats
                            .note_seen(*src.ip(), h.stream_name, h.channels, h.sample_rate);
                    }
                    Route::NoStream => {
                        self.stats.unmatched.fetch_add(1, Relaxed);
                        self.stats
                            .note_seen(*src.ip(), h.stream_name, h.channels, h.sample_rate);
                    }
                }
            }
            Err(PacketError::Header(VbanParseError::NotAudio(Some(sub)))) => {
                self.on_control(sub, data, src)
            }
            Err(_) => {
                self.stats.malformed.fetch_add(1, Relaxed);
            }
        }
    }

    fn on_control(&mut self, sub: VbanSubProtocol, data: &[u8], src: SocketAddrV4) {
        match sub {
            VbanSubProtocol::Text => self.stats.text.fetch_add(1, Relaxed),
            VbanSubProtocol::Service => self.stats.service.fetch_add(1, Relaxed),
            // ? VBAN-SERIAL (MIDI etc.) is out of scope; counted with the unreadable ones.
            _ => self.stats.malformed.fetch_add(1, Relaxed),
        };
        // ! TEXT is *commands* (mute, gain): same peer policy as audio. SERVICE has to be open
        // ! or nobody could ever be discovered; the handler defends itself (see module docs).
        let allowed = match sub {
            VbanSubProtocol::Text => self.policy.permits(*src.ip()),
            VbanSubProtocol::Service => true,
            _ => return,
        };
        if allowed {
            if let Some(h) = self.handler.as_mut() {
                h.on_packet(sub, data, src, &self.socket);
            }
        } else {
            self.stats.rejected.fetch_add(1, Relaxed);
        }
    }

    /// An exact source match wins over "any IP"; "any IP" needs the policy's blessing.
    fn route(&self, src: Ipv4Addr, name: &FixedStr<16>) -> Route {
        let mut wildcard = None;
        for (i, e) in self.streams.iter().enumerate() {
            if !e.key.name_matches(name) {
                continue;
            }
            match e.key.source {
                Some(ip) if ip == src => return Route::Stream(i),
                Some(_) => {}
                None => {
                    wildcard.get_or_insert(i);
                }
            }
        }
        match wildcard {
            Some(i) if self.policy.permits(src) => Route::Stream(i),
            Some(_) => Route::Rejected,
            None => Route::NoStream,
        }
    }
}

fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::PcmPacketizer;
    use crate::stream::StreamCounters;
    use audio_types::{
        ENGINE_SAMPLE_RATE, NetworkBufferPreset, VBAN_MAX_PACKET_BYTES, VbanDataType,
    };
    use rtrb::{Consumer, RingBuffer};

    const LOOPBACK: Ipv4Addr = Ipv4Addr::LOCALHOST;

    fn allow_loopback() -> RxPolicy {
        RxPolicy {
            accept_any: false,
            allowed: vec![LOOPBACK],
        }
    }

    fn open(policy: RxPolicy) -> RxSocket {
        RxSocket::bind(SocketAddrV4::new(LOOPBACK, 0), policy, None).unwrap()
    }

    fn new_stream(channels: usize) -> (StreamRx, Consumer<f32>) {
        let (p, c) = RingBuffer::<f32>::new(8 * 4096);
        let rx = StreamRx::new(
            p,
            channels,
            NetworkBufferPreset::Fast,
            Arc::new(StreamCounters::default()),
        );
        (rx, c)
    }

    fn key(name: &str) -> StreamKey {
        StreamKey {
            source: None,
            name: FixedStr::from_str_trunc(name),
        }
    }

    /// Sends Float32 stereo packets whose every sample is `value`.
    struct Sender {
        sock: UdpSocket,
        to: SocketAddrV4,
        tx: PcmPacketizer,
    }

    impl Sender {
        fn new(to: SocketAddrV4, name: &str) -> Self {
            Self {
                sock: UdpSocket::bind((LOOPBACK, 0)).unwrap(),
                to,
                tx: PcmPacketizer::new(
                    ENGINE_SAMPLE_RATE,
                    2,
                    VbanDataType::Float32,
                    128,
                    FixedStr::from_str_trunc(name),
                )
                .unwrap(),
            }
        }
        fn send(&mut self, value: f32) {
            let samples = vec![value; self.tx.samples_per_packet()];
            let mut buf = [0u8; VBAN_MAX_PACKET_BYTES];
            let len = self.tx.build(&samples, &mut buf).unwrap();
            self.sock.send_to(&buf[..len], self.to).unwrap();
        }
    }

    /// Wait (up to 2 s) until `cond` holds.
    fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(2);
        while Instant::now() < end {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        cond()
    }

    #[test]
    fn audio_reaches_the_ring_of_the_matching_stream() {
        let sock = open(allow_loopback());
        let (rx, mut cons) = new_stream(2);
        sock.add_stream(key("mic"), rx).map_err(|e| e.0).unwrap();
        let mut s = Sender::new(sock.local_addr(), "mic");
        for _ in 0..10 {
            s.send(0.25);
        }
        assert!(
            wait_for(|| cons.slots() >= 10 * 128 * 2),
            "audio did not arrive"
        );
        let n = cons.slots();
        let chunk = cons.read_chunk(n).unwrap();
        assert!(chunk.as_slices().0.iter().all(|&v| v == 0.25));
        chunk.commit_all();
        assert_eq!(RxStats::get(&sock.stats().audio), 10);
        assert_eq!(RxStats::get(&sock.stats().unmatched), 0);
    }

    #[test]
    fn two_streams_on_one_port_are_demuxed_by_name() {
        let sock = open(allow_loopback());
        let (a, mut ca) = new_stream(2);
        let (b, mut cb) = new_stream(2);
        sock.add_stream(key("a"), a).map_err(|e| e.0).unwrap();
        sock.add_stream(key("b"), b).map_err(|e| e.0).unwrap();
        let mut sa = Sender::new(sock.local_addr(), "a");
        let mut sb = Sender::new(sock.local_addr(), "b");
        for _ in 0..6 {
            sa.send(0.1);
            sb.send(-0.3);
        }
        assert!(wait_for(|| ca.slots() >= 6 * 256 && cb.slots() >= 6 * 256));
        let (na, nb) = (ca.slots(), cb.slots());
        let (xa, xb) = (ca.read_chunk(na).unwrap(), cb.read_chunk(nb).unwrap());
        assert!(xa.as_slices().0.iter().all(|&v| v == 0.1));
        assert!(xb.as_slices().0.iter().all(|&v| v == -0.3));
        xa.commit_all();
        xb.commit_all();
    }

    #[test]
    fn an_explicit_source_ip_beats_any_ip_and_the_wildcard_name_matches_everything() {
        let sock = open(RxPolicy {
            accept_any: true,
            allowed: vec![],
        });
        let (specific, c_specific) = new_stream(2);
        let (anyone, c_any) = new_stream(2);
        let exact = StreamKey {
            source: Some(LOOPBACK),
            name: FixedStr::from_str_trunc("x"),
        };
        sock.add_stream(key("x"), anyone).map_err(|e| e.0).unwrap();
        sock.add_stream(exact, specific).map_err(|e| e.0).unwrap();
        let mut s = Sender::new(sock.local_addr(), "x");
        for _ in 0..4 {
            s.send(0.5);
        }
        assert!(wait_for(|| c_specific.slots() >= 4 * 256));
        assert_eq!(c_any.slots(), 0, "the exact source match must win");
        // An empty name takes whatever arrives.
        let sock2 = open(allow_loopback());
        let (catch_all, c_all) = new_stream(2);
        sock2
            .add_stream(key(""), catch_all)
            .map_err(|e| e.0)
            .unwrap();
        Sender::new(sock2.local_addr(), "whatever").send(0.5);
        assert!(wait_for(|| c_all.slots() >= 256));
    }

    #[test]
    fn the_default_policy_refuses_strangers_but_an_explicit_source_is_trusted() {
        // Nobody on the allow list: a wildcard stream hears nothing.
        let sock = open(RxPolicy::default());
        let (rx, cons) = new_stream(2);
        sock.add_stream(key("m"), rx).map_err(|e| e.0).unwrap();
        let mut s = Sender::new(sock.local_addr(), "m");
        for _ in 0..5 {
            s.send(0.5);
        }
        assert!(wait_for(|| RxStats::get(&sock.stats().rejected) == 5));
        assert_eq!(cons.slots(), 0);
        let seen = sock.stats().seen_senders();
        assert_eq!(seen.len(), 1, "the UI should be able to offer this sender");
        assert_eq!(
            (seen[0].ip, seen[0].name.as_str(), seen[0].packets),
            (LOOPBACK, "m", 5)
        );

        // Adding the sender to the allow list takes effect immediately.
        sock.set_policy(allow_loopback()).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        s.send(0.5);
        assert!(wait_for(|| cons.slots() >= 256), "allow list ignored");

        // A stream that names its source IP needs no allow-list entry.
        let sock = open(RxPolicy::default());
        let (rx, cons) = new_stream(2);
        let exact = StreamKey {
            source: Some(LOOPBACK),
            name: FixedStr::from_str_trunc("m"),
        };
        sock.add_stream(exact, rx).map_err(|e| e.0).unwrap();
        Sender::new(sock.local_addr(), "m").send(0.5);
        assert!(wait_for(|| cons.slots() >= 256));
        assert_eq!(RxStats::get(&sock.stats().rejected), 0);
    }

    #[test]
    fn unknown_names_and_garbage_are_counted_not_delivered() {
        let sock = open(allow_loopback());
        let (rx, cons) = new_stream(2);
        sock.add_stream(key("mine"), rx).map_err(|e| e.0).unwrap();
        Sender::new(sock.local_addr(), "other").send(0.5);
        let raw = UdpSocket::bind((LOOPBACK, 0)).unwrap();
        for junk in [
            &b"GET / HTTP/1.1\r\n\r\n"[..],
            &b"VBAN"[..],
            &[0u8; 3000][..],
            &b""[..],
        ] {
            let _ = raw.send_to(junk, sock.local_addr());
        }
        assert!(wait_for(|| {
            RxStats::get(&sock.stats().unmatched) == 1 && RxStats::get(&sock.stats().malformed) >= 3
        }));
        assert_eq!(cons.slots(), 0);
    }

    #[test]
    fn removing_a_stream_returns_it_with_its_ring_and_duplicates_are_refused() {
        let sock = open(allow_loopback());
        let (rx, cons) = new_stream(2);
        sock.add_stream(key("a"), rx).map_err(|e| e.0).unwrap();

        let (dup, _c) = new_stream(2);
        let err = sock.add_stream(key("a"), dup).unwrap_err();
        assert!(
            err.1.is_some(),
            "a refused stream must come back, ring and all"
        );

        let mut s = Sender::new(sock.local_addr(), "a");
        s.send(0.5);
        assert!(wait_for(|| cons.slots() >= 256));

        let back = sock
            .remove_stream(key("a"))
            .unwrap()
            .expect("stream was registered");
        let _producer = back.into_producer(); // can be wired to another socket
        s.send(0.5);
        assert!(wait_for(|| RxStats::get(&sock.stats().unmatched) == 1));
        assert!(sock.remove_stream(key("a")).unwrap().is_none());
    }

    #[test]
    fn at_most_sixteen_streams_per_socket() {
        let sock = open(allow_loopback());
        for i in 0..MAX_LAN_STREAMS {
            let (rx, c) = new_stream(2);
            std::mem::forget(c);
            sock.add_stream(key(&format!("s{i}")), rx)
                .map_err(|e| e.0)
                .unwrap();
        }
        let (rx, _c) = new_stream(2);
        assert!(matches!(
            sock.add_stream(key("one-too-many"), rx),
            Err((RxError::Full, Some(_)))
        ));
    }

    #[test]
    fn dropping_the_socket_stops_the_thread_promptly() {
        let sock = open(allow_loopback());
        let t = Instant::now();
        drop(sock);
        assert!(
            t.elapsed() < Duration::from_millis(250),
            "shutdown took {:?}",
            t.elapsed()
        );
    }

    #[test]
    fn a_busy_port_is_an_error_not_a_silent_share() {
        let first = open(allow_loopback());
        let second = RxSocket::bind(first.local_addr(), RxPolicy::default(), None);
        assert!(matches!(second, Err(RxError::Io(_))));
    }

    #[test]
    fn telemetry_shows_how_stale_a_stream_is() {
        use audio_types::Telemetry;
        let sock = open(allow_loopback());
        let telemetry = Arc::new(Telemetry::default());
        let (mut rx, _cons) = new_stream(2);
        rx.attach_telemetry(telemetry.clone(), 3);
        sock.add_stream(key("t"), rx).map_err(|e| e.0).unwrap();
        assert!(wait_for(|| telemetry.lan_rx[3]
            .last_packet_ms_ago
            .load(Relaxed)
            == NEVER));
        Sender::new(sock.local_addr(), "t").send(0.1);
        assert!(wait_for(|| telemetry.lan_rx[3]
            .last_packet_ms_ago
            .load(Relaxed)
            < 100));
        std::thread::sleep(Duration::from_millis(80));
        let age = telemetry.lan_rx[3].last_packet_ms_ago.load(Relaxed);
        assert!((60..1000).contains(&age), "age {age} ms");
    }
}
