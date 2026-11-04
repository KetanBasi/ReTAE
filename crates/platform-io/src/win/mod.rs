//! Windows backend on WASAPI (PLAN.md §6.1).
//!
//! * Devices are WASAPI endpoints. Streams are event-driven shared mode at 48 kHz `f32`, with
//!   `IAudioClient3`'s smallest period when the driver offers it, or exclusive mode on request.
//! * Device loopback records what an output device plays. Per-application capture uses process
//!   loopback (Windows build 20348 or newer) and is switched off in `capabilities()` before that.
//! * Virtual inputs and microphones come from an installed virtual audio cable (VB-CABLE and
//!   its siblings) or the Virtual Audio Driver; we cannot make a kernel device ourselves, and
//!   VoiceMeeter's own devices do not count (they need VoiceMeeter's engine running).
//! * Like every non-master device, a stream runs on its own clock: inputs are steered by the
//!   drift resampler inside the engine, outputs by the one in the device thread (CLAUDE.md
//!   law 2). `TimerMaster` clocks the mixer.

// ? `sys` holds the only `unsafe` code of the crate (MMCSS, OS build, process list).
#[allow(unsafe_code)]
pub(crate) mod sys;

mod devices;
mod stream;

use crate::backend::{
    AppInfo, AudioBackend, AudioStream, BackendError, Capabilities, ClosedPort, DeviceInfo,
    StreamStats,
};
use crate::ring_io::{CaptureState, Counters, PlaybackState, Slot, take_input, take_output};
use crate::win_rules::{
    VirtualIn, VirtualMic, loopback_root, plan_virtuals, process_loopback_supported,
};
use audio_engine::{InputPort, OutputPort};
use audio_types::{ProcessLoopbackMode, SinkEndpoint, SourceEndpoint};
use std::collections::HashSet;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex, PoisonError};
use stream::{Request, Target, Worker};

pub use sys::promote_current_thread_mmcss;

#[derive(Clone, Copy, Debug)]
pub struct WasapiOptions {
    /// Audio kept queued between the mixer and an output device (the output's jitter buffer).
    /// Must exceed one 2.7 ms block because the mixer hands blocks over in bursts.
    pub output_target_ms: f32,
    /// Try the driver's smallest period (`IAudioClient3`) before the system default.
    pub low_latency: bool,
    /// Open hardware devices in exclusive mode. Gives the lowest latency, but other programs
    /// lose the device and the card must accept 48 kHz as it is. Expert option.
    pub exclusive: bool,
}

impl Default for WasapiOptions {
    fn default() -> Self {
        Self {
            output_target_ms: 8.0,
            low_latency: true,
            exclusive: false,
        }
    }
}

#[derive(Default)]
struct Virtuals {
    inputs: Vec<VirtualIn>,
    mics: Vec<VirtualMic>,
}

pub struct WasapiBackend {
    options: WasapiOptions,
    build: u32,
    /// Cable list from the last device listing. Opening or `capabilities()` must not enumerate
    /// every endpoint again, so `devices()` refreshes this and the rest reads it.
    virtuals: Mutex<Virtuals>,
    /// Cables in use, so one cannot be opened both as an input and as a mic.
    claims: Arc<Mutex<HashSet<String>>>,
}

impl WasapiBackend {
    /// Fails if Windows' audio service cannot be reached (it is stopped or disabled).
    pub fn connect(options: WasapiOptions) -> Result<Self, BackendError> {
        let listed = devices::list_endpoints()?;
        let backend = Self {
            options,
            build: sys::os_build(),
            virtuals: Mutex::default(),
            claims: Arc::default(),
        };
        backend.refresh_virtuals(&listed);
        Ok(backend)
    }

    /// The Windows build number (for example 22631), 0 if unknown.
    pub fn os_build(&self) -> u32 {
        self.build
    }

    fn refresh_virtuals(&self, listed: &[devices::Listed]) {
        let endpoints: Vec<_> = listed.iter().map(|l| l.endpoint.clone()).collect();
        let (inputs, mics) = plan_virtuals(&endpoints);
        *self.virtuals.lock().unwrap_or_else(PoisonError::into_inner) = Virtuals { inputs, mics };
    }

    fn process_loopback_ok(&self) -> bool {
        process_loopback_supported(self.build)
    }

    fn claim(&self, key: &str) -> Result<Claim, BackendError> {
        let mut set = self.claims.lock().unwrap_or_else(PoisonError::into_inner);
        if set.insert(key.to_owned()) {
            Ok(Claim {
                set: self.claims.clone(),
                key: key.to_owned(),
            })
        } else {
            Err(BackendError::Busy(format!(
                "{key} is already in use here (one cable cannot be both a virtual input and a \
                 virtual mic; install a second cable)"
            )))
        }
    }

