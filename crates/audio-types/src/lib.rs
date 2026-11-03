//! crates/audio-types/src/lib.rs
//! Zero-allocation data contracts shared by the RT engine, the network engine,
//! the control plane, and every UI (egui desktop, Makepad Android).
//!
//! ? Split of responsibilities:
//! ?   * `*Config` / `*Endpoint` types  -> control plane + persistence (serde, may be large)
//! ?   * `EngineCommand`                -> RT thread only (Copy, small, no device/socket data)
//! ?   * `*Atomics`                     -> RT/net threads -> UI telemetry

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

// SECTION: Engine-wide constants
pub const MAX_STRIPS: usize = 16;
pub const MAX_BUSES: usize = 16;
pub const MAX_CHANNELS: usize = 8; // * VoiceMeeter buses are 8-channel; stereo is just ch=2
pub const MAX_LAN_STREAMS: usize = 16;
pub const ENGINE_SAMPLE_RATE: u32 = 48_000;
pub const ENGINE_BLOCK_FRAMES: usize = 128; // * 2.67 ms @ 48 kHz
// !SECTION: Engine-wide constants

// SECTION: Fixed stack string
/// Stack-allocated UTF-8 string. Never allocates, never frees.
/// ? Serialized as a plain string so TOML/JSON configs stay human-readable.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FixedStr<const N: usize> {
    bytes: [u8; N],
    len: u8,
}

impl<const N: usize> FixedStr<N> {
    // !! `len` is a u8: compile-time guard against N > 255.
    const CAP_OK: () = assert!(N <= 255, "FixedStr<N> requires N <= 255");
    pub const EMPTY: Self = {
        let () = Self::CAP_OK;
        Self {
            bytes: [0; N],
            len: 0,
        }
    };

    /// Truncates on a UTF-8 char boundary (never produces invalid UTF-8).
    pub fn from_str_trunc(s: &str) -> Self {
        let () = Self::CAP_OK;
        let mut end = s.len().min(N);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        let mut bytes = [0u8; N];
        bytes[..end].copy_from_slice(&s.as_bytes()[..end]);
        Self {
            bytes,
            len: end as u8,
        }
    }

    /// Parses a NUL-padded wire field (e.g. VBAN stream name). Keeps the valid UTF-8 prefix.
    pub fn from_wire(raw: &[u8]) -> Self {
        let raw = &raw[..raw.len().min(N)];
        let nul = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        let valid = match core::str::from_utf8(&raw[..nul]) {
            Ok(s) => s,
            Err(e) => core::str::from_utf8(&raw[..e.valid_up_to()]).unwrap_or(""),
        };
        Self::from_str_trunc(valid)
    }

    pub fn as_str(&self) -> &str {
        // ? Invariant: bytes[..len] is always valid UTF-8 (both constructors guarantee it).
        core::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or("")
    }

    /// NUL-padded copy for the wire.
    pub fn to_wire(&self) -> [u8; N] {
        self.bytes
    }
}

impl<const N: usize> Default for FixedStr<N> {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl<const N: usize> core::fmt::Debug for FixedStr<N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

impl<const N: usize> Serialize for FixedStr<N> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de, const N: usize> Deserialize<'de> for FixedStr<N> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<const N: usize>;
        impl<const N: usize> serde::de::Visitor<'_> for V<N> {
            type Value = FixedStr<N>;
            fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                write!(f, "a string of at most {N} bytes")
            }
            // ? visit_str also receives borrowed and owned strings -> works for TOML and JSON.
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(FixedStr::from_str_trunc(v))
            }
        }
        d.deserialize_str(V::<N>)
    }
}
// !SECTION: Fixed stack string

// SECTION: VBAN wire protocol (VB-Audio spec)
pub const VBAN_HEADER_SIZE: usize = 28;
pub const VBAN_MAGIC: [u8; 4] = *b"VBAN";
pub const VBAN_MAX_PAYLOAD_BYTES: usize = 1436;
pub const VBAN_MAX_PACKET_BYTES: usize = VBAN_HEADER_SIZE + VBAN_MAX_PAYLOAD_BYTES; // 1464
pub const VBAN_MAX_FRAMES_PER_PACKET: usize = 256;
pub const VBAN_DEFAULT_PORT: u16 = 6980;

