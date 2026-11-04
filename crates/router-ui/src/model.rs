//! What the UI knows about the daemon, and the rules for keeping it current.
//!
//! The UI never owns audio state. It holds the newest `StateSnapshot` / `MeterSnapshot` the daemon
//! sent and replaces them wholesale. The one thing it does locally is *optimistic* application of
//! a command, so a dragged fader follows the mouse instead of waiting for a round trip.

use crate::link::{Job, Link, Reply};
use app_core::types::{BusState, Command, MeterLevels, MeterSnapshot, StateSnapshot, StripState};
use audio_types::FixedStr;
use platform_io::{AppInfo, DeviceInfo};

/// Snapshots are pulled at least this often (seconds); peers, RTT and link states change slowly.
pub const SNAPSHOT_EVERY_S: f64 = 0.5;
/// After a command, the next snapshot is allowed this soon, so a burst of drag events costs one.
pub const SNAPSHOT_AFTER_COMMAND_S: f64 = 0.1;
/// Meters are pulled at most this often (seconds): 30 Hz.
pub const METERS_EVERY_S: f64 = 1.0 / 30.0;
/// How long a message in the status line stays.
pub const NOTICE_S: f64 = 6.0;

#[derive(Clone, Debug, PartialEq)]
pub enum Conn {
    /// Nothing answered yet.
    Connecting,
    Up,
    Down(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub text: String,
    pub until_s: f64,
}

// SECTION: Optimistic application
/// Apply `cmd` to a snapshot the way the daemon will. Only the commands whose result is certain
/// are applied; opening a device or a socket (`StripSource`, `BusSink`, profiles...) is left to the
/// next snapshot because it can fail.
// ? Also used by the test daemon, so the fake and the UI cannot drift apart.
pub fn apply_command(s: &mut StateSnapshot, cmd: &Command) {
    // ? Plain functions, not closures: a closure cannot say "the result borrows from my argument".
    fn strip(s: &mut StateSnapshot, i: u8) -> Option<&mut StripState> {
        s.strips.get_mut(usize::from(i))
    }
    fn bus(s: &mut StateSnapshot, i: u8) -> Option<&mut BusState> {
        s.buses.get_mut(usize::from(i))
    }
    match cmd {
        Command::StripGain { strip: i, gain_db } => {
            if let Some(x) = strip(s, *i) {
                x.config.gain_db = *gain_db;
            }
        }
        Command::StripPan { strip: i, pan } => {
            if let Some(x) = strip(s, *i) {
                x.config.pan = *pan;
            }
        }
        Command::StripMute { strip: i, mute } => {
            if let Some(x) = strip(s, *i) {
                x.config.mute = *mute;
            }
        }
        Command::StripSolo { strip: i, solo } => {
            if let Some(x) = strip(s, *i) {
                x.config.solo = *solo;
            }
        }
        Command::StripLabel { strip: i, label } => {
            if let Some(x) = strip(s, *i) {
                x.config.label = FixedStr::from_str_trunc(label);
            }
        }
        Command::StripDsp { strip: i, dsp } => {
            if let Some(x) = strip(s, *i) {
                x.config.dsp = *dsp;
            }
        }
        Command::Route {
            strip: i,
            bus: b,
            send,
        } => {
            if let Some(x) = strip(s, *i) {
                if let Some(slot) = x.config.sends.get_mut(usize::from(*b)) {
                    *slot = *send;
                }
            }
        }
        Command::BusGain { bus: b, gain_db } => {
            if let Some(x) = bus(s, *b) {
                x.config.gain_db = *gain_db;
            }
        }
        Command::BusMute { bus: b, mute } => {
            if let Some(x) = bus(s, *b) {
                x.config.mute = *mute;
            }
        }
        Command::BusLabel { bus: b, label } => {
            if let Some(x) = bus(s, *b) {
                x.config.label = FixedStr::from_str_trunc(label);
            }
        }
        Command::BusEq { bus: b, eq } => {
            if let Some(x) = bus(s, *b) {
                x.config.eq = *eq;
            }
        }
        Command::BusRecord { bus: b, enabled } => {
            if let Some(x) = bus(s, *b) {
                x.config.record = *enabled;
            }
        }
        Command::Beacon { on } => s.beacon = *on,
        Command::Ui(layout) => s.ui = layout.clone().sanitized(),
        Command::StripSource { .. }
        | Command::BusSink { .. }
        | Command::RxPolicy { .. }
        | Command::Ping { .. }
        | Command::SaveProfile { .. }
        | Command::LoadProfile { .. }
        | Command::DeleteProfile { .. } => {}
    }
}
// !SECTION: Optimistic application

// SECTION: Peak hold
/// Per-channel peak markers that stay for a moment, then fall.
#[derive(Clone, Debug, Default)]
pub struct PeakHold {
    /// `[channel] = (dB, time it was set)`
    held: Vec<[(f32, f64); 8]>,
}

impl PeakHold {
    pub const HOLD_S: f64 = 1.5;
    pub const FALL_DB_PER_S: f32 = 24.0;
    pub const FLOOR_DB: f32 = -120.0;

