//! The JNI calls the lifecycle needs, one small function each. No decisions in here: what to do
//! and in which order is `lifecycle::Lifecycle`'s job, and this file only does it.
//!
//! * Every function attaches the calling thread through `platform_io::android::jvm::with_jni`,
//!   so it may be called from the main thread, a Java callback or a Rust thread alike.
//! * Every signature below was checked against the real API 35 `android.jar` with `javap`.

use super::Classes;
use jni::JNIEnv;
use jni::errors::Result as JniResult;
use jni::objects::{GlobalRef, JClass, JObject, JObjectArray, JString, JValue};
use platform_io::BackendError;
use platform_io::android::jvm::with_jni;
use std::path::PathBuf;

/// The id of the one notification of the foreground service.
pub(super) const NOTIFICATION_ID: i32 = 7001;
const CHANNEL_ID: &str = "retae_engine";
const CHANNEL_NAME: &str = "Audio router";
/// `NotificationManager.IMPORTANCE_LOW`: no sound, still visible.
const IMPORTANCE_LOW: i32 = 2;
/// `PendingIntent.FLAG_IMMUTABLE | FLAG_UPDATE_CURRENT`.
const PENDING_FLAGS: i32 = 0x0400_0000 | 0x0800_0000;
/// `Intent.FLAG_ACTIVITY_NEW_TASK`.
const FLAG_ACTIVITY_NEW_TASK: i32 = 0x1000_0000;
/// `android.R.drawable.stat_sys_headset`: a framework icon, so the app needs no drawable.
const SMALL_ICON: i32 = 17_301_635;
/// `WifiManager.WIFI_MODE_FULL_LOW_LATENCY` (API 29).
const WIFI_MODE_FULL_LOW_LATENCY: i32 = 4;
/// `PowerManager.PARTIAL_WAKE_LOCK`.
const PARTIAL_WAKE_LOCK: i32 = 1;

pub(super) const REQUEST_PROJECTION: i32 = 4101;
pub(super) const REQUEST_PERMISSIONS: i32 = 4102;

/// The text under the title of the notification.
pub(super) const RUNNING_TEXT: &str = "Router running";

type Result<T> = core::result::Result<T, BackendError>;

fn string(env: &mut JNIEnv, s: JObject) -> JniResult<String> {
    if s.is_null() {
        return Ok(String::new());
    }
    Ok(env.get_string(&JString::from(s))?.into())
}

/// A global reference's object as a local `JClass`, which `new_object` accepts.
fn class_of<'a>(env: &mut JNIEnv<'a>, class: &GlobalRef) -> JniResult<JClass<'a>> {
    Ok(JClass::from(env.new_local_ref(class.as_obj())?))
}

/// `new Intent(context, class)` with an action.
fn explicit_intent<'a>(
    env: &mut JNIEnv<'a>,
    ctx: &JObject,
    class: &GlobalRef,
    action: &str,
) -> JniResult<JObject<'a>> {
    let class = class_of(env, class)?;
    let intent = env.new_object(
        &class,
        "(Landroid/content/Context;Ljava/lang/Class;)V",
        &[JValue::Object(ctx), JValue::Object(&class)],
    )?;
    let action = env.new_string(action)?;
    env.call_method(
        &intent,
        "setAction",
        "(Ljava/lang/String;)Landroid/content/Intent;",
        &[JValue::Object(&action)],
    )?;
    Ok(intent)
}

fn system_service<'a>(env: &mut JNIEnv<'a>, ctx: &JObject, name: &str) -> JniResult<JObject<'a>> {
    let name = env.new_string(name)?;
    env.call_method(
        ctx,
        "getSystemService",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        &[JValue::Object(&name)],
    )?
    .l()
}

// SECTION: Services and activities
/// `startForegroundService(Intent(ctx, AudioEngineService).setAction(action))`.
pub(super) fn start_service(classes: &Classes, action: &str) -> Result<()> {
    with_jni(|env, ctx| {
        let intent = explicit_intent(env, ctx, &classes.service, action)?;
        env.call_method(
            ctx,
            "startForegroundService",
            "(Landroid/content/Intent;)Landroid/content/ComponentName;",
            &[JValue::Object(&intent)],
        )?;
        Ok(())
    })
}

/// Show the consent Activity in one of its two modes (`natives::ACTION_ASK_*`).
pub(super) fn launch_consent(classes: &Classes, action: &str) -> Result<()> {
    with_jni(|env, ctx| {
        let intent = explicit_intent(env, ctx, &classes.consent, action)?;
        env.call_method(
            &intent,
            "addFlags",
            "(I)Landroid/content/Intent;",
            &[JValue::Int(FLAG_ACTIVITY_NEW_TASK)],
        )?;
        env.call_method(
            ctx,
            "startActivity",
            "(Landroid/content/Intent;)V",
            &[JValue::Object(&intent)],
        )?;
        Ok(())
    })
}

