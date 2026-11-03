//! VBAN-SERVICE (ping / identity), VBAN-TEXT (remote-control subset) and the discovery beacon
//! (PLAN.md §4.1 and §4.5; CLAUDE.md law 3: audio is unicast, broadcast is for discovery only).
//!
//! ```text
//!  RX thread:   rx.rs --SERVICE (anyone) / TEXT (peers only)--> ServiceHandler
//!                                      |  reply: unicast PING0 reply, rate limited, LAN only
//!                                      |  peers table <-- pings and replies
//!                                      |  TextEvent  --> bounded channel --> app (try_recv_command)
//!               every loop: ServiceHandler::tick --> 2 s beacon + queued pings, from the RX socket
//!  control:     Service (this module's handle): peers(), ping(), set_beacon(), set_targets()
//! ```
//!
//! !! UNVERIFIED AGAINST VOICEMEETER. The PING0 layout, the type/function bytes and the VBAN-TEXT
//! !! header below are written from the public VBAN protocol description, without a capture.
//! !! PLAN.md §4.1 and §12 require a Wireshark check against a real VoiceMeeter before anyone
//! !! relies on them: does it reply to our ping, does it accept our TEXT packets, does it read
//! !! our identity? Everything here is therefore tolerant on input (short or odd payloads are
//! !! parsed as far as they go) and conservative on output.
//!
//! # Not abusable
//! A ping is answered with a 700-byte reply, so an open responder could be used to reflect
//! traffic at a spoofed victim. The handler answers only LAN-range sources (private, link-local,
//! loopback, CGNAT for VPNs), at most one reply per source per 250 ms and 50 replies/s in total,
//! and never answers a reply (no ping-pong).

use crate::rx::ControlHandler;
use audio_types::{
    FixedStr, MAX_BUSES, MAX_STRIPS, VBAN_DEFAULT_PORT, VBAN_HEADER_SIZE, VBAN_MAGIC,
    VBAN_MAX_PAYLOAD_BYTES, VbanSubProtocol,
};
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Stream name of VBAN-SERVICE packets.
pub const SERVICE_STREAM_NAME: &str = "VBAN Service";
/// VoiceMeeter's default name for its VBAN-TEXT command stream.
pub const TEXT_STREAM_NAME: &str = "Command1";
/// How often the discovery beacon goes out while it is switched on (PLAN.md §4.5).
pub const BEACON_INTERVAL: Duration = Duration::from_secs(2);
/// A peer not heard from for this long disappears from `Service::peers`.
pub const PEER_TTL: Duration = Duration::from_secs(15);
/// Size of the PING0 payload.
pub const PING0_BYTES: usize = 676;
/// VoiceMeeter's gain range; commands outside it are clamped, not refused.
pub const GAIN_MIN_DB: f32 = -60.0;
pub const GAIN_MAX_DB: f32 = 12.0;
/// Pending text commands the app has not collected yet; the rest are dropped and counted.
pub const TEXT_QUEUE: usize = 256;

const PER_IP_MIN_GAP: Duration = Duration::from_millis(250);
const GLOBAL_REPLIES_PER_S: f32 = 50.0;
const GLOBAL_BURST: f32 = 20.0;
const MAX_PEERS: usize = 64;
const MAX_PENDING_PINGS: usize = 32;
const PENDING_TTL: Duration = Duration::from_secs(5);

// SECTION: Wire constants
const SERVICE_IDENTIFICATION: u8 = 0; // service type (header byte 5)
const FUNCTION_PING0: u8 = 0; // service function (header byte 6)
const FUNCTION_REPLY: u8 = 0x80; // OR-ed into the function of a reply
const TEXT_UTF8: u8 = 0x10; // header byte 7 high nibble: UTF-8 text (0x00 = ASCII)

// ? From memory of the VBAN spec; see the module warning. Only used to describe ourselves.
const TYPE_RECEPTOR: u32 = 0x0000_0001;
const TYPE_TRANSMITTER: u32 = 0x0000_0002;
const FEATURE_AUDIO: u32 = 0x0000_0001;

/// Our own marker inside PING0's `reservedEx` field: `"LRT1"` + a 16-byte random instance id.
/// A receiver that does not know it ignores reserved bytes; we use it to recognise our own
/// broadcast coming back (and, later, other copies of this app).
const MARKER: [u8; 4] = *b"LRT1";

/// Byte offsets inside the 676-byte PING0 payload.
mod off {
    pub const BIT_TYPE: usize = 0;
    pub const BIT_FEATURE: usize = 4;
    pub const PREFERRED_RATE: usize = 12;
    pub const MIN_RATE: usize = 16;
    pub const MAX_RATE: usize = 20;
    pub const VERSION: usize = 28;
    pub const RESERVED_EX: usize = 64;
    pub const DEVICE: usize = 164;
    pub const MANUFACTURER: usize = 228;
    pub const APPLICATION: usize = 292;
    pub const HOST: usize = 356;
    pub const USER: usize = 420;
    pub const COMMENT: usize = 548;
    pub const END: usize = 676;
}
// !SECTION: Wire constants

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // ? A panic elsewhere must not take discovery down with it: use the data anyway.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// SECTION: Header helpers
fn write_header(
    out: &mut [u8],
    sub: VbanSubProtocol,
    b5: u8,
    b6: u8,
    b7: u8,
    name: &FixedStr<16>,
    counter: u32,
) {
    out[..4].copy_from_slice(&VBAN_MAGIC);
    out[4] = sub as u8;
    out[5] = b5;
    out[6] = b6;
    out[7] = b7;
    out[8..24].copy_from_slice(&name.to_wire());
    out[24..28].copy_from_slice(&counter.to_le_bytes());
}

fn has_vban_header(data: &[u8], sub: VbanSubProtocol) -> bool {
    data.len() >= VBAN_HEADER_SIZE
        && data[..4] == VBAN_MAGIC
        && VbanSubProtocol::from_byte(data[4]) == Some(sub)
}

fn read_counter(data: &[u8]) -> u32 {
    u32::from_le_bytes([data[24], data[25], data[26], data[27]])
}

/// Sources a LAN tool answers: private, link-local, loopback, and 100.64/10 (CGNAT, used by
/// VPN overlays such as Tailscale).
pub fn is_lan(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || (o[0] == 100 && (o[1] & 0xC0) == 64)
}
// !SECTION: Header helpers

// SECTION: Identity and PING0
/// What we tell other machines about ourselves in a ping or its reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub device_name: String,
    pub manufacturer: String,
    pub application: String,
    pub host_name: String,
    /// Empty by default: a user name travels to every machine on the LAN.
    pub user_name: String,
    pub comment: String,
    pub version: [u8; 4],
}

impl Identity {
    /// `device_name` is what peers show (for example "Living room PC").
    pub fn new(device_name: &str) -> Self {
        let host = std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_default();
        let mut version = [0u8; 4];
        for (slot, part) in version.iter_mut().zip(env!("CARGO_PKG_VERSION").split('.')) {
            *slot = part.parse().unwrap_or(0);
        }
        Self {
            device_name: device_name.to_owned(),
            manufacturer: "ReTAE".to_owned(),
            application: "ReTAE".to_owned(),
            host_name: host,
            user_name: String::new(),
            comment: String::new(),
            version,
        }
    }
}

