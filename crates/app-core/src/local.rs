//! `LocalControl`: the control plane that owns the audio backend, the sockets and the engine's
//! command queue (CLAUDE.md law 1: devices and sockets are opened here, wired to a slot of the
//! pre-allocated port pool, then attached to the engine with `Attach*{slot}`).
//!
//! The mixer itself is started separately (a device callback or `platform_io::TimerMaster`);
//! this type only talks to it through `EngineHandles`.

use crate::control::{ControlApi, ControlResult};
use crate::paths;
use crate::probe;
use crate::profile::{self, NetworkProfile, Profile, default_bus, default_strip};
use crate::types::{
    BusState, Command, ControlError, DiscoveredState, DiscoveryState, EngineStatus, LanRxLevels,
    LinkState, MeterLevels, MeterSnapshot, PROTOCOL_VERSION, PeerState, PolicyState, ProbeReport,
    ProbeRequest, SeenSenderState, StateSnapshot, StripState, TxState,
};
use crate::ui_state::UiLayout;
use audio_engine::{
    ControlPorts, EngineHandles, EngineStats, InputPort, OutputPort, sanitize_bands, sanitize_dsp,
};
use audio_types::{
    BusConfig, BusId, ClockDomain, EngineCommand, EqBand, FixedStr, MAX_BUSES, MAX_CHANNELS,
    MAX_STRIPS, PortSlot, RouteSend, SinkEndpoint, SourceEndpoint, StripConfig, StripDsp, StripId,
    Telemetry, load_f32,
};
use net_vban::discovery::{Advert, Discovery};
use net_vban::rx::{RxPolicy, RxSocket, StreamKey};
use net_vban::service::{Identity, Service, TextCommand};
use net_vban::stream::{StreamCounters, StreamRx};
use net_vban::tx::TxStream;
use platform_io::{AppInfo, AudioBackend, AudioStream, Capabilities, ClosedPort, DeviceInfo};
use rtrb::Producer;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const GAIN_MIN_DB: f32 = -200.0;
const GAIN_MAX_DB: f32 = 24.0;
/// How long a command waits for room in the engine's queue (the mixer drains it each block).
const QUEUE_WAIT: Duration = Duration::from_millis(500);
const HOUSEKEEPING_TICK: Duration = Duration::from_millis(20);
const DISCOVERY_EVERY: Duration = Duration::from_millis(500);
/// Peers answer one ping per 250 ms each; stay a little above that.
const PING_SPACING: Duration = Duration::from_millis(300);
const MAX_PINGS: u8 = 32;
const MAX_CLICKS: u8 = 50;

// SECTION: Configuration
/// How the daemon identifies itself and where it keeps things.
#[derive(Clone, Debug)]
pub struct DaemonConfig {
    /// Shown to other machines (mDNS name, VBAN-SERVICE device name).
    pub name: String,
    /// UDP port of the VBAN receive socket. 0 picks a free one (tests).
    pub rx_port: u16,
    /// Announce and browse over mDNS.
    pub discovery: bool,
    /// Jitter-buffer target for hardware devices that are not the master clock.
    pub device_target_ms: f32,
    pub config_dir: PathBuf,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            name: "ReTAE".to_owned(),
            rx_port: audio_types::VBAN_DEFAULT_PORT,
            discovery: false,
            device_target_ms: 8.0,
            config_dir: paths::config_dir(),
        }
    }
}
// !SECTION: Configuration

// SECTION: Runtime state
// ? What is open for one strip. The `Box<dyn AudioStream>` owns a thread and (inside it) the
// ? ring's producer; `close()` returns the port so the pool never loses a ring.
enum StripLink {
    None,
    Device(Box<dyn AudioStream>),
    Lan { key: StreamKey, slot: PortSlot },
}

enum BusLink {
    None,
    Device(Box<dyn AudioStream>),
    Lan { tx: TxStream, slot: PortSlot },
}

struct StripSlot {
    config: StripConfig,
    link: StripLink,
    state: LinkState,
}

struct BusSlot {
    config: BusConfig,
    link: BusLink,
    state: LinkState,
}

struct PolicySetting {
    accept_any: bool,
    manual: Vec<[u8; 4]>,
    trust_discovered: bool,
}

struct Inner {
    cfg: DaemonConfig,
    commands: Producer<EngineCommand>,
    ports: ControlPorts,
    backend: Box<dyn AudioBackend>,
    telemetry: Arc<Telemetry>,
    strips: Vec<StripSlot>,
    buses: Vec<BusSlot>,
    rx: RxSocket,
    service: Service,
    discovery: Option<Discovery>,
    discovery_state: DiscoveryState,
    policy: PolicySetting,
    applied_policy: RxPolicy,
    beacon: bool,
    ui: UiLayout,
    profile: Option<String>,
    probe_running: bool,
    last_discovery_poll: Instant,
}

