//! Starting `routerd` from the "daemon not running" banner.
//!
//! The daemon is a separate program that outlives this window (the UI can be closed while audio
//! keeps flowing), so it is started detached: no console, no waiting, and closing the UI does not
//! end it.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// `routerd` next to the running executable if it is there, else whatever `PATH` finds.
pub fn daemon_program(current_exe: Option<&Path>) -> PathBuf {
    let name = format!("routerd{}", std::env::consts::EXE_SUFFIX);
    if let Some(dir) = current_exe.and_then(Path::parent) {
        let beside = dir.join(&name);
        if beside.is_file() {
            return beside;
        }
    }
    PathBuf::from(name)
}

pub fn daemon_args(instance: Option<&str>) -> Vec<String> {
    match instance {
        Some(i) if !i.is_empty() => vec!["--instance".into(), i.into()],
        _ => Vec::new(),
    }
}

pub fn start(instance: Option<&str>) -> io::Result<()> {
    let exe = std::env::current_exe().ok();
    let mut cmd = Command::new(daemon_program(exe.as_deref()));
    cmd.args(daemon_args(instance))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: the daemon is a console program; it must not pop up a console.
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn()?;
    // ? Reap it in the background. Dropping a `Child` does not kill it, but on Unix an unreaped
    // ? child lingers as a zombie until this program exits.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_instance_is_passed_on() {
        assert!(daemon_args(None).is_empty());
        assert!(daemon_args(Some("")).is_empty());
        assert_eq!(daemon_args(Some("test")), ["--instance", "test"]);
    }

    #[test]
    fn the_daemon_is_looked_for_beside_the_ui_first() {
        let dir = std::env::temp_dir().join(format!("retae-ui-daemon-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ui = dir.join(format!("router-ui{}", std::env::consts::EXE_SUFFIX));
        // Not there: fall back to the bare name for PATH lookup.
        assert_eq!(
            daemon_program(Some(&ui)),
            PathBuf::from(format!("routerd{}", std::env::consts::EXE_SUFFIX))
        );
        let beside = dir.join(format!("routerd{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&beside, b"").unwrap();
        assert_eq!(daemon_program(Some(&ui)), beside);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
