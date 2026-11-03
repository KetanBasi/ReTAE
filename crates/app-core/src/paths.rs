//! Where profiles live and what the daemon's local socket is called.

use std::path::{Path, PathBuf};

/// Environment variable that moves the config directory (tests, portable installs).
pub const CONFIG_DIR_ENV: &str = "RETAE_CONFIG_DIR";

/// Default local-socket name of the daemon. A second daemon on the same machine (tests) adds
/// an instance suffix.
pub const IPC_NAME: &str = "retae-routerd";

const PROFILE_EXT: &str = "toml";
const MAX_PROFILE_NAME: usize = 64;

/// The per-user config directory: `$RETAE_CONFIG_DIR`, else `%APPDATA%\retae` on Windows,
/// else `$XDG_CONFIG_HOME/retae` or `~/.config/retae`.
// ? No `dirs` crate: three environment variables are all this needs, and fewer dependencies is
// ? less to audit.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = env_path(CONFIG_DIR_ENV) {
        return dir;
    }
    if cfg!(windows) {
        if let Some(dir) = env_path("APPDATA") {
            return dir.join("retae");
        }
    }
    if let Some(dir) = env_path("XDG_CONFIG_HOME") {
        return dir.join("retae");
    }
    if let Some(home) = env_path("HOME").or_else(|| env_path("USERPROFILE")) {
        return home.join(".config").join("retae");
    }
    PathBuf::from(".retae")
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

pub fn profiles_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("profiles")
}

pub fn profile_path(config_dir: &Path, name: &str) -> PathBuf {
    profiles_dir(config_dir).join(format!("{name}.{PROFILE_EXT}"))
}

pub fn profile_ext() -> &'static str {
    PROFILE_EXT
}

/// A profile name becomes a file name, so it may not contain anything that could leave the
/// profiles directory.
// !! A name like `../../.ssh/authorized_keys` must never reach `Path::join`. Only letters,
// !! digits, space, `-`, `_` and `.` inside are allowed, and it cannot start with a dot.
pub fn validate_profile_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_PROFILE_NAME {
        return Err(format!(
            "a profile name has 1 to {MAX_PROFILE_NAME} characters"
        ));
    }
    if name.starts_with('.') || name.ends_with(' ') || name.starts_with(' ') {
        return Err(
            "a profile name cannot start with a dot or a space, or end with a space".into(),
        );
    }
    let ok = |c: char| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.');
    if !name.chars().all(ok) {
        return Err("a profile name may only use letters, digits, space, - _ and .".into());
    }
    Ok(())
}

/// The socket name for `instance` (None = the one real daemon of this user).
pub fn ipc_name(instance: Option<&str>) -> String {
    match instance {
        Some(i) if !i.is_empty() => format!("{IPC_NAME}-{i}"),
        _ => IPC_NAME.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_names_cannot_escape_the_directory() {
        for bad in [
            "",
            ".",
            "..",
            "../x",
            "a/b",
            "a\\b",
            ".hidden",
            " lead",
            "trail ",
            "nul\0",
            "semi;colon",
            &"x".repeat(65),
        ] {
            assert!(
                validate_profile_name(bad).is_err(),
                "{bad:?} must be refused"
            );
        }
        for good in ["default", "Living room", "stream-2_v1.0", "Küche"] {
            assert!(
                validate_profile_name(good).is_ok(),
                "{good:?} must be allowed"
            );
        }
    }

    #[test]
    fn instance_names_make_distinct_sockets() {
        assert_eq!(ipc_name(None), IPC_NAME);
        assert_eq!(ipc_name(Some("")), IPC_NAME);
        assert_ne!(ipc_name(Some("a")), ipc_name(Some("b")));
    }
}
