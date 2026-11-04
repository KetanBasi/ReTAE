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
use app_core::lan::{self, LanServer, PairingCode};
use app_core::{ControlApi, DaemonConfig, LocalControl, paths};
use audio_engine::create_engine;
use platform_io::{AudioBackend, MockBackend, MockControl, TimerMaster};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

// SECTION: Options
/// Which audio backend the daemon uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    /// Fake devices on paced threads. Always available.
    Mock,
    /// Linux, PipeWire. Needs the `pipewire` feature and a running server.
    PipeWire,
    /// Linux, ALSA through `cpal`. Needs the `alsa` feature. Hardware devices only.
    Alsa,
    /// Windows, WASAPI. Always built on Windows; needs the Windows audio service running.
    Wasapi,
    /// The best real backend this build has and this machine can reach: WASAPI on Windows,
    /// PipeWire then ALSA on Linux.
    Auto,
}

impl BackendKind {
    /// What `routerd` uses when no `--backend` is given: `auto` if the build has a real
    /// backend, otherwise `mock`.
    pub fn preferred() -> Self {
        if cfg!(any(
            windows,
            all(
                target_os = "linux",
                any(feature = "pipewire", feature = "alsa")
            )
        )) {
            Self::Auto
        } else {
            Self::Mock
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mock => "mock",
            Self::PipeWire => "pipewire",
            Self::Alsa => "alsa",
            Self::Wasapi => "wasapi",
            Self::Auto => "auto",
        }
    }
}

impl FromStr for BackendKind {
    type Err = String;

    // ? Every name parses in every build; whether the build can actually *use* it is checked
    // ? when the daemon starts, so the message can say which feature to turn on.
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "mock" => Ok(Self::Mock),
            "pipewire" => Ok(Self::PipeWire),
            "alsa" => Ok(Self::Alsa),
            "wasapi" => Ok(Self::Wasapi),
            "auto" => Ok(Self::Auto),
            other => Err(format!(
                "unknown backend {other:?} (mock, pipewire, alsa, wasapi or auto)"
            )),
        }
    }
}

/// Open the backend `kind` names. The mock's steering handle comes back with it.
fn make_backend(
    kind: BackendKind,
) -> Result<(Box<dyn AudioBackend>, Option<MockControl>), DaemonError> {
    match kind {
        BackendKind::Mock => {
            let (backend, control) = MockBackend::new();
            Ok((Box::new(backend), Some(control)))
        }
        BackendKind::PipeWire => pipewire_backend().map(|b| (b, None)),
        BackendKind::Alsa => alsa_backend().map(|b| (b, None)),
        BackendKind::Wasapi => wasapi_backend().map(|b| (b, None)),
        BackendKind::Auto => {
            // ? On Windows there is one choice. On Linux, PipeWire first: it can also make
            // ? virtual devices and capture one program; ALSA is the fallback for a machine
            // ? without a PipeWire server. Every error is kept, so a person sees why nothing
            // ? worked.
            if cfg!(windows) {
                return wasapi_backend().map(|b| (b, None));
            }
            match pipewire_backend() {
                Ok(b) => Ok((b, None)),
                Err(pw) => match alsa_backend() {
                    Ok(b) => Ok((b, None)),
                    Err(alsa) => Err(DaemonError::Start(format!(
                        "no audio backend available (pipewire: {pw}; alsa: {alsa})"
                    ))),
                },
            }
        }
    }
}

#[cfg(windows)]
fn wasapi_backend() -> Result<Box<dyn AudioBackend>, DaemonError> {
    platform_io::WasapiBackend::connect(platform_io::WasapiOptions::default())
        .map(|b| Box::new(b) as Box<dyn AudioBackend>)
        .map_err(|e| DaemonError::Start(format!("cannot reach Windows audio: {e}")))
}

#[cfg(not(windows))]
fn wasapi_backend() -> Result<Box<dyn AudioBackend>, DaemonError> {
    Err(DaemonError::Start(
        "WASAPI is only available on Windows".into(),
    ))
}

#[cfg(all(target_os = "linux", feature = "pipewire"))]
fn pipewire_backend() -> Result<Box<dyn AudioBackend>, DaemonError> {
    platform_io::PipeWireBackend::connect(platform_io::PipeWireOptions::default())
        .map(|b| Box::new(b) as Box<dyn AudioBackend>)
        .map_err(|e| DaemonError::Start(format!("cannot reach PipeWire: {e}")))
}