struct Shared {
    inner: Mutex<Inner>,
    telemetry: Arc<Telemetry>,
    stats: Arc<EngineStats>,
    rx_port: u16,
    name: String,
    backend_name: &'static str,
    capabilities: Capabilities,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // ? A panic in one request must not make every later request fail with "poisoned".
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
// !SECTION: Runtime state

// SECTION: Validation helpers
fn finite(v: f32, what: &str) -> ControlResult<f32> {
    if v.is_finite() {
        Ok(v)
    } else {
        Err(ControlError::Invalid(format!("{what} is not a number")))
    }
}

fn gain(v: f32, what: &str) -> ControlResult<f32> {
    Ok(finite(v, what)?.clamp(GAIN_MIN_DB, GAIN_MAX_DB))
}

fn strip_index(i: u8) -> ControlResult<usize> {
    let i = usize::from(i);
    if i < MAX_STRIPS {
        Ok(i)
    } else {
        Err(ControlError::Invalid(format!(
            "strip {i} does not exist (0..{MAX_STRIPS})"
        )))
    }
}

fn bus_index(i: u8) -> ControlResult<usize> {
    let i = usize::from(i);
    if i < MAX_BUSES {
        Ok(i)
    } else {
        Err(ControlError::Invalid(format!(
            "bus {i} does not exist (0..{MAX_BUSES})"
        )))
    }
}

fn channel_count(c: u8) -> ControlResult<u8> {
    if (1..=MAX_CHANNELS as u8).contains(&c) {
        Ok(c)
    } else {
        Err(ControlError::Invalid(format!(
            "{c} channels (1..={MAX_CHANNELS})"
        )))
    }
}
// !SECTION: Validation helpers

impl Inner {
    // SECTION: Engine commands
    /// Queue a command for the mixer. If the queue is full, wait for the mixer to drain it.
    fn send(&mut self, cmd: EngineCommand) -> ControlResult<()> {
        let deadline = Instant::now() + QUEUE_WAIT;
        loop {
            match self.commands.push(cmd) {
                Ok(()) => return Ok(()),
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(1)),
                Err(_) => {
                    return Err(ControlError::Backend(
                        "the mixer is not taking commands (is it running?)".into(),
                    ));
                }
            }
        }
    }

    /// Send everything about strip `i` that is not its input: fader, pan, mute, solo, DSP, sends.
    fn push_strip_params(&mut self, i: usize) -> ControlResult<()> {
        let (strip, c) = (StripId(i as u8), self.strips[i].config);
        self.send(EngineCommand::SetStripGain {
            strip,
            gain_db: c.gain_db,
        })?;
        self.send(EngineCommand::SetStripPan { strip, pan: c.pan })?;
        self.send(EngineCommand::SetStripMute {
            strip,
            mute: c.mute,
        })?;
        self.send(EngineCommand::SetStripSolo {
            strip,
            solo: c.solo,
        })?;
        self.send(EngineCommand::SetStripDsp { strip, dsp: c.dsp })?;
        for (b, send) in c.sends.iter().enumerate() {
            self.send(EngineCommand::SetRouteSend {
                strip,
                bus: BusId(b as u8),
                send: *send,
            })?;
        }
        Ok(())
    }

    fn push_bus_params(&mut self, b: usize) -> ControlResult<()> {
        let (bus, c) = (BusId(b as u8), self.buses[b].config);
        self.send(EngineCommand::SetBusGain {
            bus,
            gain_db: c.gain_db,
        })?;
        self.send(EngineCommand::SetBusMute { bus, mute: c.mute })?;
        self.send(EngineCommand::SetBusEq { bus, eq: c.eq })?;
        self.send(EngineCommand::SetBusRecord {
            bus,
            enabled: c.record,
        })
    }
    // !SECTION: Engine commands

    // SECTION: Strip sources
    /// Close whatever feeds strip `i` and give its ring back to the pool.
    fn close_strip(&mut self, i: usize) {
        let strip = StripId(i as u8);
        match std::mem::replace(&mut self.strips[i].link, StripLink::None) {
            StripLink::None => {}
            StripLink::Device(stream) => {
                // Detach first so the mixer stops reading before the ring can be reused.
                let _ = self.send(EngineCommand::DetachStripInput { strip });
                if let Some(ClosedPort::Input(port)) = stream.close() {
                    self.ports.release_input(port);
                }
            }
            StripLink::Lan { key, slot } => {
                let _ = self.send(EngineCommand::DetachStripInput { strip });
                if let Ok(Some(stream)) = self.rx.remove_stream(key) {
                    self.ports
                        .release_input(InputPort::from_parts(slot, stream.into_producer()));
                }
            }
        }
        self.strips[i].state = LinkState::Idle;
    }

    /// Record the request, close the old input, open the new one. A failure leaves the request
    /// in the config with `LinkState::Failed` so the UI can show what did not work.
    fn open_strip(&mut self, i: usize, source: SourceEndpoint, channels: u8) -> ControlResult<()> {
        self.close_strip(i);
        self.strips[i].config.source = source;
        self.strips[i].config.channels = channels;
        let result = self.try_open_strip(i, source, channels);
        self.strips[i].state = match &result {
            Ok(()) if source == SourceEndpoint::Unassigned => LinkState::Idle,
            Ok(()) => LinkState::Running,
            Err(e) => LinkState::Failed(e.to_string()),
        };
        result
    }

