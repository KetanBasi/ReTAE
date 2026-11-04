//! `MockBackend`: fake devices for tests and for `routerd --backend mock`.
//!
//! Every mock stream runs on its own paced thread (see `RateClock`) and behaves like a device
//! whose crystal is not ours: its clock can be a few ppm off, and it can drop out for a while.
//! `MockControl` is the remote control the tests hold to steer them.
//!
//! * All mock streams are *external* clocked (`is_master_clocked() == false`), so they go
//!   through the drift resampler exactly like a second sound card would.

use crate::backend::{
    AppInfo, AudioBackend, AudioStream, BackendError, Capabilities, ClosedPort, DeviceInfo,
    DeviceKind, StreamStats,
};
use crate::clock::RateClock;
use audio_engine::{InputPort, OutputPort};
use audio_types::{ENGINE_SAMPLE_RATE, SinkEndpoint, SourceEndpoint};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

/// Frames moved per ring access. Small enough that a late wake still fits a ring, large enough
/// to keep the thread cheap.
const CHUNK_FRAMES: usize = 96;
const TICK: Duration = Duration::from_millis(1);
const MOCK_CHANNELS: u8 = 2;
const VIRTUAL_INPUTS: u8 = 2;
const VIRTUAL_MICS: u8 = 2;

// SECTION: Signals
/// What a mock input produces.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Signal {
    Silence,
    Sine {
        freq_hz: f32,
        amp: f32,
    },
    Noise {
        amp: f32,
    },
    /// One full-scale-`amp` frame (on every channel) every `period_frames` frames.
    Click {
        period_frames: u32,
        amp: f32,
    },
}

#[derive(Debug)]
struct Generator {
    phase: f64,
    rng: u32,
    frames_since_click: u32,
}

impl Generator {
    fn new() -> Self {
        Self {
            phase: 0.0,
            rng: 0x1234_5678,
            frames_since_click: u32::MAX / 2,
        }
    }

    /// Fill `out` (interleaved, `channels` per frame) with the next frames of `signal`.
    fn fill(&mut self, signal: Signal, channels: usize, out: &mut [f32]) {
        for frame in out.chunks_exact_mut(channels) {
            let v = match signal {
                Signal::Silence => 0.0,
                Signal::Sine { freq_hz, amp } => {
                    let v = (self.phase * core::f64::consts::TAU).sin() as f32 * amp;
                    self.phase += f64::from(freq_hz) / f64::from(ENGINE_SAMPLE_RATE);
                    self.phase -= self.phase.floor();
                    v
                }
                Signal::Noise { amp } => {
                    // xorshift32: cheap, deterministic, good enough for a test signal.
                    self.rng ^= self.rng << 13;
                    self.rng ^= self.rng >> 17;
                    self.rng ^= self.rng << 5;
                    (self.rng as f32 / u32::MAX as f32 * 2.0 - 1.0) * amp
                }
                Signal::Click { period_frames, amp } => {
                    self.frames_since_click = self.frames_since_click.saturating_add(1);
                    if self.frames_since_click >= period_frames.max(1) {
                        self.frames_since_click = 0;
                        amp
                    } else {
                        0.0
                    }
                }
            };
            frame.fill(v);
        }
    }
}
// !SECTION: Signals

// SECTION: Shared per-device state
/// What an output device has heard so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Capture {
    /// Frames that really arrived (an underrun is not counted).
    pub frames: u64,
    pub peak: f32,
    pub sum_sq: f64,
    /// Frames whose every channel was exactly zero.
    pub silent_frames: u64,
}

impl Capture {
    pub fn rms(&self) -> f32 {
        if self.frames == 0 {
            0.0
        } else {
            (self.sum_sq / self.frames as f64).sqrt() as f32
        }
    }
}

#[derive(Debug)]
struct DeviceState {
    signal: Mutex<Signal>,
    /// Clock offset in ppm, stored as `f32` bits.
    ppm: AtomicU32,
    /// Frames still to be swallowed (the device "stopped" for that long).
    dropout_frames: AtomicU64,
    open: AtomicBool,
    capture: Mutex<Capture>,
}