    pub fn update(&mut self, now_s: f64, levels: &[MeterLevels]) {
        self.held.resize(levels.len(), [(Self::FLOOR_DB, now_s); 8]);
        for (held, lv) in self.held.iter_mut().zip(levels) {
            for (h, &peak) in held.iter_mut().zip(&lv.peak_db) {
                let age = now_s - h.1;
                let current = if age <= Self::HOLD_S {
                    h.0
                } else {
                    (h.0 - Self::FALL_DB_PER_S * (age - Self::HOLD_S) as f32).max(Self::FLOOR_DB)
                };
                if peak >= current {
                    *h = (peak, now_s);
                } else if age > Self::HOLD_S {
                    // ? Store the fallen value, restarting the clock at "now minus the hold", so
                    // ? the next call continues the fall instead of restarting it.
                    *h = (current, now_s - Self::HOLD_S);
                }
            }
        }
    }

    /// The marker to draw for `channel` of entry `index`, in dB.
    pub fn db(&self, index: usize, channel: usize) -> Option<f32> {
        self.held
            .get(index)
            .and_then(|c| c.get(channel))
            .map(|&(db, _)| db)
            .filter(|&db| db > Self::FLOOR_DB)
    }
}
// !SECTION: Peak hold

pub struct Model {
    link: Link,
    pub conn: Conn,
    pub snapshot: Option<StateSnapshot>,
    pub meters: Option<MeterSnapshot>,
    pub devices: Vec<DeviceInfo>,
    pub apps: Vec<AppInfo>,
    pub notice: Option<Notice>,
    pub strip_hold: PeakHold,
    pub bus_hold: PeakHold,
    /// Count of commands sent on this connection. A snapshot requested before a command and
    /// delivered after it is *older* than the optimistic state, and is dropped.
    generation: u64,
    meters_in_flight: bool,
    snapshot_in_flight: bool,
    snapshot_due: bool,
    last_snapshot_s: f64,
    last_meters_s: f64,
}

impl Model {
    pub fn new(link: Link) -> Self {
        Self {
            link,
            conn: Conn::Connecting,
            snapshot: None,
            meters: None,
            devices: Vec::new(),
            apps: Vec::new(),
            notice: None,
            strip_hold: PeakHold::default(),
            bus_hold: PeakHold::default(),
            generation: 0,
            meters_in_flight: false,
            snapshot_in_flight: false,
            snapshot_due: false,
            last_snapshot_s: f64::NEG_INFINITY,
            last_meters_s: f64::NEG_INFINITY,
        }
    }

    /// Called once per frame: take in what arrived, then ask for what is due.
    pub fn pump(&mut self, now_s: f64) {
        for reply in self.link.drain() {
            self.apply(reply, now_s);
        }
        // ? About 30 Hz is plenty for meters (ControlApi::meters says so), even when the window
        // ? draws at 60 fps; the extra frames just redraw the same levels.
        if !self.meters_in_flight && now_s - self.last_meters_s >= METERS_EVERY_S {
            self.meters_in_flight = true;
            self.last_meters_s = now_s;
            self.link.send(Job::Meters);
        }
        let age = now_s - self.last_snapshot_s;
        let due = age >= SNAPSHOT_EVERY_S || (self.snapshot_due && age >= SNAPSHOT_AFTER_COMMAND_S);
        if due && !self.snapshot_in_flight {
            self.snapshot_in_flight = true;
            self.snapshot_due = false;
            self.last_snapshot_s = now_s;
            self.link.send(Job::Snapshot {
                generation: self.generation,
            });
        }
        if self.notice.as_ref().is_some_and(|n| now_s >= n.until_s) {
            self.notice = None;
        }
    }