    fn try_open_strip(
        &mut self,
        i: usize,
        source: SourceEndpoint,
        channels: u8,
    ) -> ControlResult<()> {
        let strip = StripId(i as u8);
        match source {
            SourceEndpoint::Unassigned => Ok(()),
            SourceEndpoint::LanIn(cfg) => {
                let port = self.ports.acquire_input().ok_or(ControlError::NoFreePort)?;
                let (slot, producer) = port.into_parts();
                let counters = Arc::new(StreamCounters::default());
                let mut stream = StreamRx::new(
                    producer,
                    usize::from(channels),
                    cfg.buffer,
                    Arc::clone(&counters),
                );
                stream.attach_telemetry(Arc::clone(&self.telemetry), i);
                let key = StreamKey::from_config(&cfg);
                if let Err((e, back)) = self.rx.add_stream(key, stream) {
                    if let Some(back) = back {
                        self.ports
                            .release_input(InputPort::from_parts(slot, back.into_producer()));
                    }
                    return Err(ControlError::Network(e.to_string()));
                }
                self.strips[i].link = StripLink::Lan { key, slot };
                self.attach_strip(strip, slot, channels, counters.target_ms())
            }
            device_source => {
                let port = self.ports.acquire_input().ok_or(ControlError::NoFreePort)?;
                let slot = port.slot();
                match self.backend.open_input(&device_source, channels, port) {
                    Ok(stream) => {
                        let clock = if stream.is_master_clocked() {
                            None
                        } else {
                            Some(self.cfg.device_target_ms)
                        };
                        self.strips[i].link = StripLink::Device(stream);
                        match clock {
                            None => self.attach_strip_master(strip, slot, channels),
                            Some(t) => self.attach_strip(strip, slot, channels, t),
                        }
                    }
                    Err((e, port)) => {
                        self.ports.release_input(port);
                        Err(ControlError::Backend(e.to_string()))
                    }
                }
            }
        }
    }

    fn attach_strip(
        &mut self,
        strip: StripId,
        slot: PortSlot,
        channels: u8,
        target_ms: f32,
    ) -> ControlResult<()> {
        self.send(EngineCommand::AttachStripInput {
            strip,
            slot,
            channels,
        })?;
        self.send(EngineCommand::SetStripClock {
            strip,
            clock: ClockDomain::External { target_ms },
        })
    }

    fn attach_strip_master(
        &mut self,
        strip: StripId,
        slot: PortSlot,
        channels: u8,
    ) -> ControlResult<()> {
        self.send(EngineCommand::AttachStripInput {
            strip,
            slot,
            channels,
        })?;
        self.send(EngineCommand::SetStripClock {
            strip,
            clock: ClockDomain::Master,
        })
    }
    // !SECTION: Strip sources

    // SECTION: Bus sinks
    fn close_bus(&mut self, b: usize) {
        let bus = BusId(b as u8);
        match std::mem::replace(&mut self.buses[b].link, BusLink::None) {
            BusLink::None => {}
            BusLink::Device(stream) => {
                let _ = self.send(EngineCommand::DetachBusOutput { bus });
                if let Some(ClosedPort::Output(port)) = stream.close() {
                    self.ports.release_output(port);
                }
            }
            BusLink::Lan { tx, slot } => {
                let _ = self.send(EngineCommand::DetachBusOutput { bus });
                if let Some(consumer) = tx.stop() {
                    self.ports
                        .release_output(OutputPort::from_parts(slot, consumer));
                }
            }
        }
        self.buses[b].state = LinkState::Idle;
    }

    fn open_bus(&mut self, b: usize, sink: SinkEndpoint, channels: u8) -> ControlResult<()> {
        self.close_bus(b);
        self.buses[b].config.sink = sink;
        self.buses[b].config.channels = channels;
        let result = self.try_open_bus(b, sink, channels);
        self.buses[b].state = match &result {
            Ok(()) if sink == SinkEndpoint::Unassigned => LinkState::Idle,
            Ok(()) => LinkState::Running,
            Err(e) => LinkState::Failed(e.to_string()),
        };
        result
    }

    fn try_open_bus(&mut self, b: usize, sink: SinkEndpoint, channels: u8) -> ControlResult<()> {
        let bus = BusId(b as u8);
        match sink {
            SinkEndpoint::Unassigned => Ok(()),
            SinkEndpoint::LanOut(cfg) => {
                let port = self
                    .ports
                    .acquire_output()
                    .ok_or(ControlError::NoFreePort)?;
                let (slot, consumer) = port.into_parts();
                match TxStream::spawn(cfg, consumer, usize::from(channels)) {
                    Ok(tx) => {
                        self.buses[b].link = BusLink::Lan { tx, slot };
                        self.send(EngineCommand::AttachBusOutput {
                            bus,
                            slot,
                            channels,
                        })
                    }
                    Err((e, back)) => {
                        if let Some(consumer) = back {
                            self.ports
                                .release_output(OutputPort::from_parts(slot, consumer));
                        }
                        Err(ControlError::Network(format!("{e:?}")))
                    }
                }
            }
            device_sink => {
                let port = self
                    .ports
                    .acquire_output()
                    .ok_or(ControlError::NoFreePort)?;
                let slot = port.slot();
                match self.backend.open_output(&device_sink, channels, port) {
                    Ok(stream) => {
                        self.buses[b].link = BusLink::Device(stream);
                        self.send(EngineCommand::AttachBusOutput {
                            bus,
                            slot,
                            channels,
                        })
                    }
                    Err((e, port)) => {
                        self.ports.release_output(port);
                        Err(ControlError::Backend(e.to_string()))
                    }
                }
            }
        }
    }
    // !SECTION: Bus sinks

    // SECTION: Network policy and discovery
    fn wanted_policy(&self) -> RxPolicy {
        let mut allowed: Vec<Ipv4Addr> = self.policy.manual.iter().map(|&o| o.into()).collect();
        if self.policy.trust_discovered {
            if let Some(d) = &self.discovery {
                for device in d.devices() {
                    allowed.extend(device.addrs.iter().copied());
                }
            }
        }
        allowed.sort();
        allowed.dedup();
        RxPolicy {
            accept_any: self.policy.accept_any,
            allowed,
        }
    }

