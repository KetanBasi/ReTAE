//! TX: bus output ring -> channel map -> packetize -> `send_to` (PLAN.md §5.2; CLAUDE.md law 3).
//!
//! ```text
//!  engine (RT) --producer--> bus ring --consumer--> TxCore::pump --> UDP send_to
//!                                                      ^
//!                         thread: park_timeout(1 ms) loop (unpark = "wake now")
//! ```
//!
//! ? Why a 1 ms tick and not a futex that the RT thread signals: waking another thread is a
//! ? syscall, and `process_block` may not make any (CLAUDE.md law 1). So the TX thread looks
//! ? at the ring every millisecond; a packet leaves 0..1 ms after the engine produced it
//! ? (0.5 ms on average), inside the latency budget. `park_timeout` is a futex wait on
//! ? Linux/Android and `WaitOnAddress` on Windows; `TxStream::wake` lets a NON-RT producer
//! ? cut the wait short. (PLAN.md named `atomic-wait`, but it has no timed wait.)
//!
//! The packet loop is in `TxCore`, which has no thread and no socket, so duplication order,
//! channel mapping and backlog trimming are tested without timing.

#[cfg(feature = "opus")]
use crate::opus_codec::OpusPacketizer;
use crate::packet::{PcmPacketizer, map_channels};
use crate::qos::{self, QosGuard, QosOutcome};
use crate::timer::TimerResolution;
use audio_types::{
    ENGINE_SAMPLE_RATE, LanTxConfig, MAX_CHANNELS, VBAN_MAX_PACKET_BYTES, WireCodec,
};
use rtrb::Consumer;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often the TX thread looks at the ring when nothing wakes it earlier.
pub const TICK: Duration = Duration::from_millis(1);

/// More audio than this waiting in the ring (40 ms) means the thread was stalled or attached
/// late: the oldest audio is dropped instead of being sent so late that it is useless.
/// ? A short stall (a few ms) is *not* dropped: it goes out as a quick burst and the receiver's
/// ? jitter buffer absorbs it. The bus ring itself holds 4096 frames (85 ms).
pub const BACKLOG_CAP_FRAMES: usize = 1920;

#[derive(Debug)]
pub enum TxError {
    Io(io::Error),
    /// A configuration value that can never work (the text says which).
    BadConfig(&'static str),
    /// The config asks for Opus but this build has no `opus` feature.
    CodecUnavailable,
}

impl From<io::Error> for TxError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl core::fmt::Display for TxError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "socket error: {e}"),
            Self::BadConfig(why) => write!(f, "invalid stream configuration: {why}"),
            Self::CodecUnavailable => f.write_str("this build has no Opus support"),
        }
    }
}

impl std::error::Error for TxError {}

/// Counters the UI can read at any time.
#[derive(Debug, Default)]
pub struct TxStats {
    /// Distinct packets built and handed to the socket (duplicates not included).
    pub packets: AtomicU64,
    /// Bytes handed to the socket, duplicates included.
    pub bytes: AtomicU64,
    /// Second copies sent (`LanTxConfig::duplicate`).
    pub duplicates: AtomicU64,
    /// `send_to` failures (network down, no route...). The audio is dropped, not retried:
    /// a stale packet is worse than a missing one.
    pub send_errors: AtomicU32,
    /// A packet the encoder refused (should not happen).
    pub encode_errors: AtomicU32,
    /// Frames thrown away because the ring held more than `BACKLOG_CAP_FRAMES`.
    pub dropped_frames: AtomicU64,
    /// Longest time between two wake-ups of the TX thread, in microseconds. Next to 1000 is
    /// healthy; ~15600 on Windows means the 1 ms timer request did not take (PLAN.md §12).
    pub max_tick_gap_us: AtomicU32,
}

