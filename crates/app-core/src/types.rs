//! Everything that crosses the `ControlApi` boundary. All of it is plain serde data, so the
//! same types serve an in-process UI and the IPC protocol.

pub use crate::recorder::{RecordFormat, RecordingState};
use crate::ui_state::UiLayout;
use audio_types::{
    BusConfig, EqBand, RouteSend, SinkEndpoint, SourceEndpoint, StripConfig, StripDsp,
};
use platform_io::{Capabilities, StreamStats};
use serde::{Deserialize, Serialize};
use std::net::SocketAddrV4;

/// Bumped whenever a message changes shape. The IPC handshake refuses a mismatch.
pub const PROTOCOL_VERSION: u32 = 2;

// SECTION: Errors
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlError {
    /// The request itself is wrong (index out of range, NaN, bad name...).
    Invalid(String),
    /// The audio backend refused (no such device, busy, bad channel count).
    Backend(String),
    /// A socket could not be opened or a stream could not be started.
    Network(String),
    /// Every ring of the pool is in use.
    NoFreePort,
    /// A probe is already running.
    Busy(String),
    Profile(String),
    /// The daemon cannot be reached (IPC client only).
    Disconnected(String),
    /// The other side spoke a different protocol version or sent garbage.
    Protocol(String),
}

impl core::fmt::Display for ControlError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Invalid(m) => write!(f, "invalid request: {m}"),
            Self::Backend(m) => write!(f, "audio backend: {m}"),
            Self::Network(m) => write!(f, "network: {m}"),
            Self::NoFreePort => f.write_str("all audio ports are in use"),
            Self::Busy(m) => write!(f, "busy: {m}"),
            Self::Profile(m) => write!(f, "profile: {m}"),
            Self::Disconnected(m) => write!(f, "not connected to the daemon: {m}"),
            Self::Protocol(m) => write!(f, "protocol error: {m}"),
        }
    }
}

impl std::error::Error for ControlError {}
// !SECTION: Errors

// SECTION: Commands
/// Everything a UI or the CLI can ask the daemon to do. Strips and buses are numbered from 0.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Command {
    StripGain {
        strip: u8,
        gain_db: f32,
    },
    StripPan {
        strip: u8,
        pan: f32,
    },
    StripMute {
        strip: u8,
        mute: bool,
    },
    StripSolo {
        strip: u8,
        solo: bool,
    },
    StripLabel {
        strip: u8,
        label: String,
    },
    StripDsp {
        strip: u8,
        dsp: StripDsp,
    },
    /// Open `source` into the strip. `Unassigned` closes whatever is open.
    StripSource {
        strip: u8,
        source: SourceEndpoint,
        channels: u8,
    },
    Route {
        strip: u8,
        bus: u8,
        send: RouteSend,
    },
    BusGain {
        bus: u8,
        gain_db: f32,
    },
    BusMute {
        bus: u8,
        mute: bool,
    },
    BusLabel {
        bus: u8,
        label: String,
    },
    BusEq {
        bus: u8,
        eq: [EqBand; 3],
    },
    BusRecord {
        bus: u8,
        enabled: bool,
    },
    /// Open `sink` for the bus. `Unassigned` closes whatever is open.
    BusSink {
        bus: u8,
        sink: SinkEndpoint,
        channels: u8,
    },
    /// Who may send audio to a LAN input that does not name a source IP.
    RxPolicy {
        accept_any: bool,
        allowed: Vec<[u8; 4]>,
        /// Also allow every machine mDNS discovery finds.
        trust_discovered: bool,
    },
    /// The 2 s VBAN-SERVICE beacon ("find devices" screen open).
    Beacon {
        on: bool,
    },
    Ping {
        peer: SocketAddrV4,
    },
    /// Send VBAN-TEXT to `peer` (a VoiceMeeter on another PC, or another ReTAE), e.g.
    /// `Strip[0].Gain=-6; Bus[1].Mute=1`. The text is passed as typed, so a VoiceMeeter peer
    /// understands its whole command language, not only what this router accepts (N7).
    PeerText {
        peer: SocketAddrV4,
        text: String,
    },
    Ui(UiLayout),
    SaveProfile {
        name: String,
    },
    LoadProfile {
        name: String,
    },
    DeleteProfile {
        name: String,
    },
}
// !SECTION: Commands