impl DeviceState {
    fn new() -> Self {
        Self {
            signal: Mutex::new(Signal::Silence),
            ppm: AtomicU32::new(0.0_f32.to_bits()),
            dropout_frames: AtomicU64::new(0),
            open: AtomicBool::new(false),
            capture: Mutex::new(Capture::default()),
        }
    }
    fn ppm(&self) -> f32 {
        f32::from_bits(self.ppm.load(Relaxed))
    }
}

// ? A panicking test thread must not poison the mock for everyone else; the data is plain
// ? numbers, so reading it after a poison is safe.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
// !SECTION: Shared per-device state

// SECTION: MockControl
/// The test's handle on the fake devices. Cheap to clone; all clones steer the same devices.
///
/// Keys are `MockBackend`'s device ids: `mock-in-1`, `mock-out-1`, the app key `app:1001`, the
/// virtual input `virtual-in:0` and the virtual mic `virtual-mic:0`. A loopback of an output
/// device uses `loopback:<output id>`.
#[derive(Clone, Debug)]
pub struct MockControl {
    devices: Arc<HashMap<String, Arc<DeviceState>>>,
}

impl MockControl {
    fn dev(&self, key: &str) -> &Arc<DeviceState> {
        // ? Unknown keys are a bug in the test, so fail loudly instead of silently doing nothing.
        self.devices
            .get(key)
            .unwrap_or_else(|| panic!("no mock device {key:?}"))
    }

    pub fn set_signal(&self, key: &str, signal: Signal) {
        *lock(&self.dev(key).signal) = signal;
    }

    /// Make the device's clock run `ppm` fast (negative: slow).
    pub fn set_ppm(&self, key: &str, ppm: f32) {
        self.dev(key).ppm.store(ppm.to_bits(), Relaxed);
    }

    /// The device stops for `ms` (time passes, frames are lost).
    pub fn inject_dropout(&self, key: &str, ms: u32) {
        let frames = u64::from(ms) * u64::from(ENGINE_SAMPLE_RATE) / 1000;
        self.dev(key).dropout_frames.fetch_add(frames, Relaxed);
    }

    pub fn capture(&self, key: &str) -> Capture {
        *lock(&self.dev(key).capture)
    }

    pub fn reset_capture(&self, key: &str) {
        *lock(&self.dev(key).capture) = Capture::default();
    }

    pub fn is_open(&self, key: &str) -> bool {
        self.dev(key).open.load(Relaxed)
    }
}
// !SECTION: MockControl

// SECTION: MockBackend
pub struct MockBackend {
    ctl: MockControl,
}

fn all_keys() -> Vec<String> {
    let mut keys: Vec<String> = ["mock-in-1", "mock-in-2", "mock-out-1", "mock-out-2"]
        .map(String::from)
        .to_vec();
    keys.extend(["loopback:mock-out-1", "loopback:mock-out-2"].map(String::from));
    keys.extend(["app:1001", "app:1002"].map(String::from));
    keys.extend((0..VIRTUAL_INPUTS).map(|i| format!("virtual-in:{i}")));
    keys.extend((0..VIRTUAL_MICS).map(|i| format!("virtual-mic:{i}")));
    keys
}

impl MockBackend {
    pub fn new() -> (Self, MockControl) {
        let devices = all_keys()
            .into_iter()
            .map(|k| (k, Arc::new(DeviceState::new())))
            .collect();
        let ctl = MockControl {
            devices: Arc::new(devices),
        };
        (Self { ctl: ctl.clone() }, ctl)
    }

    fn input_key(source: &SourceEndpoint) -> Result<String, BackendError> {
        Ok(match source {
            SourceEndpoint::HardwareInput { device_id } => device_id.as_str().to_owned(),
            SourceEndpoint::DeviceLoopback { device_id } => {
                format!("loopback:{}", device_id.as_str())
            }
            SourceEndpoint::ProcessLoopback { pid, .. } => format!("app:{pid}"),
            SourceEndpoint::VirtualInput { index } => format!("virtual-in:{index}"),
            other => {
                return Err(BackendError::NotSupported(format!(
                    "{other:?} is not a device source"
                )));
            }
        })
    }