/// Index = bits 0..4 of byte 4.
pub const VBAN_SAMPLE_RATES: [u32; 21] = [
    6000, 12000, 24000, 48000, 96000, 192000, 384000, 8000, 16000, 32000, 64000, 128000, 256000,
    512000, 11025, 22050, 44100, 88200, 176400, 352800, 705600,
];

pub fn vban_sr_index(rate: u32) -> Option<u8> {
    VBAN_SAMPLE_RATES
        .iter()
        .position(|&r| r == rate)
        .map(|i| i as u8)
}

/// Bits 5..7 of byte 4.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VbanSubProtocol {
    Audio = 0x00,
    Serial = 0x20,
    Text = 0x40,    // * VBAN-TEXT: VoiceMeeter remote-control commands
    Service = 0x60, // * PING / discovery
}

impl VbanSubProtocol {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b & 0xE0 {
            0x00 => Some(Self::Audio),
            0x20 => Some(Self::Serial),
            0x40 => Some(Self::Text),
            0x60 => Some(Self::Service),
            _ => None,
        }
    }
}

/// Bits 0..2 of byte 7. ? Gemini's version only had 3 of these 8.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum VbanDataType {
    Byte8 = 0x00,
    #[default]
    Int16 = 0x01,
    Int24 = 0x02,
    Int32 = 0x03,
    Float32 = 0x04,
    Float64 = 0x05,
    Bits12 = 0x06,
    Bits10 = 0x07,
}

impl VbanDataType {
    pub fn from_byte(b: u8) -> Self {
        match b & 0x07 {
            0 => Self::Byte8,
            1 => Self::Int16,
            2 => Self::Int24,
            3 => Self::Int32,
            4 => Self::Float32,
            5 => Self::Float64,
            6 => Self::Bits12,
            _ => Self::Bits10,
        }
    }
    /// `None` for packed 10/12-bit formats (rejected on receive).
    pub const fn bytes_per_sample(self) -> Option<usize> {
        match self {
            Self::Byte8 => Some(1),
            Self::Int16 => Some(2),
            Self::Int24 => Some(3),
            Self::Int32 | Self::Float32 => Some(4),
            Self::Float64 => Some(8),
            Self::Bits12 | Self::Bits10 => None,
        }
    }
}

/// Bits 4..7 of byte 7.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum VbanCodec {
    #[default]
    Pcm = 0x00,
    Vbca = 0x10,
    Vbcv = 0x20,
    /// ? Our Opus rides here, prefixed by `OPUS_USER_TAG` so foreign USER payloads are ignored.
    User = 0xF0,
}

impl VbanCodec {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b & 0xF0 {
            0x00 => Some(Self::Pcm),
            0x10 => Some(Self::Vbca),
            0x20 => Some(Self::Vbcv),
            0xF0 => Some(Self::User),
            _ => None,
        }
    }
}

/// First 2 payload bytes of a `VbanCodec::User` packet that carries our Opus frame.
pub const OPUS_USER_TAG: [u8; 2] = *b"O1";

/// Largest frame count that fits one VBAN packet for a given PCM layout.
/// ? Gemini hard-coded 128/256 frames; 8ch Float32 only fits 44.
pub const fn vban_max_frames(channels: usize, bytes_per_sample: usize) -> usize {
    let by_bytes = VBAN_MAX_PAYLOAD_BYTES / (channels * bytes_per_sample);
    if by_bytes < VBAN_MAX_FRAMES_PER_PACKET {
        by_bytes
    } else {
        VBAN_MAX_FRAMES_PER_PACKET
    }
}

/// Decoded audio header. ? Plain struct + explicit LE (de)serialization instead of
/// `#[repr(packed)]`: no unaligned-reference UB, no endianness assumptions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VbanAudioHeader {
    pub sample_rate: u32,
    pub frames: u16,   // 1..=256
    pub channels: u16, // 1..=256
    pub data_type: VbanDataType,
    pub codec: VbanCodec,
    pub stream_name: FixedStr<16>,
    pub frame_counter: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VbanParseError {
    TooShort,
    BadMagic,
    NotAudio(Option<VbanSubProtocol>),
    BadSampleRate(u8),
    UnknownCodec(u8),
}

