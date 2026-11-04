//! What the compact phone screen shows, and what a tap turns into (PLAN.md §9.1 Simple mode).
//!
//! * No UI toolkit in here. `screen()` turns the engine's snapshot, its meters and the lifecycle
//!   state into plain data a view draws; `send_commands` / `receive_commands` turn a tap into
//!   `ControlApi` commands (CLAUDE.md law 5: the UI is a thin view over `ControlApi`).
//! * Like the desktop Simple mode it owns one strip and one bus per card, found by label, so a
//!   profile made on the desktop opens the same way. Everything here is host-tested.

use crate::lifecycle::{Lifecycle, Warning};
use app_core::types::{Command, LinkState, MeterSnapshot, StateSnapshot};
use audio_types::{
    AndroidInputPreset, FixedStr, LanRxConfig, LanTxConfig, NetworkBufferPreset,
    PlaybackCaptureFilter, RouteSend, SinkEndpoint, SourceEndpoint,
};

pub const SEND_LABEL: &str = "Simple send";
pub const RECEIVE_LABEL: &str = "Simple receive";
/// The VBAN stream name the phone sends and listens for.
pub const STREAM_NAME: &str = "Stream1";

/// The text of the banner and of the notification when app audio ends (PLAN.md §6.3 step 5).
pub const APP_AUDIO_PAUSED: &str = "App audio paused \u{2014} tap to resume";

// SECTION: Finding the cards' strips and buses
fn find_strip(snap: &StateSnapshot, label: &str) -> Option<u8> {
    let by_label = snap
        .strips
        .iter()
        .position(|s| s.config.label.as_str() == label);
    let free = || {
        snap.strips
            .iter()
            .position(|s| s.config.source == SourceEndpoint::Unassigned)
    };
    by_label.or_else(free).and_then(|i| u8::try_from(i).ok())
}

fn find_bus(snap: &StateSnapshot, label: &str) -> Option<u8> {
    let by_label = snap
        .buses
        .iter()
        .position(|b| b.config.label.as_str() == label);
    let free = || {
        snap.buses
            .iter()
            .position(|b| b.config.sink == SinkEndpoint::Unassigned)
    };
    by_label.or_else(free).and_then(|i| u8::try_from(i).ok())
}
// !SECTION: Finding the cards' strips and buses

// SECTION: Taps to commands
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendSource {
    Off,
    Mic,
    /// Everything the phone plays, or one app (or all but one).
    AppAudio(PlaybackCaptureFilter),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// Every strip or bus already has something in it.
    NothingFree,
    /// The text is not `a.b.c.d`.
    BadAddress(String),
}

impl core::fmt::Display for PlanError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NothingFree => f.write_str("no free strip or bus: clear one on the desktop"),
            Self::BadAddress(s) => write!(f, "{s:?} is not an IPv4 address"),
        }
    }
}

/// `"192.168.1.20"` to four bytes.
pub fn parse_ipv4(text: &str) -> Result<[u8; 4], PlanError> {
    let bad = || PlanError::BadAddress(text.trim().to_owned());
    let mut out = [0_u8; 4];
    let mut parts = text.trim().split('.');
    for slot in &mut out {
        *slot = parts
            .next()
            .ok_or_else(bad)?
            .parse::<u8>()
            .map_err(|_| bad())?;
    }
    if parts.next().is_some() {
        return Err(bad());
    }
    Ok(out)
}

fn on_send() -> RouteSend {
    RouteSend {
        enabled: true,
        gain_db: 0.0,
        pre_fader: false,
    }
}