/// What a ping or reply told us about the machine that sent it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeerInfo {
    pub device_name: String,
    pub manufacturer: String,
    pub application: String,
    pub host_name: String,
    pub user_name: String,
    pub comment: String,
    pub version: [u8; 4],
    pub bit_type: u32,
    pub bit_feature: u32,
    /// Set when the identity carries our marker (another copy of this app, or ourselves).
    pub instance: Option<[u8; 16]>,
}

impl PeerInfo {
    pub fn is_lan_router(&self) -> bool {
        self.instance.is_some()
    }
}

/// A fresh random instance id.
/// ? `RandomState` is seeded from the OS random source, which is all the randomness we need
/// ? (telling two running copies apart) without adding a crate for it.
fn new_instance_id() -> [u8; 16] {
    let mut id = [0u8; 16];
    for half in id.chunks_exact_mut(8) {
        half.copy_from_slice(&RandomState::new().build_hasher().finish().to_le_bytes());
    }
    id
}

/// Copy `s` into a NUL-terminated field of `cap` bytes, cutting on a character boundary.
fn put_str(buf: &mut [u8], at: usize, cap: usize, s: &str) {
    let mut end = s.len().min(cap - 1);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    buf[at..at + end].copy_from_slice(&s.as_bytes()[..end]);
}

fn put_u32(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// A NUL-terminated wire string made safe to show: lossy UTF-8, no control characters.
fn wire_str(raw: &[u8]) -> String {
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..end])
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

/// A complete VBAN-SERVICE datagram: header plus our PING0 identity.
pub fn build_ping(
    identity: &Identity,
    instance: &[u8; 16],
    reply: bool,
    counter: u32,
    stream: &FixedStr<16>,
) -> Vec<u8> {
    let mut out = vec![0u8; VBAN_HEADER_SIZE + PING0_BYTES];
    let function = if reply {
        FUNCTION_REPLY | FUNCTION_PING0
    } else {
        FUNCTION_PING0
    };
    write_header(
        &mut out,
        VbanSubProtocol::Service,
        SERVICE_IDENTIFICATION,
        function,
        0,
        stream,
        counter,
    );
    let p = &mut out[VBAN_HEADER_SIZE..];
    put_u32(p, off::BIT_TYPE, TYPE_RECEPTOR | TYPE_TRANSMITTER);
    put_u32(p, off::BIT_FEATURE, FEATURE_AUDIO);
    // We only run at the engine rate.
    for at in [off::PREFERRED_RATE, off::MIN_RATE, off::MAX_RATE] {
        put_u32(p, at, audio_types::ENGINE_SAMPLE_RATE);
    }
    p[off::VERSION..off::VERSION + 4].copy_from_slice(&identity.version);
    p[off::RESERVED_EX..off::RESERVED_EX + 4].copy_from_slice(&MARKER);
    p[off::RESERVED_EX + 4..off::RESERVED_EX + 20].copy_from_slice(instance);
    put_str(p, off::DEVICE, 64, &identity.device_name);
    put_str(p, off::MANUFACTURER, 64, &identity.manufacturer);
    put_str(p, off::APPLICATION, 64, &identity.application);
    put_str(p, off::HOST, 64, &identity.host_name);
    put_str(p, off::USER, 128, &identity.user_name);
    put_str(p, off::COMMENT, 128, &identity.comment);
    out
}

/// The header of a VBAN-SERVICE datagram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServiceHeader {
    pub service_type: u8,
    pub function: u8,
    pub stream_name: FixedStr<16>,
    pub counter: u32,
}

impl ServiceHeader {
    pub fn is_reply(&self) -> bool {
        self.function & FUNCTION_REPLY != 0
    }
    /// A request (or reply) of the identity ping.
    pub fn is_ping(&self) -> bool {
        self.service_type == SERVICE_IDENTIFICATION
            && self.function & !FUNCTION_REPLY == FUNCTION_PING0
    }
}

/// Split a VBAN-SERVICE datagram into header and payload.
pub fn parse_service(data: &[u8]) -> Option<(ServiceHeader, &[u8])> {
    if !has_vban_header(data, VbanSubProtocol::Service) {
        return None;
    }
    let header = ServiceHeader {
        service_type: data[5],
        function: data[6],
        stream_name: FixedStr::from_wire(&data[8..24]),
        counter: read_counter(data),
    };
    Some((header, &data[VBAN_HEADER_SIZE..]))
}

/// Read a PING0 payload as far as it goes. Never fails: a short or odd payload just leaves
/// fields empty.
pub fn parse_ping0(payload: &[u8]) -> PeerInfo {
    let field = |at: usize, len: usize| -> &[u8] {
        let start = at.min(payload.len());
        &payload[start..(at + len).min(payload.len())]
    };
    let u32_at = |at: usize| -> u32 {
        let f = field(at, 4);
        let mut b = [0u8; 4];
        b[..f.len()].copy_from_slice(f);
        u32::from_le_bytes(b)
    };
    let mut version = [0u8; 4];
    let v = field(off::VERSION, 4);
    version[..v.len()].copy_from_slice(v);
    let marker = field(off::RESERVED_EX, 20);
    let instance = (marker.len() == 20 && marker[..4] == MARKER).then(|| {
        let mut id = [0u8; 16];
        id.copy_from_slice(&marker[4..]);
        id
    });
    PeerInfo {
        device_name: wire_str(field(off::DEVICE, 64)),
        manufacturer: wire_str(field(off::MANUFACTURER, 64)),
        application: wire_str(field(off::APPLICATION, 64)),
        host_name: wire_str(field(off::HOST, 64)),
        user_name: wire_str(field(off::USER, 128)),
        comment: wire_str(field(off::COMMENT, off::END - off::COMMENT)),
        version,
        bit_type: u32_at(off::BIT_TYPE),
        bit_feature: u32_at(off::BIT_FEATURE),
        instance,
    }
}
// !SECTION: Identity and PING0

