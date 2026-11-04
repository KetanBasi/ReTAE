//! One WASAPI stream = one worker thread (PLAN.md §6.1).
//!
//! The thread joins COM, asks MMCSS for "Pro Audio", opens and starts the endpoint, tells the
//! opener whether that worked, and then moves audio between the endpoint and the port ring
//! until it is told to stop. Everything the thread touches is created on it, because COM
//! objects belong to the apartment of the thread that made them.
//!
//! ```text
//!   open ──spawn──► [thread: COM, MMCSS, init client, event] ──ready(Ok/Err)──► open returns
//!                          │ loop: wait event (20 ms) → read/write device buffer ↔ ring
//!   close ──stop flag──────┘ then stop_stream, release, exit (join), port goes back
//! ```
//!
//! Start-up order of preference, best latency first:
//!
//! 1. **Exclusive** (only when asked): the device's own format at its smallest period.
//! 2. **Shared, `IAudioClient3`** (when `low_latency`): the engine's own format at the smallest
//!    period the driver allows, typically 128 to 480 frames instead of the default 480.
//! 3. **Shared, classic**: 48 kHz `f32`, the system converts rate, sample type and channels.
//!
//! Loopback (device and per-process) always uses 3: the `IAudioClient3` call cannot loopback.

use super::devices::{ensure_com, hresult_of, map_error, open_device};
use super::sys::promote_current_thread_mmcss;
use crate::backend::{BackendError, DeviceKind};
use crate::pcm::{SampleFormat, read_samples, write_samples};
use crate::ring_io::{CaptureState, PlaybackState, Slot};
use audio_types::{ENGINE_SAMPLE_RATE, MAX_CHANNELS};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;
use wasapi::{
    AudioClient, BufferFlags, Device, Direction, Handle, SampleType, StreamMode, TimingMode,
    WasapiError, WaveFormat, make_channelmasks,
};

/// How long the loop waits for the device before looking at the stop flag. A loopback stream of
/// a silent device never signals at all, so a timeout is normal and the loop still polls.
const WAIT_MS: u32 = 20;
/// Activation of a process-loopback client can take a moment; nothing else should.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest packet a capture read can return when the device does not tell us its buffer size
/// (process loopback reports a nonsense size): half a second.
const FALLBACK_SCRATCH_FRAMES: u32 = 24_000;

/// What to open.
#[derive(Clone, Debug)]
pub enum Target {
    Capture {
        id: String,
    },
    /// Record what an *output* device plays.
    Loopback {
        id: String,
    },
    /// Record one program (and its children).
    Process {
        pid: u32,
        include_tree: bool,
    },
    Playback {
        id: String,
    },
}

#[derive(Clone, Debug)]
pub struct Request {
    pub target: Target,
    pub channels: usize,
    pub exclusive: bool,
    pub low_latency: bool,
}

/// A one-line description of how a stream was opened, or why it stopped.
pub type Detail = Arc<Mutex<String>>;

fn set_detail(detail: &Detail, text: String) {
    *detail.lock().unwrap_or_else(PoisonError::into_inner) = text;
}

/// A running worker. `stop` joins the thread, after which the ring can be taken back.
pub struct Worker {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    pub detail: Detail,
}

impl Worker {
    pub fn stop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
    }
}

// SECTION: Starting
type Ready = mpsc::Sender<Result<(), BackendError>>;

fn spawn(
    name: &str,
    body: impl FnOnce(&AtomicBool, &Detail, Ready) + Send + 'static,
) -> Result<Worker, BackendError> {
    let stop = Arc::new(AtomicBool::new(false));
    let detail: Detail = Arc::default();
    let (ready_tx, ready_rx) = mpsc::channel();
    let join = {
        let (stop, detail) = (stop.clone(), detail.clone());
        std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || body(&stop, &detail, ready_tx))
            .map_err(|e| BackendError::Io(format!("cannot start the audio thread: {e}")))?
    };
    let mut worker = Worker {
        stop,
        join: Some(join),
        detail,
    };
    match ready_rx.recv_timeout(OPEN_TIMEOUT) {
        Ok(Ok(())) => Ok(worker),
        Ok(Err(e)) => {
            worker.stop();
            Err(e)
        }
        Err(RecvTimeoutError::Timeout) => {
            worker.stop();
            Err(BackendError::Io(
                "Windows did not answer in time (the device may be stuck)".into(),
            ))
        }
        Err(RecvTimeoutError::Disconnected) => {
            worker.stop();
            Err(BackendError::Io(
                "the audio thread ended while opening".into(),
            ))
        }
    }
}

