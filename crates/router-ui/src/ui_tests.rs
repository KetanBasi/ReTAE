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
use audio_types::{FixedStr, SinkEndpoint};
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
fn the_toolbar_record_button_waits_for_a_bus_with_an_output() {
    let (fake, mut h) = expert();
    assert!(
        h.get_by_label("⏺ Record").accesskit_node().is_disabled(),
        "no bus has an output yet"
    );
    fake.edit(|s| {
        s.buses[0].config.sink = SinkEndpoint::HardwareOutput {
            device_id: FixedStr::from_str_trunc("mock-out-1"),
        };
    });
    advance(&mut h, 1.0);
    let rec = h.get_by_label("⏺ Record");
    assert!(!rec.accesskit_node().is_disabled());
    fake.clear_commands();
    rec.click();
    h.run_steps(3);
    assert!(
        fake.commands().contains(&Command::BusRecord {
            bus: 0,
            enabled: true
        }),
        "{:?}",
        fake.commands()
    );
}

#[test]
fn a_running_recording_shows_its_length_and_the_stop_button_stops_it() {
    let (fake, mut h) = expert();
    fake.edit(|s| {
        s.buses[0].config.record = true;
        s.buses[0].recording = Some(app_core::types::RecordingState {
            path: "/tmp/take.wav".into(),
            format: app_core::types::RecordFormat::Wav24,
            frames: 48_000 * 75,
            seconds: 75.0,
            error: None,
        });
    });
    advance(&mut h, 1.0);
    let stop = h.get_by_label_contains("Stop recording 1:15");
    fake.clear_commands();
    stop.click();
    h.run_steps(3);
    assert!(
        fake.commands().contains(&Command::BusRecord {
            bus: 0,
            enabled: false
        }),
        "{:?}",
        fake.commands()
    );
}

#[test]
fn a_bus_has_its_own_record_button_and_shows_why_a_take_failed() {
    let (fake, mut h) = expert();
    h.get_by_label("Record bus 1").click();
    h.run_steps(3);
    assert!(fake.commands().contains(&Command::BusRecord {
        bus: 0,
        enabled: true
    }));
    fake.edit(|s| s.buses[1].record_error = Some("disk full".into()));
    advance(&mut h, 2.0);
    assert!(h.query_by_label_contains("disk full").is_some());
}

#[test]
fn clock_text_is_minutes_and_seconds() {
    use crate::toolbar::clock;
    assert_eq!(clock(0.0), "0:00");
    assert_eq!(clock(75.9), "1:15");
    assert_eq!(clock(3_725.0), "1:02:05");
    assert_eq!(clock(-3.0), "0:00");
}
// !SECTION: Other screens

// SECTION: Patchbay Cards
/// Expert mode, Patchbay layout, a wide window.
fn patchbay() -> (Arc<FakeDaemon>, H) {
    let fake = FakeDaemon::new();
    fake.edit(|s| {
        s.ui.mode = UiMode::Expert;
        s.ui.layout = LayoutMode::PatchbayCards;
    });
    let h = harness_sized(&fake, 1200.0, 720.0);
    (fake, h)
}

fn secondary_click(h: &mut H, pos: Pos2) {
    pointer(h, Event::PointerMoved(pos));
    for pressed in [true, false] {
        pointer(
            h,
            Event::PointerButton {
                pos,
                button: PointerButton::Secondary,
                pressed,
                modifiers: Modifiers::NONE,
            },
        );
    }
    h.run_steps(2);
}

fn routes(fake: &FakeDaemon) -> Vec<(u8, u8, bool)> {
    fake.commands()
        .into_iter()
        .filter_map(|c| match c {
            Command::Route { strip, bus, send } => Some((strip, bus, send.enabled)),
            _ => None,
        })
        .collect()
}

#[test]
fn the_patchbay_toggle_is_live_and_draws_cards_with_sockets() {
    let (fake, mut h) = expert();
    h.get_by_label("Patchbay").click();
    h.run_steps(3);
    assert_eq!(fake.snapshot_now().ui.layout, LayoutMode::PatchbayCards);
    for name in [
        "Output socket strip 1",
        "Output socket strip 4",
        "Input socket bus 1",
        "Input socket bus 3",
        "Fader strip 1",
        "Mute bus 1",
    ] {
        assert!(h.query_by_label(name).is_some(), "missing {name}");
    }
    // Sources on the left, buses on the right.
    let src = h.get_by_label("Output socket strip 1").rect().center();
    let bus = h.get_by_label("Input socket bus 1").rect().center();
    assert!(src.x < bus.x - 100.0, "{src:?} {bus:?}");
}