// SECTION: Validation
fn validate(cfg: &LanTxConfig, src_channels: usize) -> Result<SocketAddrV4, TxError> {
    if src_channels == 0 || src_channels > MAX_CHANNELS {
        return Err(TxError::BadConfig(
            "the source ring must have 1..=8 channels",
        ));
    }
    if cfg.channels == 0 || usize::from(cfg.channels) > MAX_CHANNELS {
        return Err(TxError::BadConfig("channels must be 1..=8"));
    }
    if cfg.port == 0 {
        return Err(TxError::BadConfig("port 0 is not a destination"));
    }
    let ip = Ipv4Addr::from(cfg.peer_ipv4);
    // CLAUDE.md law 3: audio is unicast only; broadcast/multicast are for discovery.
    if ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast() {
        return Err(TxError::BadConfig("audio goes to one unicast address"));
    }
    Ok(SocketAddrV4::new(ip, cfg.port))
}
// !SECTION: Validation

// SECTION: Packetizer choice
enum Packetizer {
    Pcm(PcmPacketizer),
    #[cfg(feature = "opus")]
    Opus(OpusPacketizer),
}

impl Packetizer {
    fn new(cfg: &LanTxConfig) -> Result<Self, TxError> {
        let channels = usize::from(cfg.channels);
        match cfg.codec {
            WireCodec::Pcm { data_type } => PcmPacketizer::new(
                ENGINE_SAMPLE_RATE,
                channels,
                data_type,
                cfg.frames_per_packet,
                cfg.stream_name,
            )
            .map(Self::Pcm)
            .ok_or(TxError::BadConfig(
                "unsupported PCM format or channel count",
            )),
            #[cfg(feature = "opus")]
            WireCodec::Opus {
                bitrate_kbps,
                frame,
            } => OpusPacketizer::new(channels, bitrate_kbps, frame, cfg.stream_name)
                .map(Self::Opus)
                .ok_or(TxError::BadConfig("Opus carries 1 or 2 channels")),
            #[cfg(not(feature = "opus"))]
            WireCodec::Opus { .. } => Err(TxError::CodecUnavailable),
        }
    }

    fn frames(&self) -> usize {
        match self {
            Self::Pcm(p) => p.frames(),
            #[cfg(feature = "opus")]
            Self::Opus(p) => p.frames(),
        }
    }

    fn build(&mut self, samples: &[f32], out: &mut [u8; VBAN_MAX_PACKET_BYTES]) -> Option<usize> {
        match self {
            Self::Pcm(p) => p.build(samples, out),
            #[cfg(feature = "opus")]
            Self::Opus(p) => p.build(samples, out),
        }
    }
}
// !SECTION: Packetizer choice

// SECTION: Core (no thread, no socket)
/// One outgoing stream's packet loop.
struct TxCore {
    consumer: Consumer<f32>,
    src_channels: usize,
    dst_channels: usize,
    packetizer: Packetizer,
    /// Frames one packet consumes from the ring.
    frames: usize,
    duplicate: bool,
    stats: Arc<TxStats>,
    /// `frames * src_channels` samples read from the ring.
    scratch_in: Vec<f32>,
    /// The same frames mapped to the wire's channel count.
    scratch_out: Vec<f32>,
    wire: [u8; VBAN_MAX_PACKET_BYTES],
    /// The previous packet, kept to send it a second time (`duplicate`).
    prev: [u8; VBAN_MAX_PACKET_BYTES],
    prev_len: usize,
}

/// How a datagram leaves; `TxStream` plugs in the socket, tests plug in a `Vec`.
type SendFn<'a> = &'a mut dyn FnMut(&[u8]) -> io::Result<()>;

impl TxCore {
    /// On failure the consumer is handed back: the ring belongs to the port pool and must be
    /// reused, never dropped.
    fn new(
        cfg: &LanTxConfig,
        src_channels: usize,
        consumer: Consumer<f32>,
        stats: Arc<TxStats>,
    ) -> Result<Self, (TxError, Consumer<f32>)> {
        let packetizer = match Packetizer::new(cfg) {
            Ok(p) => p,
            Err(e) => return Err((e, consumer)),
        };
        let dst_channels = usize::from(cfg.channels);
        let frames = packetizer.frames();
        Ok(Self {
            consumer,
            src_channels,
            dst_channels,
            packetizer,
            frames,
            duplicate: cfg.duplicate,
            stats,
            scratch_in: vec![0.0; frames * src_channels],
            scratch_out: vec![0.0; frames * dst_channels],
            wire: [0; VBAN_MAX_PACKET_BYTES],
            prev: [0; VBAN_MAX_PACKET_BYTES],
            prev_len: 0,
        })
    }