impl VbanAudioHeader {
    pub fn encode(&self, out: &mut [u8; VBAN_HEADER_SIZE]) -> Option<()> {
        let sr = vban_sr_index(self.sample_rate)?;
        if !(1..=256).contains(&self.frames) || !(1..=256).contains(&self.channels) {
            return None;
        }
        out[0..4].copy_from_slice(&VBAN_MAGIC);
        out[4] = sr | VbanSubProtocol::Audio as u8;
        out[5] = (self.frames - 1) as u8;
        out[6] = (self.channels - 1) as u8;
        out[7] = self.data_type as u8 | self.codec as u8; // ! bit 3 reserved = 0
        out[8..24].copy_from_slice(&self.stream_name.to_wire());
        out[24..28].copy_from_slice(&self.frame_counter.to_le_bytes());
        Some(())
    }

    pub fn decode(pkt: &[u8]) -> Result<Self, VbanParseError> {
        if pkt.len() < VBAN_HEADER_SIZE {
            return Err(VbanParseError::TooShort);
        }
        if pkt[0..4] != VBAN_MAGIC {
            return Err(VbanParseError::BadMagic);
        }
        let sub = VbanSubProtocol::from_byte(pkt[4]);
        if sub != Some(VbanSubProtocol::Audio) {
            return Err(VbanParseError::NotAudio(sub));
        }
        let sr_idx = pkt[4] & 0x1F;
        let sample_rate = *VBAN_SAMPLE_RATES
            .get(sr_idx as usize)
            .ok_or(VbanParseError::BadSampleRate(sr_idx))?;
        let codec = VbanCodec::from_byte(pkt[7]).ok_or(VbanParseError::UnknownCodec(pkt[7]))?;
        Ok(Self {
            sample_rate,
            frames: pkt[5] as u16 + 1,
            channels: pkt[6] as u16 + 1,
            data_type: VbanDataType::from_byte(pkt[7]),
            codec,
            stream_name: FixedStr::from_wire(&pkt[8..24]),
            frame_counter: u32::from_le_bytes([pkt[24], pkt[25], pkt[26], pkt[27]]),
        })
    }
}
// !SECTION: VBAN wire protocol (VB-Audio spec)

// SECTION: LAN stream configuration (control plane)
/// Receiver-side jitter target. Names mirror VoiceMeeter's "Net Quality".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkBufferPreset {
    Optimal, // wired / idle LAN
    #[default]
    Fast, // clean 5 GHz Wi-Fi
    Medium,  // busy Wi-Fi
    Slow,    // congested
    VerySlow, // !! exceeds the 70 ms goal on its own; opt-in only
    Custom {
        target_ms: u16,
    },
}