#[test]
fn a_card_has_mute_solo_and_a_fader_like_a_strip() {
    let (fake, mut h) = patchbay();
    h.get_by_label("Mute strip 2").click();
    h.get_by_label("Solo strip 2").click();
    h.get_by_label("Mute bus 2").click();
    h.run_steps(2);
    let cmds = fake.commands();
    assert!(
        cmds.contains(&Command::StripMute {
            strip: 1,
            mute: true
        }),
        "{cmds:?}"
    );
    assert!(
        cmds.contains(&Command::StripSolo {
            strip: 1,
            solo: true
        }),
        "{cmds:?}"
    );
    assert!(
        cmds.contains(&Command::BusMute { bus: 1, mute: true }),
        "{cmds:?}"
    );

    // A key press on the focused fader moves it.
    h.get_by_label("Fader strip 1").focus();
    h.key_press(egui::Key::ArrowLeft);
    h.run_steps(2);
    assert!(!strip_gains(&fake, 0).is_empty(), "{:?}", fake.commands());
}

#[test]
fn dragging_from_a_source_socket_to_a_bus_socket_makes_the_send() {
    let (fake, mut h) = patchbay();
    let from = h.get_by_label("Output socket strip 2").rect().center();
    let to = h.get_by_label("Input socket bus 3").rect().center();
    drag(&mut h, from, to);
    assert_eq!(routes(&fake), vec![(1, 2, true)], "{:?}", fake.commands());
    assert!(fake.snapshot_now().strips[1].config.sends[2].enabled);

    // Dropping on empty space does nothing.
    fake.clear_commands();
    drag(&mut h, from, from + vec2(60.0, 200.0));
    assert!(routes(&fake).is_empty());
    assert!(!h.state().view.patch.busy(), "the gesture ended");
}

#[test]
fn a_dropped_wire_keeps_the_sends_gain_and_a_live_send_is_left_alone() {
    let (fake, mut h) = patchbay();
    fake.edit(|s| {
        s.strips[0].config.sends[1].enabled = false;
        s.strips[0].config.sends[1].gain_db = -8.0;
    });
    advance(&mut h, 1.0);
    let from = h.get_by_label("Output socket strip 1").rect().center();
    let to = h.get_by_label("Input socket bus 2").rect().center();
    drag(&mut h, from, to);
    let s = fake.snapshot_now().strips[0].config.sends[1];
    assert!(s.enabled && s.gain_db == -8.0, "{s:?}");
    // Dropping on a bus that is already connected sends nothing.
    fake.clear_commands();
    drag(&mut h, from, to);
    assert!(routes(&fake).is_empty());
}

#[test]
fn right_clicking_a_wire_opens_its_gain_and_disconnect() {
    let (fake, mut h) = patchbay();
    fake.edit(|s| s.strips[0].config.sends[0].enabled = true);
    advance(&mut h, 1.0);
    assert!(h.query_by_label("Wire gain strip 1 bus 1").is_none());

    let a = h.get_by_label("Output socket strip 1").rect().center();
    let b = h.get_by_label("Input socket bus 1").rect().center();
    let middle = crate::motion::bezier_at(&crate::motion::wire(a, b), 0.5);
    secondary_click(&mut h, middle);
    assert!(
        h.query_by_label("Wire gain strip 1 bus 1").is_some(),
        "the wire's gain did not open"
    );

    // A right-click away from every wire leaves the pick as it is.
    let empty = middle + vec2(0.0, 150.0);
    secondary_click(&mut h, empty);
    // A plain click on empty space drops it.
    press(&mut h, empty);
    release(&mut h, empty);
    h.run_steps(2);
    assert!(h.query_by_label("Wire gain strip 1 bus 1").is_none());

    // Pick it again and cut it.
    secondary_click(&mut h, middle);
    fake.clear_commands();
    h.get_by_label("Disconnect strip 1 from bus 1").click();
    h.run_steps(3);
    assert_eq!(routes(&fake), vec![(0, 0, false)]);
    assert!(
        h.query_by_label("Wire gain strip 1 bus 1").is_none(),
        "the bar closes"
    );
}

#[test]
fn the_expand_arrow_opens_the_cards_drawer_in_place() {
    let (fake, mut h) = patchbay();
    assert!(h.query_by_label("Send gain strip 1 bus 1").is_none());
    let y2_before = h.get_by_label("Output socket strip 2").rect().center().y;
    h.get_by_label("Expand card strip 1").click();
    h.run_steps(3);
    assert!(fake.snapshot_now().ui.strips[0].expanded_in_cards);
    h.run_steps(180);
    for name in [
        "Send gain strip 1 bus 1",
        "Send gain strip 1 bus 3",
        "Pan strip 1",
    ] {
        assert!(h.query_by_label(name).is_some(), "missing {name}");
    }
    // Strip 2's card moved down to make room.
    let y2 = h.get_by_label("Output socket strip 2").rect().center().y;
    assert!(y2 > y2_before + 100.0, "strip 2 went {y2_before} -> {y2}");

    // The drawer's send switch sends the route.
    fake.clear_commands();
    h.get_by_label("Send strip 1 to bus 2").click();
    h.run_steps(2);
    assert_eq!(routes(&fake), vec![(0, 1, true)]);

    // The inspector opens beside the cards and the cards stay usable.
    h.get_by_label("Open inspector strip 1").click();
    h.run_steps(3);
    assert!(h.query_by_label_contains("Inspector strip 1").is_some());
    assert!(h.query_by_label("Output socket strip 3").is_some());
    h.get_by_label("Expand card strip 1").click();
    h.run_steps(3);
    assert!(!fake.snapshot_now().ui.strips[0].expanded_in_cards);
}