pub fn start_capture(req: Request, slot: Arc<Slot<CaptureState>>) -> Result<Worker, BackendError> {
    spawn("wasapi-capture", move |stop, detail, ready| {
        let note = promote_note();
        let prepared = match prepare(&req) {
            Ok(p) => p,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        set_detail(detail, format!("{} {note}", prepared.note));
        let _ = ready.send(Ok(()));
        capture_loop(&prepared, &slot, stop, detail);
    })
}

pub fn start_playback(
    req: Request,
    slot: Arc<Slot<PlaybackState>>,
) -> Result<Worker, BackendError> {
    spawn("wasapi-playback", move |stop, detail, ready| {
        let note = promote_note();
        let prepared = match prepare(&req) {
            Ok(p) => p,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        set_detail(detail, format!("{} {note}", prepared.note));
        let _ = ready.send(Ok(()));
        playback_loop(&prepared, &slot, stop, detail);
    })
}

fn promote_note() -> String {
    match promote_current_thread_mmcss() {
        crate::rt::RtOutcome::Promoted => "[MMCSS Pro Audio]".into(),
        crate::rt::RtOutcome::Unavailable(why) => format!("[no MMCSS: {why}]"),
    }
}
// !SECTION: Starting

// SECTION: Opening the endpoint
/// An initialised, not yet started client with everything the loop needs.
struct Prepared {
    client: AudioClient,
    event: Handle,
    fmt: SampleFormat,
    channels: usize,
    buffer_frames: u32,
    shared: bool,
    note: String,
}

impl Prepared {
    fn frame_bytes(&self) -> usize {
        self.channels * self.fmt.bytes()
    }
}

/// Where clients come from. A client that failed `Initialize` is not reusable, so the fallback
/// steps below ask for a fresh one each time.
enum Source {
    Device(Device),
    Process { pid: u32, include_tree: bool },
}

impl Source {
    fn client(&self) -> Result<AudioClient, WasapiError> {
        match self {
            Self::Device(d) => d.get_iaudioclient(),
            Self::Process { pid, include_tree } => {
                AudioClient::new_application_loopback_client(*pid, *include_tree)
            }
        }
    }
}

fn prepare(req: &Request) -> Result<Prepared, BackendError> {
    // ? Process loopback activates its client without going through `open_device`, so COM has to
    // ? be joined here, before either path runs.
    ensure_com();
    if req.channels == 0 || req.channels > MAX_CHANNELS {
        return Err(BackendError::BadChannels(req.channels as u8));
    }
    // (source, direction of the *stream*, is it a loopback of some kind)
    let (source, stream_dir, loopback, what) = match &req.target {
        Target::Capture { id } => (
            Source::Device(open_device(id, DeviceKind::Input)?),
            Direction::Capture,
            false,
            id.clone(),
        ),
        Target::Loopback { id } => (
            Source::Device(open_device(id, DeviceKind::Output)?),
            Direction::Capture,
            true,
            id.clone(),
        ),
        Target::Process { pid, include_tree } => (
            Source::Process {
                pid: *pid,
                include_tree: *include_tree,
            },
            Direction::Capture,
            true,
            format!("process {pid}"),
        ),
        Target::Playback { id } => (
            Source::Device(open_device(id, DeviceKind::Output)?),
            Direction::Render,
            false,
            id.clone(),
        ),
    };
    let is_process = matches!(source, Source::Process { .. });
    let fail = |e: WasapiError| map_error(&e, &what);

    if req.exclusive && !loopback {
        let client = source.client().map_err(fail)?;
        return open_exclusive(client, stream_dir, req.channels, &what);
    }
    if req.low_latency && !loopback {
        // ? Any failure here just means "not this way": drivers differ a lot in what they allow,
        // ? and the classic path below always works.
        if let Ok(client) = source.client() {
            if let Ok(p) = open_low_latency(client, stream_dir, req.channels) {
                return Ok(p);
            }
        }
    }
    let client = source.client().map_err(fail)?;
    open_classic(client, stream_dir, req.channels, is_process, loopback).map_err(|e| {
        let mapped = map_error(&e, &what);
        // ? Process loopback has the least forgiving format rules; say what was asked.
        if is_process && matches!(mapped, BackendError::Io(_)) {
            BackendError::Io(format!(
                "{what}: Windows refused {} channel(s) at 48 kHz ({e})",
                req.channels
            ))
        } else {
            mapped
        }
    })
}

fn engine_rate() -> usize {
    ENGINE_SAMPLE_RATE as usize
}

fn mask_for(channels: usize) -> Option<u32> {
    make_channelmasks(channels).first().copied()
}

fn finish(
    client: AudioClient,
    fmt: SampleFormat,
    channels: usize,
    shared: bool,
    note: String,
) -> Result<Prepared, WasapiError> {
    let event = client.set_get_eventhandle()?;
    // ? Process loopback reports a nonsense size (billions of frames); the loop sizes its
    // ? scratch space from the clamped value below instead.
    let buffer_frames = client
        .get_buffer_size()
        .ok()
        .filter(|b| (16..=192_000).contains(b))
        .unwrap_or(FALLBACK_SCRATCH_FRAMES);
    Ok(Prepared {
        client,
        event,
        fmt,
        channels,
        buffer_frames,
        shared,
        note,
    })
}

fn open_classic(
    mut client: AudioClient,
    dir: Direction,
    channels: usize,
    is_process: bool,
    loopback: bool,
) -> Result<Prepared, WasapiError> {
    let wf = WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        engine_rate(),
        channels,
        mask_for(channels),
    );
    // ? Event-driven shared mode ignores the buffer duration for timing; 0 asks for the engine's
    // ? default. Process loopback has no device period to ask about.
    let period = if is_process {
        0
    } else {
        client.get_device_period().map_or(0, |p| p.0)
    };
    client.initialize_client(
        &wf,
        &dir,
        &StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: period,
        },
    )?;
    let kind = if is_process {
        "process loopback"
    } else if loopback {
        "device loopback"
    } else {
        "shared"
    };
    let note = format!("{kind}, event-driven, 48 kHz f32 x{channels} (system converts)");
    finish(client, SampleFormat::F32, channels, true, note)
}

