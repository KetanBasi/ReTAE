//! Linux backend on PipeWire (PLAN.md §6.2).
//!
//! * Devices are PipeWire nodes. A stream is a `pw_stream` with `node.latency = 128/48000`.
//! * A virtual input is our own `Audio/Sink` node, a virtual mic our own `Audio/Source` node
//!   (marked `node.virtual`). Both exist only while the stream is open (no `pactl load-module`).
//! * Per-application capture links the application's output ports to a capture node of ours.
//! * Every stream is *externally* clocked: the mixer runs on `TimerMaster`, so input FIFOs are
//!   steered by the drift resampler inside the engine, and output streams run the same
//!   `ClockDomainAdapter` here on the device side (CLAUDE.md law 2).

mod stream;
mod thread;

use crate::backend::{
    AppInfo, AudioBackend, AudioStream, BackendError, Capabilities, ClosedPort, DeviceInfo,
    DeviceKind, StreamStats,
};
use crate::graph::OWN_NODE_PREFIX;
use crate::ring_io::{CaptureState, PlaybackState, Slot, take_input, take_output};
use audio_engine::{InputPort, OutputPort};
use audio_types::{ProcessLoopbackMode, SinkEndpoint, SourceEndpoint};
use std::collections::HashSet;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex, PoisonError};
use thread::{Data, Kind, Loop, OpenSpec};

/// What `open_input` decided to open: stream kind, target node name, whether to capture a
/// sink's monitor, the application pid (per-app capture), our own node name (virtual input).
type InputPlan = (Kind, Option<String>, bool, Option<u32>, Option<String>);

/// How many virtual inputs and virtual mics we offer.
pub const VIRTUAL_NODES: u8 = 4;

#[derive(Clone, Copy, Debug)]
pub struct PipeWireOptions {
    /// Audio kept queued between the mixer and an output device (the output's jitter buffer).
    /// The mixer hands blocks over in bursts, so this must exceed one block (2.7 ms).
    pub output_target_ms: f32,
}

impl Default for PipeWireOptions {
    fn default() -> Self {
        Self {
            output_target_ms: 8.0,
        }
    }
}

pub struct PipeWireBackend {
    lp: Arc<Loop>,
    options: PipeWireOptions,
    next_id: u64,
    /// Names of our own nodes that are open, so one cannot be opened twice.
    own_open: Arc<Mutex<HashSet<String>>>,
}

impl PipeWireBackend {
    /// Connect to the PipeWire server (`XDG_RUNTIME_DIR` / `PIPEWIRE_REMOTE`). Fails when none
    /// is running, which is how `routerd` knows to fall back to ALSA.
    pub fn connect(options: PipeWireOptions) -> Result<Self, BackendError> {
        let lp = Loop::start().map_err(BackendError::Io)?;
        Ok(Self {
            lp: Arc::new(lp),
            options,
            next_id: 1,
            own_open: Arc::default(),
        })
    }

    /// A copy of what the registry shows right now. Used by tests and diagnostics; the normal
    /// way to read it is `devices()` and `apps()`.
    pub fn graph(&self) -> crate::graph::GraphState {
        self.lp.graph()
    }

    fn claim_own(&self, name: &str) -> Result<(), BackendError> {
        let mut open = self.own_open.lock().unwrap_or_else(PoisonError::into_inner);
        if open.insert(name.to_owned()) {
            Ok(())
        } else {
            Err(BackendError::Busy(name.to_owned()))
        }
    }

    fn spec(&mut self, kind: Kind, channels: u8, data: Data) -> OpenSpec {
        let id = self.next_id;
        self.next_id += 1;
        OpenSpec {
            id,
            kind,
            node_name: format!("{OWN_NODE_PREFIX}stream-{id}"),
            description: format!("ReTAE stream {id}"),
            target: None,
            capture_sink: false,
            channels,
            pid: None,
            data,
        }
    }
}

fn check_channels(channels: u8) -> Result<(), BackendError> {
    if (1..=8).contains(&channels) {
        Ok(())
    } else {
        Err(BackendError::BadChannels(channels))
    }
}

/// `"default"` means "whatever the system default is".
fn is_default_id(id: &str) -> bool {
    id.is_empty() || id == "default"
}

