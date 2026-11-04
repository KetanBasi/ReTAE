//! The seam between the mixer and a platform's audio devices (PLAN.md §6).
//!
//! A backend turns "open this device" into a thread that moves audio between the device and
//! one of the engine's pre-allocated port rings. The control plane (`app-core`) owns the
//! backend, picks a free port, hands it over, and gets it back when the stream is closed.
//!
//! ```text
//!   device  <-->  backend thread  <-->  InputPort / OutputPort (rtrb ring)  <-->  MixerEngine
//! ```
// ? Why the port travels *into* `open_*` and comes back on every failure and on `close`: the
// ? rings are a fixed pool (CLAUDE.md law 1) and are never freed. If an `open_*` that fails
// ? swallowed the port, the pool would shrink by one every failed attempt.

use audio_engine::{InputPort, OutputPort};
use audio_types::{SinkEndpoint, SourceEndpoint};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceKind {
    Input,
    Output,
}

/// A hardware (or mock) device as the UI lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// Stable identifier, what goes into `SourceEndpoint::HardwareInput { device_id }`.
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
    pub channels: u8,
    pub is_default: bool,
}

/// An application that can be captured on its own (Windows process loopback, PipeWire stream,
/// Android playback capture).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppInfo {
    pub pid: u32,
    pub name: String,
    /// Currently producing audio.
    pub playing: bool,
}

/// What this backend can do, so the UI can grey out the rest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub hardware_input: bool,
    pub hardware_output: bool,
    pub device_loopback: bool,
    pub process_loopback: bool,
    /// How many virtual inputs / virtual mics the backend can create.
    pub virtual_inputs: u8,
    pub virtual_mics: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendError {
    /// The backend cannot do this at all (for example a virtual mic on Android).
    NotSupported(String),
    NoSuchDevice(String),
    /// The device is open already or exclusively held by someone else.
    Busy(String),
    /// The channel count is not available on the device.
    BadChannels(u8),
    Io(String),
}

impl core::fmt::Display for BackendError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotSupported(what) => write!(f, "not supported: {what}"),
            Self::NoSuchDevice(id) => write!(f, "no such device: {id}"),
            Self::Busy(id) => write!(f, "device is busy: {id}"),
            Self::BadChannels(n) => write!(f, "the device cannot do {n} channels"),
            Self::Io(e) => write!(f, "audio I/O error: {e}"),
        }
    }
}

impl std::error::Error for BackendError {}

/// Counters of one running stream, read by the UI.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StreamStats {
    /// Frames moved so far.
    pub frames: u64,
    /// Input: frames lost because the ring was full. Output: times the device found the ring
    /// empty after it had started playing.
    pub xruns: u32,
    /// The clock of this device against ours, in ppm (positive = runs fast), if known.
    pub clock_ppm: f32,
}

/// The ring a closed stream gives back.
#[derive(Debug)]
pub enum ClosedPort {
    Input(InputPort),
    Output(OutputPort),
}

/// One open device stream. Dropping it stops the thread but loses the port; call `close`.
pub trait AudioStream: Send {
    fn stats(&self) -> StreamStats;

    /// True if the device's samples are clocked by the very callback that runs the mixer, so
    /// the strip can read its ring directly (`ClockDomain::Master`). Every other stream goes
    /// through the drift resampler (CLAUDE.md law 2).
    fn is_master_clocked(&self) -> bool {
        false
    }

    /// How the stream was opened, or why it stopped, in one line for diagnostics (for example
    /// "shared, IAudioClient3 period 128 frames"). Empty when the backend has nothing to say.
    fn detail(&self) -> String {
        String::new()
    }

    /// Stop the thread and hand the ring back to the pool.
    fn close(self: Box<Self>) -> Option<ClosedPort>;
}

pub trait AudioBackend: Send {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    fn devices(&self) -> Vec<DeviceInfo>;
    fn apps(&self) -> Vec<AppInfo>;

    /// Start feeding `port` from `source` with `channels` interleaved channels.
    fn open_input(
        &mut self,
        source: &SourceEndpoint,
        channels: u8,
        port: InputPort,
    ) -> Result<Box<dyn AudioStream>, (BackendError, InputPort)>;

    /// Start draining `port` into `sink`.
    fn open_output(
        &mut self,
        sink: &SinkEndpoint,
        channels: u8,
        port: OutputPort,
    ) -> Result<Box<dyn AudioStream>, (BackendError, OutputPort)>;
}