    /// Hand the socket the current policy if it changed.
    fn refresh_policy(&mut self) {
        let wanted = self.wanted_policy();
        if wanted != self.applied_policy && self.rx.set_policy(wanted.clone()).is_ok() {
            self.applied_policy = wanted;
        }
    }

    fn poll_discovery(&mut self) {
        let Some(d) = &mut self.discovery else {
            return;
        };
        if !d.poll().is_empty() {
            self.refresh_policy();
        }
    }
    // !SECTION: Network policy and discovery

    // SECTION: Commands
    fn apply(&mut self, cmd: Command) -> ControlResult<()> {
        match cmd {
            Command::StripGain { strip, gain_db } => {
                let i = strip_index(strip)?;
                let db = gain(gain_db, "gain")?;
                self.strips[i].config.gain_db = db;
                self.send(EngineCommand::SetStripGain {
                    strip: StripId(strip),
                    gain_db: db,
                })
            }
            Command::StripPan { strip, pan } => {
                let i = strip_index(strip)?;
                let pan = finite(pan, "pan")?.clamp(-1.0, 1.0);
                self.strips[i].config.pan = pan;
                self.send(EngineCommand::SetStripPan {
                    strip: StripId(strip),
                    pan,
                })
            }
            Command::StripMute { strip, mute } => {
                let i = strip_index(strip)?;
                self.strips[i].config.mute = mute;
                self.send(EngineCommand::SetStripMute {
                    strip: StripId(strip),
                    mute,
                })
            }
            Command::StripSolo { strip, solo } => {
                let i = strip_index(strip)?;
                self.strips[i].config.solo = solo;
                self.send(EngineCommand::SetStripSolo {
                    strip: StripId(strip),
                    solo,
                })
            }
            Command::StripLabel { strip, label } => {
                let i = strip_index(strip)?;
                self.strips[i].config.label = FixedStr::from_str_trunc(&label);
                Ok(())
            }
            Command::StripDsp { strip, dsp } => {
                let i = strip_index(strip)?;
                let dsp = sanitize_dsp(&dsp)
                    .ok_or_else(|| ControlError::Invalid("a DSP value is not a number".into()))?;
                self.strips[i].config.dsp = dsp;
                self.send(EngineCommand::SetStripDsp {
                    strip: StripId(strip),
                    dsp,
                })
            }
            Command::StripSource {
                strip,
                source,
                channels,
            } => {
                let i = strip_index(strip)?;
                self.open_strip(i, source, channel_count(channels)?)
            }
            Command::Route { strip, bus, send } => {
                let (i, b) = (strip_index(strip)?, bus_index(bus)?);
                let send = RouteSend {
                    gain_db: gain(send.gain_db, "send gain")?,
                    ..send
                };
                self.strips[i].config.sends[b] = send;
                self.send(EngineCommand::SetRouteSend {
                    strip: StripId(strip),
                    bus: BusId(bus),
                    send,
                })
            }
            Command::BusGain { bus, gain_db } => {
                let b = bus_index(bus)?;
                let db = gain(gain_db, "gain")?;
                self.buses[b].config.gain_db = db;
                self.send(EngineCommand::SetBusGain {
                    bus: BusId(bus),
                    gain_db: db,
                })
            }
            Command::BusMute { bus, mute } => {
                let b = bus_index(bus)?;
                self.buses[b].config.mute = mute;
                self.send(EngineCommand::SetBusMute {
                    bus: BusId(bus),
                    mute,
                })
            }
            Command::BusLabel { bus, label } => {
                let b = bus_index(bus)?;
                self.buses[b].config.label = FixedStr::from_str_trunc(&label);
                Ok(())
            }
            Command::BusEq { bus, eq } => {
                let b = bus_index(bus)?;
                let eq = sanitize_bands(&eq)
                    .ok_or_else(|| ControlError::Invalid("an EQ value is not a number".into()))?;
                self.buses[b].config.eq = eq;
                self.send(EngineCommand::SetBusEq {
                    bus: BusId(bus),
                    eq,
                })
            }
            Command::BusRecord { bus, enabled } => {
                let b = bus_index(bus)?;
                self.buses[b].config.record = enabled;
                self.send(EngineCommand::SetBusRecord {
                    bus: BusId(bus),
                    enabled,
                })
            }
            Command::BusSink {
                bus,
                sink,
                channels,
            } => {
                let b = bus_index(bus)?;
                self.open_bus(b, sink, channel_count(channels)?)
            }
            Command::RxPolicy {
                accept_any,
                allowed,
                trust_discovered,
            } => {
                self.policy = PolicySetting {
                    accept_any,
                    manual: allowed,
                    trust_discovered,
                };
                self.refresh_policy();
                Ok(())
            }
            Command::Beacon { on } => {
                self.beacon = on;
                self.service.set_beacon(on);
                Ok(())
            }
            Command::Ping { peer } => {
                self.service.ping(peer);
                Ok(())
            }
            Command::Ui(layout) => {
                self.ui = layout.sanitized();
                Ok(())
            }
            Command::SaveProfile { name } => {
                profile::save(&self.cfg.config_dir, &name, &self.current_profile())
                    .map_err(|e| ControlError::Profile(e.to_string()))?;
                self.profile = Some(name);
                Ok(())
            }
            Command::LoadProfile { name } => {
                let p = profile::load(&self.cfg.config_dir, &name)
                    .map_err(|e| ControlError::Profile(e.to_string()))?;
                self.apply_profile(p)?;
                self.profile = Some(name);
                Ok(())
            }
            Command::DeleteProfile { name } => {
                profile::delete(&self.cfg.config_dir, &name)
                    .map_err(|e| ControlError::Profile(e.to_string()))?;
                if self.profile.as_deref() == Some(name.as_str()) {
                    self.profile = None;
                }
                Ok(())
            }
        }
    }