    fn into_consumer(self) -> Consumer<f32> {
        self.consumer
    }

    /// Send every whole packet that is waiting in the ring.
    fn pump(&mut self, send: SendFn<'_>) {
        self.trim_backlog();
        let need = self.frames * self.src_channels;
        while self.consumer.slots() >= need {
            // ? `read_chunk` borrows the ring; the two slices are the (possibly wrapped) data.
            // ? Copy them out, then `commit_all` releases the space to the producer.
            let Ok(chunk) = self.consumer.read_chunk(need) else {
                break;
            };
            let (a, b) = chunk.as_slices();
            self.scratch_in[..a.len()].copy_from_slice(a);
            self.scratch_in[a.len()..need].copy_from_slice(b);
            chunk.commit_all();

            map_channels(
                &self.scratch_in,
                self.src_channels,
                &mut self.scratch_out,
                self.dst_channels,
                self.frames,
            );
            let Some(len) = self.packetizer.build(&self.scratch_out, &mut self.wire) else {
                self.stats.encode_errors.fetch_add(1, Relaxed);
                continue;
            };

            // ? The copy of packet k-1 goes out BEFORE packet k. If k-1 was lost, the receiver
            // ? then gets k-1 and k in order (no hole, no reordering to wait out); if k-1 did
            // ? arrive, the copy is simply counted as a duplicate.
            if self.duplicate && self.prev_len > 0 {
                self.send_prev(send);
            }
            self.send_wire(send, len);
            self.stats.packets.fetch_add(1, Relaxed);
            if self.duplicate {
                self.prev[..len].copy_from_slice(&self.wire[..len]);
                self.prev_len = len;
            }
        }
    }

    /// Send the packet just built.
    fn send_wire(&self, send: SendFn<'_>, len: usize) {
        match send(&self.wire[..len]) {
            Ok(()) => {
                self.stats.bytes.fetch_add(len as u64, Relaxed);
            }
            Err(_) => {
                self.stats.send_errors.fetch_add(1, Relaxed);
            }
        }
    }

    /// Send the stored previous packet a second time.
    fn send_prev(&self, send: SendFn<'_>) {
        match send(&self.prev[..self.prev_len]) {
            Ok(()) => {
                self.stats.bytes.fetch_add(self.prev_len as u64, Relaxed);
                self.stats.duplicates.fetch_add(1, Relaxed);
            }
            Err(_) => {
                self.stats.send_errors.fetch_add(1, Relaxed);
            }
        }
    }

    /// If far too much audio is queued, drop the oldest and keep two packets' worth.
    fn trim_backlog(&mut self) {
        let queued = self.consumer.slots() / self.src_channels;
        if queued <= BACKLOG_CAP_FRAMES {
            return;
        }
        let drop_frames = queued - 2 * self.frames;
        // ? `drop_frames * src_channels` keeps the read position on a frame boundary.
        if let Ok(chunk) = self.consumer.read_chunk(drop_frames * self.src_channels) {
            chunk.commit_all();
            self.stats
                .dropped_frames
                .fetch_add(drop_frames as u64, Relaxed);
        }
    }
}
// !SECTION: Core (no thread, no socket)

// SECTION: Stream (thread + socket)
/// An outgoing stream: its socket, its thread, its counters. Dropping it stops the thread.
pub struct TxStream {
    stop: Arc<AtomicBool>,
    stats: Arc<TxStats>,
    local: SocketAddrV4,
    dest: SocketAddrV4,
    frames: usize,
    qos: QosOutcome,
    // ? Never read: it only has to live as long as the stream (on Windows it is the qWAVE flow
    // ? that makes the packets priority-marked; dropping it removes the flow).
    _qos_guard: QosGuard,
    thread: Option<JoinHandle<Consumer<f32>>>,
}

