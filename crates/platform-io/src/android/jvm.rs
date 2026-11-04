//! The JVM handle every Android call goes through.
//!
//! `android-app` installs the `JavaVM` and the application `Context` once (from `JNI_OnLoad` and
//! the first call from Java). After that any thread of the backend can attach itself and make a
//! JNI call with `with_jni`.

use crate::backend::BackendError;
use jni::objects::{GlobalRef, JObject, JValue};
use jni::{JNIEnv, JavaVM};
use std::sync::{Mutex, OnceLock, PoisonError};

struct Host {
    vm: JavaVM,
    /// The *application* context: it lives as long as the process, unlike an Activity.
    context: GlobalRef,
}

static HOST: OnceLock<Host> = OnceLock::new();

// ? The screen-capture grant (`android.media.projection.MediaProjection`). `android-app` puts it
// ? here after the consent dialog and the foreground service are in place, and takes it away on
// ? `MediaProjection.Callback.onStop`. A `Mutex` is fine: only control and capture-start paths
// ? read it, never the audio callbacks.
static PROJECTION: Mutex<Option<GlobalRef>> = Mutex::new(None);

/// Store the JVM and the application context. Returns `false` if that was done already.
// ? `GlobalRef` is a JNI "global reference": a handle that keeps a Java object alive and may be
// ? used from any thread. A plain local reference would be invalid as soon as the Java call that
// ? made it returned.
pub fn install(vm: JavaVM, context: GlobalRef) -> bool {
    HOST.set(Host { vm, context }).is_ok()
}

pub fn is_installed() -> bool {
    HOST.get().is_some()
}

/// The JVM, for code outside this crate that needs its own attached thread.
pub fn java_vm() -> Option<&'static JavaVM> {
    HOST.get().map(|h| &h.vm)
}

/// The application context.
pub fn app_context() -> Option<&'static GlobalRef> {
    HOST.get().map(|h| &h.context)
}

/// Hand over (or take away, with `None`) the screen-capture grant.
pub fn set_projection(projection: Option<GlobalRef>) {
    *PROJECTION.lock().unwrap_or_else(PoisonError::into_inner) = projection;
}

pub fn projection() -> Option<GlobalRef> {
    PROJECTION
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Turn a JNI failure into a `BackendError`, clearing a pending Java exception first.
// !! A pending Java exception makes every later JNI call on that thread fail or abort, so it
// !! must be cleared. `exception_describe` also prints it to logcat, which is where a person
// !! debugging a phone will look.
pub fn java_error(env: &mut JNIEnv, e: &jni::errors::Error) -> BackendError {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    BackendError::Io(format!("Android call failed: {e}"))
}

/// Run `f` with a JNI environment for the calling thread and the application context.
///
/// The thread is attached on first use and stays attached (Rust threads are never "returned to
/// Java", so detaching on every call would be wasted work). Every call gets its own local-
/// reference frame, so the objects it makes are freed when it returns instead of piling up for
/// the life of a long-running thread.
pub fn with_jni<R>(
    f: impl FnOnce(&mut JNIEnv, &JObject) -> jni::errors::Result<R>,
) -> Result<R, BackendError> {
    let host = HOST
        .get()
        .ok_or_else(|| BackendError::Io("the Android context was not installed".into()))?;
    let mut env = host
        .vm
        .attach_current_thread_permanently()
        .map_err(|e| BackendError::Io(format!("cannot attach to the JVM: {e}")))?;
    let context = host.context.as_obj();
    match env.with_local_frame(64, |env| f(env, context)) {
        Ok(v) => Ok(v),
        Err(e) => Err(java_error(&mut env, &e)),
    }
}

/// `android.os.Process.THREAD_PRIORITY_URGENT_AUDIO`.
const THREAD_PRIORITY_URGENT_AUDIO: i32 = -19;

/// Ask Android for audio priority for the calling thread (PLAN §4.3).
pub fn set_urgent_audio_priority() -> Result<(), BackendError> {
    with_jni(|env, _| {
        env.call_static_method(
            "android/os/Process",
            "setThreadPriority",
            "(I)V",
            &[JValue::Int(THREAD_PRIORITY_URGENT_AUDIO)],
        )?;
        Ok(())
    })
}
