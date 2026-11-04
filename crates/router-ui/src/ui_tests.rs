//! Headless tests of the real screens (`egui_kittest`): they draw `RouterApp` against a scripted
//! daemon, find widgets by their accessible names, and press them with synthetic pointer events.

use crate::app::RouterApp;
use crate::governor::{Rate, Repaint};
use crate::link::{Link, Session};
use crate::model::Conn;
use crate::testing::FakeDaemon;
use crate::view::{PILLAR_W, STRIP_W};
use app_core::types::{Command, PeerState};
use app_core::ui_state::{GraphicsSettings, LayoutMode, UiMode};
use egui::{Event, Modifiers, PointerButton, Pos2, ViewportId, ViewportInfo, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

type H = Harness<'static, RouterApp>;

fn harness_sized(fake: &Arc<FakeDaemon>, w: f32, h: f32) -> H {
    let f = Arc::clone(fake);
    let link = Link::inline(Session::new(Box::new(move || f.connect()), Duration::ZERO));
    let mut app = RouterApp::with_link(link, None);
    app.clock_override_s = Some(0.0);
    let mut harness = Harness::builder()
        .with_size(vec2(w, h))
        // Frames 16 ms apart, so two clicks in a row count as a double click (egui's limit is 0.3 s).
        .with_step_dt(0.016)
        .build_state(|ctx, app: &mut RouterApp| app.run_frame(ctx), app);
    harness.run_steps(3);
    harness
}

/// A daemon already in Expert mode with the strips layout.
fn expert() -> (Arc<FakeDaemon>, H) {
    let fake = FakeDaemon::new();
    fake.edit(|s| s.ui.mode = UiMode::Expert);
    let h = harness_sized(&fake, 1200.0, 720.0);
    (fake, h)
}

/// Move the clock and draw a few frames.
fn advance(h: &mut H, to_s: f64) {
    h.state_mut().clock_override_s = Some(to_s);
    h.run_steps(3);
}

fn pointer(h: &mut H, event: Event) {
    h.input_mut().events.push(event);
    h.step();
}

fn press(h: &mut H, pos: Pos2) {
    pointer(h, Event::PointerMoved(pos));
    pointer(
        h,
        Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::NONE,
        },
    );
}

fn release(h: &mut H, pos: Pos2) {
    pointer(
        h,
        Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed: false,
            modifiers: Modifiers::NONE,
        },
    );
}

fn drag(h: &mut H, from: Pos2, to: Pos2) {
    press(h, from);
    // Several small moves, like a real mouse.
    for k in 1..=6 {
        let t = k as f32 / 6.0;
        pointer(h, Event::PointerMoved(from + (to - from) * t));
    }
    release(h, to);
    h.run_steps(2);
}

fn strip_gains(fake: &FakeDaemon, strip: u8) -> Vec<f32> {
    fake.commands()
        .into_iter()
        .filter_map(|c| match c {
            Command::StripGain { strip: s, gain_db } if s == strip => Some(gain_db),
            _ => None,
        })
        .collect()
}

// SECTION: PLAN §11 S7 acceptance tests
#[test]
fn dragging_a_fader_emits_strip_gain() {
    let (fake, mut h) = expert();
    let fader = h.get_by_label("Fader strip 1").rect();
    // From the 0 dB handle (near the top) down to the middle of the travel: about -24 dB.
    let grab = 22.0;
    let travel = fader.height() - grab;
    let top = Pos2::new(
        fader.center().x,
        fader.bottom() - grab / 2.0 - crate::widgets::db_to_pos(0.0) * travel,
    );
    let mid = Pos2::new(fader.center().x, fader.bottom() - grab / 2.0 - 0.5 * travel);
    drag(&mut h, top, mid);
    let gains = strip_gains(&fake, 0);
    assert!(!gains.is_empty(), "no StripGain was sent");
    let last = *gains.last().unwrap();
    assert!(
        (last - crate::widgets::pos_to_db(0.5)).abs() < 3.0,
        "ended at {last} dB, expected about -24"
    );
    // The daemon applied it, and the fader shows it.
    assert!((fake.snapshot_now().strips[0].config.gain_db - last).abs() < 0.01);
}

