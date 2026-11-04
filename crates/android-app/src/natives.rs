//! The table of Java `native` methods that Rust registers in `JNI_OnLoad`.
//!
//! * One list per Java class: the name and the JNI signature. The Android glue pairs each name
//!   with a Rust function; the host test below reads the Java sources and checks that this table
//!   and the `native` declarations in them agree, so a typo is a failed test and not an
//!   `UnsatisfiedLinkError` on a phone.

pub struct NativeClass {
    /// The class in JNI form (slashes).
    pub class: &'static str,
    /// `(method name, JNI signature)`.
    pub methods: &'static [(&'static str, &'static str)],
}

pub const SERVICE: NativeClass = NativeClass {
    class: "dev/f/lanrouter/AudioEngineService",
    methods: &[
        ("nativeOnCreate", "()V"),
        ("nativeOnStartCommand", "(Landroid/content/Intent;I)I"),
        ("nativeOnDestroy", "()V"),
    ],
};

pub const CONSENT: NativeClass = NativeClass {
    class: "dev/f/lanrouter/ProjectionConsentActivity",
    methods: &[
        ("nativeOnCreate", "(Landroid/content/Intent;)V"),
        ("nativeOnActivityResult", "(IILandroid/content/Intent;)V"),
        (
            "nativeOnRequestPermissionsResult",
            "(I[Ljava/lang/String;[I)V",
        ),
        ("nativeOnDestroy", "()V"),
    ],
};

pub const STOP_CALLBACK: NativeClass = NativeClass {
    class: "dev/f/lanrouter/ProjectionStopCallback",
    methods: &[("nativeOnStop", "()V")],
};

pub const ALL: [&NativeClass; 3] = [&SERVICE, &CONSENT, &STOP_CALLBACK];

/// The Java entry in `Intent` extras / actions that the glue and the manifest agree on.
pub const ACTION_START_MIC: &str = "dev.f.lanrouter.action.START_MIC";
pub const ACTION_START_PROJECTION: &str = "dev.f.lanrouter.action.START_PROJECTION";
pub const ACTION_STOP: &str = "dev.f.lanrouter.action.STOP";
/// The consent Activity does one of these, chosen by its start intent.
pub const ACTION_ASK_PROJECTION: &str = "dev.f.lanrouter.action.ASK_PROJECTION";
pub const ACTION_ASK_PERMISSIONS: &str = "dev.f.lanrouter.action.ASK_PERMISSIONS";

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    const SERVICE_JAVA: &str =
        include_str!("../resources/android/java/dev/f/lanrouter/AudioEngineService.java");
    const CONSENT_JAVA: &str =
        include_str!("../resources/android/java/dev/f/lanrouter/ProjectionConsentActivity.java");
    const STOP_JAVA: &str =
        include_str!("../resources/android/java/dev/f/lanrouter/ProjectionStopCallback.java");

    /// Java type text → JNI type text. Only the types the shim uses.
    fn jni_type(java: &str) -> String {
        let java = java.trim();
        if let Some(elem) = java.strip_suffix("[]") {
            return format!("[{}", jni_type(elem));
        }
        match java {
            "void" => "V".into(),
            "int" => "I".into(),
            "boolean" => "Z".into(),
            "long" => "J".into(),
            "String" => "Ljava/lang/String;".into(),
            "Intent" => "Landroid/content/Intent;".into(),
            "Bundle" => "Landroid/os/Bundle;".into(),
            other => panic!("jni_type: add `{other}` to the test"),
        }
    }

    /// Every `native` declaration of a Java file as `(name, signature)`.
    fn declared(source: &str) -> BTreeSet<(String, String)> {
        let mut out = BTreeSet::new();
        // ? A declaration may be split over lines; join until the `;`.
        let flat = source.replace('\n', " ");
        for decl in flat.split(';') {
            let Some(at) = decl.find(" native ") else {
                continue;
            };
            let decl = &decl[at + " native ".len()..];
            let (ret_and_name, params) = decl.split_once('(').expect("`(` in native decl");
            let params = params.trim_end().trim_end_matches(')');
            let (ret, name) = ret_and_name.trim().rsplit_once(' ').expect("return type");
            let mut sig = String::from("(");
            for p in params.split(',').filter(|p| !p.trim().is_empty()) {
                let (ty, _name) = p.trim().rsplit_once(' ').expect("param type");
                sig.push_str(&jni_type(ty));
            }
            sig.push(')');
            sig.push_str(&jni_type(ret));
            out.insert((name.trim().to_string(), sig));
        }
        out
    }

    fn table(class: &NativeClass) -> BTreeSet<(String, String)> {
        class
            .methods
            .iter()
            .map(|(n, s)| ((*n).to_string(), (*s).to_string()))
            .collect()
    }

    #[test]
    fn the_rust_table_matches_the_java_native_declarations() {
        assert_eq!(declared(SERVICE_JAVA), table(&SERVICE));
        assert_eq!(declared(CONSENT_JAVA), table(&CONSENT));
        assert_eq!(declared(STOP_JAVA), table(&STOP_CALLBACK));
    }

    #[test]
    fn every_java_method_is_a_one_line_forward() {
        // The convention: no logic in Java. Every non-native method body is one statement
        // that calls a `native`, apart from the lifecycle `super` calls.
        for (name, src) in [
            ("service", SERVICE_JAVA),
            ("consent", CONSENT_JAVA),
            ("stop", STOP_JAVA),
        ] {
            for forbidden in ["if (", "for (", "while (", "switch (", "new Thread"] {
                assert!(
                    !src.contains(forbidden),
                    "{name}: Java must not contain `{forbidden}`"
                );
            }
        }
    }

    #[test]
    fn the_manifest_declares_what_the_java_defines() {
        let manifest = include_str!("../resources/android/AndroidManifest.xml.template");
        for class in ALL {
            let dotted = class.class.replace('/', ".");
            // The stop callback is not a manifest component.
            if class.class.ends_with("ProjectionStopCallback") {
                continue;
            }
            assert!(manifest.contains(&dotted), "manifest lacks {dotted}");
        }
        assert!(manifest.contains("foregroundServiceType=\"microphone|mediaProjection\""));
        for permission in [
            "RECORD_AUDIO",
            "FOREGROUND_SERVICE",
            "FOREGROUND_SERVICE_MICROPHONE",
            "FOREGROUND_SERVICE_MEDIA_PROJECTION",
            "POST_NOTIFICATIONS",
            "WAKE_LOCK",
            "ACCESS_WIFI_STATE",
            "CHANGE_WIFI_MULTICAST_STATE",
            "INTERNET",
        ] {
            assert!(
                manifest.contains(&format!("android.permission.{permission}\"")),
                "manifest lacks {permission}"
            );
        }
        for token in [
            "{package_id}",
            "{label}",
            "{class_name}",
            "{min_sdk_version}",
            "{target_sdk_version}",
            "{version_code}",
            "{version_name}",
            "{debuggable}",
        ] {
            assert!(manifest.contains(token), "manifest lacks token {token}");
        }
        // Android 10 is the floor.
        assert!(!manifest.contains("minSdkVersion=\"2"));
    }
}