    fn request(&self, target: Target, channels: u8) -> Request {
        Request {
            target,
            channels: usize::from(channels),
            // ? Exclusive mode only makes sense for real hardware: virtual cables and the
            // ? loopbacks are always shared.
            exclusive: self.options.exclusive,
            low_latency: self.options.low_latency,
        }
    }
}

/// Releases a cable claim when the stream goes away, on `close` and on a plain drop alike.
struct Claim {
    set: Arc<Mutex<HashSet<String>>>,
    key: String,
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.set
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.key);
    }
}

fn check_channels(channels: u8) -> Result<(), BackendError> {
    if (1..=8).contains(&channels) {
        Ok(())
    } else {
        Err(BackendError::BadChannels(channels))
    }
}

impl AudioBackend for WasapiBackend {
    fn name(&self) -> &'static str {
        "wasapi"
    }

    fn capabilities(&self) -> Capabilities {
        let v = self.virtuals.lock().unwrap_or_else(PoisonError::into_inner);
        Capabilities {
            hardware_input: true,
            hardware_output: true,
            device_loopback: true,
            process_loopback: self.process_loopback_ok(),
            virtual_inputs: v.inputs.len().min(255) as u8,
            virtual_mics: v.mics.len().min(255) as u8,
        }
    }

    fn devices(&self) -> Vec<DeviceInfo> {
        // ? An audio service that stopped answering gives an empty list rather than an error:
        // ? the trait has no way to say "failed", and the next call may work again.
        let Ok(listed) = devices::list_endpoints() else {
            return Vec::new();
        };
        self.refresh_virtuals(&listed);
        listed
            .into_iter()
            .map(|l| DeviceInfo {
                id: l.endpoint.id,
                name: l.endpoint.name,
                kind: l.endpoint.kind,
                channels: l.channels,
                is_default: l.is_default,
            })
            .collect()
    }

    fn apps(&self) -> Vec<AppInfo> {
        if !self.process_loopback_ok() {
            return Vec::new();
        }
        let table = sys::process_table();
        let own = std::process::id();
        let mut apps: Vec<AppInfo> = Vec::new();
        for (pid, playing) in devices::app_sessions() {
            let root = loopback_root(pid, &table);
            // The router's own sessions are never offered: capturing them would be a loop.
            if root == own || pid == own {
                continue;
            }
            match apps.iter_mut().find(|a| a.pid == root) {
                Some(app) => app.playing |= playing,
                None => {
                    let name = table
                        .iter()
                        .find(|e| e.pid == root)
                        .map_or_else(|| format!("pid {root}"), |e| e.exe.clone());
                    apps.push(AppInfo {
                        pid: root,
                        name,
                        playing,
                    });
                }
            }
        }
        apps.sort_by(|a, b| (!a.playing, &a.name).cmp(&(!b.playing, &b.name)));
        apps
    }

    fn open_input(
        &mut self,
        source: &SourceEndpoint,
        channels: u8,
        port: InputPort,
    ) -> Result<Box<dyn AudioStream>, (BackendError, InputPort)> {
        let slot = Slot::new(CaptureState::new(port, usize::from(channels)));
        let fail = |e| Err((e, take_input(&slot)));
        if let Err(e) = check_channels(channels) {
            return fail(e);
        }
        let mut claim = None;
        let target = match source {
            SourceEndpoint::HardwareInput { device_id } => Target::Capture {
                id: device_id.as_str().to_owned(),
            },
            SourceEndpoint::DeviceLoopback { device_id } => Target::Loopback {
                id: device_id.as_str().to_owned(),
            },
            SourceEndpoint::ProcessLoopback { pid, mode, .. } => {
                if !self.process_loopback_ok() {
                    return fail(BackendError::NotSupported(format!(
                        "capturing one application needs Windows build {} or newer (this is \
                         build {}); record the device with device loopback instead",
                        crate::win_rules::PROCESS_LOOPBACK_MIN_BUILD,
                        self.build
                    )));
                }
                let root = loopback_root(*pid, &sys::process_table());
                Target::Process {
                    pid: root,
                    // ? `wasapi` maps `true` to "include the process tree" and `false` to
                    // ? "exclude it", whatever its documentation says about "target only".
                    include_tree: *mode == ProcessLoopbackMode::IncludeTree,
                }
            }
            SourceEndpoint::VirtualInput { index } => {
                let entry = self
                    .virtuals
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .inputs
                    .get(usize::from(*index))
                    .cloned();
                let Some(v) = entry else {
                    return fail(no_cable("virtual input", *index));
                };
                match self.claim(&v.claim) {
                    Ok(c) => claim = Some(c),
                    Err(e) => return fail(e),
                }
                if v.via_loopback {
                    Target::Loopback { id: v.endpoint_id }
                } else {
                    Target::Capture { id: v.endpoint_id }
                }
            }
            other => {
                return fail(BackendError::NotSupported(format!(
                    "{other:?} is not a Windows source"
                )));
            }
        };
        let mut req = self.request(target, channels);
        if claim.is_some() || matches!(req.target, Target::Loopback { .. } | Target::Process { .. })
        {
            req.exclusive = false;
        }
        match stream::start_capture(req, slot.clone()) {
            Ok(worker) => Ok(Box::new(WasapiStream {
                worker,
                slot: Slotted::Capture(slot),
                _claim: claim,
            })),
            Err(e) => {
                drop(claim);
                fail(e)
            }
        }
    }

    fn open_output(
        &mut self,
        sink: &SinkEndpoint,
        channels: u8,
        port: OutputPort,
    ) -> Result<Box<dyn AudioStream>, (BackendError, OutputPort)> {
        let slot = Slot::new(PlaybackState::new(
            port,
            usize::from(channels),
            self.options.output_target_ms,
        ));
        let fail = |e| Err((e, take_output(&slot)));
        if let Err(e) = check_channels(channels) {
            return fail(e);
        }
        let mut claim = None;
        let mut virtual_mic = false;
        let target = match sink {
            SinkEndpoint::HardwareOutput { device_id } => Target::Playback {
                id: device_id.as_str().to_owned(),
            },
            SinkEndpoint::VirtualMic { index } => {
                let entry = self
                    .virtuals
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .mics
                    .get(usize::from(*index))
                    .cloned();
                let Some(v) = entry else {
                    return fail(no_cable("virtual mic", *index));
                };
                match self.claim(&v.claim) {
                    Ok(c) => claim = Some(c),
                    Err(e) => return fail(e),
                }
                virtual_mic = true;
                Target::Playback { id: v.endpoint_id }
            }
            other => {
                return fail(BackendError::NotSupported(format!(
                    "{other:?} is not a Windows sink"
                )));
            }
        };
        let mut req = self.request(target, channels);
        if virtual_mic {
            req.exclusive = false;
        }
        match stream::start_playback(req, slot.clone()) {
            Ok(worker) => Ok(Box::new(WasapiStream {
                worker,
                slot: Slotted::Playback(slot),
                _claim: claim,
            })),
            Err(e) => {
                drop(claim);
                fail(e)
            }
        }
    }
}