#[test]
fn a_bus_card_has_a_drawer_with_its_output() {
    let (fake, mut h) = patchbay();
    h.get_by_label("Expand card bus 1").click();
    h.run_steps(3);
    assert!(fake.snapshot_now().ui.buses[0].expanded_in_cards);
    assert!(h.query_by_label("Open inspector bus 1").is_some());
}

#[test]
fn double_clicking_a_name_edits_it() {
    let (fake, mut h) = patchbay();
    let name = h.get_by_label("Name of strip 1");
    name.click();
    name.click();
    h.run_steps(3);
    let edit = h.get_by_label("Label of strip 1");
    edit.focus();
    h.run_steps(1);
    // ? kittest 0.32 has no "key with modifiers" helper, so select-all is a raw event.
    h.input_mut().events.push(Event::Key {
        key: egui::Key::A,
        physical_key: Some(egui::Key::A),
        pressed: true,
        repeat: false,
        modifiers: Modifiers::COMMAND,
    });
    h.run_steps(1);
    h.get_by_label("Label of strip 1").type_text("Guitar");
    h.run_steps(1);
    h.key_press(egui::Key::Enter);
    h.run_steps(3);
    let sent = fake.commands().into_iter().find_map(|c| match c {
        Command::StripLabel { strip: 0, label } => Some(label),
        _ => None,
    });
    assert_eq!(sent.as_deref(), Some("Guitar"), "{:?}", fake.commands());
    assert!(
        h.query_by_label("Label of strip 1").is_none(),
        "the box closed"
    );
}

#[test]
fn dragging_a_card_by_its_handle_reorders_the_cards() {
    let (fake, mut h) = patchbay();
    let handle = h.get_by_label("Move card strip 1").rect().center();
    // Cards are 58 px apart; drag the first one below the second one's centre.
    drag(&mut h, handle, handle + vec2(0.0, 80.0));
    let layout = fake.snapshot_now().ui;
    let orders: Vec<u8> = layout.strips.iter().map(|s| s.card_order).collect();
    assert!(
        orders[0] > orders[1],
        "strip 1 should now come after strip 2: {orders:?}"
    );
    assert_eq!(orders[1], 0);
    assert!(!h.state().view.patch.busy());
    // Settled, strip 2 is above strip 1.
    advance(&mut h, 3.0);
    h.run_steps(180);
    let y = |h: &H, n: u8| {
        h.get_by_label(&format!("Output socket strip {n}"))
            .rect()
            .center()
            .y
    };
    assert!(
        y(&h, 2) < y(&h, 1),
        "strip 2 {} above strip 1 {}",
        y(&h, 2),
        y(&h, 1)
    );
}

#[test]
fn rich_motion_springs_cards_to_their_new_place_and_eco_snaps() {
    for (profile, animated) in [
        (GraphicsSettings::RICH, true),
        (GraphicsSettings::ECO, false),
    ] {
        let (fake, mut h) = patchbay();
        fake.edit(|s| s.ui.graphics = Some(profile));
        advance(&mut h, 1.0);
        let y = |h: &H| h.get_by_label("Output socket strip 1").rect().center().y;
        let before = y(&h);
        // Another UI moved strip 1 to the bottom.
        fake.edit(|s| {
            s.ui.strips[0].card_order = 9;
        });
        h.state_mut().clock_override_s = Some(2.0);
        h.step();
        h.step();
        let after_two_frames = y(&h);
        h.run_steps(240);
        let settled = y(&h);
        assert!(
            settled > before + 100.0,
            "{profile:?}: strip 1 should be lower: {before} -> {settled}"
        );
        if animated {
            assert!(
                after_two_frames < settled - 20.0,
                "Rich: still on its way: {after_two_frames} vs {settled}"
            );
        } else {
            assert!(
                (after_two_frames - settled).abs() < 1.0,
                "Eco: already there: {after_two_frames} vs {settled}"
            );
        }
    }
}

#[test]
fn a_window_too_narrow_for_two_columns_shows_the_matrix() {
    let fake = FakeDaemon::new();
    fake.edit(|s| {
        s.ui.mode = UiMode::Expert;
        s.ui.layout = LayoutMode::PatchbayCards;
    });
    let h = harness_sized(&fake, 640.0, 600.0);
    assert!(h.query_by_label_contains("showing the matrix").is_some());
    assert!(h.query_by_label("Route strip 1 to bus 1").is_some());
    assert!(h.query_by_label("Output socket strip 1").is_none());
}

#[test]
fn the_patchbay_goes_quiet_when_minimized_like_every_other_view() {
    let (_fake, mut h) = patchbay();
    h.input_mut().viewports.insert(
        ViewportId::ROOT,
        ViewportInfo {
            minimized: Some(true),
            focused: Some(false),
            ..Default::default()
        },
    );
    h.step();
    assert_eq!(h.state().last_decision.1, Repaint::None);
}
// !SECTION: Patchbay Cards
