//! The router engine, inside the Android app's own process.
//!
//! Desktop runs the engine in `routerd` and the UI talks to it over IPC. On a phone there is one
//! process, so the UI holds a `LocalControl` directly (the same type `routerd` serves over IPC,
//! CLAUDE.md law 5) and this module builds it exactly the way `routerd::Daemon::start` does,
//! minus the IPC socket.

use app_core::{ControlApi, DaemonConfig, LocalControl};
use audio_engine::create_engine;
use platform_io::{AndroidBackend, AndroidOptions, TimerMaster};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

/// A running engine. Dropping it stops the mixer first and closes devices and sockets after.
// ? Rust drops fields in declaration order: `master` (the mixer clock) goes before `control`
// ? (devices, sockets), so no device thread feeds a mixer that has already stopped.
struct Engine {
    // ? Never read: it is here to be dropped (which stops the mixer thread), in this order.
    _master: TimerMaster,
    control: Arc<LocalControl>,
}

static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);

/// Start the engine if it is not running, and return its control. `config_dir` holds profiles.
pub fn start(name: &str, config_dir: PathBuf) -> Result<Arc<dyn ControlApi>, String> {
    let mut slot = ENGINE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(e) = slot.as_ref() {
        return Ok(e.control.clone());
    }
    // ? Network threads ask for real-time priority when they start, so the hook must be in
    // ? place before the first socket is opened. On Android this is a JNI call to
    // ? `Process.setThreadPriority(THREAD_PRIORITY_URGENT_AUDIO)`.
    net_vban::hook::set_thread_start_hook(|_role| {
        let _ = platform_io::rt::promote_current_thread(48, 48_000);
    });

    let (engine, handles) = create_engine();
    let backend = AndroidBackend::connect(AndroidOptions::default())
        .map_err(|e| format!("cannot reach Android audio: {e}"))?;
    let control = Arc::new(
        LocalControl::new(
            handles,
            Box::new(backend),
            DaemonConfig {
                name: name.to_owned(),
                config_dir,
                ..DaemonConfig::default()
            },
        )
        .map_err(|e| e.to_string())?,
    );
    let master = TimerMaster::spawn(engine, 0.0)
        .map_err(|e| format!("cannot start the mixer clock: {e}"))?;
    let handle = control.clone();
    *slot = Some(Engine {
        _master: master,
        control,
    });
    Ok(handle)
}

/// The control of the running engine, if there is one.
pub fn control() -> Option<Arc<dyn ControlApi>> {
    let slot = ENGINE.lock().unwrap_or_else(PoisonError::into_inner);
    slot.as_ref()
        .map(|e| e.control.clone() as Arc<dyn ControlApi>)
}

/// Stop the engine and close every device and socket.
pub fn stop() {
    let engine = ENGINE.lock().unwrap_or_else(PoisonError::into_inner).take();
    // ? Dropped outside the lock: closing devices can take a moment.
    drop(engine);
}
