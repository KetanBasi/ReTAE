//! A scripted daemon for the tests: a `ControlApi` that keeps a snapshot, records the commands it
//! accepted, and can be switched off or told to refuse. Compiled for tests only.

use crate::model::apply_command;
use app_core::ControlApi;
use app_core::control::ControlResult;
use app_core::profile::{default_bus, default_strip};
use app_core::types::{
    BusState, Command, ControlError, DiscoveryState, LanRxLevels, LinkState, MeterLevels,
    MeterSnapshot, PolicyState, ProbeReport, ProbeRequest, StateSnapshot, StripState,
};
use app_core::ui_state::UiLayout;
use audio_types::{MAX_BUSES, MAX_STRIPS};
use platform_io::{AppInfo, Capabilities, DeviceInfo, DeviceKind};
use std::sync::{Arc, Mutex, MutexGuard};

pub struct FakeDaemon {
    inner: Mutex<Inner>,
}

struct Inner {
    snapshot: StateSnapshot,
    meters: MeterSnapshot,
    devices: Vec<DeviceInfo>,
    apps: Vec<AppInfo>,
    commands: Vec<Command>,
    down: bool,
    refuse: Option<String>,
    attempts: u32,
}

pub fn silent_levels() -> MeterLevels {
    MeterLevels {
        peak_db: [-120.0; 8],
        rms_db: [-120.0; 8],
        clipping: false,
    }
}

pub fn sample_snapshot() -> StateSnapshot {
    StateSnapshot {
        protocol: app_core::types::PROTOCOL_VERSION,
        name: "TestRouter".into(),
        backend: "mock".into(),
        capabilities: Capabilities {
            hardware_input: true,
            hardware_output: true,
            device_loopback: true,
            process_loopback: true,
            virtual_inputs: 2,
            virtual_mics: 2,
        },
        rx_port: 6980,
        strips: (0..MAX_STRIPS)
            .map(|i| StripState {
                config: default_strip(i),
                link: LinkState::Idle,
                stream: None,
            })
            .collect(),
        buses: (0..MAX_BUSES)
            .map(|b| BusState {
                config: default_bus(b),
                link: LinkState::Idle,
                stream: None,
                tx: None,
            })
            .collect(),
        policy: PolicyState {
            accept_any: false,
            allowed: Vec::new(),
            trust_discovered: false,
            effective: Vec::new(),
        },
        beacon: false,
        peers: Vec::new(),
        discovery: DiscoveryState::Off,
        discovered: Vec::new(),
        seen_senders: Vec::new(),
        profile: None,
        profiles: Vec::new(),
        ui: UiLayout::default(),
    }
}

pub fn sample_meters() -> MeterSnapshot {
    MeterSnapshot {
        strips: vec![silent_levels(); MAX_STRIPS],
        buses: vec![silent_levels(); MAX_BUSES],
        lan_rx: vec![
            LanRxLevels {
                buffer_ms: 0.0,
                target_ms: 0.0,
                drift_ppm: 0.0,
                packets_lost: 0,
                packets_late: 0,
                underruns: 0,
                last_packet_ms_ago: u32::MAX,
            };
            MAX_STRIPS
        ],
        any_audio_active: false,
        engine: app_core::types::EngineStatus::default(),
    }
}

impl FakeDaemon {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                snapshot: sample_snapshot(),
                meters: sample_meters(),
                devices: vec![
                    DeviceInfo {
                        id: "mock-in-1".into(),
                        name: "Mock Microphone".into(),
                        kind: DeviceKind::Input,
                        channels: 2,
                        is_default: true,
                    },
                    DeviceInfo {
                        id: "mock-out-1".into(),
                        name: "Mock Speakers".into(),
                        kind: DeviceKind::Output,
                        channels: 2,
                        is_default: true,
                    },
                ],
                apps: vec![AppInfo {
                    pid: 4242,
                    name: "music-player".into(),
                    playing: true,
                }],
                commands: Vec::new(),
                down: false,
                refuse: None,
                attempts: 0,
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// What the connector of a `Session` calls.
    pub fn connect(self: &Arc<Self>) -> ControlResult<Arc<dyn ControlApi>> {
        let mut g = self.lock();
        g.attempts += 1;
        if g.down {
            return Err(ControlError::Disconnected("cannot reach routerd".into()));
        }
        Ok(Arc::clone(self) as Arc<dyn ControlApi>)
    }