#[test]
fn double_clicking_a_fader_goes_back_to_zero_db() {
    let (fake, mut h) = expert();
    fake.edit(|s| s.strips[1].config.gain_db = -30.0);
    advance(&mut h, 1.0);
    h.get_by_label("Fader strip 2").click();
    h.get_by_label("Fader strip 2").click();
    h.run_steps(2);
    // Two quick clicks: egui reports a double click; the last command must be 0 dB.
    let g = strip_gains(&fake, 1);
    assert!(g.contains(&0.0), "{g:?}");
}

#[test]
fn collapsing_a_strip_makes_its_column_28_px() {
    let (fake, mut h) = expert();
    assert_eq!(h.get_by_label("Strip 1 column").rect().width(), STRIP_W);
    h.get_by_label("Collapse strip 1").click();
    h.run_steps(3);
    assert_eq!(h.get_by_label("Strip 1 column").rect().width(), PILLAR_W);
    assert_eq!(PILLAR_W, 28.0);
    // Other strips are untouched, and the choice was sent to the daemon so every UI agrees.
    assert_eq!(h.get_by_label("Strip 2 column").rect().width(), STRIP_W);
    assert!(fake.snapshot_now().ui.strips[0].collapsed_in_strips);
    // And back.
    h.get_by_label("Expand strip 1").click();
    h.run_steps(3);
    assert_eq!(h.get_by_label("Strip 1 column").rect().width(), STRIP_W);
}

#[test]
fn right_clicking_a_routing_button_opens_the_send_knob() {
    let (fake, mut h) = expert();
    assert!(h.query_by_label("Send gain strip 1 to bus 1").is_none());
    h.get_by_label("Route strip 1 to bus 1").click_secondary();
    h.run_steps(2);
    assert!(
        h.query_by_label("Send gain strip 1 to bus 1").is_some(),
        "the knob did not open"
    );
    assert!(h.query_by_label("Pre-fader strip 1 to bus 1").is_some());
    // Opening the knob must not switch the send on.
    assert!(fake.commands().is_empty(), "{:?}", fake.commands());
    // A left click does switch it.
    h.get_by_label("Route strip 1 to bus 1").click();
    h.run_steps(2);
    assert!(
        matches!(fake.commands().as_slice(), [Command::Route { strip: 0, bus: 0, send }] if send.enabled)
    );
}

#[test]
fn dragging_the_send_knob_changes_the_send_gain() {
    let (fake, mut h) = expert();
    h.get_by_label("Route strip 1 to bus 1").click_secondary();
    h.run_steps(2);
    let k = h.get_by_label("Send gain strip 1 to bus 1").rect();
    drag(&mut h, k.center(), k.center() + vec2(0.0, -40.0));
    let sends: Vec<_> = fake
        .commands()
        .into_iter()
        .filter_map(|c| match c {
            Command::Route {
                strip: 0,
                bus: 0,
                send,
            } => Some(send),
            _ => None,
        })
        .collect();
    assert!(sends.last().is_some_and(|s| s.gain_db > 0.0), "{sends:?}");
}
// !SECTION: PLAN §11 S7 acceptance tests

// SECTION: The repaint governor, in the real app
#[test]
fn a_minimized_window_requests_no_repaint_in_rich_and_eco() {
    for gfx in [None, Some(GraphicsSettings::ECO)] {
        let (fake, mut h) = expert();
        fake.edit(|s| s.ui.graphics = gfx);
        fake.make_noise(true);
        advance(&mut h, 2.0);
        // Visible and loud: a timed repaint is requested.
        assert!(matches!(h.state().last_decision.1, Repaint::After(_)));
        // Minimize.
        h.input_mut().viewports.insert(
            ViewportId::ROOT,
            ViewportInfo {
                minimized: Some(true),
                focused: Some(false),
                ..Default::default()
            },
        );
        h.run_steps(2);
        assert_eq!(
            h.state().last_decision,
            (Rate::Paused, Repaint::None),
            "gfx {gfx:?}"
        );
        let delay = h
            .output()
            .viewport_output
            .get(&ViewportId::ROOT)
            .map(|v| v.repaint_delay);
        assert_eq!(
            delay,
            Some(Duration::MAX),
            "egui was asked to repaint while minimized"
        );
    }
}