#[cfg(not(all(target_os = "linux", feature = "pipewire")))]
fn pipewire_backend() -> Result<Box<dyn AudioBackend>, DaemonError> {
    Err(DaemonError::Start(
        "built without PipeWire (Linux only; rebuild with --features pipewire)".into(),
    ))
}

#[cfg(all(target_os = "linux", feature = "alsa"))]
fn alsa_backend() -> Result<Box<dyn AudioBackend>, DaemonError> {
    platform_io::AlsaBackend::connect(platform_io::AlsaOptions::default())
        .map(|b| Box::new(b) as Box<dyn AudioBackend>)
        .map_err(|e| DaemonError::Start(format!("cannot open ALSA: {e}")))
}

#[cfg(not(all(target_os = "linux", feature = "alsa")))]
fn alsa_backend() -> Result<Box<dyn AudioBackend>, DaemonError> {
    Err(DaemonError::Start(
        "built without ALSA (Linux only; rebuild with --features alsa)".into(),
    ))
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
    /// Where bus recordings are written.
    pub recordings_dir: PathBuf,
    pub record_format: app_core::recorder::RecordFormat,
    /// TCP port for control from other machines (encrypted, paired devices only). `None` = off.
    pub lan_control: Option<u16>,
    /// Open a pairing window as soon as the LAN service is up (needs `lan_control`).
    pub pair: bool,
}

/// How long a pairing window stays open.
pub const PAIRING_WINDOW: std::time::Duration = std::time::Duration::from_secs(300);

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
            recordings_dir: cfg.recordings_dir,
            record_format: cfg.record_format,
            lan_control: None,
            pair: false,
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
    lan: Option<LanServer>,
    pairing_code: Option<PairingCode>,
    ipc: Option<IpcServer>,
    master: TimerMaster,
    control: Arc<LocalControl>,
    mock: Option<MockControl>,
    backend_name: &'static str,
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

        if options.pair && options.lan_control.is_none() {
            return Err(DaemonError::Start(
                "--pair needs --lan-control (there is nothing to pair with otherwise)".into(),
            ));
        }

        let (engine, handles) = create_engine();
        let (backend, mock) = make_backend(options.backend)?;
        let backend_name = backend.name();
        let name = options.name.clone();
        let config_dir = options.config_dir.clone();
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
                    recordings_dir: options.recordings_dir,
                    record_format: options.record_format,
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

        // SECTION: LAN control
        let mut lan = None;
        let mut pairing_code = None;
        if let Some(port) = options.lan_control {
            let start = || -> std::io::Result<LanServer> {
                let identity =
                    lan::Identity::load_or_create(&paths::lan_identity_path(&config_dir))?;
                let book = lan::PeerBook::load(&paths::lan_peers_path(&config_dir))?;
                LanServer::start(
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port),
                    &name,
                    identity,
                    Arc::new(Mutex::new(book)),
                    Arc::downgrade(&as_dyn),
                )
            };
            let server = start()
                .map_err(|e| DaemonError::Start(format!("cannot start LAN control: {e}")))?;
            if options.pair {
                pairing_code = Some(server.open_pairing(PAIRING_WINDOW).map_err(|e| {
                    DaemonError::Start(format!("cannot open a pairing window: {e}"))
                })?);
            }
            lan = Some(server);
        }
        // !SECTION: LAN control

        Ok(Self {
            lan,
            pairing_code,
            ipc: Some(ipc),
            master,
            control,
            mock,
            backend_name,
            ipc_name,
        })
    }

    /// Which backend `--backend auto` ended up with (`"pipewire"`, `"alsa"`, `"mock"`).
    pub fn backend_name(&self) -> &'static str {
        self.backend_name
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

    /// Where LAN control listens, when it is on.
    pub fn lan_addr(&self) -> Option<SocketAddr> {
        self.lan.as_ref().map(LanServer::local_addr)
    }

    /// The code of the pairing window opened at start (`--pair`), to show to a person.
    pub fn pairing_code(&self) -> Option<&PairingCode> {
        self.pairing_code.as_ref()
    }

    /// Open (or renew) a pairing window. `None` if LAN control is off.
    pub fn open_pairing(&self) -> Option<std::io::Result<PairingCode>> {
        self.lan.as_ref().map(|l| l.open_pairing(PAIRING_WINDOW))
    }

    /// Stop answering IPC requests now (also happens on drop).
    pub fn stop_ipc(&mut self) {
        self.ipc = None;
    }
}
// !SECTION: Daemon