    fn output_key(sink: &SinkEndpoint) -> Result<String, BackendError> {
        Ok(match sink {
            SinkEndpoint::HardwareOutput { device_id } => device_id.as_str().to_owned(),
            SinkEndpoint::VirtualMic { index } => format!("virtual-mic:{index}"),
            other => {
                return Err(BackendError::NotSupported(format!(
                    "{other:?} is not a device sink"
                )));
            }
        })
    }

    /// Look the key up and take the "open" flag, or say why not.
    fn claim(&self, key: &str, want_input: bool) -> Result<Arc<DeviceState>, BackendError> {
        let right_kind = if want_input {
            key.starts_with("mock-in-")
                || key.starts_with("loopback:")
                || key.starts_with("app:")
                || key.starts_with("virtual-in:")
        } else {
            key.starts_with("mock-out-") || key.starts_with("virtual-mic:")
        };
        let state = self
            .ctl
            .devices
            .get(key)
            .filter(|_| right_kind)
            .ok_or_else(|| BackendError::NoSuchDevice(key.to_owned()))?;
        if state.open.swap(true, Relaxed) {
            return Err(BackendError::Busy(key.to_owned()));
        }
        Ok(Arc::clone(state))
    }
}

fn check_channels(channels: u8) -> Result<(), BackendError> {
    if (1..=MOCK_CHANNELS).contains(&channels) {
        Ok(())
    } else {
        Err(BackendError::BadChannels(channels))
    }
}

impl AudioBackend for MockBackend {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            hardware_input: true,
            hardware_output: true,
            device_loopback: true,
            process_loopback: true,
            virtual_inputs: VIRTUAL_INPUTS,
            virtual_mics: VIRTUAL_MICS,
        }
    }

    fn devices(&self) -> Vec<DeviceInfo> {
        let dev = |id: &str, name: &str, kind, is_default| DeviceInfo {
            id: id.to_owned(),
            name: name.to_owned(),
            kind,
            channels: MOCK_CHANNELS,
            is_default,
        };
        vec![
            dev("mock-in-1", "Mock microphone", DeviceKind::Input, true),
            dev("mock-in-2", "Mock line in", DeviceKind::Input, false),
            dev("mock-out-1", "Mock speakers", DeviceKind::Output, true),
            dev("mock-out-2", "Mock headphones", DeviceKind::Output, false),
        ]
    }

    fn apps(&self) -> Vec<AppInfo> {
        vec![
            AppInfo {
                pid: 1001,
                name: "mock-player".to_owned(),
                playing: true,
            },
            AppInfo {
                pid: 1002,
                name: "mock-browser".to_owned(),
                playing: false,
            },
        ]
    }

    fn open_input(
        &mut self,
        source: &SourceEndpoint,
        channels: u8,
        port: InputPort,
    ) -> Result<Box<dyn AudioStream>, (BackendError, InputPort)> {
        let state = match check_channels(channels)
            .and_then(|()| Self::input_key(source))
            .and_then(|key| self.claim(&key, true))
        {
            Ok(state) => state,
            Err(e) => return Err((e, port)),
        };
        let counters = Arc::new(Counters::default());
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (state, counters, stop) = (state.clone(), counters.clone(), stop.clone());
            std::thread::Builder::new()
                .name("mock-in".into())
                .spawn(move || input_loop(&state, usize::from(channels), port, &counters, &stop))
        };
        match thread {
            Ok(thread) => Ok(Box::new(MockStream {
                state,
                counters,
                stop,
                thread: Some(thread),
                wrap: ClosedPort::Input,
            })),
            Err(e) => {
                state.open.store(false, Relaxed);
                // ? The closure (and the port in it) went down with the failed spawn, so there
                // ? is no port to give back. A thread that cannot be started is a dying machine.
                Err((BackendError::Io(e.to_string()), lost_port()))
            }
        }
    }

    fn open_output(
        &mut self,
        sink: &SinkEndpoint,
        channels: u8,
        port: OutputPort,
    ) -> Result<Box<dyn AudioStream>, (BackendError, OutputPort)> {
        let state = match check_channels(channels)
            .and_then(|()| Self::output_key(sink))
            .and_then(|key| self.claim(&key, false))
        {
            Ok(state) => state,
            Err(e) => return Err((e, port)),
        };
        let counters = Arc::new(Counters::default());
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (state, counters, stop) = (state.clone(), counters.clone(), stop.clone());
            std::thread::Builder::new()
                .name("mock-out".into())
                .spawn(move || output_loop(&state, usize::from(channels), port, &counters, &stop))
        };
        match thread {
            Ok(thread) => Ok(Box::new(MockStream {
                state,
                counters,
                stop,
                thread: Some(thread),
                wrap: ClosedPort::Output,
            })),
            Err(e) => {
                state.open.store(false, Relaxed);
                Err((BackendError::Io(e.to_string()), lost_port()))
            }
        }
    }
}
// !SECTION: MockBackend