    pub fn set_down(&self, down: bool) {
        self.lock().down = down;
    }

    pub fn refuse_commands(&self, why: &str) {
        self.lock().refuse = Some(why.into());
    }

    pub fn commands(&self) -> Vec<Command> {
        self.lock().commands.clone()
    }

    pub fn clear_commands(&self) {
        self.lock().commands.clear();
    }

    pub fn connect_attempts(&self) -> u32 {
        self.lock().attempts
    }

    pub fn snapshot_now(&self) -> StateSnapshot {
        self.lock().snapshot.clone()
    }

    pub fn snapshot_now_with(&self, f: impl FnOnce(&mut StateSnapshot)) -> StateSnapshot {
        let mut s = self.snapshot_now();
        f(&mut s);
        s
    }

    /// Change what the daemon reports, as if the engine had done it.
    pub fn edit(&self, f: impl FnOnce(&mut StateSnapshot)) {
        f(&mut self.lock().snapshot);
    }

    pub fn set_meters(&self, m: MeterSnapshot) {
        self.lock().meters = m;
    }

    /// Loud audio on strip 0 and `any_audio_active`.
    pub fn make_noise(&self, active: bool) {
        let mut g = self.lock();
        g.meters.any_audio_active = active;
        g.meters.strips[0].peak_db = [if active { -6.0 } else { -120.0 }; 8];
        g.meters.strips[0].rms_db = [if active { -12.0 } else { -120.0 }; 8];
    }
}

fn gone() -> ControlError {
    ControlError::Disconnected("cannot reach routerd".into())
}

impl ControlApi for FakeDaemon {
    fn command(&self, cmd: Command) -> ControlResult<()> {
        let mut g = self.lock();
        if g.down {
            return Err(gone());
        }
        if let Some(why) = &g.refuse {
            return Err(ControlError::Invalid(why.clone()));
        }
        apply_command(&mut g.snapshot, &cmd);
        // The real daemon also opens what it is told to open. The fake just says it worked.
        match &cmd {
            Command::StripSource {
                strip,
                source,
                channels,
            } => {
                if let Some(s) = g.snapshot.strips.get_mut(usize::from(*strip)) {
                    s.config.source = *source;
                    s.config.channels = *channels;
                    s.link = if matches!(source, audio_types::SourceEndpoint::Unassigned) {
                        LinkState::Idle
                    } else {
                        LinkState::Running
                    };
                }
            }
            Command::BusSink {
                bus,
                sink,
                channels,
            } => {
                if let Some(b) = g.snapshot.buses.get_mut(usize::from(*bus)) {
                    b.config.sink = *sink;
                    b.config.channels = *channels;
                    b.link = if matches!(sink, audio_types::SinkEndpoint::Unassigned) {
                        LinkState::Idle
                    } else {
                        LinkState::Running
                    };
                }
            }
            _ => {}
        }
        g.commands.push(cmd);
        Ok(())
    }

    fn snapshot(&self) -> ControlResult<StateSnapshot> {
        let g = self.lock();
        if g.down {
            return Err(gone());
        }
        Ok(g.snapshot.clone())
    }

    fn meters(&self) -> ControlResult<MeterSnapshot> {
        let g = self.lock();
        if g.down {
            return Err(gone());
        }
        Ok(g.meters.clone())
    }

    fn devices(&self) -> ControlResult<Vec<DeviceInfo>> {
        let g = self.lock();
        if g.down {
            return Err(gone());
        }
        Ok(g.devices.clone())
    }

    fn apps(&self) -> ControlResult<Vec<AppInfo>> {
        let g = self.lock();
        if g.down {
            return Err(gone());
        }
        Ok(g.apps.clone())
    }

    fn probe(&self, _request: ProbeRequest) -> ControlResult<ProbeReport> {
        Ok(ProbeReport::default())
    }
}