fn open_low_latency(
    mut client: AudioClient,
    _dir: Direction,
    channels: usize,
) -> Result<Prepared, WasapiError> {
    let mix = client.get_mixformat()?;
    // ? `InitializeSharedAudioStream` takes the engine's own format and cannot convert, so it is
    // ? only usable when that format already is what the mixer wants.
    if mix.get_samplespersec() as usize != engine_rate()
        || mix.get_nchannels() as usize != channels
        || mix.get_subformat()? != SampleType::Float
    {
        return Err(WasapiError::UnsupportedFormat);
    }
    let p = client.get_shared_mode_engine_period(&mix)?;
    client.initialize_shared_audio_stream(&mix, TimingMode::Events, p.min)?;
    let note = format!(
        "shared, IAudioClient3 period {} frames ({:.1} ms; default {}, step {}, max {})",
        p.min,
        p.min as f64 * 1000.0 / engine_rate() as f64,
        p.default,
        p.fundamental,
        p.max
    );
    finish(client, SampleFormat::F32, channels, true, note)
}

fn open_exclusive(
    mut client: AudioClient,
    dir: Direction,
    channels: usize,
    what: &str,
) -> Result<Prepared, BackendError> {
    // (format, store bits, valid bits, sample type)
    const CANDIDATES: [(SampleFormat, usize, usize, SampleType); 5] = [
        (SampleFormat::F32, 32, 32, SampleType::Float),
        (SampleFormat::I32, 32, 24, SampleType::Int),
        (SampleFormat::I32, 32, 32, SampleType::Int),
        (SampleFormat::I24, 24, 24, SampleType::Int),
        (SampleFormat::I16, 16, 16, SampleType::Int),
    ];
    let io = |e: WasapiError| map_error(&e, what);
    let mut chosen = None;
    for (fmt, store, valid, ty) in CANDIDATES {
        let wf = WaveFormat::new(
            store,
            valid,
            &ty,
            engine_rate(),
            channels,
            mask_for(channels),
        );
        if let Ok(accepted) = client.is_supported_exclusive_with_quirks(&wf) {
            chosen = Some((fmt, accepted));
            break;
        }
    }
    let Some((fmt, wf)) = chosen else {
        return Err(BackendError::Io(format!(
            "{what}: the device does not accept {channels} channel(s) at 48 kHz in exclusive mode"
        )));
    };
    // ? Smallest period the driver allows. HD Audio hardware wants buffers in multiples of 128
    // ? bytes; asking for an unaligned size fails with AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED.
    let period = client
        .calculate_aligned_period_near(0, Some(128), &wf)
        .map_err(io)?;
    client
        .initialize_client(
            &wf,
            &dir,
            &StreamMode::EventsExclusive { period_hns: period },
        )
        .map_err(io)?;
    let note = format!(
        "EXCLUSIVE, {fmt:?} x{channels}, period {:.2} ms",
        period as f64 / 10_000.0
    );
    finish(client, fmt, channels, false, note).map_err(io)
}
// !SECTION: Opening the endpoint

// SECTION: Loops
fn fail(detail: &Detail, what: &str, e: &WasapiError) {
    let why = match hresult_of(e) {
        Some(0x8889_0004) => "the device was removed or changed".to_owned(),
        _ => e.to_string(),
    };
    set_detail(detail, format!("STOPPED while {what}: {why}"));
}