#[test]
fn silence_longer_than_half_a_second_drops_to_silent_fps() {
    for (gfx, active, silent) in [(None, 60, 15), (Some(GraphicsSettings::ECO), 30, 5)] {
        let (fake, mut h) = expert();
        fake.edit(|s| s.ui.graphics = gfx);
        fake.make_noise(true);
        advance(&mut h, 10.0);
        advance(&mut h, 10.2); // meters refresh: still loud
        let fps = |h: &H| match h.state().last_decision.1 {
            Repaint::After(d) => (1.0 / d.as_secs_f64()).round() as u32,
            Repaint::None => 0,
        };
        assert_eq!(fps(&h), active, "loud, {gfx:?}");
        fake.make_noise(false);
        advance(&mut h, 10.4); // first quiet meters arrive
        assert_eq!(fps(&h), active, "quiet for under 500 ms, {gfx:?}");
        advance(&mut h, 11.0);
        assert_eq!(h.state().last_decision.0, Rate::Silent);
        assert_eq!(fps(&h), silent, "quiet for over 500 ms, {gfx:?}");
    }
}

#[test]
fn an_unfocused_window_runs_at_silent_fps() {
    let (fake, mut h) = expert();
    fake.make_noise(true);
    advance(&mut h, 1.0);
    h.input_mut().viewports.insert(
        ViewportId::ROOT,
        ViewportInfo {
            minimized: None,
            focused: Some(false),
            ..Default::default()
        },
    );
    h.run_steps(2);
    assert_eq!(h.state().last_decision.0, Rate::Silent);
}
// !SECTION: The repaint governor, in the real app

// SECTION: Other screens
#[test]
fn the_matrix_toggles_a_send_and_right_click_resets_the_gain() {
    let (fake, mut h) = expert();
    h.get_by_label("Matrix").click();
    h.run_steps(3);
    assert_eq!(fake.snapshot_now().ui.layout, LayoutMode::Matrix);
    assert!(
        h.query_by_label("Fader strip 1").is_none(),
        "the strips are gone"
    );
    h.get_by_label("Route strip 2 to bus 3").click();
    h.run_steps(2);
    assert!(fake.snapshot_now().strips[1].config.sends[2].enabled);
    fake.edit(|s| s.strips[1].config.sends[2].gain_db = -9.0);
    advance(&mut h, 5.0);
    h.get_by_label("Route strip 2 to bus 3").click_secondary();
    h.run_steps(2);
    assert_eq!(fake.snapshot_now().strips[1].config.sends[2].gain_db, 0.0);
}

#[test]
fn dragging_a_matrix_cell_sets_and_enables_the_send() {
    let (fake, mut h) = expert();
    h.get_by_label("Matrix").click();
    h.run_steps(3);
    let c = h.get_by_label("Route strip 1 to bus 1").rect();
    drag(&mut h, c.center(), c.center() + vec2(0.0, -20.0));
    let s = fake.snapshot_now().strips[0].config.sends[0];
    assert!(s.enabled && s.gain_db > 0.0, "{s:?}");
}