    /// A command from a remote machine (VBAN-TEXT). Bad ones are ignored, as VoiceMeeter does.
    fn apply_text(&mut self, command: TextCommand) {
        let cmd = match command {
            TextCommand::StripGain { index, db } => Command::StripGain {
                strip: index,
                gain_db: db,
            },
            TextCommand::StripMute { index, muted } => Command::StripMute {
                strip: index,
                mute: muted,
            },
            TextCommand::BusGain { index, db } => Command::BusGain {
                bus: index,
                gain_db: db,
            },
            TextCommand::BusMute { index, muted } => Command::BusMute {
                bus: index,
                mute: muted,
            },
        };
        let _ = self.apply(cmd);
    }
    // !SECTION: Commands

    // SECTION: Profiles
    fn current_profile(&self) -> Profile {
        Profile {
            version: profile::PROFILE_VERSION,
            strips: self.strips.iter().map(|s| s.config).collect(),
            buses: self.buses.iter().map(|b| b.config).collect(),
            network: NetworkProfile {
                accept_any: self.policy.accept_any,
                allowed: self.policy.manual.clone(),
                trust_discovered: self.policy.trust_discovered,
                beacon: self.beacon,
            },
            ui: self.ui.clone(),
        }
    }

    /// Replace the whole setup. Endpoints that cannot be opened (a device that is unplugged)
    /// are left `Failed`; the rest of the profile still loads.
    fn apply_profile(&mut self, p: Profile) -> ControlResult<()> {
        let p = p.normalize();
        for i in 0..MAX_STRIPS {
            self.close_strip(i);
        }
        for b in 0..MAX_BUSES {
            self.close_bus(b);
        }
        for (i, config) in p.strips.iter().enumerate() {
            self.strips[i].config = StripConfig {
                source: SourceEndpoint::Unassigned,
                ..*config
            };
            self.push_strip_params(i)?;
        }
        for (b, config) in p.buses.iter().enumerate() {
            self.buses[b].config = BusConfig {
                sink: SinkEndpoint::Unassigned,
                ..*config
            };
            self.push_bus_params(b)?;
        }
        self.policy = PolicySetting {
            accept_any: p.network.accept_any,
            manual: p.network.allowed.clone(),
            trust_discovered: p.network.trust_discovered,
        };
        self.refresh_policy();
        self.beacon = p.network.beacon;
        self.service.set_beacon(self.beacon);
        self.ui = p.ui.clone();
        // Open last: the mixer is already configured when the first audio arrives.
        for (i, config) in p.strips.iter().enumerate() {
            let _ = self.open_strip(i, config.source, config.channels);
        }
        for (b, config) in p.buses.iter().enumerate() {
            let _ = self.open_bus(b, config.sink, config.channels);
        }
        Ok(())
    }
    // !SECTION: Profiles

    // SECTION: Probe rig
    /// Borrow a free strip and bus, attach rings to them and route one to the other.
    fn setup_probe_rig(&mut self) -> ControlResult<ProbeRig> {
        if self.strips.iter().any(|s| s.config.solo) {
            return Err(ControlError::Invalid(
                "a strip is soloed, which would mute the probe; clear solo first".into(),
            ));
        }
        let strip = (0..MAX_STRIPS)
            .rev()
            .find(|&i| {
                let s = &self.strips[i];
                matches!(s.link, StripLink::None)
                    && s.config.source == SourceEndpoint::Unassigned
                    && !s.config.sends.iter().any(|x| x.enabled)
            })
            .ok_or_else(|| ControlError::Invalid("no free strip for the probe".into()))?;
        let bus = (0..MAX_BUSES)
            .rev()
            .find(|&b| {
                matches!(self.buses[b].link, BusLink::None)
                    && self.buses[b].config.sink == SinkEndpoint::Unassigned
                    && !self.strips.iter().any(|s| s.config.sends[b].enabled)
            })
            .ok_or_else(|| ControlError::Invalid("no free bus for the probe".into()))?;
        let input = self.ports.acquire_input().ok_or(ControlError::NoFreePort)?;
        let Some(output) = self.ports.acquire_output() else {
            self.ports.release_input(input);
            return Err(ControlError::NoFreePort);
        };
        let rig = ProbeRig {
            strip,
            bus,
            input: Some(input),
            output: Some(output),
            target_ms: self.cfg.device_target_ms,
        };
        match self.wire_probe_rig(&rig) {
            Ok(()) => Ok(rig),
            Err(e) => {
                self.teardown_probe_rig(rig);
                Err(e)
            }
        }
    }