impl TxStream {
    /// Start sending what appears in `consumer` (interleaved `f32`, `src_channels` per frame:
    /// the `channels` given to `AttachBusOutput`) to `cfg.peer_ipv4:cfg.port`.
    ///
    /// On failure the ring's consumer comes back (except if the OS could not start the thread).
    pub fn spawn(
        cfg: LanTxConfig,
        consumer: Consumer<f32>,
        src_channels: usize,
    ) -> Result<Self, (TxError, Option<Consumer<f32>>)> {
        Self::spawn_from(
            SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0),
            cfg,
            consumer,
            src_channels,
        )
    }

    /// Like `spawn`, sending from `local` (a fixed source port, or an interface address).
    pub fn spawn_from(
        local: SocketAddrV4,
        cfg: LanTxConfig,
        consumer: Consumer<f32>,
        src_channels: usize,
    ) -> Result<Self, (TxError, Option<Consumer<f32>>)> {
        let dest = match validate(&cfg, src_channels) {
            Ok(d) => d,
            Err(e) => return Err((e, Some(consumer))),
        };
        let stats = Arc::new(TxStats::default());
        let core = match TxCore::new(&cfg, src_channels, consumer, stats.clone()) {
            Ok(c) => c,
            Err((e, consumer)) => return Err((e, Some(consumer))),
        };
        let frames = core.frames;
        let (socket, local, guard, outcome) = match open_socket(local, dest, cfg.dscp) {
            Ok(opened) => opened,
            Err(e) => return Err((e, Some(core.into_consumer()))),
        };

        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (stop, stats) = (stop.clone(), stats.clone());
            std::thread::Builder::new()
                .name(format!("vban-tx-{}", cfg.stream_name.as_str()))
                .spawn(move || run(core, &socket, dest, &stop, &stats))
                // ? If the OS can't start the thread, the closure (and with it the consumer)
                // ? is gone; there is nothing to hand back.
                .map_err(|e| (TxError::Io(e), None))?
        };
        Ok(Self {
            stop,
            stats,
            local,
            dest,
            frames,
            qos: outcome,
            _qos_guard: guard,
            thread: Some(thread),
        })
    }

    pub fn stats(&self) -> &Arc<TxStats> {
        &self.stats
    }

    /// The socket's own address (its port is what the receiver sees as the source).
    pub fn local_addr(&self) -> SocketAddrV4 {
        self.local
    }

    pub fn dest(&self) -> SocketAddrV4 {
        self.dest
    }

    /// Frames in each packet (after `frames_per_packet` auto-planning).
    pub fn frames_per_packet(&self) -> usize {
        self.frames
    }

    /// Whether DSCP / qWAVE marking took effect.
    pub fn qos(&self) -> QosOutcome {
        self.qos
    }

    /// End the current wait now and look at the ring. For NON-RT producers only (tests, a
    /// device thread): the RT thread must not call this, it is a syscall.
    pub fn wake(&self) {
        if let Some(t) = &self.thread {
            t.thread().unpark();
        }
    }

    /// Stop the thread and take the ring's consumer back so the port pool can reuse the ring.
    pub fn stop(mut self) -> Option<Consumer<f32>> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> Option<Consumer<f32>> {
        self.stop.store(true, Relaxed);
        let thread = self.thread.take()?;
        thread.thread().unpark();
        thread.join().ok()
    }
}

impl Drop for TxStream {
    fn drop(&mut self) {
        // ? The consumer is dropped here; callers that want the ring back use `stop()`.
        let _ = self.shutdown();
    }
}

/// Ask the OS for the same 256 KB of send buffer as the RX side; returns what it granted.
/// ? Matters when the TX thread sends a backlog burst (up to 40 ms of audio at once) on a
/// ? default-sized buffer; a refusal just leaves the default in place.
pub(crate) fn grow_send_buffer(socket: &UdpSocket) -> usize {
    let sock = socket2::SockRef::from(socket);
    let _ = sock.set_send_buffer_size(crate::rx::SOCKET_BUFFER_BYTES);
    sock.send_buffer_size().unwrap_or(0)
}

fn open_socket(
    local: SocketAddrV4,
    dest: SocketAddrV4,
    dscp: u8,
) -> Result<(UdpSocket, SocketAddrV4, QosGuard, QosOutcome), TxError> {
    let socket = UdpSocket::bind(local)?;
    grow_send_buffer(&socket);
    let local = match socket.local_addr()? {
        SocketAddr::V4(a) => a,
        SocketAddr::V6(_) => return Err(TxError::Io(io::ErrorKind::Unsupported.into())),
    };
    let (guard, outcome) = qos::mark(&socket, dest, dscp);
    Ok((socket, local, guard, outcome))
}