/// The Send card: `source` goes to the PC at `peer`. `Off` closes the source and keeps the bus.
pub fn send_commands(
    snap: &StateSnapshot,
    source: SendSource,
    peer: ([u8; 4], u16),
) -> Result<Vec<Command>, PlanError> {
    let strip = find_strip(snap, SEND_LABEL).ok_or(PlanError::NothingFree)?;
    let bus = find_bus(snap, SEND_LABEL).ok_or(PlanError::NothingFree)?;
    let (endpoint, channels) = match source {
        SendSource::Off => (SourceEndpoint::Unassigned, 1),
        // ? One microphone channel. The strip's pan puts it in the middle of the stereo bus.
        SendSource::Mic => (
            SourceEndpoint::AndroidMic {
                preset: AndroidInputPreset::VoicePerformance,
            },
            1,
        ),
        SendSource::AppAudio(filter) => (SourceEndpoint::AndroidPlaybackCapture { filter }, 2),
    };
    let tx = LanTxConfig {
        peer_ipv4: peer.0,
        port: peer.1,
        stream_name: FixedStr::from_str_trunc(STREAM_NAME),
        channels: 2,
        ..LanTxConfig::default()
    };
    Ok(vec![
        Command::StripLabel {
            strip,
            label: SEND_LABEL.to_owned(),
        },
        Command::BusLabel {
            bus,
            label: SEND_LABEL.to_owned(),
        },
        Command::BusSink {
            bus,
            sink: SinkEndpoint::LanOut(tx),
            channels: 2,
        },
        Command::StripSource {
            strip,
            source: endpoint,
            channels,
        },
        Command::Route {
            strip,
            bus,
            send: if source == SendSource::Off {
                RouteSend::default()
            } else {
                on_send()
            },
        },
    ])
}

/// The Receive card: audio from the PC (`Stream1`, any address) plays on the phone's output.
pub fn receive_commands(
    snap: &StateSnapshot,
    on: bool,
    buffer: NetworkBufferPreset,
) -> Result<Vec<Command>, PlanError> {
    let strip = find_strip(snap, RECEIVE_LABEL).ok_or(PlanError::NothingFree)?;
    let bus = find_bus(snap, RECEIVE_LABEL).ok_or(PlanError::NothingFree)?;
    let (source, sink) = if on {
        (
            SourceEndpoint::LanIn(LanRxConfig {
                source_ipv4: [0, 0, 0, 0],
                stream_name: FixedStr::from_str_trunc(STREAM_NAME),
                buffer,
                adaptive: true,
            }),
            SinkEndpoint::HardwareOutput {
                device_id: FixedStr::from_str_trunc(platform_io::android_rules::DEFAULT_DEVICE_ID),
            },
        )
    } else {
        (SourceEndpoint::Unassigned, SinkEndpoint::Unassigned)
    };
    Ok(vec![
        Command::StripLabel {
            strip,
            label: RECEIVE_LABEL.to_owned(),
        },
        Command::BusLabel {
            bus,
            label: RECEIVE_LABEL.to_owned(),
        },
        Command::BusSink {
            bus,
            sink,
            channels: 2,
        },
        Command::StripSource {
            strip,
            source,
            channels: 2,
        },
        Command::Route {
            strip,
            bus,
            send: if on { on_send() } else { RouteSend::default() },
        },
    ])
}
// !SECTION: Taps to commands

// SECTION: The screen
#[derive(Clone, Debug, PartialEq)]
pub enum Banner {
    /// PLAN.md §6.3 step 5; tapping it asks for the screen-capture consent again.
    AppAudioPaused,
    /// Something did not start; the text says why.
    Problem(String),
}

impl Banner {
    pub fn text(&self) -> &str {
        match self {
            Self::AppAudioPaused => APP_AUDIO_PAUSED,
            Self::Problem(t) => t,
        }
    }

    pub fn tap_to_resume(&self) -> bool {
        matches!(self, Self::AppAudioPaused)
    }
}

/// How the link of a card looks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkView {
    Off,
    Running,
    Failed(String),
}