    fn wire_probe_rig(&mut self, rig: &ProbeRig) -> ControlResult<()> {
        let (strip, bus) = (StripId(rig.strip as u8), BusId(rig.bus as u8));
        let (Some(input), Some(output)) = (&rig.input, &rig.output) else {
            return Err(ControlError::Invalid("probe rig has no ports".into()));
        };
        let (in_slot, out_slot) = (input.slot(), output.slot());
        // Known-neutral settings, whatever the user had on these unused channels.
        self.send(EngineCommand::SetStripGain {
            strip,
            gain_db: 0.0,
        })?;
        self.send(EngineCommand::SetStripPan { strip, pan: 0.0 })?;
        self.send(EngineCommand::SetStripMute { strip, mute: false })?;
        self.send(EngineCommand::SetStripSolo { strip, solo: false })?;
        self.send(EngineCommand::SetStripDsp {
            strip,
            dsp: StripDsp::default(),
        })?;
        self.send(EngineCommand::SetBusGain { bus, gain_db: 0.0 })?;
        self.send(EngineCommand::SetBusMute { bus, mute: false })?;
        self.send(EngineCommand::SetBusEq {
            bus,
            eq: [EqBand::default(); 3],
        })?;
        self.send(EngineCommand::AttachBusOutput {
            bus,
            slot: out_slot,
            channels: 2,
        })?;
        self.attach_strip(strip, in_slot, 2, rig.target_ms)?;
        self.send(EngineCommand::SetRouteSend {
            strip,
            bus,
            send: RouteSend {
                enabled: true,
                gain_db: 0.0,
                pre_fader: false,
            },
        })
    }

    /// Undo `wire_probe_rig` and put the unused strip and bus back as the config says.
    fn teardown_probe_rig(&mut self, mut rig: ProbeRig) {
        let (strip, bus) = (StripId(rig.strip as u8), BusId(rig.bus as u8));
        let _ = self.send(EngineCommand::SetRouteSend {
            strip,
            bus,
            send: RouteSend::default(),
        });
        let _ = self.send(EngineCommand::DetachStripInput { strip });
        let _ = self.send(EngineCommand::DetachBusOutput { bus });
        let _ = self.push_strip_params(rig.strip);
        let _ = self.push_bus_params(rig.bus);
        if let Some(p) = rig.input.take() {
            self.ports.release_input(p);
        }
        if let Some(p) = rig.output.take() {
            self.ports.release_output(p);
        }
    }
    // !SECTION: Probe rig
}

/// The borrowed strip, bus and rings of one click-loop run.
struct ProbeRig {
    strip: usize,
    bus: usize,
    input: Option<InputPort>,
    output: Option<OutputPort>,
    target_ms: f32,
}

// SECTION: LocalControl
/// The in-process `ControlApi`. Dropping it stops the background thread and closes every
/// device, stream and socket.
pub struct LocalControl {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    housekeeper: Option<JoinHandle<()>>,
}

impl LocalControl {
    /// Take over the engine's handles, open the receive socket and start housekeeping.
    pub fn new(
        handles: EngineHandles,
        backend: Box<dyn AudioBackend>,
        cfg: DaemonConfig,
    ) -> ControlResult<Self> {
        let EngineHandles {
            commands,
            ports,
            telemetry,
            stats,
        } = handles;

        let (service, handler) = Service::new(Identity::new(&cfg.name));
        let policy = RxPolicy::default();
        let rx = RxSocket::bind(
            SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, cfg.rx_port),
            policy.clone(),
            Some(Box::new(handler)),
        )
        .map_err(|e| ControlError::Network(format!("cannot open UDP port {}: {e}", cfg.rx_port)))?;
        let rx_port = rx.local_addr().port();

        let (discovery, discovery_state) = if cfg.discovery {
            start_discovery(&cfg.name, rx_port)
        } else {
            (None, DiscoveryState::Off)
        };

        let name = cfg.name.clone();
        let backend_name = backend.name();
        let capabilities = backend.capabilities();
        let inner = Inner {
            cfg,
            commands,
            ports,
            backend,
            telemetry: Arc::clone(&telemetry),
            strips: (0..MAX_STRIPS)
                .map(|i| StripSlot {
                    config: default_strip(i),
                    link: StripLink::None,
                    state: LinkState::Idle,
                })
                .collect(),
            buses: (0..MAX_BUSES)
                .map(|b| BusSlot {
                    config: default_bus(b),
                    link: BusLink::None,
                    state: LinkState::Idle,
                })
                .collect(),
            rx,
            service,
            discovery,
            discovery_state,
            policy: PolicySetting {
                accept_any: false,
                manual: Vec::new(),
                trust_discovered: false,
            },
            applied_policy: policy,
            beacon: false,
            ui: UiLayout::default(),
            profile: None,
            probe_running: false,
            last_discovery_poll: Instant::now(),
        };
        let shared = Arc::new(Shared {
            inner: Mutex::new(inner),
            telemetry,
            stats,
            rx_port,
            name,
            backend_name,
            capabilities,
        });

        let stop = Arc::new(AtomicBool::new(false));
        let housekeeper = {
            // ? A `Weak` pointer: the thread must not keep the control alive after the owner
            // ? dropped it, or `Drop` (which joins this thread) could never finish.
            let weak = Arc::downgrade(&shared);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("control-housekeeping".into())
                .spawn(move || housekeeping(&weak, &stop))
                .map_err(|e| ControlError::Backend(format!("cannot start a thread: {e}")))?
        };
        Ok(Self {
            shared,
            stop,
            housekeeper: Some(housekeeper),
        })
    }

    /// The UDP port the VBAN receive socket is bound to.
    pub fn rx_port(&self) -> u16 {
        self.shared.rx_port
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        lock(&self.shared.inner)
    }
}

