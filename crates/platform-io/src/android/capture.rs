//! App audio: `AudioRecord` with an `AudioPlaybackCaptureConfiguration`, driven from a Rust thread
//! over JNI (PLAN §6.3). AAudio cannot do playback capture, so this is the one place Android audio
//! is not AAudio.
//!
//! ```text
//!   other apps' audio --> MediaProjection --> AudioRecord.read(directByteBuffer)
//!                                                 |  (zero-copy: Rust reads the buffer's memory)
//!                                          CaptureState::feed --> engine ring
//! ```
// ? The one `unsafe` is reading the direct buffer's memory; it has a SAFETY note.
#![allow(unsafe_code)]

use super::jvm::{self, java_error};
use crate::android_rules::{
    CapturePlan, NEEDS_PROJECTION, capture_buffer_bytes, capture_channel_mask, capture_plan,
    capture_read_bytes,
};
use crate::backend::{AudioStream, BackendError, ClosedPort, StreamStats};
use crate::ring_io::{CaptureState, Counters, Slot};
use audio_types::{ENGINE_SAMPLE_RATE, SourceEndpoint};
use jni::JNIEnv;
use jni::objects::{GlobalRef, JByteBuffer, JObject, JValue};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

/// `AudioFormat.ENCODING_PCM_FLOAT`.
const ENCODING_PCM_FLOAT: i32 = 4;
/// `AudioRecord.READ_BLOCKING`.
const READ_BLOCKING: i32 = 0;
/// `AudioRecord.STATE_INITIALIZED`.
const STATE_INITIALIZED: i32 = 1;
/// How long `open` waits for the capture thread to report that it started.
const START_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct CaptureStream {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    slot: Arc<Slot<CaptureState>>,
    /// Kept so `close` can call `AudioRecord.stop()`, which wakes a blocked `read`.
    record: GlobalRef,
    detail: String,
    fault: Arc<Mutex<Option<String>>>,
}

/// What the capture thread tells `open` once it has an `AudioRecord` running.
struct Ready {
    record: GlobalRef,
    detail: String,
}

/// The buffer `AudioRecord.read` fills, and where its memory is.
struct DirectBuffer {
    /// The Java `ByteBuffer`. Holding the global reference also keeps its memory alive.
    object: GlobalRef,
    /// Address of the buffer's memory, stable for a direct buffer.
    address: usize,
    capacity: usize,
}

