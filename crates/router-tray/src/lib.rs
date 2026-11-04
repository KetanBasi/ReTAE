//! Tray helper (PLAN.md §11 S6): keeps the engine and the UI process alive on request.
//!
//! The tray owns no audio. It starts `routerd` if no engine answers, and starts or stops
//! `router-ui` from its menu. The platform-independent parts live here so they are tested on
//! every OS; the icon and message loop are in `win.rs` (Windows only).

// ? `deny`, not `forbid`: the Windows message loop in `win` needs three FFI calls and opts in.
#![deny(unsafe_code)]

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

#[cfg(windows)]
#[allow(unsafe_code)]
pub mod win;

/// The program `name` installed next to the running executable (`router-ui` -> `router-ui.exe`
/// on Windows). Programs of this project are shipped in one folder.
pub fn sibling_exe(current: &Path, name: &str) -> PathBuf {
    current.with_file_name(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

/// One child process the tray may start and stop.
#[derive(Debug, Default)]
pub struct ChildSlot {
    child: Option<Child>,
}

impl ChildSlot {
    /// An empty slot. `const` so it can initialise a `static`.
    pub const fn new() -> Self {
        Self { child: None }
    }

    /// True while the child we started is still running (an exited child is forgotten).
    pub fn is_running(&mut self) -> bool {
        match self.child.as_mut().map(Child::try_wait) {
            Some(Ok(None)) => true,
            Some(_) => {
                self.child = None;
                false
            }
            None => false,
        }
    }

    /// Start the child unless it already runs. Returns whether a new process was started.
    pub fn start(&mut self, mut command: Command) -> io::Result<bool> {
        if self.is_running() {
            return Ok(false);
        }
        self.child = Some(command.spawn()?);
        Ok(true)
    }

    /// Stop the child. There is no graceful path yet (no shutdown command in the IPC, TODO C5),
    /// so the process is ended and the OS closes its devices and sockets.
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    pub fn was_started_here(&self) -> bool {
        self.child.is_some()
    }
}

impl Drop for ChildSlot {
    // ? Quitting the tray must not kill a UI the user is looking at, nor an engine that streams.
    // ? So dropping forgets the child instead of killing it; only `stop` ends a process.
    fn drop(&mut self) {
        self.child = None;
    }
}

/// Is an engine answering on the default IPC name?
pub fn engine_running(instance: Option<&str>) -> bool {
    app_core::ipc::is_running(&app_core::paths::ipc_name(instance))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn siblings_sit_next_to_the_current_exe() {
        let p = sibling_exe(Path::new("/opt/retae/router-tray"), "router-ui");
        assert_eq!(p.parent(), Some(Path::new("/opt/retae")));
        let file = p.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(file, format!("router-ui{}", std::env::consts::EXE_SUFFIX));
    }

    #[cfg(unix)]
    #[test]
    fn a_child_is_started_once_and_can_be_stopped() {
        let mut slot = ChildSlot::default();
        assert!(!slot.is_running());
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        assert!(slot.start(cmd).unwrap(), "first start launches it");
        assert!(slot.is_running());
        let mut again = Command::new("sleep");
        again.arg("30");
        assert!(
            !slot.start(again).unwrap(),
            "a running child is not doubled"
        );
        slot.stop();
        assert!(!slot.is_running());
        assert!(!slot.was_started_here());
    }

    #[cfg(unix)]
    #[test]
    fn a_child_that_exited_can_be_started_again() {
        let mut slot = ChildSlot::default();
        assert!(slot.start(Command::new("true")).unwrap());
        for _ in 0..100 {
            if !slot.is_running() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!slot.is_running());
        assert!(slot.start(Command::new("true")).unwrap());
        slot.stop();
    }

    #[test]
    fn a_missing_program_is_an_error_not_a_panic() {
        let mut slot = ChildSlot::default();
        assert!(
            slot.start(Command::new("/definitely/not/a/program"))
                .is_err()
        );
        assert!(!slot.is_running());
    }
}
