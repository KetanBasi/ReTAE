//! AAudio microphone and output streams through the `ndk` crate (PLAN §6.3).
//!
//! Streams ask for 48 kHz `f32`, `LowLatency`, and are tried exclusive first, then shared
//! (`android_rules::input_attempts` / `output_attempts`). The data callback runs on AAudio's own
//! real-time thread, so it follows the same rules as every device callback here: no allocation,
//! no blocking lock (`try_lock`), no I/O.
// ? The two `unsafe` spots are the callbacks, which receive a raw pointer to AAudio's buffer, and
// ? `Handle`, which asserts that a stream may be moved to another thread. Each has a SAFETY note.
#![allow(unsafe_code)]

use super::AndroidOptions;
use crate::android_rules::{
    Attempt, InputPreset, Sharing, adapt_channels, describe_open, input_attempts_for,
    output_attempts, output_buffer_frames, parse_device_id,
};
use crate::backend::{AudioStream, BackendError, ClosedPort, StreamStats};
use crate::ring_io::{CaptureState, Counters, PlaybackState, Slot};
use audio_types::{ENGINE_BLOCK_FRAMES, ENGINE_SAMPLE_RATE, MAX_CHANNELS};
use ndk::audio::{
    AudioCallbackResult, AudioDirection, AudioError, AudioFormat, AudioInputPreset,
    AudioPerformanceMode, AudioSharingMode, AudioStream as NdkStream, AudioStreamBuilder,
    AudioStreamErrorCallback,
};
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, PoisonError};

/// Frames converted at a time when the device's channel count differs from the strip's.
const STAGE_FRAMES: usize = ENGINE_BLOCK_FRAMES;
const STAGE_SAMPLES: usize = STAGE_FRAMES * MAX_CHANNELS;

// SECTION: Stream handle
/// An open AAudio stream that may be moved to the thread that closes it.
struct Handle(NdkStream);

// SAFETY: AAudio documents its stream functions (`request_start`, `request_stop`, close, the
// getters) as callable from any thread, except from inside the stream's own callbacks. `ndk`
// does not say so in the type because it holds a `NonNull`. We call them from the control
// thread only, and never from a callback.
unsafe impl Send for Handle {}

/// The ring of a stream, to hand back when it closes.
enum Slotted {
    Capture(Arc<Slot<CaptureState>>),
    Playback(Arc<Slot<PlaybackState>>),
}

pub(crate) struct AaStream {
    // ? `Option` so `close` can stop and drop (that is what closes the AAudio stream) *before*
    // ? it takes the port out of the slot: once the stream is gone no callback can still use it.
    stream: Option<Handle>,
    slot: Slotted,
    detail: String,
    /// Set by the error callback (device unplugged, audio server died).
    fault: Arc<Mutex<Option<String>>>,
}

impl AaStream {
    fn counters(&self) -> &Counters {
        match &self.slot {
            Slotted::Capture(s) => &s.counters,
            Slotted::Playback(s) => &s.counters,
        }
    }
}

impl AudioStream for AaStream {
    fn stats(&self) -> StreamStats {
        let c = self.counters();
        StreamStats {
            frames: c.frames.load(Relaxed),
            xruns: c.xruns.load(Relaxed),
            clock_ppm: f32::from_bits(c.ppm_bits.load(Relaxed)),
        }
    }

    fn detail(&self) -> String {
        match &*self.fault.lock().unwrap_or_else(PoisonError::into_inner) {
            Some(why) => format!("STOPPED: {why} (was: {})", self.detail),
            None => self.detail.clone(),
        }
    }

    fn close(mut self: Box<Self>) -> Option<ClosedPort> {
        if let Some(h) = self.stream.take() {
            // ? A stop that fails (the stream is already gone) changes nothing: the drop below
            // ? closes it either way.
            let _ = h.0.request_stop();
            drop(h);
        }
        match &self.slot {
            Slotted::Capture(s) => s.take().map(|st| ClosedPort::Input(st.into_port())),
            Slotted::Playback(s) => s.take().map(|st| ClosedPort::Output(st.into_port())),
        }
    }
}
// !SECTION: Stream handle

// SECTION: Opening
struct Opened {
    stream: NdkStream,
    attempt: Attempt,
}

fn map_open_error(e: AudioError, device: i32, what: &str) -> BackendError {
    match e {
        AudioError::IllegalArgument | AudioError::InvalidHandle if device != 0 => {
            BackendError::NoSuchDevice(format!("aaudio:{device}"))
        }
        AudioError::Unavailable | AudioError::NoFreeHandles => {
            BackendError::Busy(format!("{what}: {e}"))
        }
        // ? AAudio gives no separate "permission denied". A microphone that fails to open at
        // ? all on a phone that has one is most often exactly that, so say it.
        other => BackendError::Io(format!(
            "{what}: {other} (for a microphone, check the RECORD_AUDIO permission)"
        )),
    }
}