impl AudioBackend for PipeWireBackend {
    fn name(&self) -> &'static str {
        "pipewire"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            hardware_input: true,
            hardware_output: true,
            device_loopback: true,
            process_loopback: true,
            virtual_inputs: VIRTUAL_NODES,
            virtual_mics: VIRTUAL_NODES,
        }
    }

    fn devices(&self) -> Vec<DeviceInfo> {
        self.lp.graph().devices()
    }

    fn apps(&self) -> Vec<AppInfo> {
        self.lp.graph().apps()
    }

    fn open_input(
        &mut self,
        source: &SourceEndpoint,
        channels: u8,
        port: InputPort,
    ) -> Result<Box<dyn AudioStream>, (BackendError, InputPort)> {
        if let Err(e) = check_channels(channels) {
            return Err((e, port));
        }
        let graph = self.lp.graph();
        let slot = Slot::new(CaptureState::new(port, usize::from(channels)));
        // Work out what to open. Errors here must return the port, which sits in `slot`.
        let planned: Result<InputPlan, BackendError> = match source {
            SourceEndpoint::HardwareInput { device_id } => {
                let id = device_id.as_str();
                if is_default_id(id) {
                    Ok((Kind::Capture, None, false, None, None))
                } else if graph.find_device(id, DeviceKind::Input).is_some() {
                    Ok((Kind::Capture, Some(id.to_owned()), false, None, None))
                } else {
                    Err(BackendError::NoSuchDevice(id.to_owned()))
                }
            }
            SourceEndpoint::DeviceLoopback { device_id } => {
                let id = device_id.as_str();
                if is_default_id(id) {
                    Ok((Kind::Capture, None, true, None, None))
                } else if graph.find_device(id, DeviceKind::Output).is_some() {
                    Ok((Kind::Capture, Some(id.to_owned()), true, None, None))
                } else {
                    Err(BackendError::NoSuchDevice(id.to_owned()))
                }
            }
            SourceEndpoint::ProcessLoopback { pid, mode, .. } => {
                if *mode == ProcessLoopbackMode::ExcludeTree {
                    // ? "Everything except this app" is a Windows idiom. On PipeWire the
                    // ? same effect is the default sink's monitor minus one stream, which
                    // ? a link-based design cannot express.
                    Err(BackendError::NotSupported(
                        "capturing everything except one application".into(),
                    ))
                } else if graph.app_nodes(*pid).is_empty() {
                    Err(BackendError::NoSuchDevice(format!("app:{pid}")))
                } else {
                    Ok((Kind::AppCapture, None, false, Some(*pid), None))
                }
            }
            SourceEndpoint::VirtualInput { index } => {
                if *index >= VIRTUAL_NODES {
                    Err(BackendError::NoSuchDevice(format!("virtual-in:{index}")))
                } else {
                    let name = format!("{OWN_NODE_PREFIX}virtual-in-{index}");
                    Ok((Kind::VirtualSink, None, false, None, Some(name)))
                }
            }
            other => Err(BackendError::NotSupported(format!(
                "{other:?} is not a Linux source"
            ))),
        };
        let (kind, target, capture_sink, pid, own_name) = match planned {
            Ok(p) => p,
            Err(e) => return Err((e, take_input(&slot))),
        };

        let mut spec = self.spec(kind, channels, Data::Capture(slot.clone()));
        spec.target = target;
        spec.capture_sink = capture_sink;
        spec.pid = pid;
        if let Some(name) = &own_name {
            if let Err(e) = self.claim_own(name) {
                return Err((e, take_input(&slot)));
            }
            spec.node_name.clone_from(name);
            spec.description = format!("ReTAE Virtual Input {}", virtual_number(name));
        }
        let id = spec.id;
        if let Err(e) = self.lp.open(spec) {
            self.lp.close(id);
            self.release_own(own_name.as_deref());
            return Err((BackendError::Io(e), take_input(&slot)));
        }
        Ok(Box::new(PwStream {
            id,
            lp: self.lp.clone(),
            own_open: self.own_open.clone(),
            own_name,
            slot: Slotted::Capture(slot),
        }))
    }

    fn open_output(
        &mut self,
        sink: &SinkEndpoint,
        channels: u8,
        port: OutputPort,
    ) -> Result<Box<dyn AudioStream>, (BackendError, OutputPort)> {
        if let Err(e) = check_channels(channels) {
            return Err((e, port));
        }
        let graph = self.lp.graph();
        let slot = Slot::new(PlaybackState::new(
            port,
            usize::from(channels),
            self.options.output_target_ms,
        ));
        let planned: Result<(Kind, Option<String>, Option<String>), BackendError> = match sink {
            SinkEndpoint::HardwareOutput { device_id } => {
                let id = device_id.as_str();
                if is_default_id(id) {
                    Ok((Kind::Playback, None, None))
                } else if graph.find_device(id, DeviceKind::Output).is_some() {
                    Ok((Kind::Playback, Some(id.to_owned()), None))
                } else {
                    Err(BackendError::NoSuchDevice(id.to_owned()))
                }
            }
            SinkEndpoint::VirtualMic { index } => {
                if *index >= VIRTUAL_NODES {
                    Err(BackendError::NoSuchDevice(format!("virtual-mic:{index}")))
                } else {
                    let name = format!("{OWN_NODE_PREFIX}virtual-mic-{index}");
                    Ok((Kind::VirtualSource, None, Some(name)))
                }
            }
            other => Err(BackendError::NotSupported(format!(
                "{other:?} is not a Linux sink"
            ))),
        };
        let (kind, target, own_name) = match planned {
            Ok(p) => p,
            Err(e) => return Err((e, take_output(&slot))),
        };

        let mut spec = self.spec(kind, channels, Data::Playback(slot.clone()));
        spec.target = target;
        if let Some(name) = &own_name {
            if let Err(e) = self.claim_own(name) {
                return Err((e, take_output(&slot)));
            }
            spec.node_name.clone_from(name);
            spec.description = format!("ReTAE Virtual Mic {}", virtual_number(name));
        }
        let id = spec.id;
        if let Err(e) = self.lp.open(spec) {
            self.lp.close(id);
            self.release_own(own_name.as_deref());
            return Err((BackendError::Io(e), take_output(&slot)));
        }
        Ok(Box::new(PwStream {
            id,
            lp: self.lp.clone(),
            own_open: self.own_open.clone(),
            own_name,
            slot: Slotted::Playback(slot),
        }))
    }
}