impl Drop for LocalControl {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(t) = self.housekeeper.take() {
            let _ = t.join();
        }
        // Closing every endpoint returns the rings; the rest goes with `Shared`.
        let mut inner = lock(&self.shared.inner);
        for i in 0..MAX_STRIPS {
            inner.close_strip(i);
        }
        for b in 0..MAX_BUSES {
            inner.close_bus(b);
        }
    }
}

fn start_discovery(name: &str, rx_port: u16) -> (Option<Discovery>, DiscoveryState) {
    let mut discovery = match Discovery::new() {
        Ok(d) => d,
        Err(e) => return (None, DiscoveryState::Failed(e.to_string())),
    };
    let mut advert = Advert::new(name);
    advert.vban_port = rx_port;
    if let Err(e) = discovery.announce(&advert) {
        return (None, DiscoveryState::Failed(e.to_string()));
    }
    if let Err(e) = discovery.start_browse() {
        return (None, DiscoveryState::Failed(e.to_string()));
    }
    (Some(discovery), DiscoveryState::Running)
}

/// Applies commands that arrive over VBAN-TEXT and keeps the receive policy in step with
/// discovery.
fn housekeeping(shared: &std::sync::Weak<Shared>, stop: &AtomicBool) {
    while !stop.load(Relaxed) {
        std::thread::sleep(HOUSEKEEPING_TICK);
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let mut inner = lock(&shared.inner);
        while let Some(event) = inner.service.try_recv_command() {
            inner.apply_text(event.command);
        }
        if inner.last_discovery_poll.elapsed() >= DISCOVERY_EVERY {
            inner.last_discovery_poll = Instant::now();
            inner.poll_discovery();
        }
    }
}
// !SECTION: LocalControl

// SECTION: ControlApi
fn levels(m: &audio_types::MeterAtomics) -> MeterLevels {
    MeterLevels {
        peak_db: core::array::from_fn(|c| load_f32(&m.peak_db[c])),
        rms_db: core::array::from_fn(|c| load_f32(&m.rms_db[c])),
        clipping: m.clipping.load(Relaxed),
    }
}

fn ms(d: Duration) -> f32 {
    d.as_secs_f32() * 1000.0
}

impl ControlApi for LocalControl {
    fn command(&self, cmd: Command) -> ControlResult<()> {
        self.lock().apply(cmd)
    }