/// Largest number of frames one read or write may move, sized once at start.
fn scratch_frames(p: &Prepared) -> usize {
    (p.buffer_frames as usize).max(4_800)
}

fn capture_loop(p: &Prepared, slot: &Slot<CaptureState>, stop: &AtomicBool, detail: &Detail) {
    let capture = match p.client.get_audiocaptureclient() {
        Ok(c) => c,
        Err(e) => return fail(detail, "starting", &e),
    };
    let cap = scratch_frames(p);
    let mut bytes = vec![0u8; cap * p.frame_bytes()];
    let mut stage = vec![0.0f32; cap * p.channels];
    if let Err(e) = p.client.start_stream() {
        return fail(detail, "starting", &e);
    }
    while !stop.load(Relaxed) {
        // ? Timeout is fine: a loopback of a silent device never signals, and polling then
        // ? finds nothing to read.
        let _ = p.event.wait_for_event(WAIT_MS);
        for _ in 0..16 {
            match capture.get_next_packet_size() {
                Ok(Some(0)) => break,
                Ok(_) => {}
                Err(e) => return fail(detail, "recording", &e),
            }
            let (frames, info) = match capture.read_from_device(&mut bytes) {
                Ok(r) => r,
                Err(e) => return fail(detail, "recording", &e),
            };
            let frames = frames as usize;
            if frames > 0 {
                let samples = frames * p.channels;
                if info.flags.silent {
                    // ? The SILENT flag means the buffer content is garbage and must be heard
                    // ? as silence.
                    stage[..samples].fill(0.0);
                } else {
                    read_samples(
                        p.fmt,
                        &bytes[..frames * p.frame_bytes()],
                        &mut stage[..samples],
                    );
                }
                if info.flags.data_discontinuity {
                    slot.counters.xruns.fetch_add(1, Relaxed);
                }
                feed(slot, &stage[..samples]);
            }
            // Exclusive mode hands over exactly one buffer per event.
            if !p.shared || frames == 0 {
                break;
            }
        }
    }
    let _ = p.client.stop_stream();
}

fn feed(slot: &Slot<CaptureState>, samples: &[f32]) {
    // ? `try_lock` never waits: the only other holder is `close`, which runs after this thread
    // ? has been joined. A real `lock()` would be a way to block the audio thread.
    if let Ok(mut guard) = slot.state.try_lock() {
        if let Some(state) = guard.as_mut() {
            state.feed_samples(samples, &slot.counters);
        }
    }
}

fn playback_loop(p: &Prepared, slot: &Slot<PlaybackState>, stop: &AtomicBool, detail: &Detail) {
    let render = match p.client.get_audiorenderclient() {
        Ok(r) => r,
        Err(e) => return fail(detail, "starting", &e),
    };
    let cap = scratch_frames(p);
    let fb = p.frame_bytes();
    let mut bytes = vec![0u8; cap * fb];
    let mut stage = vec![0.0f32; cap * p.channels];

    // Start with a buffer of silence so the first event does not find it empty.
    let first = match p.client.get_available_space_in_frames() {
        Ok(n) => (n as usize).min(cap),
        Err(e) => return fail(detail, "starting", &e),
    };
    let silent = BufferFlags {
        data_discontinuity: false,
        silent: true,
        timestamp_error: false,
    };
    if let Err(e) = render.write_to_device(first, &bytes[..first * fb], Some(silent)) {
        return fail(detail, "starting", &e);
    }
    if let Err(e) = p.client.start_stream() {
        return fail(detail, "starting", &e);
    }

    while !stop.load(Relaxed) {
        let _ = p.event.wait_for_event(WAIT_MS);
        let frames = match p.client.get_available_space_in_frames() {
            Ok(n) => (n as usize).min(cap),
            Err(e) => return fail(detail, "playing", &e),
        };
        if frames == 0 {
            continue;
        }
        let samples = &mut stage[..frames * p.channels];
        draw(slot, samples, frames);
        write_samples(p.fmt, samples, &mut bytes[..frames * fb]);
        if let Err(e) = render.write_to_device(frames, &bytes[..frames * fb], None) {
            return fail(detail, "playing", &e);
        }
    }
    let _ = p.client.stop_stream();
}

/// The mixer's next `frames` frames, or silence if the state is not available this cycle.
fn draw(slot: &Slot<PlaybackState>, out: &mut [f32], frames: usize) {
    match slot.state.try_lock() {
        Ok(mut guard) => match guard.as_mut() {
            Some(state) => state.render_samples(out, frames, &slot.counters),
            None => out.fill(0.0),
        },
        Err(_) => out.fill(0.0),
    }
}
// !SECTION: Loops