fn link_view(link: &LinkState, assigned: bool) -> LinkView {
    match link {
        _ if !assigned => LinkView::Off,
        LinkState::Idle => LinkView::Off,
        LinkState::Running => LinkView::Running,
        LinkState::Failed(why) => LinkView::Failed(why.clone()),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SendCard {
    pub source: Option<SendSource>,
    /// `0.0..=1.0` meter fill.
    pub level: f32,
    /// `ip:port` the audio goes to.
    pub target: Option<String>,
    pub link: LinkView,
    pub clipping: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReceiveCard {
    pub on: bool,
    pub level: f32,
    pub link: LinkView,
    pub buffer_target_ms: f32,
    pub buffer_ms: f32,
    pub lost: u32,
    pub underruns: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Screen {
    /// The foreground service is running (`Start` pressed).
    pub running: bool,
    pub banner: Option<Banner>,
    pub send: SendCard,
    pub receive: ReceiveCard,
    /// The mixer missed its deadline or rejected commands.
    pub engine_warning: Option<String>,
}

/// dB to a `0.0..=1.0` bar: -60 dB is empty, 0 dB is full.
pub fn level_fraction(db: f32) -> f32 {
    if db.is_nan() {
        return 0.0;
    }
    ((db + 60.0) / 60.0).clamp(0.0, 1.0)
}

fn classify(source: &SourceEndpoint) -> Option<SendSource> {
    match source {
        SourceEndpoint::AndroidMic { .. } | SourceEndpoint::HardwareInput { .. } => {
            Some(SendSource::Mic)
        }
        SourceEndpoint::AndroidPlaybackCapture { filter } => Some(SendSource::AppAudio(*filter)),
        SourceEndpoint::DeviceLoopback { .. } => Some(SendSource::AppAudio(
            PlaybackCaptureFilter::AllMediaAndGames,
        )),
        _ => None,
    }
}

fn problem_text(w: Warning) -> &'static str {
    match w {
        Warning::NeedsUiVisible => "Open the app to start the microphone service",
        Warning::ConsentDenied => "Screen-capture permission was refused, so app audio is off",
        Warning::ProjectionFailed => "Could not start app-audio capture",
        Warning::ForegroundRefused => "Android refused the foreground service",
    }
}

/// Build the whole screen. `last_warning` is the newest `Warning` the glue reported, cleared by
/// the view when it is dismissed.
pub fn screen(
    snap: &StateSnapshot,
    meters: &MeterSnapshot,
    life: &Lifecycle,
    last_warning: Option<Warning>,
) -> Screen {
    let strip = find_strip(snap, SEND_LABEL).map(usize::from);
    let bus = find_bus(snap, SEND_LABEL).map(usize::from);
    let r_strip = find_strip(snap, RECEIVE_LABEL).map(usize::from);

    let send = {
        let st = strip.and_then(|i| snap.strips.get(i));
        let source = st.and_then(|s| classify(&s.config.source));
        let level = strip
            .and_then(|i| meters.strips.get(i))
            .map_or(0.0, |m| level_fraction(m.peak_db[0].max(m.peak_db[1])));
        let target = bus
            .and_then(|i| snap.buses.get(i))
            .and_then(|b| match b.config.sink {
                SinkEndpoint::LanOut(tx) => Some(format!(
                    "{}.{}.{}.{}:{}",
                    tx.peer_ipv4[0], tx.peer_ipv4[1], tx.peer_ipv4[2], tx.peer_ipv4[3], tx.port
                )),
                _ => None,
            });
        SendCard {
            source,
            level,
            target,
            link: st.map_or(LinkView::Off, |s| link_view(&s.link, source.is_some())),
            clipping: strip
                .and_then(|i| meters.strips.get(i))
                .is_some_and(|m| m.clipping),
        }
    };

    let receive = {
        let st = r_strip.and_then(|i| snap.strips.get(i));
        let on = st.is_some_and(|s| matches!(s.config.source, SourceEndpoint::LanIn(_)));
        let lan = r_strip.and_then(|i| meters.lan_rx.get(i));
        ReceiveCard {
            on,
            level: r_strip
                .and_then(|i| meters.strips.get(i))
                .map_or(0.0, |m| level_fraction(m.peak_db[0].max(m.peak_db[1]))),
            link: st.map_or(LinkView::Off, |s| link_view(&s.link, on)),
            buffer_target_ms: lan.map_or(0.0, |l| l.target_ms),
            buffer_ms: lan.map_or(0.0, |l| l.buffer_ms),
            lost: lan.map_or(0, |l| l.packets_lost),
            underruns: lan.map_or(0, |l| l.underruns),
        }
    };

    let banner = if life.app_audio_paused() {
        Some(Banner::AppAudioPaused)
    } else {
        last_warning.map(|w| Banner::Problem(problem_text(w).to_owned()))
    };

    let e = meters.engine;
    let engine_warning = (e.rt_overruns > 0 || e.rejected_commands > 0).then(|| {
        format!(
            "mixer: {} missed deadlines, {} rejected commands",
            e.rt_overruns, e.rejected_commands
        )
    });

    Screen {
        running: life.fgs() != crate::lifecycle::Fgs::None,
        banner,
        send,
        receive,
        engine_warning,
    }
}
// !SECTION: The screen

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::{Event, Lifecycle};
    use app_core::types::{
        BusState, EngineStatus, LanRxLevels, MeterLevels, PolicyState, StripState,
    };
    use app_core::ui_state::UiLayout;

    /// An empty router: 4 strips and 2 buses with nothing in them.
    fn empty_snapshot() -> StateSnapshot {
        let strip = StripState {
            config: app_core::profile::default_strip(0),
            link: LinkState::Idle,
            stream: None,
        };
        let bus = BusState {
            config: app_core::profile::default_bus(0),
            link: LinkState::Idle,
            stream: None,
            tx: None,
            recording: None,
            record_error: None,
        };
        StateSnapshot {
            protocol: 1,
            name: "t".into(),
            backend: "aaudio".into(),
            capabilities: platform_io::Capabilities::default(),
            rx_port: 6980,
            strips: vec![strip; 4],
            buses: vec![bus; 2],
            policy: PolicyState {
                accept_any: true,
                allowed: Vec::new(),
                trust_discovered: false,
                effective: Vec::new(),
            },
            beacon: false,
            peers: Vec::new(),
            discovery: app_core::types::DiscoveryState::Off,
            discovered: Vec::new(),
            seen_senders: Vec::new(),
            profile: None,
            profiles: Vec::new(),
            ui: UiLayout::default(),
        }
    }

    fn quiet_meters() -> MeterSnapshot {
        let silent = MeterLevels {
            peak_db: [-120.0; 8],
            rms_db: [-120.0; 8],
            clipping: false,
        };
        MeterSnapshot {
            strips: vec![silent; 4],
            buses: vec![silent; 2],
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
                4
            ],
            any_audio_active: false,
            engine: EngineStatus::default(),
        }
    }

    #[test]
    fn addresses_parse_strictly() {
        assert_eq!(parse_ipv4(" 192.168.1.20 "), Ok([192, 168, 1, 20]));
        for bad in ["", "1.2.3", "1.2.3.4.5", "1.2.3.256", "a.b.c.d", "1..2.3"] {
            assert!(parse_ipv4(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_levels_map_minus_sixty_to_zero_db_onto_the_bar() {
        assert_eq!(level_fraction(-120.0), 0.0);
        assert_eq!(level_fraction(-60.0), 0.0);
        assert_eq!(level_fraction(-30.0), 0.5);
        assert_eq!(level_fraction(6.0), 1.0);
        assert_eq!(level_fraction(f32::NAN), 0.0);
    }

    #[test]
    fn the_mic_goes_to_the_pc_over_a_lan_bus() {
        let snap = empty_snapshot();
        let cmds = send_commands(&snap, SendSource::Mic, ([192, 168, 1, 20], 6980)).unwrap();
        assert!(cmds.iter().any(|c| matches!(
            c,
            Command::StripSource {
                source: SourceEndpoint::AndroidMic {
                    preset: AndroidInputPreset::VoicePerformance
                },
                channels: 1,
                ..
            }
        )));
        let sink = cmds.iter().find_map(|c| match c {
            Command::BusSink { sink, .. } => Some(*sink),
            _ => None,
        });
        match sink {
            Some(SinkEndpoint::LanOut(tx)) => {
                assert_eq!(tx.peer_ipv4, [192, 168, 1, 20]);
                assert_eq!(tx.port, 6980);
                assert_eq!(tx.stream_name.as_str(), STREAM_NAME);
            }
            other => panic!("not a LAN output: {other:?}"),
        }
        assert!(cmds.iter().any(|c| matches!(
            c,
            Command::Route { send, .. } if send.enabled
        )));
    }

    #[test]
    fn app_audio_carries_its_filter_and_turning_it_off_cuts_the_route() {
        let snap = empty_snapshot();
        let one_app = PlaybackCaptureFilter::OnlyUid(10_123);
        let on =
            send_commands(&snap, SendSource::AppAudio(one_app), ([10, 0, 0, 2], 6980)).unwrap();
        assert!(on.iter().any(|c| matches!(
            c,
            Command::StripSource {
                source: SourceEndpoint::AndroidPlaybackCapture { filter },
                channels: 2,
                ..
            } if *filter == one_app
        )));
        let off = send_commands(&snap, SendSource::Off, ([10, 0, 0, 2], 6980)).unwrap();
        assert!(off.iter().any(|c| matches!(
            c,
            Command::StripSource {
                source: SourceEndpoint::Unassigned,
                ..
            }
        )));
        assert!(off.iter().any(|c| matches!(
            c,
            Command::Route { send, .. } if !send.enabled
        )));
    }

    #[test]
    fn the_receive_card_listens_for_the_stream_and_plays_on_the_phone() {
        let snap = empty_snapshot();
        let cmds = receive_commands(&snap, true, NetworkBufferPreset::Medium).unwrap();
        assert!(cmds.iter().any(|c| matches!(
            c,
            Command::StripSource {
                source: SourceEndpoint::LanIn(rx),
                ..
            } if rx.buffer == NetworkBufferPreset::Medium && rx.source_ipv4 == [0, 0, 0, 0]
        )));
        assert!(cmds.iter().any(|c| matches!(
            c,
            Command::BusSink {
                sink: SinkEndpoint::HardwareOutput { .. },
                ..
            }
        )));
        let off = receive_commands(&snap, false, NetworkBufferPreset::Medium).unwrap();
        assert!(off.iter().any(|c| matches!(
            c,
            Command::BusSink {
                sink: SinkEndpoint::Unassigned,
                ..
            }
        )));
    }

    #[test]
    fn the_two_cards_never_share_a_strip_or_a_bus() {
        let mut snap = empty_snapshot();
        let send = send_commands(&snap, SendSource::Mic, ([1, 2, 3, 4], 6980)).unwrap();
        // Apply the labels and the sources the way the engine would.
        for c in &send {
            match c {
                Command::StripLabel { strip, label } => {
                    snap.strips[usize::from(*strip)].config.label = FixedStr::from_str_trunc(label);
                }
                Command::StripSource { strip, source, .. } => {
                    snap.strips[usize::from(*strip)].config.source = *source;
                }
                Command::BusLabel { bus, label } => {
                    snap.buses[usize::from(*bus)].config.label = FixedStr::from_str_trunc(label);
                }
                Command::BusSink { bus, sink, .. } => {
                    snap.buses[usize::from(*bus)].config.sink = *sink;
                }
                _ => {}
            }
        }
        let recv = receive_commands(&snap, true, NetworkBufferPreset::Fast).unwrap();
        let strip_of = |cmds: &[Command]| {
            cmds.iter().find_map(|c| match c {
                Command::StripSource { strip, .. } => Some(*strip),
                _ => None,
            })
        };
        let bus_of = |cmds: &[Command]| {
            cmds.iter().find_map(|c| match c {
                Command::BusSink { bus, .. } => Some(*bus),
                _ => None,
            })
        };
        assert_ne!(strip_of(&send), strip_of(&recv));
        assert_ne!(bus_of(&send), bus_of(&recv));
    }

    #[test]
    fn a_full_router_says_so() {
        let mut snap = empty_snapshot();
        for s in &mut snap.strips {
            s.config.source = SourceEndpoint::AndroidMic {
                preset: AndroidInputPreset::Unprocessed,
            };
            s.config.label = FixedStr::from_str_trunc("x");
        }
        assert_eq!(
            send_commands(&snap, SendSource::Mic, ([1, 1, 1, 1], 1)),
            Err(PlanError::NothingFree)
        );
    }

    #[test]
    fn the_banner_shows_the_paused_text_with_a_tap_to_resume() {
        let snap = empty_snapshot();
        let meters = quiet_meters();
        let mut life = Lifecycle::new();
        life.handle(Event::UiVisible(true));
        life.handle(Event::StreamingWanted(true));
        life.handle(Event::ForegroundStarted {
            with_projection: false,
        });
        life.handle(Event::AppAudioRequested);
        life.handle(Event::ConsentResult { granted: true });
        life.handle(Event::ForegroundStarted {
            with_projection: true,
        });
        life.handle(Event::ProjectionReady);
        assert_eq!(screen(&snap, &meters, &life, None).banner, None);
        life.handle(Event::ProjectionStopped);
        let s = screen(&snap, &meters, &life, None);
        let banner = s.banner.expect("banner");
        assert_eq!(banner.text(), "App audio paused \u{2014} tap to resume");
        assert!(banner.tap_to_resume());
        assert!(s.running, "the microphone service is still up");
    }

    #[test]
    fn a_warning_is_a_banner_that_does_not_offer_to_resume() {
        let snap = empty_snapshot();
        let s = screen(
            &snap,
            &quiet_meters(),
            &Lifecycle::new(),
            Some(Warning::ConsentDenied),
        );
        let b = s.banner.expect("banner");
        assert!(!b.tap_to_resume());
        assert!(b.text().contains("refused"));
        assert!(!s.running);
    }

    #[test]
    fn the_cards_read_their_own_strips_levels() {
        let mut snap = empty_snapshot();
        let mut meters = quiet_meters();
        let cmds = receive_commands(&snap, true, NetworkBufferPreset::Fast).unwrap();
        let strip = cmds
            .iter()
            .find_map(|c| match c {
                Command::StripSource { strip, source, .. } => Some((*strip, *source)),
                _ => None,
            })
            .unwrap();
        snap.strips[usize::from(strip.0)].config.source = strip.1;
        snap.strips[usize::from(strip.0)].config.label = FixedStr::from_str_trunc(RECEIVE_LABEL);
        snap.strips[usize::from(strip.0)].link = LinkState::Running;
        meters.strips[usize::from(strip.0)].peak_db[0] = -30.0;
        meters.lan_rx[usize::from(strip.0)].target_ms = 24.0;
        meters.lan_rx[usize::from(strip.0)].underruns = 2;
        let s = screen(&snap, &meters, &Lifecycle::new(), None);
        assert!(s.receive.on);
        assert_eq!(s.receive.level, 0.5);
        assert_eq!(s.receive.link, LinkView::Running);
        assert_eq!(s.receive.buffer_target_ms, 24.0);
        assert_eq!(s.receive.underruns, 2);
        assert_eq!(s.send.source, None);
    }

    #[test]
    fn engine_trouble_is_reported() {
        let snap = empty_snapshot();
        let mut meters = quiet_meters();
        assert_eq!(
            screen(&snap, &meters, &Lifecycle::new(), None).engine_warning,
            None
        );
        meters.engine.rt_overruns = 3;
        assert!(
            screen(&snap, &meters, &Lifecycle::new(), None)
                .engine_warning
                .is_some_and(|w| w.contains("3 missed"))
        );
    }
}