/// Try each attempt in order, and for each one the strip's channel count and then the device's own
/// (`0`). The first stream that opens at 48 kHz wins.
fn open_with(
    direction: AudioDirection,
    device: i32,
    channels: u8,
    attempts: &[Attempt],
    dev_ch: &AtomicUsize,
    mut add_callbacks: impl FnMut(AudioStreamBuilder) -> AudioStreamBuilder,
) -> Result<Opened, AudioError> {
    let mut last = AudioError::Unavailable;
    for &attempt in attempts {
        for want in [i32::from(channels), 0] {
            let mut b = AudioStreamBuilder::new()?
                .direction(direction)
                .format(AudioFormat::PCM_Float)
                .sample_rate(ENGINE_SAMPLE_RATE as i32)
                .channel_count(want)
                .performance_mode(AudioPerformanceMode::LowLatency)
                .sharing_mode(match attempt.sharing {
                    Sharing::Exclusive => AudioSharingMode::Exclusive,
                    Sharing::Shared => AudioSharingMode::Shared,
                })
                .device_id(device);
            if let Some(p) = attempt.preset {
                b = b.input_preset(match p {
                    InputPreset::VoicePerformance => AudioInputPreset::VoicePerformance,
                    InputPreset::Unprocessed => AudioInputPreset::Unprocessed,
                    InputPreset::Camcorder => AudioInputPreset::Camcorder,
                    InputPreset::VoiceRecognition => AudioInputPreset::VoiceRecognition,
                    InputPreset::Generic => AudioInputPreset::Generic,
                });
            }
            match add_callbacks(b).open_stream() {
                Ok(stream) => {
                    // ? The engine is 48 kHz only (NOTES A2). AAudio converts when asked for
                    // ? 48 kHz in shared mode; an exclusive stream cannot, and may report the
                    // ? hardware rate instead. Such a stream is closed and the next attempt runs.
                    if stream.sample_rate() != ENGINE_SAMPLE_RATE as i32 {
                        last = AudioError::InvalidRate;
                        continue;
                    }
                    dev_ch.store(
                        usize::try_from(stream.channel_count()).unwrap_or(1),
                        Relaxed,
                    );
                    return Ok(Opened { stream, attempt });
                }
                Err(e) => last = e,
            }
        }
    }
    Err(last)
}

fn usable_attempts<const N: usize>(all: [Attempt; N], options: &AndroidOptions) -> Vec<Attempt> {
    all.into_iter()
        .filter(|a| options.exclusive || a.sharing == Sharing::Shared)
        .collect()
}

fn error_callback(fault: &Arc<Mutex<Option<String>>>) -> AudioStreamErrorCallback {
    let fault = fault.clone();
    Box::new(move |_stream, err| {
        // Not the real-time thread, so a String here is fine.
        *fault.lock().unwrap_or_else(PoisonError::into_inner) = Some(err.to_string());
    })
}

pub(crate) fn open_input(
    device_text: &str,
    preset: InputPreset,
    channels: u8,
    slot: &Arc<Slot<CaptureState>>,
    options: &AndroidOptions,
) -> Result<AaStream, BackendError> {
    let device = parse_device_id(device_text)
        .ok_or_else(|| BackendError::NoSuchDevice(device_text.to_owned()))?;
    let attempts = usable_attempts(input_attempts_for(preset), options);
    let dev_ch = Arc::new(AtomicUsize::new(usize::from(channels)));
    let fault = Arc::new(Mutex::new(None));
    let port_ch = usize::from(channels);

    let opened = open_with(
        AudioDirection::Input,
        device,
        channels,
        &attempts,
        &dev_ch,
        |b| {
            // ? Each attempt builds new callbacks, and a callback is moved into AAudio, so every
            // ? one gets its own clones of the `Arc`s (shared pointers) and owns those.
            let (slot, dev_ch) = (slot.clone(), dev_ch.clone());
            b.data_callback(Box::new(move |_stream, data, frames| {
                capture_callback(&slot, &dev_ch, port_ch, data, frames)
            }))
            .error_callback(error_callback(&fault))
        },
    )
    .map_err(|e| map_open_error(e, device, "cannot open the microphone"))?;

    opened
        .stream
        .request_start()
        .map_err(|e| map_open_error(e, device, "cannot start the microphone"))?;
    let detail = describe(&opened, &dev_ch);
    Ok(AaStream {
        stream: Some(Handle(opened.stream)),
        slot: Slotted::Capture(slot.clone()),
        detail,
        fault,
    })
}

