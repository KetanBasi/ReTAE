//! The Android glue: it feeds events to `lifecycle::Lifecycle` and performs the actions that
//! come back, over JNI. Only builds for Android; the rules it follows are host-tested in
//! `lifecycle`, and the Java it talks to is listed in `natives`.
//!
//! ```text
//!   Java callbacks (Service, consent Activity, MediaProjection.Callback)
//!         | native methods registered by `init`
//!         v
//!   dispatch(Event) --> Lifecycle::handle --> Vec<Action> --> run(Action) --> ops::* (JNI)
//!         ^                                                        |
//!         +---------------- results come back as Events -----------+
//! ```
//!
//! Registration is `init`, not `JNI_OnLoad`: a Makepad `app_main!` already exports `JNI_OnLoad`
//! for the library (two would not link), so the app calls `init` once its Activity exists.

pub mod host;
mod ops;

pub use ops::app_dir;

use crate::lifecycle::{Action, Event, Fgs, Lifecycle, Warning};
use crate::natives::{self, NativeClass};
use crate::view_model::APP_AUDIO_PAUSED;
use jni::objects::{GlobalRef, JClass, JIntArray, JObject, JValue};
use jni::sys::jint;
use jni::{JNIEnv, NativeMethod};
use platform_io::android::jvm;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Mutex, OnceLock, PoisonError};

// SECTION: State
/// The Java classes, kept as global references: a Rust thread cannot `FindClass` an app class,
/// because it only sees the system class loader.
pub(crate) struct Classes {
    pub service: GlobalRef,
    pub consent: GlobalRef,
    pub stop_callback: GlobalRef,
}

static CLASSES: OnceLock<Classes> = OnceLock::new();

/// The screen-capture consent, until `getMediaProjection` uses it.
struct Grant {
    result_code: i32,
    data: GlobalRef,
}

struct Glue {
    life: Lifecycle,
    service: Option<GlobalRef>,
    consent: Option<GlobalRef>,
    grant: Option<Grant>,
    projection: Option<GlobalRef>,
    locks: Option<ops::Locks>,
    warning: Option<Warning>,
    /// "Start" was pressed before the microphone permission was granted.
    pending_start: bool,
}

static GLUE: Mutex<Glue> = Mutex::new(Glue {
    life: Lifecycle::new(),
    service: None,
    consent: None,
    grant: None,
    projection: None,
    locks: None,
    warning: None,
    pending_start: false,
});

fn glue() -> std::sync::MutexGuard<'static, Glue> {
    // ? A poisoned lock means a panic while it was held. The state is still usable (every
    // ? field is plain data), and giving up would leave the foreground service stuck.
    GLUE.lock().unwrap_or_else(PoisonError::into_inner)
}

fn log(text: &str) {
    // ? Makepad sends stderr to logcat; without it this goes nowhere, which is harmless.
    eprintln!("retae: {text}");
}
// !SECTION: State

// SECTION: Dispatch
/// Feed one event to the state machine and perform what it asks.
// ? The lock is released before any action runs: an action calls into Java, and Java may call
// ? straight back into a native method that wants the lock again.
pub fn dispatch(event: Event) {
    let actions = glue().life.handle(event);
    for action in actions {
        run(action);
    }
}

fn paused_text() -> Option<&'static str> {
    glue().life.app_audio_paused().then_some(APP_AUDIO_PAUSED)
}