// SECTION: VBAN-TEXT
/// The remote-control subset: `Strip[i].Gain/Mute` and `Bus[i].Gain/Mute` (PLAN.md §4.1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TextCommand {
    StripGain { index: u8, db: f32 },
    StripMute { index: u8, muted: bool },
    BusGain { index: u8, db: f32 },
    BusMute { index: u8, muted: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextError {
    /// Not one of the supported commands (a query, another parameter, noise).
    Unknown,
    /// The strip/bus number does not exist.
    BadIndex,
    /// The value is not a number / not a boolean, or not finite.
    BadValue,
}

/// Parse VoiceMeeter-style command text such as `Strip[0].Gain=-6; Bus[1].Mute=1`.
/// Commands are separated by `;`, `,` or a line break, names are case-insensitive, and a bad
/// command does not stop the ones after it.
pub fn parse_commands(text: &str) -> impl Iterator<Item = Result<TextCommand, TextError>> + '_ {
    text.split([';', ',', '\n', '\r'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(parse_one)
}

fn parse_one(item: &str) -> Result<TextCommand, TextError> {
    let (lhs, rhs) = item.split_once('=').ok_or(TextError::Unknown)?;
    let lhs = lhs.trim().to_ascii_lowercase();
    let rhs = rhs.trim();
    let (is_strip, rest) = if let Some(r) = lhs.strip_prefix("strip[") {
        (true, r)
    } else if let Some(r) = lhs.strip_prefix("bus[") {
        (false, r)
    } else {
        return Err(TextError::Unknown);
    };
    let (index, property) = rest.split_once("].").ok_or(TextError::Unknown)?;
    let index: usize = index.trim().parse().map_err(|_| TextError::BadIndex)?;
    if index >= if is_strip { MAX_STRIPS } else { MAX_BUSES } {
        return Err(TextError::BadIndex);
    }
    let index = index as u8; // < 16, checked above
    match property.trim() {
        "gain" => {
            let db: f32 = rhs.parse().map_err(|_| TextError::BadValue)?;
            if !db.is_finite() {
                return Err(TextError::BadValue);
            }
            let db = db.clamp(GAIN_MIN_DB, GAIN_MAX_DB);
            Ok(if is_strip {
                TextCommand::StripGain { index, db }
            } else {
                TextCommand::BusGain { index, db }
            })
        }
        "mute" => {
            let muted = parse_bool(rhs).ok_or(TextError::BadValue)?;
            Ok(if is_strip {
                TextCommand::StripMute { index, muted }
            } else {
                TextCommand::BusMute { index, muted }
            })
        }
        _ => Err(TextError::Unknown),
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "true" | "on" => Some(true),
        "false" | "off" => Some(false),
        n => n.parse::<i32>().ok().map(|n| n != 0),
    }
}

impl TextCommand {
    /// The command as text another instance (or VoiceMeeter) understands.
    pub fn to_text(&self) -> String {
        match *self {
            Self::StripGain { index, db } => format!("Strip[{index}].Gain={db};"),
            Self::StripMute { index, muted } => format!("Strip[{index}].Mute={};", u8::from(muted)),
            Self::BusGain { index, db } => format!("Bus[{index}].Gain={db};"),
            Self::BusMute { index, muted } => format!("Bus[{index}].Mute={};", u8::from(muted)),
        }
    }
}

/// A VBAN-TEXT datagram carrying `text` (UTF-8).
pub fn build_text_packet(stream: &FixedStr<16>, counter: u32, text: &str) -> Option<Vec<u8>> {
    if text.len() > VBAN_MAX_PAYLOAD_BYTES {
        return None;
    }
    let mut out = vec![0u8; VBAN_HEADER_SIZE + text.len()];
    write_header(
        &mut out,
        VbanSubProtocol::Text,
        0,
        0,
        TEXT_UTF8,
        stream,
        counter,
    );
    out[VBAN_HEADER_SIZE..].copy_from_slice(text.as_bytes());
    Some(out)
}

/// The text of a VBAN-TEXT datagram (ASCII or UTF-8 only; trailing NULs are dropped).
pub fn parse_text_packet(data: &[u8]) -> Option<&str> {
    if !has_vban_header(data, VbanSubProtocol::Text) || !matches!(data[7] & 0xF0, 0x00 | TEXT_UTF8)
    {
        return None;
    }
    let body = &data[VBAN_HEADER_SIZE..];
    let end = body.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    std::str::from_utf8(&body[..end]).ok()
}

/// Sends VBAN-TEXT commands to a peer (a VoiceMeeter on another PC, or another instance).
pub struct TextSender {
    socket: UdpSocket,
    stream: FixedStr<16>,
    counter: u32,
}

impl TextSender {
    pub fn new(stream_name: &str) -> io::Result<Self> {
        Ok(Self {
            socket: UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?,
            stream: FixedStr::from_str_trunc(stream_name),
            counter: 0,
        })
    }

    pub fn send_text(&mut self, dest: SocketAddrV4, text: &str) -> io::Result<()> {
        let pkt = build_text_packet(&self.stream, self.counter, text).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "text too long for one packet")
        })?;
        self.counter = self.counter.wrapping_add(1);
        self.socket.send_to(&pkt, dest).map(|_| ())
    }

    pub fn send_command(&mut self, dest: SocketAddrV4, command: &TextCommand) -> io::Result<()> {
        self.send_text(dest, &command.to_text())
    }
}
// !SECTION: VBAN-TEXT

// SECTION: Shared state
/// A command received over VBAN-TEXT, with who sent it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextEvent {
    pub from: Ipv4Addr,
    pub command: TextCommand,
}

/// Counters for the UI and for tests.
#[derive(Debug, Default)]
pub struct ServiceStats {
    pub pings_rx: AtomicU32,
    pub replies_tx: AtomicU32,
    pub replies_rx: AtomicU32,
    pub beacons_tx: AtomicU32,
    /// Pings that got no reply because of the per-source or global limit.
    pub rate_limited: AtomicU32,
    /// SERVICE packets from outside the LAN ranges, ignored.
    pub non_lan: AtomicU32,
    /// Our own broadcast arriving back.
    pub own_ignored: AtomicU32,
    pub malformed: AtomicU32,
    pub text_packets: AtomicU32,
    pub text_commands: AtomicU32,
    /// Text that was not a supported command.
    pub text_ignored: AtomicU32,
    /// Valid commands dropped because the app was not collecting them.
    pub text_dropped: AtomicU32,
    pub send_errors: AtomicU32,
}

/// A machine that answered (or sent) a ping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    /// Where its latest packet came from (reply address for pings).
    pub addr: SocketAddrV4,
    pub info: PeerInfo,
    /// Round trip of our last ping it answered.
    pub rtt: Option<Duration>,
    /// Time since we last heard from it.
    pub age: Duration,
}

struct PeerEntry {
    addr: SocketAddrV4,
    info: PeerInfo,
    rtt: Option<Duration>,
    last_seen: Instant,
}

struct Shared {
    identity: Mutex<Identity>,
    instance: [u8; 16],
    beacon: AtomicBool,
    targets: Mutex<Vec<SocketAddrV4>>,
    ping_queue: Mutex<Vec<SocketAddrV4>>,
    peers: Mutex<Vec<PeerEntry>>,
    counter: AtomicU32,
    stats: ServiceStats,
}

/// Where the beacon goes unless told otherwise: the limited broadcast address on the VBAN port.
/// ? `255.255.255.255` leaves through the default interface only; on a multi-homed machine add
/// ? each subnet's directed broadcast or the peers' unicast addresses with `set_targets`.
pub fn default_targets() -> Vec<SocketAddrV4> {
    vec![SocketAddrV4::new(Ipv4Addr::BROADCAST, VBAN_DEFAULT_PORT)]
}
// !SECTION: Shared state

// SECTION: Control-plane handle
/// The app's side of discovery and remote control. The other half, [`ServiceHandler`], runs on
/// the RX thread (pass it to `RxSocket::bind`).
pub struct Service {
    shared: Arc<Shared>,
    commands: Receiver<TextEvent>,
}

impl Service {
    pub fn new(identity: Identity) -> (Self, ServiceHandler) {
        let shared = Arc::new(Shared {
            identity: Mutex::new(identity),
            instance: new_instance_id(),
            beacon: AtomicBool::new(false),
            targets: Mutex::new(default_targets()),
            ping_queue: Mutex::new(Vec::new()),
            peers: Mutex::new(Vec::new()),
            counter: AtomicU32::new(1),
            stats: ServiceStats::default(),
        });
        let (tx, rx) = sync_channel(TEXT_QUEUE);
        let handler = ServiceHandler {
            shared: shared.clone(),
            commands: tx,
            limiter: ReplyLimiter::new(Instant::now()),
            pending: Vec::new(),
            next_beacon: None,
            broadcast_enabled: false,
        };
        (
            Self {
                shared,
                commands: rx,
            },
            handler,
        )
    }