pub(super) fn stop_self(service: &GlobalRef) -> Result<()> {
    with_jni(|env, _| {
        env.call_method(service.as_obj(), "stopSelf", "()V", &[])?;
        Ok(())
    })
}

pub(super) fn finish(activity: &GlobalRef) -> Result<()> {
    with_jni(|env, _| {
        env.call_method(activity.as_obj(), "finish", "()V", &[])?;
        Ok(())
    })
}

/// `Intent.getAction()` of an intent a Java callback handed over.
pub(super) fn intent_action(intent: &JObject) -> Result<Option<String>> {
    with_jni(|env, _| {
        if intent.is_null() {
            return Ok(None);
        }
        let a = env
            .call_method(intent, "getAction", "()Ljava/lang/String;", &[])?
            .l()?;
        if a.is_null() {
            return Ok(None);
        }
        Ok(Some(string(env, a)?))
    })
}

/// `Activity.startActivityForResult(createScreenCaptureIntent(), REQUEST_PROJECTION)`.
pub(super) fn ask_projection(activity: &GlobalRef) -> Result<()> {
    with_jni(|env, ctx| {
        let mpm = system_service(env, ctx, "media_projection")?;
        let intent = env
            .call_method(
                &mpm,
                "createScreenCaptureIntent",
                "()Landroid/content/Intent;",
                &[],
            )?
            .l()?;
        env.call_method(
            activity.as_obj(),
            "startActivityForResult",
            "(Landroid/content/Intent;I)V",
            &[JValue::Object(&intent), JValue::Int(REQUEST_PROJECTION)],
        )?;
        Ok(())
    })
}

fn sdk_int(env: &mut JNIEnv) -> JniResult<i32> {
    env.get_static_field("android/os/Build$VERSION", "SDK_INT", "I")?
        .i()
}

/// `Activity.requestPermissions`: the microphone, and the notification permission on Android 13+.
pub(super) fn ask_permissions(activity: &GlobalRef) -> Result<()> {
    with_jni(|env, _| {
        let mut names = vec!["android.permission.RECORD_AUDIO"];
        if sdk_int(env)? >= 33 {
            names.push("android.permission.POST_NOTIFICATIONS");
        }
        let string_class = env.find_class("java/lang/String")?;
        let empty = env.new_string("")?;
        let arr: JObjectArray = env.new_object_array(names.len() as i32, &string_class, &empty)?;
        for (i, n) in names.iter().enumerate() {
            let s = env.new_string(*n)?;
            env.set_object_array_element(&arr, i as i32, &s)?;
        }
        env.call_method(
            activity.as_obj(),
            "requestPermissions",
            "([Ljava/lang/String;I)V",
            &[JValue::Object(&arr), JValue::Int(REQUEST_PERMISSIONS)],
        )?;
        Ok(())
    })
}

/// Is the microphone permission granted (`Context.checkSelfPermission == PERMISSION_GRANTED`)?
pub(super) fn microphone_permitted() -> bool {
    with_jni(|env, ctx| {
        let p = env.new_string("android.permission.RECORD_AUDIO")?;
        let r = env
            .call_method(
                ctx,
                "checkSelfPermission",
                "(Ljava/lang/String;)I",
                &[JValue::Object(&p)],
            )?
            .i()?;
        Ok(r == 0)
    })
    .unwrap_or(false)
}
// !SECTION: Services and activities