// SECTION: Stream threads
#[derive(Debug, Default)]
struct Counters {
    frames: AtomicU64,
    xruns: AtomicU32,
}

fn input_loop(
    state: &DeviceState,
    channels: usize,
    mut port: InputPort,
    counters: &Counters,
    stop: &AtomicBool,
) -> InputPort {
    let mut clock = RateClock::new(f64::from(ENGINE_SAMPLE_RATE));
    let mut generator = Generator::new();
    let mut buf = vec![0.0_f32; CHUNK_FRAMES * channels];
    while !stop.load(Relaxed) {
        std::thread::sleep(TICK);
        clock.set_ppm(f64::from(state.ppm()));
        let mut due = clock.due();

        // The device was "stopped": that stretch of time produces no frames at all.
        let swallow = state.dropout_frames.load(Relaxed).min(due as u64);
        if swallow > 0 {
            state.dropout_frames.fetch_sub(swallow, Relaxed);
            due -= swallow as usize;
        }

        let signal = *lock(&state.signal);
        while due > 0 {
            let n = due.min(CHUNK_FRAMES);
            let chunk = &mut buf[..n * channels];
            generator.fill(signal, channels, chunk);
            let accepted = port.push_frames(chunk, channels);
            if accepted < chunk.len() {
                counters.xruns.fetch_add(1, Relaxed);
            }
            counters.frames.fetch_add(n as u64, Relaxed);
            due -= n;
        }
    }
    port
}

fn output_loop(
    state: &DeviceState,
    channels: usize,
    mut port: OutputPort,
    counters: &Counters,
    stop: &AtomicBool,
) -> OutputPort {
    let mut clock = RateClock::new(f64::from(ENGINE_SAMPLE_RATE));
    let mut buf = vec![0.0_f32; CHUNK_FRAMES * channels];
    let mut started = false;
    while !stop.load(Relaxed) {
        std::thread::sleep(TICK);
        clock.set_ppm(f64::from(state.ppm()));
        let mut due = clock.due();

        let swallow = state.dropout_frames.load(Relaxed).min(due as u64);
        if swallow > 0 {
            state.dropout_frames.fetch_sub(swallow, Relaxed);
            // ? The device is stopped, so the engine's blocks pile up in the ring meanwhile.
            due -= swallow as usize;
        }

        while due > 0 {
            let n = due.min(CHUNK_FRAMES);
            let want = n * channels;
            let got = port.pop(&mut buf[..want]);
            let frames = got / channels;
            if got < want && started {
                counters.xruns.fetch_add(1, Relaxed);
            }
            if got > 0 {
                started = true;
            }
            if frames > 0 {
                record(state, &buf[..frames * channels], channels);
            }
            counters.frames.fetch_add(frames as u64, Relaxed);
            due -= n;
            if got < want {
                break; // the ring is dry; the rest of this tick would only be more silence
            }
        }
    }
    port
}