pub(crate) fn open_output(
    device_text: &str,
    channels: u8,
    slot: &Arc<Slot<PlaybackState>>,
    options: &AndroidOptions,
) -> Result<AaStream, BackendError> {
    let device = parse_device_id(device_text)
        .ok_or_else(|| BackendError::NoSuchDevice(device_text.to_owned()))?;
    let attempts = usable_attempts(output_attempts(), options);
    let dev_ch = Arc::new(AtomicUsize::new(usize::from(channels)));
    let fault = Arc::new(Mutex::new(None));
    let port_ch = usize::from(channels);

    let opened = open_with(
        AudioDirection::Output,
        device,
        channels,
        &attempts,
        &dev_ch,
        |b| {
            let (slot, dev_ch) = (slot.clone(), dev_ch.clone());
            b.data_callback(Box::new(move |_stream, data, frames| {
                playback_callback(&slot, &dev_ch, port_ch, data, frames)
            }))
            .error_callback(error_callback(&fault))
        },
    )
    .map_err(|e| map_open_error(e, device, "cannot open the output"))?;

    // ? AAudio starts a LowLatency stream with a one-burst buffer. Two bursts is the standard
    // ? double buffer; `x_run_count` (shown as xruns) says whether it was enough.
    let burst = opened.stream.frames_per_burst();
    let capacity = opened.stream.buffer_capacity_in_frames();
    let _ = opened
        .stream
        .set_buffer_size_in_frames(output_buffer_frames(burst, capacity));
    opened
        .stream
        .request_start()
        .map_err(|e| map_open_error(e, device, "cannot start the output"))?;
    let detail = describe(&opened, &dev_ch);
    Ok(AaStream {
        stream: Some(Handle(opened.stream)),
        slot: Slotted::Playback(slot.clone()),
        detail,
        fault,
    })
}

fn describe(opened: &Opened, dev_ch: &AtomicUsize) -> String {
    format!(
        "{}, {} ch",
        describe_open(
            opened.attempt,
            opened.stream.frames_per_burst(),
            opened.stream.buffer_size_in_frames(),
            opened.stream.sample_rate(),
        ),
        dev_ch.load(Relaxed)
    )
}
// !SECTION: Opening

// SECTION: Callbacks (real-time)
/// Device to ring. `data` holds `frames` frames of the device's channels as `f32`.
fn capture_callback(
    slot: &Slot<CaptureState>,
    dev_ch: &AtomicUsize,
    port_ch: usize,
    data: *mut c_void,
    frames: i32,
) -> AudioCallbackResult {
    let frames = usize::try_from(frames).unwrap_or(0);
    let dch = dev_ch.load(Relaxed).max(1);
    if data.is_null() || frames == 0 {
        return AudioCallbackResult::Continue;
    }
    // SAFETY: AAudio passes a buffer of `frames` frames in the stream's format, which we opened
    // as interleaved `f32` with `dch` channels, and keeps it valid until the callback returns.
    // The slice does not outlive this function.
    let samples =
        unsafe { std::slice::from_raw_parts(data.cast::<f32>().cast_const(), frames * dch) };
    if let Ok(mut guard) = slot.state.try_lock() {
        if let Some(state) = guard.as_mut() {
            if dch == port_ch {
                state.feed_samples(samples, &slot.counters);
            } else {
                let mut stage = [0.0_f32; STAGE_SAMPLES];
                for chunk in samples.chunks(STAGE_FRAMES * dch) {
                    let n = adapt_channels(chunk, dch, &mut stage, port_ch);
                    state.feed_samples(&stage[..n * port_ch], &slot.counters);
                }
            }
        }
    }
    AudioCallbackResult::Continue
}

/// Ring to device. Fills `frames` frames of the device's channels.
fn playback_callback(
    slot: &Slot<PlaybackState>,
    dev_ch: &AtomicUsize,
    port_ch: usize,
    data: *mut c_void,
    frames: i32,
) -> AudioCallbackResult {
    let frames = usize::try_from(frames).unwrap_or(0);
    let dch = dev_ch.load(Relaxed).max(1);
    if data.is_null() || frames == 0 {
        return AudioCallbackResult::Continue;
    }
    // SAFETY: as in `capture_callback`, and AAudio expects us to write the whole buffer.
    let out = unsafe { std::slice::from_raw_parts_mut(data.cast::<f32>(), frames * dch) };
    match slot.state.try_lock() {
        Ok(mut guard) => match guard.as_mut() {
            Some(state) if dch == port_ch => state.render_samples(out, frames, &slot.counters),
            Some(state) => {
                let mut stage = [0.0_f32; STAGE_SAMPLES];
                let mut done = 0;
                while done < frames {
                    let n = (frames - done).min(STAGE_FRAMES);
                    let piece = &mut stage[..n * port_ch];
                    state.render_samples(piece, n, &slot.counters);
                    adapt_channels(piece, port_ch, &mut out[done * dch..(done + n) * dch], dch);
                    done += n;
                }
            }
            None => out.fill(0.0),
        },
        // The control thread holds the state (closing): silence this cycle, never a wait.
        Err(_) => out.fill(0.0),
    }
    AudioCallbackResult::Continue
}
// !SECTION: Callbacks (real-time)