pub(crate) fn open(
    source: &SourceEndpoint,
    channels: u8,
    slot: &Arc<Slot<CaptureState>>,
) -> Result<CaptureStream, BackendError> {
    let plan = capture_plan(source)?;
    let mask = capture_channel_mask(channels)?;
    let projection =
        jvm::projection().ok_or_else(|| BackendError::NotSupported(NEEDS_PROJECTION.to_owned()))?;

    let stop = Arc::new(AtomicBool::new(false));
    let fault = Arc::new(Mutex::new(None));
    let (tx, rx) = mpsc::channel::<Result<Ready, BackendError>>();

    let join = {
        let (stop, fault, slot) = (stop.clone(), fault.clone(), slot.clone());
        std::thread::Builder::new()
            .name("retae-app-capture".into())
            .spawn(move || {
                capture_thread(
                    &plan,
                    channels,
                    mask,
                    &projection,
                    &stop,
                    &fault,
                    &slot,
                    &tx,
                )
            })
            .map_err(|e| BackendError::Io(format!("cannot start the capture thread: {e}")))?
    };

    match rx.recv_timeout(START_TIMEOUT) {
        Ok(Ok(ready)) => Ok(CaptureStream {
            stop,
            join: Some(join),
            slot: slot.clone(),
            record: ready.record,
            detail: ready.detail,
            fault,
        }),
        Ok(Err(e)) => {
            let _ = join.join();
            Err(e)
        }
        Err(_) => {
            // The thread did not answer: tell it to give up. It is not joined, because the
            // thing it is stuck on may never return, and `open` must.
            stop.store(true, Relaxed);
            Err(BackendError::Io(
                "the playback capture did not start in time".into(),
            ))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn capture_thread(
    plan: &CapturePlan,
    channels: u8,
    mask: i32,
    projection: &GlobalRef,
    stop: &AtomicBool,
    fault: &Mutex<Option<String>>,
    slot: &Slot<CaptureState>,
    tx: &mpsc::Sender<Result<Ready, BackendError>>,
) {
    let Some(vm) = jvm::java_vm() else {
        let _ = tx.send(Err(BackendError::Io(
            "the Android context was not installed".into(),
        )));
        return;
    };
    let Ok(mut env) = vm.attach_current_thread_permanently() else {
        let _ = tx.send(Err(BackendError::Io("cannot attach to the JVM".into())));
        return;
    };

    // The setup is its own function so that one `?` chain can build the whole object graph and
    // one place clears a pending Java exception.
    let setup = setup_record(&mut env, projection.as_obj(), plan, channels, mask)
        .map_err(|e| java_error(&mut env, &e));
    let (record, buffer) = match setup {
        Ok(v) => v,
        Err(e) => {
            let _ = tx.send(Err(e));
            return;
        }
    };
    let detail = format!(
        "playback capture, {} Hz, {} ch, {} byte reads",
        ENGINE_SAMPLE_RATE,
        channels,
        capture_read_bytes(channels)
    );
    if tx
        .send(Ok(Ready {
            record: record.clone(),
            detail,
        }))
        .is_err()
    {
        // `open` gave up. Release what was built.
        release(&mut env, &record);
        return;
    }

    // Audio priority for the thread that feeds the mixer (the plan's THREAD_PRIORITY_URGENT_AUDIO).
    let _ = jvm::set_urgent_audio_priority();

    let read_bytes = i32::try_from(capture_read_bytes(channels).min(buffer.capacity)).unwrap_or(0);
    while !stop.load(Relaxed) {
        let read = env
            .call_method(
                &record,
                "read",
                "(Ljava/nio/ByteBuffer;II)I",
                &[
                    JValue::Object(buffer.object.as_obj()),
                    JValue::Int(read_bytes),
                    JValue::Int(READ_BLOCKING),
                ],
            )
            .and_then(|v| v.i());
        match read {
            Ok(n) if n > 0 => {
                let n = usize::try_from(n).unwrap_or(0).min(buffer.capacity);
                // SAFETY: `address` is the start of a direct `ByteBuffer` of `capacity` bytes that
                // `buffer.object` keeps alive for this whole thread, and `read` has just
                // returned, so Java is no longer writing the first `n` bytes. `n <= capacity`.
                let bytes = unsafe { std::slice::from_raw_parts(buffer.address as *const u8, n) };
                if let Ok(mut guard) = slot.state.try_lock() {
                    if let Some(state) = guard.as_mut() {
                        state.feed(bytes, &slot.counters);
                    }
                }
            }
            // 0: nothing arrived (e.g. stopped). Loop and check `stop`.
            Ok(0) => {}
            Ok(code) => {
                // ? Negative `AudioRecord` results: -3 INVALID_OPERATION, -6 DEAD_OBJECT (the
                // ? projection ended or the audio server restarted). No automatic restart: the
                // ? consent token is single-use, so only a person can grant a new one.
                *fault.lock().unwrap_or_else(PoisonError::into_inner) =
                    Some(format!("AudioRecord.read returned {code}"));
                break;
            }
            Err(e) => {
                let err = java_error(&mut env, &e);
                *fault.lock().unwrap_or_else(PoisonError::into_inner) = Some(err.to_string());
                break;
            }
        }
    }
    release(&mut env, &record);
}

/// Build the `AudioRecord` and its read buffer, and start recording.
fn setup_record(
    env: &mut JNIEnv,
    projection: &JObject,
    plan: &CapturePlan,
    channels: u8,
    mask: i32,
) -> jni::errors::Result<(GlobalRef, DirectBuffer)> {
    // ? Every class, method and signature below is a plain literal on purpose: they are checked
    // ? against the real `android.jar` with `javap -s` (see docs/android-testing.md).

    // 1. Which audio to capture.
    let builder = env.new_object(
        "android/media/AudioPlaybackCaptureConfiguration$Builder",
        "(Landroid/media/projection/MediaProjection;)V",
        &[JValue::Object(projection)],
    )?;
    for usage in &plan.usages {
        env.call_method(
            &builder,
            "addMatchingUsage",
            "(I)Landroid/media/AudioPlaybackCaptureConfiguration$Builder;",
            &[JValue::Int(*usage)],
        )?;
    }
    for uid in &plan.include_uids {
        env.call_method(
            &builder,
            "addMatchingUid",
            "(I)Landroid/media/AudioPlaybackCaptureConfiguration$Builder;",
            &[JValue::Int(*uid)],
        )?;
    }
    for uid in &plan.exclude_uids {
        env.call_method(
            &builder,
            "excludeUid",
            "(I)Landroid/media/AudioPlaybackCaptureConfiguration$Builder;",
            &[JValue::Int(*uid)],
        )?;
    }
    let config = env
        .call_method(
            &builder,
            "build",
            "()Landroid/media/AudioPlaybackCaptureConfiguration;",
            &[],
        )?
        .l()?;

    // 2. The format: 48 kHz float, mono or stereo.
    let fmt_builder = env.new_object("android/media/AudioFormat$Builder", "()V", &[])?;
    env.call_method(
        &fmt_builder,
        "setEncoding",
        "(I)Landroid/media/AudioFormat$Builder;",
        &[JValue::Int(ENCODING_PCM_FLOAT)],
    )?;
    env.call_method(
        &fmt_builder,
        "setSampleRate",
        "(I)Landroid/media/AudioFormat$Builder;",
        &[JValue::Int(ENGINE_SAMPLE_RATE as i32)],
    )?;
    env.call_method(
        &fmt_builder,
        "setChannelMask",
        "(I)Landroid/media/AudioFormat$Builder;",
        &[JValue::Int(mask)],
    )?;
    let format = env
        .call_method(&fmt_builder, "build", "()Landroid/media/AudioFormat;", &[])?
        .l()?;

    // 3. The record itself, with a buffer of at least four reads.
    let minimum = env
        .call_static_method(
            "android/media/AudioRecord",
            "getMinBufferSize",
            "(III)I",
            &[
                JValue::Int(ENGINE_SAMPLE_RATE as i32),
                JValue::Int(mask),
                JValue::Int(ENCODING_PCM_FLOAT),
            ],
        )?
        .i()?;
    // A negative minimum is an error code (the combination is not queryable); use our floor.
    let size = capture_buffer_bytes(minimum.max(0), channels);
    let rec_builder = env.new_object("android/media/AudioRecord$Builder", "()V", &[])?;
    env.call_method(
        &rec_builder,
        "setAudioPlaybackCaptureConfig",
        "(Landroid/media/AudioPlaybackCaptureConfiguration;)Landroid/media/AudioRecord$Builder;",
        &[JValue::Object(&config)],
    )?;
    env.call_method(
        &rec_builder,
        "setAudioFormat",
        "(Landroid/media/AudioFormat;)Landroid/media/AudioRecord$Builder;",
        &[JValue::Object(&format)],
    )?;
    env.call_method(
        &rec_builder,
        "setBufferSizeInBytes",
        "(I)Landroid/media/AudioRecord$Builder;",
        &[JValue::Int(size)],
    )?;
    let record = env
        .call_method(&rec_builder, "build", "()Landroid/media/AudioRecord;", &[])?
        .l()?;
    let state = env.call_method(&record, "getState", "()I", &[])?.i()?;
    if state != STATE_INITIALIZED {
        env.call_method(&record, "release", "()V", &[])?;
        return Err(jni::errors::Error::NullPtr(
            "AudioRecord did not initialize (the screen-capture grant or RECORD_AUDIO is missing)",
        ));
    }

    // 4. The read buffer: a direct ByteBuffer that Rust can address.
    let capacity = capture_read_bytes(channels);
    let bb = env
        .call_static_method(
            "java/nio/ByteBuffer",
            "allocateDirect",
            "(I)Ljava/nio/ByteBuffer;",
            &[JValue::Int(i32::try_from(capacity).unwrap_or(0))],
        )?
        .l()?;
    let bb = JByteBuffer::from(bb);
    let address = env.get_direct_buffer_address(&bb)? as usize;
    let object = env.new_global_ref(&bb)?;

    env.call_method(&record, "startRecording", "()V", &[])?;
    let record = env.new_global_ref(&record)?;
    Ok((
        record,
        DirectBuffer {
            object,
            address,
            capacity,
        },
    ))
}

/// `stop()` then `release()`, ignoring failures (the record may be dead already).
fn release(env: &mut JNIEnv, record: &GlobalRef) {
    for method in ["stop", "release"] {
        if env.call_method(record, method, "()V", &[]).is_err()
            && env.exception_check().unwrap_or(false)
        {
            let _ = env.exception_clear();
        }
    }
}

impl CaptureStream {
    fn counters(&self) -> &Counters {
        &self.slot.counters
    }
}

impl AudioStream for CaptureStream {
    fn stats(&self) -> StreamStats {
        let c = self.counters();
        StreamStats {
            frames: c.frames.load(Relaxed),
            xruns: c.xruns.load(Relaxed),
            clock_ppm: 0.0,
        }
    }

    fn detail(&self) -> String {
        match &*self.fault.lock().unwrap_or_else(PoisonError::into_inner) {
            Some(why) => format!("STOPPED: {why} (was: {})", self.detail),
            None => self.detail.clone(),
        }
    }

    fn close(mut self: Box<Self>) -> Option<ClosedPort> {
        self.stop.store(true, Relaxed);
        // `AudioRecord.stop()` makes a blocked `read` return, so the thread ends promptly.
        let _ = jvm::with_jni(|env, _| {
            env.call_method(&self.record, "stop", "()V", &[])?;
            Ok(())
        });
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        self.slot.take().map(|st| ClosedPort::Input(st.into_port()))
    }
}