    fn snapshot(&self) -> ControlResult<StateSnapshot> {
        let g = self.lock();
        let strips = g
            .strips
            .iter()
            .map(|s| StripState {
                config: s.config,
                link: s.state.clone(),
                stream: match &s.link {
                    StripLink::Device(stream) => Some(stream.stats()),
                    _ => None,
                },
            })
            .collect();
        let buses = g
            .buses
            .iter()
            .map(|b| BusState {
                config: b.config,
                link: b.state.clone(),
                stream: match &b.link {
                    BusLink::Device(stream) => Some(stream.stats()),
                    _ => None,
                },
                tx: match &b.link {
                    BusLink::Lan { tx, .. } => {
                        let s = tx.stats();
                        Some(TxState {
                            packets: s.packets.load(Relaxed),
                            bytes: s.bytes.load(Relaxed),
                            send_errors: s.send_errors.load(Relaxed),
                            dropped_frames: u32::try_from(s.dropped_frames.load(Relaxed))
                                .unwrap_or(u32::MAX),
                            max_tick_gap_us: s.max_tick_gap_us.load(Relaxed),
                        })
                    }
                    _ => None,
                },
            })
            .collect();
        let peers = g
            .service
            .peers()
            .into_iter()
            .map(|p| PeerState {
                addr: p.addr,
                device_name: p.info.device_name.clone(),
                application: p.info.application.clone(),
                host_name: p.info.host_name.clone(),
                is_lan_router: p.info.is_lan_router(),
                rtt_ms: p.rtt.map(ms),
                age_ms: u32::try_from(p.age.as_millis()).unwrap_or(u32::MAX),
            })
            .collect();
        let discovered = g
            .discovery
            .as_ref()
            .map(|d| {
                d.devices()
                    .iter()
                    .map(|dev| DiscoveredState {
                        name: dev.info.name.clone(),
                        os: dev.info.os.clone(),
                        version: dev.info.version.clone(),
                        opus: dev.info.opus,
                        vban_addr: dev.vban_addr(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let seen_senders =
            g.rx.stats()
                .seen_senders()
                .into_iter()
                .map(|s| SeenSenderState {
                    ip: s.ip.octets(),
                    stream_name: s.name.as_str().to_owned(),
                    channels: s.channels,
                    sample_rate: s.sample_rate,
                    packets: s.packets,
                })
                .collect();
        Ok(StateSnapshot {
            protocol: PROTOCOL_VERSION,
            name: self.shared.name.clone(),
            backend: self.shared.backend_name.to_owned(),
            capabilities: self.shared.capabilities,
            rx_port: self.shared.rx_port,
            strips,
            buses,
            policy: PolicyState {
                accept_any: g.policy.accept_any,
                allowed: g.policy.manual.clone(),
                trust_discovered: g.policy.trust_discovered,
                effective: g
                    .applied_policy
                    .allowed
                    .iter()
                    .map(|ip| ip.octets())
                    .collect(),
            },
            beacon: g.beacon,
            peers,
            discovery: g.discovery_state.clone(),
            discovered,
            seen_senders,
            profile: g.profile.clone(),
            profiles: profile::list(&g.cfg.config_dir),
            ui: g.ui.clone(),
        })
    }

    fn meters(&self) -> ControlResult<MeterSnapshot> {
        // Atomics only: no lock, so a long probe or a slow device open never freezes the meters.
        let t = &self.shared.telemetry;
        let s = &self.shared.stats;
        Ok(MeterSnapshot {
            strips: t.strips.iter().map(levels).collect(),
            buses: t.buses.iter().map(levels).collect(),
            lan_rx: t
                .lan_rx
                .iter()
                .map(|l| LanRxLevels {
                    buffer_ms: load_f32(&l.buffer_ms),
                    target_ms: load_f32(&l.target_ms),
                    drift_ppm: load_f32(&l.drift_ppm),
                    packets_lost: l.packets_lost.load(Relaxed),
                    packets_late: l.packets_late.load(Relaxed),
                    underruns: l.underruns.load(Relaxed),
                    last_packet_ms_ago: l.last_packet_ms_ago.load(Relaxed),
                })
                .collect(),
            any_audio_active: t.any_audio_active.load(Relaxed),
            engine: EngineStatus {
                rt_load_permille: t.rt_load_permille.load(Relaxed),
                rt_overruns: t.rt_overruns.load(Relaxed),
                input_underruns: s.input_underruns.load(Relaxed),
                input_overruns: s.input_overruns.load(Relaxed),
                output_overruns: s.output_overruns.load(Relaxed),
                recorder_overruns: s.recorder_overruns.load(Relaxed),
                rejected_commands: s.rejected_commands.load(Relaxed),
            },
        })
    }

    fn devices(&self) -> ControlResult<Vec<DeviceInfo>> {
        Ok(self.lock().backend.devices())
    }

    fn apps(&self) -> ControlResult<Vec<AppInfo>> {
        Ok(self.lock().backend.apps())
    }

    fn probe(&self, request: ProbeRequest) -> ControlResult<ProbeReport> {
        if request.pings > MAX_PINGS || request.clicks > MAX_CLICKS {
            return Err(ControlError::Invalid(format!(
                "at most {MAX_PINGS} pings and {MAX_CLICKS} clicks"
            )));
        }
        {
            let mut g = self.lock();
            if g.probe_running {
                return Err(ControlError::Busy("a probe is already running".into()));
            }
            g.probe_running = true;
        }
        let _reset = ProbeFlag(&self.shared);

        let mut report = ProbeReport::default();
        if let Some(peer) = request.peer {
            report.rtt = Some(self.probe_rtt(peer, request.pings));
            if report.rtt.as_ref().is_some_and(|r| r.received == 0) {
                report.notes.push(format!(
                    "{peer} did not answer any ping (not running, or its firewall drops UDP {})",
                    peer.port()
                ));
            }
        }
        if request.click_loop {
            match self.probe_click_loop(usize::from(request.clicks.max(1))) {
                Ok(c) => {
                    if c.clicks_found == 0 {
                        report.notes.push(
                            "no click came out: nothing is running the mixer (no master clock)"
                                .into(),
                        );
                    }
                    report.click_loop = Some(c);
                }
                Err(e) => report.notes.push(format!("click-loop skipped: {e}")),
            }
        }
        Ok(report)
    }
}

/// Clears `probe_running` however the probe ends.
struct ProbeFlag<'a>(&'a Arc<Shared>);

impl Drop for ProbeFlag<'_> {
    fn drop(&mut self) {
        lock(&self.0.inner).probe_running = false;
    }
}

impl LocalControl {
    /// Ping `peer` `pings` times, one per `PING_SPACING`, and collect the round trips.
    fn probe_rtt(&self, peer: SocketAddrV4, pings: u8) -> crate::types::RttReport {
        let mut samples = Vec::new();
        for _ in 0..pings {
            let started = Instant::now();
            let replies_before = {
                let g = self.lock();
                let before = g.service.stats().replies_rx.load(Relaxed);
                g.service.ping(peer);
                before
            };
            let answered = loop {
                std::thread::sleep(Duration::from_millis(2));
                let g = self.lock();
                if g.service.stats().replies_rx.load(Relaxed) != replies_before {
                    break g
                        .service
                        .peers()
                        .into_iter()
                        .find(|p| p.addr.ip() == peer.ip())
                        .and_then(|p| p.rtt);
                }
                if started.elapsed() > PING_SPACING {
                    break None;
                }
            };
            if let Some(rtt) = answered {
                samples.push(ms(rtt));
            }
            if let Some(rest) = PING_SPACING.checked_sub(started.elapsed()) {
                std::thread::sleep(rest);
            }
        }
        probe::rtt_report(u32::from(pings), samples)
    }

    fn probe_click_loop(&self, clicks: usize) -> ControlResult<crate::types::ClickLoopReport> {
        let mut rig = self.lock().setup_probe_rig()?;
        let (Some(mut input), Some(mut output)) = (rig.input.take(), rig.output.take()) else {
            return Err(ControlError::Invalid("probe rig has no ports".into()));
        };
        let target_ms = rig.target_ms;
        // The lock is not held while the clicks run, so meters and other requests stay live.
        let run = probe::run_click_loop(&mut input, &mut output, clicks);
        rig.input = Some(input);
        rig.output = Some(output);
        self.lock().teardown_probe_rig(rig);

        if run.stalled {
            return Err(ControlError::Backend(
                "the input ring is full: nothing is running the mixer".into(),
            ));
        }
        Ok(probe::click_report(&run, target_ms))
    }
}
// !SECTION: ControlApi