fn run(action: Action) {
    let Some(classes) = CLASSES.get() else {
        log("glue::init has not run");
        return;
    };
    match action {
        Action::StartMicService => {
            if let Err(e) = ops::start_service(classes, natives::ACTION_START_MIC) {
                log(&format!("cannot start the service: {e}"));
                dispatch(Event::ForegroundRefused);
            }
        }
        Action::LaunchConsent => {
            // Already open (the notification started it): nothing to launch.
            if glue().consent.is_some() {
                return;
            }
            if let Err(e) = ops::launch_consent(classes, natives::ACTION_ASK_PROJECTION) {
                log(&format!("cannot show the consent dialog: {e}"));
                dispatch(Event::ConsentResult { granted: false });
            }
        }
        Action::StartForegroundWithProjection => {
            let service = glue().service.clone();
            let ok = service.is_some_and(|s| {
                ops::start_foreground(classes, &s, Fgs::MicAndProjection.type_mask(), None)
                    .map_err(|e| log(&format!("startForeground(projection): {e}")))
                    .is_ok()
            });
            dispatch(if ok {
                Event::ForegroundStarted {
                    with_projection: true,
                }
            } else {
                Event::ForegroundRefused
            });
        }
        Action::StartForegroundMicOnly => {
            let service = glue().service.clone();
            if let Some(s) = service {
                if let Err(e) =
                    ops::start_foreground(classes, &s, Fgs::Mic.type_mask(), paused_text())
                {
                    log(&format!("startForeground(mic): {e}"));
                }
            }
        }
        Action::GetProjection => get_projection(classes),
        Action::ReleaseProjection => {
            let old = glue().projection.take();
            jvm::set_projection(None);
            if let Some(p) = old {
                // ? Already stopped when this follows `onStop`; stopping twice is harmless.
                let _ = ops::stop_projection(&p);
            }
            glue().grant = None;
        }
        Action::AcquireLocks => match ops::acquire_locks() {
            Ok(l) => glue().locks = Some(l),
            Err(e) => log(&format!("cannot take the Wi-Fi and CPU locks: {e}")),
        },
        Action::ReleaseLocks => {
            let held = glue().locks.take();
            if let Some(l) = held {
                if let Err(e) = ops::release_locks(&l) {
                    log(&format!("cannot release the locks: {e}"));
                }
            }
        }
        Action::NotifyAppAudioPaused => {
            let _ = ops::update_notification(classes, Some(APP_AUDIO_PAUSED));
        }
        Action::NotifyRunning => {
            let _ = ops::update_notification(classes, None);
        }
        Action::Warn(w) => glue().warning = Some(w),
        Action::StopService => {
            let service = glue().service.clone();
            if let Some(s) = service {
                let _ = ops::stop_self(&s);
            }
        }
    }
}

fn get_projection(classes: &Classes) {
    // ? `take`: the consent is single-use, so it is gone from here whatever happens next.
    let grant = glue().grant.take();
    let Some(grant) = grant else {
        log("no screen-capture consent to use");
        dispatch(Event::ProjectionFailed);
        return;
    };
    match ops::get_projection(classes, grant.result_code, &grant.data) {
        Ok(p) => {
            jvm::set_projection(Some(p.clone()));
            glue().projection = Some(p);
            dispatch(Event::ProjectionReady);
        }
        Err(e) => {
            log(&format!("getMediaProjection: {e}"));
            dispatch(Event::ProjectionFailed);
        }
    }
}
// !SECTION: Dispatch

// SECTION: What the app calls
/// What the screen needs from the glue.
#[derive(Clone, Copy, Debug)]
pub struct Status {
    pub life: Lifecycle,
    pub warning: Option<Warning>,
}

pub fn status() -> Status {
    let g = glue();
    Status {
        life: g.life,
        warning: g.warning,
    }
}

pub fn clear_warning() {
    glue().warning = None;
}

/// The Activity came to the front (`true`) or left it.
pub fn set_ui_visible(visible: bool) {
    dispatch(Event::UiVisible(visible));
}

/// "Start": ask for the microphone permission if needed, then run the service.
pub fn start_router() {
    if ops::microphone_permitted() {
        dispatch(Event::StreamingWanted(true));
        return;
    }
    glue().pending_start = true;
    if let Some(classes) = CLASSES.get() {
        if let Err(e) = ops::launch_consent(classes, natives::ACTION_ASK_PERMISSIONS) {
            log(&format!("cannot ask for permissions: {e}"));
            glue().pending_start = false;
        }
    }
}

pub fn stop_router() {
    dispatch(Event::StreamingWanted(false));
}

/// "Capture app audio" or "tap to resume".
pub fn request_app_audio() {
    dispatch(Event::AppAudioRequested);
}

pub fn dismiss_app_audio() {
    dispatch(Event::AppAudioDismissed);
}
// !SECTION: What the app calls

// SECTION: Registration
fn dotted(class: &NativeClass) -> String {
    class.class.replace('/', ".")
}