    fn apply(&mut self, reply: Reply, now_s: f64) {
        match reply {
            Reply::Connected => {
                self.conn = Conn::Up;
                self.generation = 0;
                self.snapshot_in_flight = false;
            }
            Reply::Disconnected(why) => {
                self.conn = Conn::Down(why);
                self.meters_in_flight = false;
                self.snapshot_in_flight = false;
            }
            Reply::Snapshot {
                generation,
                snapshot,
            } => {
                self.snapshot_in_flight = false;
                if generation >= self.generation {
                    self.conn = Conn::Up;
                    self.snapshot = Some(*snapshot);
                }
            }
            Reply::Meters(m) => {
                self.meters_in_flight = false;
                self.conn = Conn::Up;
                self.strip_hold.update(now_s, &m.strips);
                self.bus_hold.update(now_s, &m.buses);
                self.meters = Some(*m);
            }
            Reply::Devices(d) => self.devices = d,
            Reply::Apps(a) => self.apps = a,
            Reply::Refused(e) => {
                self.say(format!("refused: {e}"), now_s);
                // ? The optimistic copy may now be wrong; fetch the truth.
                self.snapshot_due = true;
            }
        }
    }

    /// Send a command. Returns false (and says why in the status line) if there is no daemon.
    pub fn command(&mut self, cmd: Command, now_s: f64) -> bool {
        if !matches!(self.conn, Conn::Up) {
            self.say("not connected to the daemon".into(), now_s);
            return false;
        }
        if let Some(s) = &mut self.snapshot {
            apply_command(s, &cmd);
        }
        self.generation += 1;
        self.snapshot_due = true;
        self.link.send(Job::Command(cmd));
        true
    }

    /// Re-read the device and app lists (when a source picker opens).
    pub fn refresh_lists(&mut self) {
        self.link.send(Job::Devices);
        self.link.send(Job::Apps);
    }

    pub fn say(&mut self, text: String, now_s: f64) {
        self.notice = Some(Notice {
            text,
            until_s: now_s + NOTICE_S,
        });
    }