// SECTION: State
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum LinkState {
    /// Nothing to open, or closed.
    Idle,
    Running,
    /// The last attempt to open it failed; the text says why.
    Failed(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TxState {
    pub packets: u64,
    pub bytes: u64,
    pub send_errors: u32,
    pub dropped_frames: u32,
    /// Longest gap between two wakes of the sender thread, microseconds.
    pub max_tick_gap_us: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StripState {
    pub config: StripConfig,
    pub link: LinkState,
    /// Device streams only.
    pub stream: Option<StreamStats>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BusState {
    pub config: BusConfig,
    pub link: LinkState,
    pub stream: Option<StreamStats>,
    /// LAN output only.
    pub tx: Option<TxState>,
    /// The file being written while `config.record` is on.
    pub recording: Option<RecordingState>,
    /// Why the last attempt to start a recording failed (folder not writable, no free ring).
    pub record_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PolicyState {
    pub accept_any: bool,
    /// The list set by the user.
    pub allowed: Vec<[u8; 4]>,
    pub trust_discovered: bool,
    /// What the socket actually uses now (user list plus discovered machines).
    pub effective: Vec<[u8; 4]>,
}

/// A machine that answered a VBAN-SERVICE ping (VoiceMeeter or us).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeerState {
    pub addr: SocketAddrV4,
    pub device_name: String,
    pub application: String,
    pub host_name: String,
    pub is_lan_router: bool,
    pub rtt_ms: Option<f32>,
    pub age_ms: u32,
}

/// A machine found by mDNS.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiscoveredState {
    pub name: String,
    pub os: String,
    pub version: String,
    pub opus: bool,
    /// Where to send audio.
    pub vban_addr: Option<SocketAddrV4>,
}

/// A sender whose audio no strip accepted ("incoming, not assigned").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SeenSenderState {
    pub ip: [u8; 4],
    pub stream_name: String,
    pub channels: u16,
    pub sample_rate: u32,
    pub packets: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DiscoveryState {
    Off,
    Running,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub protocol: u32,
    /// Instance name shown to other machines.
    pub name: String,
    pub backend: String,
    pub capabilities: Capabilities,
    /// The UDP port the VBAN receive socket is bound to.
    pub rx_port: u16,
    pub strips: Vec<StripState>,
    pub buses: Vec<BusState>,
    pub policy: PolicyState,
    pub beacon: bool,
    pub peers: Vec<PeerState>,
    pub discovery: DiscoveryState,
    pub discovered: Vec<DiscoveredState>,
    pub seen_senders: Vec<SeenSenderState>,
    /// The profile last saved or loaded.
    pub profile: Option<String>,
    pub profiles: Vec<String>,
    pub ui: UiLayout,
}
// !SECTION: State

// SECTION: Meters
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MeterLevels {
    pub peak_db: [f32; 8],
    pub rms_db: [f32; 8],
    pub clipping: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LanRxLevels {
    pub buffer_ms: f32,
    pub target_ms: f32,
    pub drift_ppm: f32,
    pub packets_lost: u32,
    pub packets_late: u32,
    pub underruns: u32,
    /// `u32::MAX` = nothing received yet.
    pub last_packet_ms_ago: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineStatus {
    pub rt_load_permille: u32,
    pub rt_overruns: u32,
    pub input_underruns: u32,
    pub input_overruns: u32,
    pub output_overruns: u32,
    pub recorder_overruns: u32,
    pub rejected_commands: u32,
}

/// What the UI polls at about 30 Hz. Cheap: it only reads atomics.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MeterSnapshot {
    pub strips: Vec<MeterLevels>,
    pub buses: Vec<MeterLevels>,
    /// Indexed by strip.
    pub lan_rx: Vec<LanRxLevels>,
    pub any_audio_active: bool,
    pub engine: EngineStatus,
}
// !SECTION: Meters

// SECTION: Probe
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProbeRequest {
    /// Ping this peer (a router or VoiceMeeter) and report the round trip.
    pub peer: Option<SocketAddrV4>,
    /// Number of pings. Peers answer one per 250 ms, so 8 pings take about 2.5 s.
    pub pings: u8,
    /// Push clicks through this machine's own mixer and time them.
    pub click_loop: bool,
    pub clicks: u8,
}

impl Default for ProbeRequest {
    fn default() -> Self {
        Self {
            peer: None,
            pings: 8,
            click_loop: true,
            clicks: 10,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RttReport {
    pub sent: u32,
    pub received: u32,
    pub min_ms: f32,
    pub p50_ms: f32,
    pub p99_ms: f32,
    pub max_ms: f32,
    /// Mean absolute difference between consecutive round trips.
    pub jitter_ms: f32,
    /// Every measured round trip, in order, for the histogram.
    pub samples_ms: Vec<f32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClickLoopReport {
    pub clicks_sent: u32,
    pub clicks_found: u32,
    /// What the configured pipeline should give: the strip's FIFO target (jitter-buffer
    /// depth). On a quiet development host the measured median lands within about a
    /// millisecond of it; the gate is one block either side.
    pub expected_ms: f32,
    pub min_ms: f32,
    pub p50_ms: f32,
    pub max_ms: f32,
    pub block_ms: f32,
    /// `|p50 - expected| <= one block`.
    pub within_one_block: bool,
    /// Time the sending thread itself woke late while the test ran. A large value means the
    /// host was too busy for the numbers to mean much.
    pub host_noise_ms: f32,
    /// Which stages the number covers (and which it does not).
    pub scope: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProbeReport {
    pub rtt: Option<RttReport>,
    pub click_loop: Option<ClickLoopReport>,
    pub notes: Vec<String>,
}
// !SECTION: Probe

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_wire_type_survives_postcard() {
        let cmd = Command::StripSource {
            strip: 3,
            source: SourceEndpoint::HardwareInput {
                device_id: audio_types::FixedStr::from_str_trunc("mock-in-1"),
            },
            channels: 2,
        };
        let bytes = postcard::to_stdvec(&cmd).unwrap();
        assert_eq!(postcard::from_bytes::<Command>(&bytes).unwrap(), cmd);

        let probe = ProbeReport {
            rtt: Some(RttReport {
                sent: 3,
                received: 2,
                min_ms: 0.1,
                p50_ms: 0.2,
                p99_ms: 0.3,
                max_ms: 0.3,
                jitter_ms: 0.05,
                samples_ms: vec![0.1, 0.3],
            }),
            click_loop: None,
            notes: vec!["x".into()],
        };
        let bytes = postcard::to_stdvec(&probe).unwrap();
        assert_eq!(postcard::from_bytes::<ProbeReport>(&bytes).unwrap(), probe);

        let err = ControlError::Backend("no such device: x".into());
        let bytes = postcard::to_stdvec(&err).unwrap();
        assert_eq!(postcard::from_bytes::<ControlError>(&bytes).unwrap(), err);
    }
}
