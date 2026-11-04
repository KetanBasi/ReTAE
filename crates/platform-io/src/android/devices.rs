//! Listing audio devices (`AudioManager.getDevices`) and capturable apps (`PackageManager`).

use super::jvm::with_jni;
use crate::android_rules::{
    DEFAULT_DEVICE_ID, RawApp, device_channels, device_id_text, device_label, device_type_offered,
    shape_app_list,
};
use crate::backend::{AppInfo, DeviceInfo, DeviceKind};
use jni::JNIEnv;
use jni::objects::{JIntArray, JObject, JObjectArray, JString, JValue};

/// `AudioManager.GET_DEVICES_ALL` (inputs and outputs).
const GET_DEVICES_ALL: i32 = 3;

/// `CharSequence` to `String`; a null becomes "".
fn text(env: &mut JNIEnv, cs: &JObject) -> jni::errors::Result<String> {
    if cs.is_null() {
        return Ok(String::new());
    }
    let s = env
        .call_method(cs, "toString", "()Ljava/lang/String;", &[])?
        .l()?;
    let js = JString::from(s);
    let out: String = env.get_string(&js)?.into();
    Ok(out)
}

/// The system's default devices first, then every route a person would choose.
pub(crate) fn list_devices() -> Vec<DeviceInfo> {
    let mut out = vec![
        DeviceInfo {
            id: DEFAULT_DEVICE_ID.into(),
            name: "Default microphone".into(),
            kind: DeviceKind::Input,
            channels: 1,
            is_default: true,
        },
        DeviceInfo {
            id: DEFAULT_DEVICE_ID.into(),
            name: "Default output".into(),
            kind: DeviceKind::Output,
            channels: 2,
            is_default: true,
        },
    ];
    // ? A failed listing is not an error for the caller: the two defaults still work.
    let found = with_jni(|env, ctx| {
        let service = env.new_string("audio")?;
        let am = env
            .call_method(
                ctx,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[JValue::Object(&service)],
            )?
            .l()?;
        let arr = env
            .call_method(
                &am,
                "getDevices",
                "(I)[Landroid/media/AudioDeviceInfo;",
                &[JValue::Int(GET_DEVICES_ALL)],
            )?
            .l()?;
        let arr = JObjectArray::from(arr);
        let n = env.get_array_length(&arr)?;
        let mut found = Vec::new();
        for i in 0..n {
            // One frame per device keeps the local references of a long list bounded.
            let one =
                env.with_local_frame(16, |env| -> jni::errors::Result<Option<DeviceInfo>> {
                    let d = env.get_object_array_element(&arr, i)?;
                    let id = env.call_method(&d, "getId", "()I", &[])?.i()?;
                    let ty = env.call_method(&d, "getType", "()I", &[])?.i()?;
                    let is_source = env.call_method(&d, "isSource", "()Z", &[])?.z()?;
                    if !device_type_offered(ty, is_source) {
                        return Ok(None);
                    }
                    let product = env
                        .call_method(&d, "getProductName", "()Ljava/lang/CharSequence;", &[])?
                        .l()?;
                    let name = text(env, &product)?;
                    let counts = env.call_method(&d, "getChannelCounts", "()[I", &[])?.l()?;
                    let counts = JIntArray::from(counts);
                    let len = usize::try_from(env.get_array_length(&counts)?).unwrap_or(0);
                    let mut buf = vec![0_i32; len];
                    env.get_int_array_region(&counts, 0, &mut buf)?;
                    Ok(Some(DeviceInfo {
                        id: device_id_text(id),
                        name: device_label(&name, ty),
                        kind: if is_source {
                            DeviceKind::Input
                        } else {
                            DeviceKind::Output
                        },
                        channels: device_channels(&buf, is_source),
                        is_default: false,
                    }))
                })?;
            found.extend(one);
        }
        Ok(found)
    });
    if let Ok(found) = found {
        out.extend(found);
    }
    out
}

/// Apps that can be captured on their own (PLAN §6.3). The list is of launchable *installed*
/// apps: which one is playing right now is not available without a hidden API.
pub(crate) fn list_apps() -> Vec<AppInfo> {
    let raw = with_jni(|env, ctx| {
        let own_uid = env
            .call_static_method("android/os/Process", "myUid", "()I", &[])?
            .i()?;
        let pm = env
            .call_method(
                ctx,
                "getPackageManager",
                "()Landroid/content/pm/PackageManager;",
                &[],
            )?
            .l()?;
        let list = env
            .call_method(
                &pm,
                "getInstalledApplications",
                "(I)Ljava/util/List;",
                &[JValue::Int(0)],
            )?
            .l()?;
        let n = env.call_method(&list, "size", "()I", &[])?.i()?;
        let mut apps = Vec::new();
        for i in 0..n {
            let one = env.with_local_frame(16, |env| -> jni::errors::Result<RawApp> {
                let ai = env
                    .call_method(&list, "get", "(I)Ljava/lang/Object;", &[JValue::Int(i)])?
                    .l()?;
                let uid = env.get_field(&ai, "uid", "I")?.i()?;
                let package_obj = env
                    .get_field(&ai, "packageName", "Ljava/lang/String;")?
                    .l()?;
                let package: String = env.get_string(&JString::from(package_obj))?.into();
                let label_cs = env
                    .call_method(
                        &ai,
                        "loadLabel",
                        "(Landroid/content/pm/PackageManager;)Ljava/lang/CharSequence;",
                        &[JValue::Object(&pm)],
                    )?
                    .l()?;
                let label = text(env, &label_cs)?;
                let pkg_j = env.new_string(&package)?;
                let launch = env
                    .call_method(
                        &pm,
                        "getLaunchIntentForPackage",
                        "(Ljava/lang/String;)Landroid/content/Intent;",
                        &[JValue::Object(&pkg_j)],
                    )?
                    .l()?;
                Ok(RawApp {
                    uid,
                    label,
                    package,
                    launchable: !launch.is_null(),
                })
            })?;
            apps.push(one);
        }
        Ok((own_uid, apps))
    });
    match raw {
        Ok((own_uid, apps)) => shape_app_list(apps, own_uid),
        Err(_) => Vec::new(),
    }
}