/// The Rust function behind each `native` method (same names as `natives.rs`).
fn fn_ptr(class: &NativeClass, name: &str) -> Option<*mut c_void> {
    let p: *mut c_void = match (class.class, name) {
        ("dev/f/lanrouter/AudioEngineService", "nativeOnCreate") => service_on_create as _,
        ("dev/f/lanrouter/AudioEngineService", "nativeOnStartCommand") => {
            service_on_start_command as _
        }
        ("dev/f/lanrouter/AudioEngineService", "nativeOnDestroy") => service_on_destroy as _,
        ("dev/f/lanrouter/ProjectionConsentActivity", "nativeOnCreate") => consent_on_create as _,
        ("dev/f/lanrouter/ProjectionConsentActivity", "nativeOnActivityResult") => {
            consent_on_activity_result as _
        }
        ("dev/f/lanrouter/ProjectionConsentActivity", "nativeOnRequestPermissionsResult") => {
            consent_on_permissions_result as _
        }
        ("dev/f/lanrouter/ProjectionConsentActivity", "nativeOnDestroy") => consent_on_destroy as _,
        ("dev/f/lanrouter/ProjectionStopCallback", "nativeOnStop") => stop_callback_on_stop as _,
        _ => return None,
    };
    Some(p)
}

fn register_class<'a>(
    env: &mut JNIEnv<'a>,
    loader: &JObject,
    class: &NativeClass,
) -> Result<GlobalRef, String> {
    let name = env.new_string(dotted(class)).map_err(|e| e.to_string())?;
    let found = env
        .call_method(
            loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[JValue::Object(&name)],
        )
        .and_then(|v| v.l())
        .map_err(|e| format!("cannot load {}: {e}", dotted(class)))?;
    let jclass = JClass::from(found);
    let methods = class
        .methods
        .iter()
        .map(|(n, sig)| {
            fn_ptr(class, n)
                .map(|fn_ptr| NativeMethod {
                    name: (*n).into(),
                    sig: (*sig).into(),
                    fn_ptr,
                })
                .ok_or_else(|| format!("no Rust function for {}.{n}", class.class))
        })
        .collect::<Result<Vec<_>, _>>()?;
    env.register_native_methods(&jclass, &methods)
        .map_err(|e| format!("RegisterNatives for {}: {e}", dotted(class)))?;
    env.new_global_ref(&jclass).map_err(|e| e.to_string())
}

/// Register the natives, hand the JVM and the application context to `platform-io`, and keep
/// the Java classes. Call once, from a thread that was started by Java (the Makepad app thread
/// qualifies once its Activity exists), with any `Context` of the app.
pub fn init(env: &mut JNIEnv, context: &JObject) -> Result<(), String> {
    if CLASSES.get().is_some() {
        return Ok(());
    }
    let vm = env.get_java_vm().map_err(|e| e.to_string())?;
    let app_ctx = env
        .call_method(
            context,
            "getApplicationContext",
            "()Landroid/content/Context;",
            &[],
        )
        .and_then(|v| v.l())
        .map_err(|e| e.to_string())?;
    let app_ctx = env.new_global_ref(&app_ctx).map_err(|e| e.to_string())?;
    // ? Already installed (a second `init` after a restart of the app thread): keep the first.
    let _ = jvm::install(vm, app_ctx);

    // ? The app's class loader sees the classes in the extra dex; `FindClass` from a Rust-made
    // ? thread would not.
    let own_class = env.get_object_class(context).map_err(|e| e.to_string())?;
    let loader = env
        .call_method(
            &own_class,
            "getClassLoader",
            "()Ljava/lang/ClassLoader;",
            &[],
        )
        .and_then(|v| v.l())
        .map_err(|e| e.to_string())?;

    let service = register_class(env, &loader, &natives::SERVICE)?;
    let consent = register_class(env, &loader, &natives::CONSENT)?;
    let stop_callback = register_class(env, &loader, &natives::STOP_CALLBACK)?;
    let _ = CLASSES.set(Classes {
        service,
        consent,
        stop_callback,
    });
    Ok(())
}
// !SECTION: Registration

// SECTION: Natives
// ? These are called by the Java VM, so a panic must not unwind out of them (that aborts the
// ? process). `guard` turns a panic into a log line and a default value.
fn guard<R>(default: R, f: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| {
        log("a panic was stopped at the Java boundary");
        default
    })
}

/// `Service.START_NOT_STICKY`: if the system kills us, do not restart without an Activity.
const START_NOT_STICKY: jint = 2;

extern "system" fn service_on_create(env: JNIEnv, this: JObject) {
    guard((), || match env.new_global_ref(&this) {
        Ok(g) => glue().service = Some(g),
        Err(e) => log(&format!("service reference: {e}")),
    });
}

