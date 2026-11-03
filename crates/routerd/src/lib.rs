//! Desktop engine daemon: owns the engine, network and audio backend; serves IPC.
//!
//! `Daemon::start` is what `main` calls, and what the tests call in-process: one `Daemon` is a
//! complete router (mixer clock + `LocalControl` + IPC server).
//!
//! ```text
//!   router-ui / router-cli --IPC--> IpcServer --> LocalControl --> backend, VBAN sockets
//!                                                      |
//!                                              EngineHandles ----> TimerMaster (mixer clock)
//! ```

#![forbid(unsafe_code)]

use app_core::ipc::{self, IpcError, IpcServer};
use app_core::{ControlApi, DaemonConfig, LocalControl, paths};
use audio_engine::create_engine;
use platform_io::{AudioBackend, MockBackend, MockControl, TimerMaster};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

// SECTION: Options
/// Which audio backend the daemon uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    /// Fake devices on paced threads. Always available.
    Mock,
}

impl FromStr for BackendKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "mock" => Ok(Self::Mock),
            // ? PipeWire arrives in S5, WASAPI in S6; until then they are not selectable.
            other => Err(format!("unknown backend {other:?} (available: mock)")),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    /// Name other machines see.
    pub name: String,
    pub backend: BackendKind,
    /// UDP port of the VBAN receive socket. 0 picks a free one (tests).
    pub rx_port: u16,
    pub discovery: bool,
    /// Suffix of the IPC socket name, so a second daemon can run on the same machine (tests).
    pub instance: Option<String>,
    pub config_dir: PathBuf,
    pub device_target_ms: f32,
    /// Make the mixer clock run this many ppm fast (tests of clock drift).
    pub master_ppm: f64,
    /// Profile to load at start.
    pub profile: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        let cfg = DaemonConfig::default();
        Self {
            name: cfg.name,
            backend: BackendKind::Mock,
            rx_port: cfg.rx_port,
            discovery: false,
            instance: None,
            config_dir: paths::config_dir(),
            device_target_ms: cfg.device_target_ms,
            master_ppm: 0.0,
            profile: None,
        }
    }
}
// !SECTION: Options

// SECTION: Daemon
#[derive(Debug)]
pub enum DaemonError {
    /// Another routerd already answers on the IPC name (the single-instance lock).
    AlreadyRunning(String),
    Start(String),
}

impl core::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::AlreadyRunning(name) => {
                write!(f, "another routerd is already running (IPC name {name:?})")
            }
            Self::Start(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for DaemonError {}

/// A running router. Dropping it shuts everything down in the right order.
// ? Rust drops fields in the order they are declared: first stop answering requests (`ipc`),
// ? then stop the mixer (`master`), and only then close devices and sockets (`control`). The
// ? other order would let a device thread feed a mixer that no longer runs.
pub struct Daemon {
    ipc: Option<IpcServer>,
    master: TimerMaster,
    control: Arc<LocalControl>,
    mock: Option<MockControl>,
    ipc_name: String,
}

impl Daemon {
    pub fn start(options: Options) -> Result<Self, DaemonError> {
        let ipc_name = paths::ipc_name(options.instance.as_deref());

        // The lock comes first: a second daemon must say "already running", not fail on
        // whichever port or device it happens to touch first.
        if ipc::is_running(&ipc_name) {
            return Err(DaemonError::AlreadyRunning(ipc_name));
        }

        let (engine, handles) = create_engine();
        let (backend, mock): (Box<dyn AudioBackend>, Option<MockControl>) = match options.backend {
            BackendKind::Mock => {
                let (backend, control) = MockBackend::new();
                (Box::new(backend), Some(control))
            }
        };
        let control = Arc::new(
            LocalControl::new(
                handles,
                backend,
                DaemonConfig {
                    name: options.name,
                    rx_port: options.rx_port,
                    discovery: options.discovery,
                    device_target_ms: options.device_target_ms,
                    config_dir: options.config_dir,
                },
            )
            .map_err(|e| DaemonError::Start(e.to_string()))?,
        );
        let master = TimerMaster::spawn(engine, options.master_ppm)
            .map_err(|e| DaemonError::Start(format!("cannot start the mixer clock: {e}")))?;

        if let Some(name) = options.profile {
            control
                .command(app_core::Command::LoadProfile { name })
                .map_err(|e| DaemonError::Start(e.to_string()))?;
        }

        let as_dyn: Arc<dyn ControlApi> = control.clone();
        let ipc = IpcServer::start(&ipc_name, Arc::downgrade(&as_dyn)).map_err(|e| match e {
            IpcError::AlreadyRunning => DaemonError::AlreadyRunning(ipc_name.clone()),
            IpcError::Io(e) => DaemonError::Start(format!("cannot open the IPC socket: {e}")),
        })?;

        Ok(Self {
            ipc: Some(ipc),
            master,
            control,
            mock,
            ipc_name,
        })
    }

    /// The in-process control (what a UI linked into the same process would hold).
    pub fn control(&self) -> &Arc<LocalControl> {
        &self.control
    }

    /// Steering handle of the fake devices, present for the mock backend.
    pub fn mock(&self) -> Option<&MockControl> {
        self.mock.as_ref()
    }

    pub fn master(&self) -> &TimerMaster {
        &self.master
    }

    /// The name a client connects to.
    pub fn ipc_name(&self) -> &str {
        &self.ipc_name
    }

    /// Stop answering IPC requests now (also happens on drop).
    pub fn stop_ipc(&mut self) {
        self.ipc = None;
    }
}
// !SECTION: Daemon