impl PipeWireBackend {
    fn release_own(&self, name: Option<&str>) {
        if let Some(name) = name {
            self.own_open
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(name);
        }
    }
}

/// "retae-virtual-in-2" -> 3 (what a person sees).
fn virtual_number(node_name: &str) -> u32 {
    node_name
        .rsplit('-')
        .next()
        .and_then(|n| n.parse::<u32>().ok())
        .map_or(0, |n| n + 1)
}

enum Slotted {
    Capture(Arc<Slot<CaptureState>>),
    Playback(Arc<Slot<PlaybackState>>),
}

struct PwStream {
    id: u64,
    lp: Arc<Loop>,
    own_open: Arc<Mutex<HashSet<String>>>,
    own_name: Option<String>,
    slot: Slotted,
}

impl PwStream {
    fn counters(&self) -> &stream::Counters {
        match &self.slot {
            Slotted::Capture(s) => &s.counters,
            Slotted::Playback(s) => &s.counters,
        }
    }

    fn free_own_name(&self) {
        if let Some(name) = &self.own_name {
            self.own_open
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(name);
        }
    }
}

impl AudioStream for PwStream {
    fn stats(&self) -> StreamStats {
        let c = self.counters();
        StreamStats {
            frames: c.frames.load(Relaxed),
            xruns: c.xruns.load(Relaxed),
            clock_ppm: f32::from_bits(c.ppm_bits.load(Relaxed)),
        }
    }

    fn close(self: Box<Self>) -> Option<ClosedPort> {
        // Destroy the stream first: once the loop thread confirms, no callback can touch the
        // port, and it can be taken out of its slot.
        self.lp.close(self.id);
        self.free_own_name();
        match &self.slot {
            Slotted::Capture(s) => s.take().map(|st| ClosedPort::Input(st.into_port())),
            Slotted::Playback(s) => s.take().map(|st| ClosedPort::Output(st.into_port())),
        }
    }
}

impl Drop for PwStream {
    fn drop(&mut self) {
        // Dropped without `close`: stop the callbacks. The port stays in its slot and is freed
        // with it; the caller chose to lose it.
        self.lp.close(self.id);
        self.free_own_name();
    }
}