// SECTION: Notification
/// Build the one notification. Paused: tapping it opens the consent dialog; otherwise the app.
fn build_notification<'a>(
    env: &mut JNIEnv<'a>,
    ctx: &JObject,
    classes: &Classes,
    paused_text: Option<&str>,
) -> JniResult<JObject<'a>> {
    let nm = system_service(env, ctx, "notification")?;
    let id = env.new_string(CHANNEL_ID)?;
    let name = env.new_string(CHANNEL_NAME)?;
    let channel = env.new_object(
        "android/app/NotificationChannel",
        "(Ljava/lang/String;Ljava/lang/CharSequence;I)V",
        &[
            JValue::Object(&id),
            JValue::Object(&name),
            JValue::Int(IMPORTANCE_LOW),
        ],
    )?;
    env.call_method(
        &nm,
        "createNotificationChannel",
        "(Landroid/app/NotificationChannel;)V",
        &[JValue::Object(&channel)],
    )?;

    // The tap target.
    let tap = if paused_text.is_some() {
        explicit_intent(
            env,
            ctx,
            &classes.consent,
            crate::natives::ACTION_ASK_PROJECTION,
        )?
    } else {
        let pm = env
            .call_method(
                ctx,
                "getPackageManager",
                "()Landroid/content/pm/PackageManager;",
                &[],
            )?
            .l()?;
        let pkg = env
            .call_method(ctx, "getPackageName", "()Ljava/lang/String;", &[])?
            .l()?;
        env.call_method(
            &pm,
            "getLaunchIntentForPackage",
            "(Ljava/lang/String;)Landroid/content/Intent;",
            &[JValue::Object(&pkg)],
        )?
        .l()?
    };
    let pending = if tap.is_null() {
        JObject::null()
    } else {
        env.call_static_method(
            "android/app/PendingIntent",
            "getActivity",
            "(Landroid/content/Context;ILandroid/content/Intent;I)Landroid/app/PendingIntent;",
            &[
                JValue::Object(ctx),
                JValue::Int(0),
                JValue::Object(&tap),
                JValue::Int(PENDING_FLAGS),
            ],
        )?
        .l()?
    };

    let builder = env.new_object(
        "android/app/Notification$Builder",
        "(Landroid/content/Context;Ljava/lang/String;)V",
        &[JValue::Object(ctx), JValue::Object(&id)],
    )?;
    let title = env.new_string("ReTAE")?;
    let text = env.new_string(paused_text.unwrap_or(RUNNING_TEXT))?;
    for (method, sig, arg) in [
        (
            "setContentTitle",
            "(Ljava/lang/CharSequence;)Landroid/app/Notification$Builder;",
            JValue::Object(&title),
        ),
        (
            "setContentText",
            "(Ljava/lang/CharSequence;)Landroid/app/Notification$Builder;",
            JValue::Object(&text),
        ),
        (
            "setSmallIcon",
            "(I)Landroid/app/Notification$Builder;",
            JValue::Int(SMALL_ICON),
        ),
        (
            "setOngoing",
            "(Z)Landroid/app/Notification$Builder;",
            JValue::Bool(1),
        ),
    ] {
        env.call_method(&builder, method, sig, &[arg])?;
    }
    if !pending.is_null() {
        env.call_method(
            &builder,
            "setContentIntent",
            "(Landroid/app/PendingIntent;)Landroid/app/Notification$Builder;",
            &[JValue::Object(&pending)],
        )?;
    }
    env.call_method(&builder, "build", "()Landroid/app/Notification;", &[])?
        .l()
}

/// `service.startForeground(id, notification, types)`.
// ? The three-argument form (API 29) is the one that takes the service types. Calling it again
// ? with fewer types is how the projection type is dropped while the microphone one stays.
pub(super) fn start_foreground(
    classes: &Classes,
    service: &GlobalRef,
    type_mask: i32,
    paused_text: Option<&str>,
) -> Result<()> {
    with_jni(|env, ctx| {
        let n = build_notification(env, ctx, classes, paused_text)?;
        env.call_method(
            service.as_obj(),
            "startForeground",
            "(ILandroid/app/Notification;I)V",
            &[
                JValue::Int(NOTIFICATION_ID),
                JValue::Object(&n),
                JValue::Int(type_mask),
            ],
        )?;
        Ok(())
    })
}

/// Replace the text of the notification that is already up.
pub(super) fn update_notification(classes: &Classes, paused_text: Option<&str>) -> Result<()> {
    with_jni(|env, ctx| {
        let n = build_notification(env, ctx, classes, paused_text)?;
        let nm = system_service(env, ctx, "notification")?;
        env.call_method(
            &nm,
            "notify",
            "(ILandroid/app/Notification;)V",
            &[JValue::Int(NOTIFICATION_ID), JValue::Object(&n)],
        )?;
        Ok(())
    })
}
// !SECTION: Notification

// SECTION: Locks
/// A held `WifiLock` or `WakeLock`.
pub(super) struct Locks {
    wifi: GlobalRef,
    wake: GlobalRef,
}