/// The TX thread body. Returns the consumer when asked to stop.
fn run(
    mut core: TxCore,
    socket: &UdpSocket,
    dest: SocketAddrV4,
    stop: &AtomicBool,
    stats: &TxStats,
) -> Consumer<f32> {
    crate::hook::thread_started(crate::hook::ThreadRole::Tx);
    // ? Held for the thread's life: Windows needs the 1 ms timer request or the tick is 15 ms.
    let _timer = TimerResolution::request_1ms();
    let mut send = |pkt: &[u8]| socket.send_to(pkt, dest).map(|_| ());
    let mut last = Instant::now();
    while !stop.load(Relaxed) {
        let now = Instant::now();
        let gap_us = u32::try_from(now.duration_since(last).as_micros()).unwrap_or(u32::MAX);
        last = now;
        stats.max_tick_gap_us.fetch_max(gap_us, Relaxed);

        core.pump(&mut send);
        std::thread::park_timeout(TICK);
    }
    core.into_consumer()
}
// !SECTION: Stream (thread + socket)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_send_buffer_is_grown_and_never_smaller_than_the_os_default() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let default = socket2::SockRef::from(&socket).send_buffer_size().unwrap();
        let granted = grow_send_buffer(&socket);
        assert!(granted >= default, "{granted} < {default}");
        assert!(granted >= 200_000, "granted only {granted} bytes");
    }
    use crate::packet::{RxPayload, decode_pcm, parse_audio_packet};
    use audio_types::{FixedStr, VbanDataType};
    use rtrb::{Producer, RingBuffer};

    const RING_SAMPLES: usize = MAX_CHANNELS * 4096;

    fn cfg(channels: u8, duplicate: bool) -> LanTxConfig {
        LanTxConfig {
            peer_ipv4: [127, 0, 0, 1],
            port: 6980,
            stream_name: FixedStr::from_str_trunc("tx-test"),
            channels,
            codec: WireCodec::Pcm {
                data_type: VbanDataType::Float32,
            },
            frames_per_packet: 128,
            dscp: 46,
            duplicate,
        }
    }

    fn make_core(cfg: &LanTxConfig, src_channels: usize) -> (TxCore, Producer<f32>, Arc<TxStats>) {
        let (prod, cons) = RingBuffer::<f32>::new(RING_SAMPLES);
        let stats = Arc::new(TxStats::default());
        let core = TxCore::new(cfg, src_channels, cons, stats.clone())
            .map_err(|(e, _)| e)
            .unwrap();
        (core, prod, stats)
    }

    fn push(prod: &mut Producer<f32>, samples: impl IntoIterator<Item = f32>) {
        for s in samples {
            prod.push(s).unwrap();
        }
    }

    /// Run one pump and collect the datagrams.
    fn pump(core: &mut TxCore) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        core.pump(&mut |pkt: &[u8]| {
            out.push(pkt.to_vec());
            Ok(())
        });
        out
    }

    /// (frame counter, samples) of a PCM Float32 datagram.
    fn decode(pkt: &[u8]) -> (u32, Vec<f32>) {
        let parsed = parse_audio_packet(pkt).unwrap();
        let RxPayload::Pcm { data_type, bytes } = parsed.payload else {
            panic!("expected PCM")
        };
        let mut out = vec![0.0; 128 * 8];
        let n = decode_pcm(data_type, bytes, &mut out).unwrap();
        out.truncate(n);
        (parsed.header.frame_counter, out)
    }

    #[test]
    fn packets_are_numbered_and_carry_the_ring_samples_in_order() {
        let (mut core, mut prod, stats) = make_core(&cfg(2, false), 2);
        // 3 whole packets + 50 frames: the partial packet must wait.
        push(&mut prod, (0..(3 * 128 + 50) * 2).map(|i| i as f32));
        let sent = pump(&mut core);
        assert_eq!(sent.len(), 3);
        for (k, pkt) in sent.iter().enumerate() {
            let (counter, samples) = decode(pkt);
            assert_eq!(counter, k as u32);
            let expect: Vec<f32> = (0..256).map(|i| (k * 256 + i) as f32).collect();
            assert_eq!(samples, expect, "packet {k}");
        }
        assert_eq!(core.consumer.slots(), 100); // 50 frames x 2 channels still queued
        assert!(pump(&mut core).is_empty(), "no new data, nothing sent");

        // The rest of the packet arrives: now it goes out, with the next counter.
        push(&mut prod, (0..78 * 2).map(|i| (3 * 256 + 100 + i) as f32));
        let sent = pump(&mut core);
        assert_eq!(sent.len(), 1);
        assert_eq!(decode(&sent[0]).0, 3);
        assert_eq!(stats.packets.load(Relaxed), 4);
        assert_eq!(stats.dropped_frames.load(Relaxed), 0);
    }

    #[test]
    fn the_wire_channel_count_follows_the_config_not_the_ring() {
        // Ring stereo, wire mono: the average.
        let (mut c, mut prod, _) = make_core(&cfg(1, false), 2);
        push(&mut prod, (0..128).flat_map(|_| [1.0, 0.0]));
        let (_, mono) = decode(&pump(&mut c)[0]);
        assert_eq!(mono.len(), 128);
        assert!(mono.iter().all(|&v| v == 0.5));

        // Ring mono, wire stereo: duplicated.
        let (mut c, mut prod, _) = make_core(&cfg(2, false), 1);
        push(&mut prod, (0..128).map(|i| i as f32));
        let (_, st) = decode(&pump(&mut c)[0]);
        assert_eq!(st.len(), 256);
        assert!(
            st.chunks(2)
                .enumerate()
                .all(|(i, f)| f == [i as f32, i as f32])
        );

        // Ring 8 channels (a VoiceMeeter bus), wire stereo: the first two channels.
        let (mut c, mut prod, _) = make_core(&cfg(2, false), 8);
        push(&mut prod, (0..128).flat_map(|_| (0..8).map(|ch| ch as f32)));
        let (_, st) = decode(&pump(&mut c)[0]);
        assert!(st.chunks(2).all(|f| f == [0.0, 1.0]));
    }

    #[test]
    fn duplication_sends_each_packet_again_one_packet_later_and_in_order() {
        let (mut core, mut prod, stats) = make_core(&cfg(2, true), 2);
        push(&mut prod, std::iter::repeat_n(0.25, 3 * 128 * 2));
        let counters: Vec<u32> = pump(&mut core).iter().map(|p| decode(p).0).collect();
        // k-1's copy goes out just before k: 0, [0 1], [1 2]. Packet 2's own copy comes next time.
        assert_eq!(counters, [0, 0, 1, 1, 2]);
        assert_eq!(stats.packets.load(Relaxed), 3);
        assert_eq!(stats.duplicates.load(Relaxed), 2);

        push(&mut prod, std::iter::repeat_n(0.25, 128 * 2));
        let counters: Vec<u32> = pump(&mut core).iter().map(|p| decode(p).0).collect();
        assert_eq!(counters, [2, 3]);
    }

    #[test]
    fn a_duplicate_is_byte_identical_to_its_original() {
        let (mut core, mut prod, _) = make_core(&cfg(2, true), 2);
        push(&mut prod, (0..2 * 128 * 2).map(|i| i as f32));
        let sent = pump(&mut core);
        assert_eq!(sent.len(), 3); // 0, 0', 1
        assert_eq!(sent[0], sent[1]);
        assert_ne!(sent[1], sent[2]);
    }

    #[test]
    fn without_duplication_nothing_is_repeated() {
        let (mut core, mut prod, stats) = make_core(&cfg(2, false), 2);
        push(&mut prod, std::iter::repeat_n(0.0, 5 * 128 * 2));
        assert_eq!(pump(&mut core).len(), 5);
        assert_eq!(stats.duplicates.load(Relaxed), 0);
    }

    #[test]
    fn a_stall_below_the_cap_is_sent_as_a_burst_not_dropped() {
        let (mut core, mut prod, stats) = make_core(&cfg(2, false), 2);
        // 1500 frames = 11 whole packets + 92 frames; under the 1920-frame cap.
        push(&mut prod, std::iter::repeat_n(0.1, 1500 * 2));
        assert_eq!(pump(&mut core).len(), 11);
        assert_eq!(stats.dropped_frames.load(Relaxed), 0);
    }

    #[test]
    fn a_late_start_is_trimmed_to_the_freshest_audio() {
        let (mut core, mut prod, stats) = make_core(&cfg(2, false), 2);
        // The ring is full (4000 frames); each sample carries its frame number.
        push(&mut prod, (0..4000).flat_map(|f| [f as f32, f as f32]));
        let sent = pump(&mut core);
        // Two packets' worth is kept, and it is the newest.
        assert_eq!(sent.len(), 2);
        assert_eq!(stats.dropped_frames.load(Relaxed), 4000 - 256);
        let (_, first) = decode(&sent[0]);
        let (_, last) = decode(&sent[1]);
        assert_eq!(first[0], (4000 - 256) as f32);
        assert_eq!(last[last.len() - 1], 3999.0);
    }

    #[test]
    fn send_failures_are_counted_and_the_audio_is_still_consumed() {
        let (mut core, mut prod, stats) = make_core(&cfg(2, false), 2);
        push(&mut prod, std::iter::repeat_n(0.0, 4 * 128 * 2));
        core.pump(&mut |_: &[u8]| Err(io::ErrorKind::NetworkUnreachable.into()));
        assert_eq!(stats.send_errors.load(Relaxed), 4);
        assert_eq!(stats.bytes.load(Relaxed), 0);
        // ? The ring is drained anyway: holding the audio back would only overflow the ring
        // ? and send stale packets when the network returns.
        assert_eq!(core.consumer.slots(), 0);
    }

    #[test]
    fn packets_use_the_planned_frame_count() {
        let mut c = cfg(2, false);
        c.frames_per_packet = 0; // auto
        let (core, _prod, _) = make_core(&c, 2);
        assert_eq!(core.frames, 128);
        c.frames_per_packet = 64;
        let (core, _prod, _) = make_core(&c, 2);
        assert_eq!(core.frames, 64);
    }

    // SECTION: Validation
    fn spawn_err(cfg: LanTxConfig, src_channels: usize) -> (TxError, bool) {
        let (_prod, cons) = RingBuffer::<f32>::new(1024);
        match TxStream::spawn(cfg, cons, src_channels) {
            Ok(_) => panic!("expected an error"),
            Err((e, back)) => (e, back.is_some()),
        }
    }

    #[test]
    fn bad_configs_are_refused_and_the_ring_comes_back() {
        for (name, mutate) in [
            (
                "multicast",
                (|c: &mut LanTxConfig| c.peer_ipv4 = [239, 1, 2, 3]) as fn(&mut LanTxConfig),
            ),
            ("broadcast", |c| c.peer_ipv4 = [255, 255, 255, 255]),
            ("unspecified", |c| c.peer_ipv4 = [0, 0, 0, 0]),
            ("port 0", |c| c.port = 0),
            ("0 channels", |c| c.channels = 0),
            ("9 channels", |c| c.channels = 9),
        ] {
            let mut c = cfg(2, false);
            mutate(&mut c);
            let (err, got_ring_back) = spawn_err(c, 2);
            assert!(matches!(err, TxError::BadConfig(_)), "{name}: {err}");
            assert!(got_ring_back, "{name}: the ring must be returned for reuse");
        }
        let (err, back) = spawn_err(cfg(2, false), 0);
        assert!(matches!(err, TxError::BadConfig(_)) && back);
        let (err, back) = spawn_err(cfg(2, false), 9);
        assert!(matches!(err, TxError::BadConfig(_)) && back);
    }

    #[test]
    fn an_unsupported_pcm_format_is_refused() {
        let mut c = cfg(2, false);
        c.codec = WireCodec::Pcm {
            data_type: VbanDataType::Bits12,
        };
        let (err, back) = spawn_err(c, 2);
        assert!(matches!(err, TxError::BadConfig(_)) && back, "{err}");
    }

    #[cfg(not(feature = "opus"))]
    #[test]
    fn opus_without_the_feature_says_so() {
        let mut c = cfg(2, false);
        c.codec = WireCodec::Opus {
            bitrate_kbps: 96,
            frame: audio_types::OpusFrame::Ms5,
        };
        let (err, back) = spawn_err(c, 2);
        assert!(matches!(err, TxError::CodecUnavailable) && back);
    }
    // !SECTION: Validation

    #[cfg(feature = "opus")]
    #[test]
    fn opus_frames_go_out_in_the_user_codec_slot() {
        let mut c = cfg(2, false);
        c.codec = WireCodec::Opus {
            bitrate_kbps: 96,
            frame: audio_types::OpusFrame::Ms10,
        };
        let (mut core, mut prod, _) = make_core(&c, 2);
        assert_eq!(core.frames, 480);
        push(
            &mut prod,
            (0..480 * 2).map(|i| 0.3 * (i as f32 * 0.05).sin()),
        );
        let sent = pump(&mut core);
        assert_eq!(sent.len(), 1);
        let parsed = parse_audio_packet(&sent[0]).unwrap();
        assert!(matches!(parsed.payload, RxPayload::Opus(p) if !p.is_empty()));
        assert_eq!(parsed.header.frame_counter, 0);
    }

    // SECTION: Real socket + thread
    fn receiver() -> (UdpSocket, u16) {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let port = s.local_addr().unwrap().port();
        (s, port)
    }

    fn recv(s: &UdpSocket) -> Vec<u8> {
        let mut buf = [0u8; 2048];
        let (n, _) = s.recv_from(&mut buf).expect("a datagram within 2 s");
        buf[..n].to_vec()
    }

    #[test]
    fn a_running_stream_delivers_sequential_packets_over_loopback() {
        let (rx, port) = receiver();
        let (mut prod, cons) = RingBuffer::<f32>::new(RING_SAMPLES);
        let mut c = cfg(2, false);
        c.port = port;
        let stream = TxStream::spawn(c, cons, 2).map_err(|(e, _)| e).unwrap();
        assert_eq!(stream.frames_per_packet(), 128);
        assert_eq!(stream.dest().port(), port);
        #[cfg(unix)]
        assert_eq!(stream.qos(), QosOutcome::Applied);

        push(&mut prod, (0..10 * 128 * 2).map(|i| i as f32));
        stream.wake();
        for k in 0..10u32 {
            let (counter, samples) = decode(&recv(&rx));
            assert_eq!(counter, k);
            assert_eq!(samples[0], (k * 256) as f32);
        }
        assert_eq!(stream.stats().packets.load(Relaxed), 10);
        assert_eq!(stream.stats().send_errors.load(Relaxed), 0);
        assert!(stream.stats().max_tick_gap_us.load(Relaxed) > 0);

        // Stopping hands the ring back, empty and reusable for the next stream.
        let mut cons = stream.stop().expect("the consumer comes back");
        assert_eq!(cons.slots(), 0);
        push(&mut prod, [1.0, 2.0]);
        assert_eq!(cons.pop().unwrap(), 1.0);
    }

    #[test]
    fn the_thread_ticks_without_being_woken() {
        let (rx, port) = receiver();
        let (mut prod, cons) = RingBuffer::<f32>::new(RING_SAMPLES);
        let mut c = cfg(2, false);
        c.port = port;
        let _stream = TxStream::spawn(c, cons, 2).map_err(|(e, _)| e).unwrap();
        // No `wake()`: the 1 ms tick alone must find the data.
        push(&mut prod, std::iter::repeat_n(0.5, 128 * 2));
        let started = Instant::now();
        let (counter, _) = decode(&recv(&rx));
        assert_eq!(counter, 0);
        // Generous on a loaded CI machine; the point is "ticks", not "is fast".
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn dropping_a_stream_stops_its_thread_promptly() {
        let (_rx, port) = receiver();
        let (_prod, cons) = RingBuffer::<f32>::new(RING_SAMPLES);
        let mut c = cfg(2, false);
        c.port = port;
        let stream = TxStream::spawn(c, cons, 2).map_err(|(e, _)| e).unwrap();
        let started = Instant::now();
        drop(stream);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );
    }
    // !SECTION: Real socket + thread
}