extern "system" fn service_on_start_command(
    _env: JNIEnv,
    _this: JObject,
    intent: JObject,
    _start_id: jint,
) -> jint {
    guard(START_NOT_STICKY, || {
        let action = ops::intent_action(&intent).ok().flatten();
        let (service, classes) = (glue().service.clone(), CLASSES.get());
        match (action.as_deref(), service, classes) {
            (Some(natives::ACTION_START_MIC), Some(s), Some(c)) => {
                // ? Within five seconds of `startForegroundService` or Android kills the app.
                match ops::start_foreground(c, &s, Fgs::Mic.type_mask(), paused_text()) {
                    Ok(()) => dispatch(Event::ForegroundStarted {
                        with_projection: false,
                    }),
                    Err(e) => {
                        log(&format!("startForeground: {e}"));
                        dispatch(Event::ForegroundRefused);
                        let _ = ops::stop_self(&s);
                    }
                }
            }
            (_, Some(s), _) => {
                // A restart without our action (or the stop action): nothing to run.
                let _ = ops::stop_self(&s);
            }
            _ => {}
        }
        START_NOT_STICKY
    })
}

extern "system" fn service_on_destroy(_env: JNIEnv, _this: JObject) {
    guard((), || {
        glue().service = None;
        dispatch(Event::ServiceDestroyed);
    });
}

extern "system" fn consent_on_create(env: JNIEnv, this: JObject, intent: JObject) {
    guard((), || {
        let Ok(activity) = env.new_global_ref(&this) else {
            return;
        };
        glue().consent = Some(activity.clone());
        let action = ops::intent_action(&intent).ok().flatten();
        match action.as_deref() {
            Some(natives::ACTION_ASK_PROJECTION) => {
                // Opened from the notification: the state machine has not been asked yet.
                if !glue().life.awaiting_consent() {
                    dispatch(Event::AppAudioRequested);
                }
                if !glue().life.awaiting_consent() {
                    // Not in a state that wants consent (already running, or refused).
                    let _ = ops::finish(&activity);
                    return;
                }
                if let Err(e) = ops::ask_projection(&activity) {
                    log(&format!("cannot ask for screen capture: {e}"));
                    dispatch(Event::ConsentResult { granted: false });
                    let _ = ops::finish(&activity);
                }
            }
            Some(natives::ACTION_ASK_PERMISSIONS) => {
                if let Err(e) = ops::ask_permissions(&activity) {
                    log(&format!("cannot ask for permissions: {e}"));
                    glue().pending_start = false;
                    let _ = ops::finish(&activity);
                }
            }
            _ => {
                let _ = ops::finish(&activity);
            }
        }
    });
}

extern "system" fn consent_on_activity_result(
    env: JNIEnv,
    _this: JObject,
    request: jint,
    result: jint,
    data: JObject,
) {
    guard((), || {
        if request != ops::REQUEST_PROJECTION {
            return;
        }
        // `Activity.RESULT_OK` is -1.
        let granted = result == -1 && !data.is_null();
        if granted {
            match env.new_global_ref(&data) {
                Ok(g) => {
                    glue().grant = Some(Grant {
                        result_code: result,
                        data: g,
                    });
                }
                Err(e) => log(&format!("consent data: {e}")),
            }
        }
        dispatch(Event::ConsentResult {
            granted: granted && glue().grant.is_some(),
        });
        let activity = glue().consent.clone();
        if let Some(a) = activity {
            let _ = ops::finish(&a);
        }
    });
}

extern "system" fn consent_on_permissions_result(
    env: JNIEnv,
    _this: JObject,
    request: jint,
    _permissions: JObject,
    results: JIntArray,
) {
    guard((), || {
        if request != ops::REQUEST_PERMISSIONS {
            return;
        }
        let len = env
            .get_array_length(&results)
            .ok()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0);
        let mut buf = vec![-1_i32; len];
        let _ = env.get_int_array_region(&results, 0, &mut buf);
        // The microphone is the first entry; notifications are optional.
        let mic_ok = buf.first().is_some_and(|r| *r == 0);
        let pending = std::mem::take(&mut glue().pending_start);
        if pending && mic_ok {
            dispatch(Event::StreamingWanted(true));
        }
        let activity = glue().consent.clone();
        if let Some(a) = activity {
            let _ = ops::finish(&a);
        }
    });
}

extern "system" fn consent_on_destroy(_env: JNIEnv, _this: JObject) {
    guard((), || glue().consent = None);
}

extern "system" fn stop_callback_on_stop(_env: JNIEnv, _this: JObject) {
    guard((), || dispatch(Event::ProjectionStopped));
}
// !SECTION: Natives