fn no_cable(what: &str, index: u8) -> BackendError {
    BackendError::NoSuchDevice(format!(
        "{what} {index}: no such virtual audio cable is installed (install VB-CABLE from \
         vb-audio.com, or the open-source Virtual Audio Driver)"
    ))
}

enum Slotted {
    Capture(Arc<Slot<CaptureState>>),
    Playback(Arc<Slot<PlaybackState>>),
}

struct WasapiStream {
    // ? Field order matters: the worker is stopped (joined) by `close` before the port is taken
    // ? out of the slot, and the cable claim is released last, when the stream is dropped.
    worker: Worker,
    slot: Slotted,
    _claim: Option<Claim>,
}

impl WasapiStream {
    fn counters(&self) -> &Counters {
        match &self.slot {
            Slotted::Capture(s) => &s.counters,
            Slotted::Playback(s) => &s.counters,
        }
    }
}

impl AudioStream for WasapiStream {
    fn stats(&self) -> StreamStats {
        let c = self.counters();
        StreamStats {
            frames: c.frames.load(Relaxed),
            xruns: c.xruns.load(Relaxed),
            clock_ppm: f32::from_bits(c.ppm_bits.load(Relaxed)),
        }
    }

    fn detail(&self) -> String {
        self.worker
            .detail
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn close(mut self: Box<Self>) -> Option<ClosedPort> {
        // Stop and join the thread first: after that no callback can touch the ring.
        self.worker.stop();
        match &self.slot {
            Slotted::Capture(s) => s.take().map(|st| ClosedPort::Input(st.into_port())),
            Slotted::Playback(s) => s.take().map(|st| ClosedPort::Output(st.into_port())),
        }
    }
}