fn record(state: &DeviceState, samples: &[f32], channels: usize) {
    let mut cap = lock(&state.capture);
    for frame in samples.chunks_exact(channels) {
        cap.frames += 1;
        let mut silent = true;
        for &v in frame {
            let a = v.abs();
            cap.peak = cap.peak.max(a);
            cap.sum_sq += f64::from(v) * f64::from(v) / channels as f64;
            silent &= v == 0.0;
        }
        cap.silent_frames += u64::from(silent);
    }
}
// !SECTION: Stream threads

// SECTION: MockStream
struct MockStream<P> {
    state: Arc<DeviceState>,
    counters: Arc<Counters>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<P>>,
    wrap: fn(P) -> ClosedPort,
}

impl<P: Send + 'static> AudioStream for MockStream<P> {
    fn stats(&self) -> StreamStats {
        StreamStats {
            frames: self.counters.frames.load(Relaxed),
            xruns: self.counters.xruns.load(Relaxed),
            clock_ppm: self.state.ppm(),
        }
    }

    fn close(mut self: Box<Self>) -> Option<ClosedPort> {
        self.stop.store(true, Relaxed);
        let port = self.thread.take()?.join().ok()?;
        Some((self.wrap)(port))
        // `self` drops here: `Drop` clears the "open" flag.
    }
}

impl<P> Drop for MockStream<P> {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.state.open.store(false, Relaxed);
    }
}
// !SECTION: MockStream