#[test]
fn simple_mode_connects_a_send_to_a_found_machine() {
    let fake = FakeDaemon::new();
    fake.edit(|s| {
        s.peers.push(PeerState {
            addr: SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 20), 6980),
            device_name: "Desktop".into(),
            application: "VoiceMeeter".into(),
            host_name: "DESKTOP".into(),
            is_lan_router: false,
            rtt_ms: None,
            age_ms: 0,
        });
    });
    let mut h = harness_sized(&fake, 1000.0, 600.0);
    assert!(
        h.query_by_label("Fader strip 1").is_none(),
        "Simple mode has no strips"
    );
    h.get_by_label("Connect send").click();
    h.run_steps(3);
    let cmds = fake.commands();
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::StripSource { strip: 0, .. })),
        "{cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| matches!(c,
            Command::BusSink { bus: 0, sink: audio_types::SinkEndpoint::LanOut(t), .. } if t.peer_ipv4 == [192, 168, 1, 20])),
        "{cmds:?}"
    );
    assert!(matches!(cmds.last(), Some(Command::Route { send, .. }) if send.enabled));
    // The same strip is visible in Expert mode, labelled by the card.
    advance(&mut h, 3.0);
    h.get_by_label("Disconnect send").click();
    h.run_steps(3);
    assert!(
        h.query_by_label("Connect send").is_some(),
        "back to connectable"
    );
}

#[test]
fn without_a_daemon_the_banner_offers_to_start_it() {
    let fake = FakeDaemon::new();
    fake.set_down(true);
    let mut h = harness_sized(&fake, 900.0, 500.0);
    assert!(matches!(h.state().model.conn, Conn::Down(_)));
    assert!(h.query_by_label("Start daemon").is_some());
    assert!(h.query_by_label_contains("is not running").is_some());
    // The daemon comes up: the banner goes away by itself.
    fake.set_down(false);
    advance(&mut h, 2.0);
    advance(&mut h, 4.0);
    assert_eq!(h.state().model.conn, Conn::Up);
    assert!(h.query_by_label("Start daemon").is_none());
}

#[test]
fn a_narrow_window_starts_with_pillars_and_full_width_inspectors() {
    let fake = FakeDaemon::new();
    fake.edit(|s| s.ui.mode = UiMode::Expert);
    let mut h = harness_sized(&fake, 480.0, 700.0);
    assert_eq!(h.get_by_label("Strip 1 column").rect().width(), PILLAR_W);
    h.get_by_label("Expand strip 1").click();
    h.run_steps(3);
    assert_eq!(h.get_by_label("Strip 1 column").rect().width(), STRIP_W);
    h.get_by_label("Open inspector strip 1").click();
    h.run_steps(3);
    let page = h.get_by_label("Inspector strip 1").rect();
    assert!(
        page.width() >= 440.0,
        "the inspector should span the window, got {}",
        page.width()
    );
    assert!(
        h.query_by_label("Fader strip 1").is_none(),
        "strips are hidden behind it"
    );
    h.get_by_label("Close inspector").click();
    h.run_steps(3);
    assert!(h.query_by_label("Fader strip 1").is_some());
}

#[test]
fn the_inspector_edits_the_compressor_and_sends_one_dsp_command() {
    let (fake, mut h) = expert();
    h.get_by_label("Open inspector strip 1").click();
    h.run_steps(3);
    assert!(h.query_by_label("Inspector strip 1").is_some());
    h.get_by_label("Compressor").click();
    h.run_steps(2);
    let dsp: Vec<_> = fake
        .commands()
        .into_iter()
        .filter_map(|c| match c {
            Command::StripDsp { strip: 0, dsp } => Some(dsp),
            _ => None,
        })
        .collect();
    assert_eq!(dsp.len(), 1, "{dsp:?}");
    assert!(dsp[0].comp.enabled);
}

#[test]
fn a_refused_command_shows_in_the_status_line() {
    let (fake, mut h) = expert();
    fake.refuse_commands("no such strip");
    h.get_by_label("Mute strip 1").click();
    h.run_steps(4);
    assert!(h.query_by_label_contains("no such strip").is_some());
    // The optimistic mute was taken back.
    advance(&mut h, 5.0);
    assert!(
        !h.state().model.snapshot.as_ref().unwrap().strips[0]
            .config
            .mute
    );
}

#[test]
fn recording_is_offered_but_disabled_until_there_is_a_recorder() {
    let (_fake, h) = expert();
    let rec = h.get_by_label_contains("Record");
    assert!(rec.accesskit_node().is_disabled());
}
// !SECTION: Other screens