/// `WifiLock(WIFI_MODE_FULL_LOW_LATENCY)` and a partial `WakeLock`, held while streaming.
// ? Low-latency mode keeps the radio out of power-save while the screen is off. It only takes
// ? effect while the app is in the foreground or runs a foreground service, which is why the
// ? locks are tied to `StreamingWanted` and not taken at start-up.
pub(super) fn acquire_locks() -> Result<Locks> {
    with_jni(|env, ctx| {
        let wm = system_service(env, ctx, "wifi")?;
        let tag = env.new_string("retae:wifi")?;
        let wifi = env
            .call_method(
                &wm,
                "createWifiLock",
                "(ILjava/lang/String;)Landroid/net/wifi/WifiManager$WifiLock;",
                &[
                    JValue::Int(WIFI_MODE_FULL_LOW_LATENCY),
                    JValue::Object(&tag),
                ],
            )?
            .l()?;
        env.call_method(&wifi, "setReferenceCounted", "(Z)V", &[JValue::Bool(0)])?;
        env.call_method(&wifi, "acquire", "()V", &[])?;

        let pm = system_service(env, ctx, "power")?;
        let tag = env.new_string("retae:cpu")?;
        let wake = env
            .call_method(
                &pm,
                "newWakeLock",
                "(ILjava/lang/String;)Landroid/os/PowerManager$WakeLock;",
                &[JValue::Int(PARTIAL_WAKE_LOCK), JValue::Object(&tag)],
            )?
            .l()?;
        env.call_method(&wake, "setReferenceCounted", "(Z)V", &[JValue::Bool(0)])?;
        env.call_method(&wake, "acquire", "()V", &[])?;
        Ok(Locks {
            wifi: env.new_global_ref(&wifi)?,
            wake: env.new_global_ref(&wake)?,
        })
    })
}

pub(super) fn release_locks(locks: &Locks) -> Result<()> {
    with_jni(|env, _| {
        env.call_method(locks.wake.as_obj(), "release", "()V", &[])?;
        env.call_method(locks.wifi.as_obj(), "release", "()V", &[])?;
        Ok(())
    })
}
// !SECTION: Locks

// SECTION: Screen capture
/// `MediaProjectionManager.getMediaProjection(resultCode, data)` and the stop callback.
// !! Must run after `startForeground` with the projection type (Android 14), and `data` is
// !! single-use: a second call with the same `Intent` throws a `SecurityException`.
pub(super) fn get_projection(
    classes: &Classes,
    result_code: i32,
    data: &GlobalRef,
) -> Result<GlobalRef> {
    with_jni(|env, ctx| {
        let mpm = system_service(env, ctx, "media_projection")?;
        let projection = env
            .call_method(
                &mpm,
                "getMediaProjection",
                "(ILandroid/content/Intent;)Landroid/media/projection/MediaProjection;",
                &[JValue::Int(result_code), JValue::Object(data.as_obj())],
            )?
            .l()?;
        // Android 14 wants the callback registered before capture starts. The handler is the
        // main looper: a Rust thread has no looper of its own.
        let looper = env
            .call_static_method(
                "android/os/Looper",
                "getMainLooper",
                "()Landroid/os/Looper;",
                &[],
            )?
            .l()?;
        let handler = env.new_object(
            "android/os/Handler",
            "(Landroid/os/Looper;)V",
            &[JValue::Object(&looper)],
        )?;
        let cb_class = class_of(env, &classes.stop_callback)?;
        let callback = env.new_object(&cb_class, "()V", &[])?;
        env.call_method(
            &projection,
            "registerCallback",
            "(Landroid/media/projection/MediaProjection$Callback;Landroid/os/Handler;)V",
            &[JValue::Object(&callback), JValue::Object(&handler)],
        )?;
        env.new_global_ref(&projection)
    })
}

pub(super) fn stop_projection(projection: &GlobalRef) -> Result<()> {
    with_jni(|env, _| {
        env.call_method(projection.as_obj(), "stop", "()V", &[])?;
        Ok(())
    })
}
// !SECTION: Screen capture

// SECTION: Folders
/// `Context.getFilesDir()` (kind `None`) or `getExternalFilesDir(null)`: the app-specific
/// folders. Neither needs a storage permission, and both go away with the app.
pub fn app_dir(external: bool) -> Result<PathBuf> {
    with_jni(|env, ctx| {
        let file = if external {
            env.call_method(
                ctx,
                "getExternalFilesDir",
                "(Ljava/lang/String;)Ljava/io/File;",
                &[JValue::Object(&JObject::null())],
            )?
            .l()?
        } else {
            env.call_method(ctx, "getFilesDir", "()Ljava/io/File;", &[])?
                .l()?
        };
        if file.is_null() {
            return Err(jni::errors::Error::NullPtr("app folder"));
        }
        let path = env
            .call_method(&file, "getAbsolutePath", "()Ljava/lang/String;", &[])?
            .l()?;
        Ok(PathBuf::from(string(env, path)?))
    })
}
// !SECTION: Folders