    /// Machines heard from in the last [`PEER_TTL`], oldest address first.
    pub fn peers(&self) -> Vec<Peer> {
        let now = Instant::now();
        let mut list = lock(&self.shared.peers);
        list.retain(|p| now.duration_since(p.last_seen) < PEER_TTL);
        let mut out: Vec<Peer> = list
            .iter()
            .map(|p| Peer {
                addr: p.addr,
                info: p.info.clone(),
                rtt: p.rtt,
                age: now.duration_since(p.last_seen),
            })
            .collect();
        out.sort_by_key(|p| p.addr);
        out
    }

    /// Switch the 2 s broadcast beacon on or off (on while the "find devices" screen is open).
    pub fn set_beacon(&self, on: bool) {
        self.shared.beacon.store(on, Relaxed);
    }

    pub fn beacon_enabled(&self) -> bool {
        self.shared.beacon.load(Relaxed)
    }

    /// Replace the beacon destinations (broadcast addresses and/or known peers).
    pub fn set_targets(&self, targets: Vec<SocketAddrV4>) {
        *lock(&self.shared.targets) = targets;
    }

    /// Ping one address now (it leaves from the RX socket within a few ms). The round trip
    /// shows up in `peers()` once it answers.
    pub fn ping(&self, dest: SocketAddrV4) {
        lock(&self.shared.ping_queue).push(dest);
    }

    pub fn set_identity(&self, identity: Identity) {
        *lock(&self.shared.identity) = identity;
    }

    /// The next remote-control command a peer sent, if any.
    pub fn try_recv_command(&self) -> Option<TextEvent> {
        self.commands.try_recv().ok()
    }

    pub fn stats(&self) -> &ServiceStats {
        &self.shared.stats
    }

    /// This instance's random id (also inside every ping we send).
    pub fn instance_id(&self) -> [u8; 16] {
        self.shared.instance
    }
}
// !SECTION: Control-plane handle

// SECTION: RX-thread handler
/// Per-source and global limit on ping replies.
struct ReplyLimiter {
    per_ip: Vec<(Ipv4Addr, Instant)>,
    tokens: f32,
    refilled: Instant,
}

impl ReplyLimiter {
    fn new(now: Instant) -> Self {
        Self {
            per_ip: Vec::new(),
            tokens: GLOBAL_BURST,
            refilled: now,
        }
    }

    fn allow(&mut self, ip: Ipv4Addr, now: Instant) -> bool {
        let dt = now.saturating_duration_since(self.refilled).as_secs_f32();
        self.refilled = now;
        self.tokens = (self.tokens + dt * GLOBAL_REPLIES_PER_S).min(GLOBAL_BURST);

        self.per_ip
            .retain(|&(_, t)| now.saturating_duration_since(t) < PER_IP_MIN_GAP);
        if self.per_ip.iter().any(|&(i, _)| i == ip) || self.tokens < 1.0 {
            return false;
        }
        if self.per_ip.len() >= MAX_PEERS {
            self.per_ip.remove(0);
        }
        self.per_ip.push((ip, now));
        self.tokens -= 1.0;
        true
    }
}

struct Pending {
    counter: u32,
    sent: Instant,
}

/// Runs on the RX thread; created by [`Service::new`].
pub struct ServiceHandler {
    shared: Arc<Shared>,
    commands: SyncSender<TextEvent>,
    limiter: ReplyLimiter,
    pending: Vec<Pending>,
    next_beacon: Option<Instant>,
    broadcast_enabled: bool,
}

impl ServiceHandler {
    fn on_service(&mut self, data: &[u8], from: SocketAddrV4, socket: &UdpSocket) {
        let stats = &self.shared.stats;
        let Some((header, payload)) = parse_service(data) else {
            stats.malformed.fetch_add(1, Relaxed);
            return;
        };
        // Chat, real-time packets and the like are not ours.
        if !header.is_ping() {
            return;
        }
        let ip = *from.ip();
        if !is_lan(ip) {
            stats.non_lan.fetch_add(1, Relaxed);
            return;
        }
        let info = parse_ping0(payload);
        if info.instance == Some(self.shared.instance) {
            stats.own_ignored.fetch_add(1, Relaxed);
            return;
        }
        let now = Instant::now();
        if header.is_reply() {
            // ! Never answer a reply: two responders would ping-pong forever.
            stats.replies_rx.fetch_add(1, Relaxed);
            let rtt = self
                .pending
                .iter()
                .find(|p| p.counter == header.counter)
                .map(|p| now.duration_since(p.sent));
            self.note_peer(from, info, rtt, now);
            return;
        }
        stats.pings_rx.fetch_add(1, Relaxed);
        self.note_peer(from, info, None, now);
        if !self.limiter.allow(ip, now) {
            stats.rate_limited.fetch_add(1, Relaxed);
            return;
        }
        let identity = lock(&self.shared.identity).clone();
        let reply = build_ping(
            &identity,
            &self.shared.instance,
            true,
            header.counter,
            &header.stream_name,
        );
        match socket.send_to(&reply, from) {
            Ok(_) => stats.replies_tx.fetch_add(1, Relaxed),
            Err(_) => stats.send_errors.fetch_add(1, Relaxed),
        };
    }

    fn on_text(&mut self, data: &[u8], from: SocketAddrV4) {
        let stats = &self.shared.stats;
        stats.text_packets.fetch_add(1, Relaxed);
        let Some(text) = parse_text_packet(data) else {
            stats.malformed.fetch_add(1, Relaxed);
            return;
        };
        for parsed in parse_commands(text) {
            let Ok(command) = parsed else {
                stats.text_ignored.fetch_add(1, Relaxed);
                continue;
            };
            match self.commands.try_send(TextEvent {
                from: *from.ip(),
                command,
            }) {
                Ok(()) => stats.text_commands.fetch_add(1, Relaxed),
                Err(TrySendError::Full(_)) => stats.text_dropped.fetch_add(1, Relaxed),
                // The app is gone; nobody to tell.
                Err(TrySendError::Disconnected(_)) => 0,
            };
        }
    }

    fn note_peer(&self, addr: SocketAddrV4, info: PeerInfo, rtt: Option<Duration>, now: Instant) {
        let mut peers = lock(&self.shared.peers);
        if let Some(p) = peers.iter_mut().find(|p| p.addr.ip() == addr.ip()) {
            p.addr = addr;
            p.info = info;
            p.rtt = rtt.or(p.rtt);
            p.last_seen = now;
            return;
        }
        if peers.len() >= MAX_PEERS {
            peers.remove(0);
        }
        peers.push(PeerEntry {
            addr,
            info,
            rtt,
            last_seen: now,
        });
    }