    pub fn any_audio_active(&self) -> bool {
        self.meters.as_ref().is_some_and(|m| m.any_audio_active)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::Session;
    use crate::testing::FakeDaemon;
    use std::sync::Arc;
    use std::time::Duration;

    fn model(fake: &Arc<FakeDaemon>) -> Model {
        let f = Arc::clone(fake);
        Model::new(Link::inline(Session::new(
            Box::new(move || f.connect()),
            Duration::ZERO,
        )))
    }

    fn settle(m: &mut Model, mut now: f64) -> f64 {
        for _ in 0..6 {
            m.pump(now);
            now += 0.2;
        }
        now
    }

    #[test]
    fn connects_and_fills_in_the_picture() {
        let fake = FakeDaemon::new();
        let mut m = model(&fake);
        assert_eq!(m.conn, Conn::Connecting);
        settle(&mut m, 0.0);
        assert_eq!(m.conn, Conn::Up);
        assert_eq!(m.snapshot.as_ref().unwrap().strips.len(), 16);
        assert!(m.meters.is_some());
        assert!(!m.devices.is_empty());
    }

    #[test]
    fn a_command_shows_at_once_and_reaches_the_daemon() {
        let fake = FakeDaemon::new();
        let mut m = model(&fake);
        let now = settle(&mut m, 0.0);
        assert!(m.command(
            Command::StripGain {
                strip: 2,
                gain_db: -6.0
            },
            now
        ));
        // Visible before any round trip.
        assert_eq!(m.snapshot.as_ref().unwrap().strips[2].config.gain_db, -6.0);
        assert_eq!(fake.commands().len(), 1);
        settle(&mut m, now);
        assert_eq!(m.snapshot.as_ref().unwrap().strips[2].config.gain_db, -6.0);
    }

    #[test]
    fn an_old_snapshot_does_not_undo_a_newer_command() {
        let fake = FakeDaemon::new();
        let mut m = model(&fake);
        let now = settle(&mut m, 0.0);
        // A snapshot reply from before the command arrives after it.
        m.apply(
            Reply::Snapshot {
                generation: 0,
                snapshot: Box::new(fake.snapshot_now()),
            },
            now,
        );
        m.command(
            Command::StripMute {
                strip: 0,
                mute: true,
            },
            now,
        );
        let stale = fake.snapshot_now_with(|s| s.strips[0].config.mute = false);
        m.apply(
            Reply::Snapshot {
                generation: 0,
                snapshot: Box::new(stale),
            },
            now,
        );
        assert!(m.snapshot.as_ref().unwrap().strips[0].config.mute);
    }

    #[test]
    fn a_refused_command_is_reported_and_the_truth_comes_back() {
        let fake = FakeDaemon::new();
        let mut m = model(&fake);
        let now = settle(&mut m, 0.0);
        fake.refuse_commands("no such strip");
        m.command(
            Command::StripMute {
                strip: 0,
                mute: true,
            },
            now,
        );
        settle(&mut m, now);
        assert!(m.notice.as_ref().unwrap().text.contains("no such strip"));
        assert!(
            !m.snapshot.as_ref().unwrap().strips[0].config.mute,
            "the optimistic change was undone by the next snapshot"
        );
    }

    #[test]
    fn without_a_daemon_commands_are_refused_politely() {
        let fake = FakeDaemon::new();
        fake.set_down(true);
        let mut m = model(&fake);
        let now = settle(&mut m, 0.0);
        assert!(matches!(m.conn, Conn::Down(_)));
        assert!(!m.command(Command::Beacon { on: true }, now));
        assert!(m.notice.is_some());
        assert!(fake.commands().is_empty());
        // The daemon appears: the model finds it by itself.
        fake.set_down(false);
        settle(&mut m, now);
        assert_eq!(m.conn, Conn::Up);
    }

    #[test]
    fn notices_expire() {
        let fake = FakeDaemon::new();
        let mut m = model(&fake);
        m.say("hello".into(), 10.0);
        m.pump(10.5);
        assert!(m.notice.is_some());
        m.pump(10.0 + NOTICE_S + 0.1);
        assert!(m.notice.is_none());
    }

    #[test]
    fn peak_hold_holds_then_falls_and_a_new_peak_wins() {
        let lv = |db: f32| MeterLevels {
            peak_db: [db; 8],
            rms_db: [db; 8],
            clipping: false,
        };
        let mut h = PeakHold::default();
        h.update(0.0, &[lv(-6.0)]);
        h.update(0.1, &[lv(-40.0)]);
        assert_eq!(h.db(0, 0), Some(-6.0), "held");
        h.update(PeakHold::HOLD_S + 0.5, &[lv(-90.0)]);
        let fallen = h.db(0, 0).unwrap();
        assert!(fallen < -6.0 && fallen > -40.0, "falling: {fallen}");
        h.update(3.0, &[lv(-3.0)]);
        assert_eq!(h.db(0, 0), Some(-3.0));
        // Silence for a long time reaches the floor and the marker disappears.
        h.update(10.0, &[lv(-120.0)]);
        h.update(20.0, &[lv(-120.0)]);
        assert_eq!(h.db(0, 0), None);
    }

    #[test]
    fn optimistic_apply_covers_the_simple_commands() {
        let fake = FakeDaemon::new();
        let mut s = fake.snapshot_now();
        apply_command(
            &mut s,
            &Command::Route {
                strip: 1,
                bus: 3,
                send: audio_types::RouteSend {
                    enabled: true,
                    gain_db: -3.0,
                    pre_fader: true,
                },
            },
        );
        assert!(s.strips[1].config.sends[3].enabled);
        apply_command(
            &mut s,
            &Command::BusLabel {
                bus: 0,
                label: "Speakers".into(),
            },
        );
        assert_eq!(s.buses[0].config.label.as_str(), "Speakers");
        // Out of range is ignored, never a panic.
        apply_command(
            &mut s,
            &Command::StripMute {
                strip: 200,
                mute: true,
            },
        );
    }
}