/// Only reachable when the OS refuses to start a thread, after the port is already gone.
// ? `open_*` must return a port with its error, but a failed `thread::spawn` has consumed the
// ? real one. Panicking is the honest answer: the pool is now short one ring and the process is
// ? out of threads anyway.
fn lost_port<T>() -> T {
    panic!("the OS could not start an audio thread and the port went with it")
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_engine::create_engine;
    use audio_types::FixedStr;

    fn hw_in(id: &str) -> SourceEndpoint {
        SourceEndpoint::HardwareInput {
            device_id: FixedStr::from_str_trunc(id),
        }
    }
    fn hw_out(id: &str) -> SinkEndpoint {
        SinkEndpoint::HardwareOutput {
            device_id: FixedStr::from_str_trunc(id),
        }
    }

    #[test]
    fn an_input_delivers_frames_at_about_real_time_and_gives_the_port_back() {
        let (mut backend, ctl) = MockBackend::new();
        let (_engine, mut h) = create_engine();
        let port = h.ports.acquire_input().unwrap();
        let stream = backend.open_input(&hw_in("mock-in-1"), 2, port).unwrap();
        assert!(ctl.is_open("mock-in-1"));
        assert!(!stream.is_master_clocked());
        std::thread::sleep(Duration::from_millis(200));
        let stats = stream.stats();
        // 200 ms is 9600 frames; the ring is bigger than that, so nothing may be lost either.
        assert!((4_000..=12_000).contains(&stats.frames), "{stats:?}");
        assert_eq!(stats.xruns, 0);
        match stream.close() {
            Some(ClosedPort::Input(p)) => h.ports.release_input(p),
            other => panic!("expected the input port back, got {other:?}"),
        }
        assert!(!ctl.is_open("mock-in-1"));
        assert_eq!(h.ports.free_inputs(), 16);
    }

    #[test]
    fn a_dropout_costs_frames_and_never_more_than_asked() {
        let (mut backend, ctl) = MockBackend::new();
        let (_engine, mut h) = create_engine();
        let port = h.ports.acquire_input().unwrap();
        let stream = backend.open_input(&hw_in("mock-in-1"), 2, port).unwrap();
        ctl.inject_dropout("mock-in-1", 100);
        std::thread::sleep(Duration::from_millis(250));
        let frames = stream.stats().frames;
        // 250 ms minus a 100 ms dropout is about 150 ms = 7200 frames (host noise allowed).
        assert!((3_000..=9_600).contains(&frames), "{frames} frames");
        drop(stream.close());
    }

    #[test]
    fn open_failures_hand_the_port_back() {
        let (mut backend, _ctl) = MockBackend::new();
        let (_engine, mut h) = create_engine();

        let port = h.ports.acquire_input().unwrap();
        let (err, port) = backend
            .open_input(&hw_in("nope"), 2, port)
            .err()
            .expect("unknown device");
        assert!(matches!(err, BackendError::NoSuchDevice(_)));

        let (err, port) = backend
            .open_input(&hw_in("mock-in-1"), 9, port)
            .err()
            .expect("too many channels");
        assert_eq!(err, BackendError::BadChannels(9));

        let (err, port) = backend
            .open_input(&SourceEndpoint::Unassigned, 2, port)
            .err()
            .expect("not a desktop source");
        assert!(matches!(err, BackendError::NotSupported(_)));

        // An output id is not an input.
        let (err, port) = backend
            .open_input(&hw_in("mock-out-1"), 2, port)
            .err()
            .expect("wrong kind");
        assert!(matches!(err, BackendError::NoSuchDevice(_)));
        h.ports.release_input(port);
        assert_eq!(h.ports.free_inputs(), 16, "no port may be lost");
    }

    #[test]
    fn a_device_can_only_be_open_once() {
        let (mut backend, ctl) = MockBackend::new();
        let (_engine, mut h) = create_engine();
        let a = h.ports.acquire_input().unwrap();
        let b = h.ports.acquire_input().unwrap();
        let first = backend.open_input(&hw_in("mock-in-2"), 2, a).unwrap();
        let (err, b) = backend
            .open_input(&hw_in("mock-in-2"), 2, b)
            .err()
            .expect("busy");
        assert!(matches!(err, BackendError::Busy(_)));
        h.ports.release_input(b);
        drop(first); // dropping must free the device too
        assert!(!ctl.is_open("mock-in-2"));
    }

    #[test]
    fn an_output_drains_its_ring_and_a_dry_ring_is_counted_after_start() {
        let (mut backend, ctl) = MockBackend::new();
        let (mut engine, mut h) = create_engine();
        let port = h.ports.acquire_output().unwrap();
        let slot = port.slot();
        let stream = backend.open_output(&hw_out("mock-out-1"), 2, port).unwrap();
        h.commands
            .push(audio_types::EngineCommand::AttachBusOutput {
                bus: audio_types::BusId(0),
                slot,
                channels: 2,
            })
            .unwrap();
        // Three blocks of (silent) engine output, then nothing: the device starts, then runs dry.
        for _ in 0..3 {
            engine.process_block(audio_types::ENGINE_BLOCK_FRAMES);
        }
        std::thread::sleep(Duration::from_millis(150));
        let cap = ctl.capture("mock-out-1");
        assert_eq!(cap.frames, 3 * audio_types::ENGINE_BLOCK_FRAMES as u64);
        assert_eq!(cap.peak, 0.0);
        assert!(stream.stats().xruns >= 1, "{:?}", stream.stats());
        match stream.close() {
            Some(ClosedPort::Output(p)) => h.ports.release_output(p),
            other => panic!("expected the output port back, got {other:?}"),
        }
    }

    #[test]
    fn the_signal_generators_do_what_they_say() {
        let mut g = Generator::new();
        let mut out = vec![0.0; 2 * 4800];
        g.fill(
            Signal::Sine {
                freq_hz: 1000.0,
                amp: 0.5,
            },
            2,
            &mut out,
        );
        let peak = out.iter().fold(0.0_f32, |m, v| m.max(v.abs()));
        assert!((peak - 0.5).abs() < 0.01, "{peak}");
        assert_eq!(out[0], out[1], "every channel carries the same sample");

        g.fill(Signal::Silence, 2, &mut out);
        assert!(out.iter().all(|&v| v == 0.0));

        g.fill(Signal::Noise { amp: 0.25 }, 2, &mut out);
        assert!(out.iter().all(|v| v.abs() <= 0.25));
        assert!(out.iter().any(|&v| v != 0.0));

        let mut g = Generator::new();
        g.fill(
            Signal::Click {
                period_frames: 100,
                amp: 1.0,
            },
            1,
            &mut out[..300],
        );
        let clicks = out[..300].iter().filter(|&&v| v == 1.0).count();
        assert_eq!(clicks, 3);
    }
}