    /// Send one ping request and remember when, so its reply gives a round-trip time.
    fn send_ping(&mut self, socket: &UdpSocket, dest: SocketAddrV4, now: Instant) {
        let counter = self.shared.counter.fetch_add(1, Relaxed);
        let identity = lock(&self.shared.identity).clone();
        let name = FixedStr::from_str_trunc(SERVICE_STREAM_NAME);
        let ping = build_ping(&identity, &self.shared.instance, false, counter, &name);
        // ? Kept for several seconds and not removed on the first reply: a broadcast ping can
        // ? be answered by many machines, and each should get a round trip.
        self.pending
            .retain(|p| now.saturating_duration_since(p.sent) < PENDING_TTL);
        if self.pending.len() >= MAX_PENDING_PINGS {
            self.pending.remove(0);
        }
        self.pending.push(Pending { counter, sent: now });
        if socket.send_to(&ping, dest).is_err() {
            self.shared.stats.send_errors.fetch_add(1, Relaxed);
        }
    }
}

impl ControlHandler for ServiceHandler {
    fn on_packet(
        &mut self,
        sub: VbanSubProtocol,
        data: &[u8],
        from: SocketAddrV4,
        socket: &UdpSocket,
    ) {
        match sub {
            VbanSubProtocol::Service => self.on_service(data, from, socket),
            VbanSubProtocol::Text => self.on_text(data, from),
            _ => {}
        }
    }