impl NetworkBufferPreset {
    pub const fn target_ms(self) -> u16 {
        match self {
            Self::Optimal => 6,
            Self::Fast => 12,
            Self::Medium => 24,
            Self::Slow => 48,
            Self::VerySlow => 96,
            Self::Custom { target_ms } => target_ms,
        }
    }
    /// ? A jitter buffer shallower than 2 packets underruns on every packet boundary.
    pub fn effective_target_ms(self, packet_ms: f32) -> f32 {
        (self.target_ms() as f32).max(2.0 * packet_ms)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpusFrame {
    Ms2_5, // 120 samples @ 48 kHz
    Ms5,   // 240 samples
    Ms10,  // 480 samples
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireCodec {
    /// 100 % VoiceMeeter-compatible.
    Pcm { data_type: VbanDataType },
    /// Our peers only (VBAN USER slot). RESTRICTED_LOWDELAY => PLC only, no in-band FEC.
    Opus { bitrate_kbps: u16, frame: OpusFrame },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanTxConfig {
    pub peer_ipv4: [u8; 4],
    pub port: u16,
    pub stream_name: FixedStr<16>,
    pub channels: u8,
    pub codec: WireCodec,
    /// 0 = auto (largest frame count <= 128 that fits `vban_max_frames`).
    pub frames_per_packet: u16,
    /// DSCP code point. 46 (EF) default; 48 (CS6) maps to WMM AC_VO on legacy APs.
    pub dscp: u8,
    /// Send every packet a second time, one packet later (PLAN.md §4.2). Costs 2x the
    /// bandwidth and recovers a single lost packet at +1 packet of receiver delay.
    /// ? `#[serde(default)]` keeps profiles written before S3 loadable.
    #[serde(default)]
    pub duplicate: bool,
}

impl Default for LanTxConfig {
    fn default() -> Self {
        Self {
            peer_ipv4: [127, 0, 0, 1],
            port: VBAN_DEFAULT_PORT,
            stream_name: FixedStr::from_str_trunc("Stream1"),
            channels: 2,
            codec: WireCodec::Pcm {
                data_type: VbanDataType::Int16,
            },
            frames_per_packet: 0,
            dscp: 46,
            duplicate: false,
        }
    }
}

/// ? Receive side: ONE socket per port; streams are demuxed by (source IP, stream name).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanRxConfig {
    /// [0,0,0,0] = accept from any IP.
    pub source_ipv4: [u8; 4],
    pub stream_name: FixedStr<16>,
    pub buffer: NetworkBufferPreset,
}
// !SECTION: LAN stream configuration (control plane)

// SECTION: Endpoints (control plane only — never sent to the RT thread)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessLoopbackMode {
    IncludeTree, // capture this app (+ children)
    ExcludeTree, // capture everything EXCEPT this app (e.g. exclude ourselves -> no feedback)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AndroidInputPreset {
    VoicePerformance, // * lowest-latency mic path (API 29+)
    Unprocessed,
    Camcorder,
    VoiceRecognition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaybackCaptureFilter {
    AllMediaAndGames, // USAGE_MEDIA | USAGE_GAME | USAGE_UNKNOWN
    OnlyUid(u32),     // AudioPlaybackCaptureConfiguration.addMatchingUid
    ExcludeUid(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceEndpoint {
    Unassigned,
    HardwareInput {
        device_id: FixedStr<128>,
    },
    DeviceLoopback {
        device_id: FixedStr<128>,
    }, // WASAPI loopback / PipeWire monitor
    ProcessLoopback {
        pid: u32,
        exe_name: FixedStr<64>,
        mode: ProcessLoopbackMode,
    },
    VirtualInput {
        index: u8,
    }, // our PipeWire sink / a cable driver on Windows
    AndroidMic {
        preset: AndroidInputPreset,
    },
    AndroidPlaybackCapture {
        filter: PlaybackCaptureFilter,
    },
    LanIn(LanRxConfig),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SinkEndpoint {
    Unassigned,
    HardwareOutput { device_id: FixedStr<128> },
    VirtualMic { index: u8 },
    LanOut(LanTxConfig),
}
// !SECTION: Endpoints (control plane only — never sent to the RT thread)

// SECTION: Mixer parameters
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RouteSend {
    pub enabled: bool,
    pub gain_db: f32,
    pub pre_fader: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EqBand {
    pub enabled: bool,
    pub freq_hz: f32,
    pub gain_db: f32,
    pub q: f32,
}

impl Default for EqBand {
    fn default() -> Self {
        Self {
            enabled: false,
            freq_hz: 1000.0,
            gain_db: 0.0,
            q: 0.707,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Gate {
    pub enabled: bool,
    pub threshold_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold_db: -50.0,
            attack_ms: 2.0,
            release_ms: 80.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Compressor {
    pub enabled: bool,
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub makeup_db: f32,
}

impl Default for Compressor {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold_db: -18.0,
            ratio: 4.0,
            attack_ms: 5.0,
            release_ms: 100.0,
            makeup_db: 0.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StripDsp {
    pub gate: Gate,
    pub eq: [EqBand; 3],
    pub comp: Compressor,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct StripConfig {
    pub label: FixedStr<24>,
    pub source: SourceEndpoint,
    pub channels: u8,
    pub gain_db: f32,
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    pub dsp: StripDsp,
    pub sends: [RouteSend; MAX_BUSES],
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct BusConfig {
    pub label: FixedStr<24>,
    pub sink: SinkEndpoint,
    pub channels: u8,
    pub gain_db: f32,
    pub mute: bool,
    pub eq: [EqBand; 3],
    pub record: bool,
}
// ? UI-only state (collapsed/expanded, card order) lives in app-core's UiLayout, not here.
// !SECTION: Mixer parameters

// SECTION: RT commands (control plane -> RT mixer)
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StripId(pub u8);
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BusId(pub u8);
/// Index into the engine's PRE-ALLOCATED pool of port ring buffers.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PortSlot(pub u8);

/// Which clock a strip's input ring runs on (PLAN.md §5.1).
/// ? Chosen by the control plane, which knows where the audio comes from: the device that
/// ? clocks the mixer is `Master`; every other device and every LAN stream is `External`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum ClockDomain {
    /// Same clock as the mixer: the ring is read directly (no resampler).
    #[default]
    Master,
    /// Another clock: read through the drift resampler, steering the ring's fill level
    /// to `target_ms`.
    /// ? For a LAN stream use `NetworkBufferPreset::effective_target_ms(packet_ms)`.
    External { target_ms: f32 },
}

/// !! Must stay `Copy` and small: popping/dropping on the RT thread may never free().
/// ? Device names, sockets, configs never travel here — the control plane opens
/// ? devices on its own thread, wires them to a PortSlot, then sends `Attach*`.
#[derive(Clone, Copy, Debug)]
pub enum EngineCommand {
    SetStripGain {
        strip: StripId,
        gain_db: f32,
    },
    SetStripPan {
        strip: StripId,
        pan: f32,
    },
    SetStripMute {
        strip: StripId,
        mute: bool,
    },
    SetStripSolo {
        strip: StripId,
        solo: bool,
    },
    SetStripDsp {
        strip: StripId,
        dsp: StripDsp,
    },
    SetRouteSend {
        strip: StripId,
        bus: BusId,
        send: RouteSend,
    },
    SetBusGain {
        bus: BusId,
        gain_db: f32,
    },
    SetBusMute {
        bus: BusId,
        mute: bool,
    },
    SetBusEq {
        bus: BusId,
        eq: [EqBand; 3],
    },
    SetBusRecord {
        bus: BusId,
        enabled: bool,
    },
    AttachStripInput {
        strip: StripId,
        slot: PortSlot,
        channels: u8,
    },
    DetachStripInput {
        strip: StripId,
    },
    /// Switch a strip between the master clock and an external one. Resets the strip's
    /// resampler and drift controller; takes effect on the next block.
    SetStripClock {
        strip: StripId,
        clock: ClockDomain,
    },
    AttachBusOutput {
        bus: BusId,
        slot: PortSlot,
        channels: u8,
    },
    DetachBusOutput {
        bus: BusId,
    },
}
// !SECTION: RT commands (control plane -> RT mixer)

// SECTION: Lock-free telemetry (RT/net -> UI)
#[inline]
pub fn store_f32(cell: &AtomicU32, v: f32) {
    cell.store(v.to_bits(), Ordering::Relaxed);
}
#[inline]
pub fn load_f32(cell: &AtomicU32) -> f32 {
    f32::from_bits(cell.load(Ordering::Relaxed))
}

pub struct MeterAtomics {
    pub peak_db: [AtomicU32; MAX_CHANNELS],
    pub rms_db: [AtomicU32; MAX_CHANNELS],
    pub clipping: AtomicBool,
}

impl Default for MeterAtomics {
    fn default() -> Self {
        let floor = (-120.0_f32).to_bits();
        Self {
            peak_db: core::array::from_fn(|_| AtomicU32::new(floor)),
            rms_db: core::array::from_fn(|_| AtomicU32::new(floor)),
            clipping: AtomicBool::new(false),
        }
    }
}

pub struct LanRxAtomics {
    pub buffer_ms: AtomicU32,    // f32: current jitter-buffer depth
    pub target_ms: AtomicU32,    // f32
    pub drift_ppm: AtomicU32,    // f32: resampler correction in ppm
    pub packets_lost: AtomicU32, // frame_counter gaps
    pub packets_late: AtomicU32, // arrived after their play-out slot
    pub underruns: AtomicU32,
    pub last_packet_ms_ago: AtomicU32,
}

impl Default for LanRxAtomics {
    fn default() -> Self {
        Self {
            buffer_ms: AtomicU32::new(0.0_f32.to_bits()),
            target_ms: AtomicU32::new(0.0_f32.to_bits()),
            drift_ppm: AtomicU32::new(0.0_f32.to_bits()),
            packets_lost: AtomicU32::new(0),
            packets_late: AtomicU32::new(0),
            underruns: AtomicU32::new(0),
            last_packet_ms_ago: AtomicU32::new(u32::MAX),
        }
    }
}

/// Shared as `Arc<Telemetry>`.
pub struct Telemetry {
    pub strips: [MeterAtomics; MAX_STRIPS],
    pub buses: [MeterAtomics; MAX_BUSES],
    pub lan_rx: [LanRxAtomics; MAX_LAN_STREAMS],
    pub any_audio_active: AtomicBool,
    pub rt_load_permille: AtomicU32, // mixer time / block time * 1000
    pub rt_overruns: AtomicU32,
}

impl Default for Telemetry {
    fn default() -> Self {
        Self {
            strips: core::array::from_fn(|_| MeterAtomics::default()),
            buses: core::array::from_fn(|_| MeterAtomics::default()),
            lan_rx: core::array::from_fn(|_| LanRxAtomics::default()),
            any_audio_active: AtomicBool::new(false),
            rt_load_permille: AtomicU32::new(0),
            rt_overruns: AtomicU32::new(0),
        }
    }
}
// !SECTION: Lock-free telemetry (RT/net -> UI)

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_copy_send<T: Copy + Send + 'static>() {}

    #[test]
    fn command_is_small_and_copy() {
        assert_copy_send::<EngineCommand>();
        let sz = core::mem::size_of::<EngineCommand>();
        println!("EngineCommand = {sz} bytes");
        assert!(sz <= 128);
    }

    #[test]
    fn clock_domain_roundtrips_and_defaults_to_master() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Cfg {
            clock: ClockDomain,
        }
        assert_eq!(ClockDomain::default(), ClockDomain::Master);
        for clock in [
            ClockDomain::Master,
            ClockDomain::External { target_ms: 12.5 },
        ] {
            let c = Cfg { clock };
            let t = toml::to_string(&c).unwrap();
            assert_eq!(toml::from_str::<Cfg>(&t).unwrap(), c, "{t}");
            let j = serde_json::to_string(&c).unwrap();
            assert_eq!(serde_json::from_str::<Cfg>(&j).unwrap(), c, "{j}");
        }
    }

    #[test]
    fn header_roundtrip() {
        let h = VbanAudioHeader {
            sample_rate: 48_000,
            frames: 128,
            channels: 2,
            data_type: VbanDataType::Int16,
            codec: VbanCodec::Pcm,
            stream_name: FixedStr::from_str_trunc("Stream1"),
            frame_counter: 0xDEAD_BEEF,
        };
        let mut buf = [0u8; VBAN_HEADER_SIZE];
        h.encode(&mut buf).unwrap();
        assert_eq!(&buf[0..4], b"VBAN");
        assert_eq!(buf[4], 3); // 48 kHz index, audio sub-protocol
        assert_eq!(buf[5], 127);
        assert_eq!(buf[7], 0x01);
        assert_eq!(VbanAudioHeader::decode(&buf).unwrap(), h);
    }

    #[test]
    fn rejects_non_audio() {
        let mut buf = [0u8; VBAN_HEADER_SIZE];
        buf[0..4].copy_from_slice(b"VBAN");
        buf[4] = 0x60; // service
        assert_eq!(
            VbanAudioHeader::decode(&buf),
            Err(VbanParseError::NotAudio(Some(VbanSubProtocol::Service)))
        );
    }

    #[test]
    fn packet_capacity() {
        assert_eq!(vban_max_frames(2, 2), 256); // stereo int16
        assert_eq!(vban_max_frames(2, 4), 179); // stereo f32
        assert_eq!(vban_max_frames(8, 2), 89); // 8ch int16 (VoiceMeeter bus)
        assert_eq!(vban_max_frames(8, 4), 44); // 8ch f32
    }

    #[test]
    fn utf8_safe_truncation() {
        assert_eq!(FixedStr::<3>::from_str_trunc("éé").as_str(), "é");
        assert_eq!(FixedStr::<16>::from_wire(b"Mic\0\0garbage").as_str(), "Mic");
    }

    #[test]
    fn config_roundtrips_toml_and_json() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Cfg {
            tx: LanTxConfig,
        }
        let c = Cfg {
            tx: LanTxConfig::default(),
        };
        let t = toml::to_string(&c).unwrap();
        println!("{t}");
        assert_eq!(toml::from_str::<Cfg>(&t).unwrap(), c);
        let j = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<Cfg>(&j).unwrap(), c);
    }

    #[test]
    fn tx_config_written_before_duplicate_existed_still_loads() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Cfg {
            tx: LanTxConfig,
        }
        let mut c = Cfg {
            tx: LanTxConfig::default(),
        };
        c.tx.duplicate = true;
        let t = toml::to_string(&c).unwrap();
        assert!(t.contains("duplicate = true"), "{t}");
        assert_eq!(toml::from_str::<Cfg>(&t).unwrap(), c);

        // A profile saved by S1/S2 has no `duplicate` key at all.
        let old: String = t
            .lines()
            .filter(|l| !l.contains("duplicate"))
            .collect::<Vec<_>>()
            .join("\n");
        let loaded = toml::from_str::<Cfg>(&old).unwrap();
        assert!(!loaded.tx.duplicate);
        assert_eq!(loaded.tx.dscp, c.tx.dscp);
    }
}