    fn tick(&mut self, now: Instant, socket: &UdpSocket) {
        let queued = std::mem::take(&mut *lock(&self.shared.ping_queue));
        for dest in queued {
            self.send_ping(socket, dest, now);
        }

        if !self.shared.beacon.load(Relaxed) {
            self.next_beacon = None;
            return;
        }
        if self.next_beacon.is_some_and(|t| now < t) {
            return;
        }
        // ? Needed once per socket before it may send to a broadcast address. Best effort: if
        // ? it fails the sends fail and are counted.
        if !self.broadcast_enabled {
            self.broadcast_enabled = socket.set_broadcast(true).is_ok();
        }
        let targets = lock(&self.shared.targets).clone();
        for dest in targets {
            self.send_ping(socket, dest, now);
        }
        self.shared.stats.beacons_tx.fetch_add(1, Relaxed);
        self.next_beacon = Some(now + BEACON_INTERVAL);
    }
}
// !SECTION: RX-thread handler

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> FixedStr<16> {
        FixedStr::from_str_trunc(s)
    }

    fn identity() -> Identity {
        Identity {
            device_name: "Test PC".into(),
            manufacturer: "ReTAE".into(),
            application: "ReTAE".into(),
            host_name: "testhost".into(),
            user_name: String::new(),
            comment: "hello".into(),
            version: [1, 2, 3, 0],
        }
    }

    // SECTION: PING0
    #[test]
    fn a_ping_is_header_plus_676_bytes_with_the_fields_where_the_layout_says() {
        let id = [7u8; 16];
        let pkt = build_ping(
            &identity(),
            &id,
            false,
            0x0102_0304,
            &name(SERVICE_STREAM_NAME),
        );
        assert_eq!(pkt.len(), 28 + 676);
        assert_eq!(&pkt[..4], b"VBAN");
        assert_eq!(pkt[4], 0x60, "sub-protocol SERVICE, rate bits 0");
        assert_eq!(pkt[5], 0, "service type: identification");
        assert_eq!(pkt[6], 0, "function: ping, not a reply");
        assert_eq!(&pkt[8..20], b"VBAN Service");
        assert_eq!(&pkt[24..28], &[4, 3, 2, 1], "counter is little-endian");
        let p = &pkt[28..];
        assert_eq!(&p[164..171], b"Test PC", "DeviceName at 164");
        assert_eq!(&p[292..297], b"ReTAE", "ApplicationName at 292");
        assert_eq!(&p[356..364], b"testhost", "HostName at 356");
        assert_eq!(&p[548..553], b"hello", "UserComment at 548");
        assert_eq!(&p[28..32], &[1, 2, 3, 0], "nVersion at 28");
        assert_eq!(&p[64..68], b"LRT1", "our marker in reservedEx");
        assert_eq!(&p[68..84], &id);
        // A reply only differs in the function byte.
        let reply = build_ping(
            &identity(),
            &id,
            true,
            0x0102_0304,
            &name(SERVICE_STREAM_NAME),
        );
        assert_eq!(reply[6], 0x80);
        assert_eq!(&reply[..6], &pkt[..6]);
        assert_eq!(&reply[7..], &pkt[7..]);
    }

    #[test]
    fn a_ping_round_trips_through_the_parser() {
        let id = [9u8; 16];
        let pkt = build_ping(&identity(), &id, true, 77, &name("VBAN Service"));
        let (h, payload) = parse_service(&pkt).unwrap();
        assert!(h.is_ping() && h.is_reply());
        assert_eq!(h.counter, 77);
        assert_eq!(h.stream_name.as_str(), "VBAN Service");
        let info = parse_ping0(payload);
        assert_eq!(info.device_name, "Test PC");
        assert_eq!(info.application, "ReTAE");
        assert_eq!(info.host_name, "testhost");
        assert_eq!(info.comment, "hello");
        assert_eq!(info.version, [1, 2, 3, 0]);
        assert_eq!(info.bit_type, TYPE_RECEPTOR | TYPE_TRANSMITTER);
        assert_eq!(info.instance, Some(id));
        assert!(info.is_lan_router());
    }

    #[test]
    fn long_names_are_cut_on_a_character_boundary_and_stay_terminated() {
        let mut long = identity();
        long.device_name = "é".repeat(100); // 200 bytes
        let pkt = build_ping(&long, &[0; 16], false, 1, &name("x"));
        let info = parse_ping0(&pkt[28..]);
        assert!(!info.device_name.is_empty() && info.device_name.chars().all(|c| c == 'é'));
        assert!(info.device_name.len() <= 63);
        // The next field is untouched by the overflow.
        assert_eq!(info.manufacturer, "ReTAE");
    }

    #[test]
    fn any_payload_parses_without_panicking_and_without_control_characters() {
        let good = build_ping(&identity(), &[1; 16], false, 1, &name("x"));
        for cut in 0..=good.len() - 28 {
            let _ = parse_ping0(&good[28..28 + cut]);
        }
        // Noise, including NULs, escape codes and invalid UTF-8 in every text field.
        let noise: Vec<u8> = (0..700u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        let info = parse_ping0(&noise);
        for s in [
            &info.device_name,
            &info.application,
            &info.host_name,
            &info.user_name,
            &info.comment,
        ] {
            assert!(s.chars().all(|c| !c.is_control()), "{s:?}");
        }
        let hostile = {
            let mut p = vec![0u8; 676];
            p[164..170].copy_from_slice(b"a\x1b[2Jb");
            p
        };
        assert_eq!(parse_ping0(&hostile).device_name, "a[2Jb");
    }

    #[test]
    fn service_parsing_rejects_what_is_not_a_service_packet() {
        assert!(parse_service(&[]).is_none());
        assert!(parse_service(b"VBAN").is_none());
        let mut pkt = build_ping(&identity(), &[0; 16], false, 1, &name("x"));
        pkt[4] = 0x40; // TEXT
        assert!(parse_service(&pkt).is_none());
        pkt[4] = 0x60;
        pkt[0] = b'X';
        assert!(parse_service(&pkt).is_none());
    }
    // !SECTION: PING0

    // SECTION: TEXT
    fn ok(text: &str) -> Vec<TextCommand> {
        parse_commands(text).map(Result::unwrap).collect()
    }

    #[test]
    fn the_supported_subset_parses() {
        assert_eq!(
            ok("Strip[0].Gain=-6; Strip[2].Mute=1; Bus[1].Gain=3.5; Bus[3].Mute=0"),
            [
                TextCommand::StripGain { index: 0, db: -6.0 },
                TextCommand::StripMute {
                    index: 2,
                    muted: true
                },
                TextCommand::BusGain { index: 1, db: 3.5 },
                TextCommand::BusMute {
                    index: 3,
                    muted: false
                },
            ]
        );
    }

    #[test]
    fn names_are_case_insensitive_and_separators_flexible() {
        let expect = [
            TextCommand::StripMute {
                index: 1,
                muted: true,
            },
            TextCommand::BusGain { index: 0, db: -3.0 },
        ];
        assert_eq!(ok("strip[1].mute=1,BUS[0].GAIN=-3"), expect);
        assert_eq!(ok("STRIP[1].MUTE = 1\nBus[0].Gain=-3;;\r\n"), expect);
        assert_eq!(ok("  Strip[ 1 ].Mute=true ;  Bus[0].Gain = -3.0 "), expect);
    }

    #[test]
    fn gain_is_clamped_to_voicemeeters_range_and_mute_accepts_words() {
        assert_eq!(
            ok("Strip[0].Gain=100")[0],
            TextCommand::StripGain { index: 0, db: 12.0 }
        );
        assert_eq!(
            ok("Strip[0].Gain=-500")[0],
            TextCommand::StripGain {
                index: 0,
                db: -60.0
            }
        );
        assert_eq!(
            ok("Bus[0].Mute=on")[0],
            TextCommand::BusMute {
                index: 0,
                muted: true
            }
        );
        assert_eq!(
            ok("Bus[0].Mute=off")[0],
            TextCommand::BusMute {
                index: 0,
                muted: false
            }
        );
        assert_eq!(
            ok("Bus[0].Mute=2")[0],
            TextCommand::BusMute {
                index: 0,
                muted: true
            }
        );
    }

    #[test]
    fn bad_commands_are_classified_and_do_not_stop_the_good_ones() {
        let results: Vec<_> = parse_commands(
            "Strip[16].Gain=0; Bus[16].Mute=1; Strip[0].Gain=nan; Strip[0].Gain=inf; Strip[0].Gain=abc; \
             Strip[0].Mute=maybe; Strip[0].Solo=1; Patch[0]=1; Strip[0].Gain; Strip[x].Gain=0; Strip[1].Mute=1",
        )
        .collect();
        assert_eq!(
            results,
            [
                Err(TextError::BadIndex),
                Err(TextError::BadIndex),
                Err(TextError::BadValue),
                Err(TextError::BadValue),
                Err(TextError::BadValue),
                Err(TextError::BadValue),
                Err(TextError::Unknown),
                Err(TextError::Unknown),
                Err(TextError::Unknown),
                Err(TextError::BadIndex),
                Ok(TextCommand::StripMute {
                    index: 1,
                    muted: true
                }),
            ]
        );
    }

    #[test]
    fn what_we_format_we_parse() {
        for cmd in [
            TextCommand::StripGain { index: 3, db: -6.5 },
            TextCommand::StripGain { index: 0, db: 0.0 },
            TextCommand::StripMute {
                index: 15,
                muted: true,
            },
            TextCommand::BusGain { index: 2, db: 12.0 },
            TextCommand::BusMute {
                index: 1,
                muted: false,
            },
        ] {
            assert_eq!(ok(&cmd.to_text()), [cmd], "{}", cmd.to_text());
        }
        assert_eq!(
            TextCommand::StripGain { index: 0, db: -6.0 }.to_text(),
            "Strip[0].Gain=-6;"
        );
    }

    #[test]
    fn a_text_packet_round_trips_and_is_checked() {
        let pkt = build_text_packet(&name("Command1"), 5, "Strip[0].Mute=1;").unwrap();
        assert_eq!(&pkt[..4], b"VBAN");
        assert_eq!(pkt[4], 0x40);
        assert_eq!(pkt[7], 0x10);
        assert_eq!(&pkt[8..16], b"Command1");
        assert_eq!(&pkt[24..28], &[5, 0, 0, 0]);
        assert_eq!(parse_text_packet(&pkt), Some("Strip[0].Mute=1;"));

        // Trailing NULs (some senders pad) are dropped; ASCII (0x00) is accepted.
        let mut padded = pkt.clone();
        padded[7] = 0x00;
        padded.extend_from_slice(&[0, 0, 0]);
        assert_eq!(parse_text_packet(&padded), Some("Strip[0].Mute=1;"));
        // Other text encodings, invalid UTF-8, wrong sub-protocol: refused.
        let mut wide = pkt.clone();
        wide[7] = 0x20;
        assert_eq!(parse_text_packet(&wide), None);
        let mut bad = pkt.clone();
        bad[28] = 0xFF;
        assert_eq!(parse_text_packet(&bad), None);
        let mut audio = pkt;
        audio[4] = 0x00;
        assert_eq!(parse_text_packet(&audio), None);
        assert!(
            build_text_packet(&name("x"), 0, &"a".repeat(VBAN_MAX_PAYLOAD_BYTES + 1)).is_none()
        );
    }
    // !SECTION: TEXT

    // SECTION: Handler (driven directly, no RX thread)
    fn handler() -> (Service, ServiceHandler) {
        Service::new(identity())
    }

    /// A socket the handler answers from, and a peer socket that can read the answers.
    fn pair() -> (UdpSocket, UdpSocket, SocketAddrV4) {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        peer.set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let addr = match peer.local_addr().unwrap() {
            std::net::SocketAddr::V4(a) => a,
            std::net::SocketAddr::V6(_) => unreachable!(),
        };
        (server, peer, addr)
    }

    fn recv(peer: &UdpSocket) -> Option<Vec<u8>> {
        let mut buf = [0u8; 2048];
        peer.recv_from(&mut buf)
            .ok()
            .map(|(n, _)| buf[..n].to_vec())
    }

    fn ping_from_peer(counter: u32) -> Vec<u8> {
        build_ping(
            &Identity::new("Peer"),
            &[3; 16],
            false,
            counter,
            &name(SERVICE_STREAM_NAME),
        )
    }

    fn deliver(
        h: &mut ServiceHandler,
        sub: VbanSubProtocol,
        data: &[u8],
        from: SocketAddrV4,
        socket: &UdpSocket,
    ) {
        h.on_packet(sub, data, from, socket);
    }

    #[test]
    fn a_ping_is_answered_with_a_unicast_reply_that_echoes_its_counter() {
        let (svc, mut h) = handler();
        let (server, peer, peer_addr) = pair();
        deliver(
            &mut h,
            VbanSubProtocol::Service,
            &ping_from_peer(1234),
            peer_addr,
            &server,
        );

        let reply = recv(&peer).expect("a reply");
        let (hdr, payload) = parse_service(&reply).unwrap();
        assert!(hdr.is_reply() && hdr.is_ping());
        assert_eq!(hdr.counter, 1234);
        assert_eq!(parse_ping0(payload).device_name, "Test PC");
        assert_eq!(svc.stats().replies_tx.load(Relaxed), 1);

        // The pinger is now a known peer, identified by its own ping.
        let peers = svc.peers();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].addr, peer_addr);
        assert_eq!(peers[0].info.device_name, "Peer");
        assert!(peers[0].info.is_lan_router());
    }

    #[test]
    fn replies_are_never_answered() {
        let (svc, mut h) = handler();
        let (server, peer, peer_addr) = pair();
        let reply = build_ping(
            &Identity::new("Peer"),
            &[3; 16],
            true,
            9,
            &name(SERVICE_STREAM_NAME),
        );
        deliver(&mut h, VbanSubProtocol::Service, &reply, peer_addr, &server);
        assert!(
            recv(&peer).is_none(),
            "answering a reply would ping-pong forever"
        );
        assert_eq!(svc.stats().replies_rx.load(Relaxed), 1);
        assert_eq!(svc.stats().replies_tx.load(Relaxed), 0);
        assert_eq!(svc.peers().len(), 1, "but the sender is learned");
    }

    #[test]
    fn our_own_broadcast_coming_back_is_ignored() {
        let (svc, mut h) = handler();
        let (server, peer, peer_addr) = pair();
        let mine = build_ping(
            &identity(),
            &svc.instance_id(),
            false,
            1,
            &name(SERVICE_STREAM_NAME),
        );
        deliver(&mut h, VbanSubProtocol::Service, &mine, peer_addr, &server);
        assert!(recv(&peer).is_none());
        assert_eq!(svc.stats().own_ignored.load(Relaxed), 1);
        assert!(svc.peers().is_empty(), "we are not our own peer");
    }

    #[test]
    fn pings_from_outside_the_lan_get_nothing() {
        let (svc, mut h) = handler();
        let (server, peer, _) = pair();
        let public = SocketAddrV4::new(Ipv4Addr::new(8, 8, 8, 8), 6980);
        deliver(
            &mut h,
            VbanSubProtocol::Service,
            &ping_from_peer(1),
            public,
            &server,
        );
        assert!(recv(&peer).is_none());
        assert_eq!(svc.stats().non_lan.load(Relaxed), 1);
        assert_eq!(svc.stats().replies_tx.load(Relaxed), 0);
        assert!(svc.peers().is_empty());
    }

    #[test]
    fn lan_ranges() {
        for ip in [
            [192, 168, 1, 5],
            [10, 0, 0, 1],
            [172, 16, 3, 3],
            [169, 254, 1, 1],
            [127, 0, 0, 1],
            [100, 100, 1, 1],
        ] {
            assert!(is_lan(Ipv4Addr::from(ip)), "{ip:?}");
        }
        for ip in [
            [8, 8, 8, 8],
            [172, 32, 0, 1],
            [100, 128, 0, 1],
            [1, 1, 1, 1],
            [224, 0, 0, 251],
        ] {
            assert!(!is_lan(Ipv4Addr::from(ip)), "{ip:?}");
        }
    }

    #[test]
    fn a_flood_from_one_source_gets_one_reply() {
        let (svc, mut h) = handler();
        let (server, peer, peer_addr) = pair();
        for i in 0..20 {
            deliver(
                &mut h,
                VbanSubProtocol::Service,
                &ping_from_peer(i),
                peer_addr,
                &server,
            );
        }
        assert_eq!(svc.stats().replies_tx.load(Relaxed), 1);
        assert_eq!(svc.stats().rate_limited.load(Relaxed), 19);
        assert!(recv(&peer).is_some());
        assert!(recv(&peer).is_none());
    }

    #[test]
    fn many_sources_are_capped_by_the_global_budget() {
        let mut lim = ReplyLimiter::new(Instant::now());
        let t0 = Instant::now();
        let allowed = (0..200u8)
            .filter(|&i| lim.allow(Ipv4Addr::new(10, 0, 0, i), t0))
            .count();
        assert_eq!(
            allowed, GLOBAL_BURST as usize,
            "a burst, then nothing until tokens refill"
        );
        // One second later the bucket has refilled.
        let t1 = t0 + Duration::from_secs(1);
        let again = (0..200u8)
            .filter(|&i| lim.allow(Ipv4Addr::new(10, 1, 0, i), t1))
            .count();
        assert_eq!(again, GLOBAL_BURST as usize);
    }

    #[test]
    fn the_same_source_may_ping_again_after_the_gap() {
        let mut lim = ReplyLimiter::new(Instant::now());
        let ip = Ipv4Addr::new(10, 0, 0, 1);
        let t0 = Instant::now();
        assert!(lim.allow(ip, t0));
        assert!(!lim.allow(ip, t0 + Duration::from_millis(100)));
        assert!(lim.allow(ip, t0 + Duration::from_millis(300)));
    }

    #[test]
    fn a_ping_we_send_gives_a_round_trip_when_it_is_answered() {
        let (svc, mut h) = handler();
        let (server, peer, peer_addr) = pair();
        svc.ping(peer_addr);
        h.tick(Instant::now(), &server);

        let request = recv(&peer).expect("the ping leaves from the RX socket");
        let (hdr, payload) = parse_service(&request).unwrap();
        assert!(hdr.is_ping() && !hdr.is_reply());
        assert_eq!(parse_ping0(payload).instance, Some(svc.instance_id()));

        std::thread::sleep(Duration::from_millis(5));
        let answer = build_ping(
            &Identity::new("Peer"),
            &[3; 16],
            true,
            hdr.counter,
            &hdr.stream_name,
        );
        deliver(
            &mut h,
            VbanSubProtocol::Service,
            &answer,
            peer_addr,
            &server,
        );
        let peers = svc.peers();
        assert_eq!(peers.len(), 1);
        let rtt = peers[0].rtt.expect("a measured round trip");
        assert!(
            rtt >= Duration::from_millis(5) && rtt < Duration::from_secs(1),
            "{rtt:?}"
        );
    }

    #[test]
    fn the_beacon_goes_out_every_two_seconds_and_only_when_switched_on() {
        let (svc, mut h) = handler();
        let (server, peer, peer_addr) = pair();
        svc.set_targets(vec![peer_addr]);
        let t0 = Instant::now();

        h.tick(t0, &server);
        assert!(recv(&peer).is_none(), "off by default");

        svc.set_beacon(true);
        h.tick(t0, &server);
        let first = recv(&peer).expect("first beacon is immediate");
        assert!(!parse_service(&first).unwrap().0.is_reply());
        assert_eq!(svc.stats().beacons_tx.load(Relaxed), 1);

        h.tick(t0 + Duration::from_millis(1900), &server);
        assert!(recv(&peer).is_none(), "not before 2 s");
        h.tick(t0 + Duration::from_millis(2000), &server);
        assert!(recv(&peer).is_some(), "at 2 s");
        assert_eq!(svc.stats().beacons_tx.load(Relaxed), 2);

        svc.set_beacon(false);
        h.tick(t0 + Duration::from_secs(10), &server);
        assert!(recv(&peer).is_none(), "switched off again");
        // Switching on again starts a new cycle straight away.
        svc.set_beacon(true);
        h.tick(t0 + Duration::from_secs(11), &server);
        assert!(recv(&peer).is_some());
    }

    #[test]
    fn the_default_beacon_target_is_broadcast_on_the_vban_port() {
        assert_eq!(
            default_targets(),
            [SocketAddrV4::new(Ipv4Addr::BROADCAST, 6980)]
        );
    }

    #[test]
    fn text_commands_reach_the_app_in_order_with_the_sender() {
        let (svc, mut h) = handler();
        let (server, _peer, peer_addr) = pair();
        let pkt = build_text_packet(
            &name("Command1"),
            0,
            "Strip[0].Gain=-6; Bus[1].Mute=1; nonsense; Strip[99].Gain=0",
        )
        .unwrap();
        deliver(&mut h, VbanSubProtocol::Text, &pkt, peer_addr, &server);
        let a = svc.try_recv_command().unwrap();
        let b = svc.try_recv_command().unwrap();
        assert_eq!(a.command, TextCommand::StripGain { index: 0, db: -6.0 });
        assert_eq!(
            b.command,
            TextCommand::BusMute {
                index: 1,
                muted: true
            }
        );
        assert_eq!(a.from, Ipv4Addr::LOCALHOST);
        assert!(svc.try_recv_command().is_none());
        assert_eq!(svc.stats().text_commands.load(Relaxed), 2);
        assert_eq!(svc.stats().text_ignored.load(Relaxed), 2);
    }

    #[test]
    fn an_app_that_never_listens_cannot_make_the_queue_grow() {
        let (svc, mut h) = handler();
        let (server, _peer, peer_addr) = pair();
        let text = "Strip[0].Mute=1;".repeat(TEXT_QUEUE + 44);
        // One packet holds at most 1436 bytes: send enough packets.
        for chunk in text.as_bytes().chunks(16 * 80) {
            let pkt =
                build_text_packet(&name("c"), 0, std::str::from_utf8(chunk).unwrap()).unwrap();
            deliver(&mut h, VbanSubProtocol::Text, &pkt, peer_addr, &server);
        }
        assert_eq!(svc.stats().text_commands.load(Relaxed) as usize, TEXT_QUEUE);
        assert_eq!(svc.stats().text_dropped.load(Relaxed), 44);
        let mut n = 0;
        while svc.try_recv_command().is_some() {
            n += 1;
        }
        assert_eq!(n, TEXT_QUEUE);
    }

    #[test]
    fn garbage_control_packets_are_counted_not_acted_on() {
        let (svc, mut h) = handler();
        let (server, peer, peer_addr) = pair();
        deliver(
            &mut h,
            VbanSubProtocol::Service,
            b"VBAN\x60",
            peer_addr,
            &server,
        );
        deliver(
            &mut h,
            VbanSubProtocol::Text,
            b"VBAN\x40",
            peer_addr,
            &server,
        );
        deliver(
            &mut h,
            VbanSubProtocol::Serial,
            b"whatever",
            peer_addr,
            &server,
        );
        assert_eq!(svc.stats().malformed.load(Relaxed), 2);
        assert!(recv(&peer).is_none());
        assert!(svc.try_recv_command().is_none());
    }

    #[test]
    fn peers_expire() {
        let (svc, h) = handler();
        let old = Instant::now().checked_sub(PEER_TTL + Duration::from_secs(1));
        // ? On a machine that booted less than 16 s ago `Instant` can't go back that far; then
        // ? there is nothing to age and the test has nothing to show.
        let Some(old) = old else { return };
        h.note_peer(
            SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 9), 6980),
            PeerInfo::default(),
            None,
            old,
        );
        h.note_peer(
            SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 10), 6980),
            PeerInfo::default(),
            None,
            Instant::now(),
        );
        let peers = svc.peers();
        assert_eq!(peers.len(), 1);
        assert_eq!(*peers[0].addr.ip(), Ipv4Addr::new(10, 0, 0, 10));
    }
    // !SECTION: Handler (driven directly, no RX thread)

    // SECTION: Through the real RX socket
    use crate::rx::{RxPolicy, RxSocket};

    fn bind_rx(policy: RxPolicy, h: ServiceHandler) -> (RxSocket, SocketAddrV4) {
        let rx = RxSocket::bind(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
            policy,
            Some(Box::new(h)),
        )
        .unwrap();
        let addr = rx.local_addr();
        (rx, addr)
    }

    #[test]
    fn a_stranger_can_ping_but_cannot_send_commands() {
        let (svc, h) = handler();
        // The default policy trusts nobody.
        let (rx, rx_addr) = bind_rx(RxPolicy::default(), h);
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        // Discovery has to work before anyone is trusted.
        peer.send_to(&ping_from_peer(5), rx_addr).unwrap();
        let reply = recv_2s(&peer).expect("a reply to the stranger's ping");
        assert_eq!(parse_service(&reply).unwrap().0.counter, 5);
        assert_eq!(svc.peers().len(), 1, "now it is discovered");

        // Commands are a different matter.
        let cmd = build_text_packet(&name("Command1"), 0, "Strip[0].Mute=1;").unwrap();
        peer.send_to(&cmd, rx_addr).unwrap();
        assert!(wait_for(|| rx.stats().rejected.load(Relaxed) == 1));
        assert!(svc.try_recv_command().is_none());
        assert_eq!(svc.stats().text_commands.load(Relaxed), 0);
    }

    #[test]
    fn an_allowed_peer_can_send_commands_through_the_socket() {
        let (svc, h) = handler();
        let policy = RxPolicy {
            accept_any: false,
            allowed: vec![Ipv4Addr::LOCALHOST],
        };
        let (_rx, rx_addr) = bind_rx(policy, h);
        let mut sender = TextSender::new(TEXT_STREAM_NAME).unwrap();
        sender
            .send_command(
                rx_addr,
                &TextCommand::StripGain {
                    index: 4,
                    db: -12.5,
                },
            )
            .unwrap();
        assert!(wait_for(|| svc.stats().text_commands.load(Relaxed) == 1));
        let event = svc.try_recv_command().unwrap();
        assert_eq!(
            event.command,
            TextCommand::StripGain {
                index: 4,
                db: -12.5
            }
        );
    }

    #[test]
    fn the_beacon_leaves_from_the_rx_socket_so_replies_return_to_it() {
        let (svc, h) = handler();
        let (_rx, rx_addr) = bind_rx(RxPolicy::default(), h);
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let peer_addr = match peer.local_addr().unwrap() {
            std::net::SocketAddr::V4(a) => a,
            std::net::SocketAddr::V6(_) => unreachable!(),
        };
        svc.set_targets(vec![peer_addr]);
        svc.set_beacon(true);

        let mut buf = [0u8; 2048];
        let (n, src) = peer.recv_from(&mut buf).expect("the beacon");
        assert_eq!(
            src.port(),
            rx_addr.port(),
            "source port = the RX socket's port"
        );
        let (hdr, _) = parse_service(&buf[..n]).unwrap();
        // The peer answers the beacon; the answer lands on the RX socket and is recorded.
        let answer = build_ping(
            &Identity::new("Peer"),
            &[3; 16],
            true,
            hdr.counter,
            &hdr.stream_name,
        );
        peer.send_to(&answer, src).unwrap();
        assert!(wait_for(|| svc.peers().len() == 1));
        assert!(svc.peers()[0].rtt.is_some());
    }

    fn recv_2s(peer: &UdpSocket) -> Option<Vec<u8>> {
        let mut buf = [0u8; 2048];
        peer.recv_from(&mut buf)
            .ok()
            .map(|(n, _)| buf[..n].to_vec())
    }

    fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        cond()
    }
    // !SECTION: Through the real RX socket
}
