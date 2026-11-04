//! Simple mode (PLAN.md §9.1): two cards, "Send" and "Receive". No strips, no buses on screen.
//!
//! The cards are a view over the same strips and buses Expert mode shows. Each card owns one strip
//! and one bus that it finds by label ("Simple send", "Simple receive"), or takes from the first
//! free ones. Switching to Expert shows exactly what the cards built.

use crate::strips::lan_targets;
use crate::view::{Ctx, PRESETS, describe_source, parse_ipv4, preset_name};
use crate::widgets::{self, db_text};
use app_core::types::{Command, LinkState, StateSnapshot};
use audio_types::{
    FixedStr, LanRxConfig, LanTxConfig, NetworkBufferPreset, ProcessLoopbackMode, RouteSend,
    SinkEndpoint, SourceEndpoint,
};
use egui::{Button, Color32, ComboBox, RichText, Ui, WidgetType};
use platform_io::{AppInfo, DeviceInfo, DeviceKind};

pub const SEND_LABEL: &str = "Simple send";
pub const RECEIVE_LABEL: &str = "Simple receive";

// SECTION: Planning (pure)
/// The strip a card owns: the one carrying its label, else the first strip with nothing in it.
pub fn find_strip(snap: &StateSnapshot, label: &str) -> Option<u8> {
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

pub fn find_bus(snap: &StateSnapshot, label: &str) -> Option<u8> {
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

/// Both the strip's source and the bus's sink are set and the send between them is on.
pub fn is_connected(snap: &StateSnapshot, strip: u8, bus: u8) -> bool {
    let (Some(s), Some(b)) = (
        snap.strips.get(usize::from(strip)),
        snap.buses.get(usize::from(bus)),
    ) else {
        return false;
    };
    s.config.source != SourceEndpoint::Unassigned
        && b.config.sink != SinkEndpoint::Unassigned
        && s.config.sends[usize::from(bus)].enabled
}

pub fn connect_plan(
    strip: u8,
    bus: u8,
    label: &str,
    source: SourceEndpoint,
    source_channels: u8,
    sink: SinkEndpoint,
    sink_channels: u8,
) -> Vec<Command> {
    vec![
        Command::StripLabel {
            strip,
            label: label.to_owned(),
        },
        Command::BusLabel {
            bus,
            label: label.to_owned(),
        },
        Command::StripSource {
            strip,
            source,
            channels: source_channels,
        },
        Command::BusSink {
            bus,
            sink,
            channels: sink_channels,
        },
        Command::Route {
            strip,
            bus,
            send: RouteSend {
                enabled: true,
                gain_db: 0.0,
                pre_fader: false,
            },
        },
    ]
}

/// Switch the send off first, so nothing plays while the ends are being closed.
pub fn disconnect_plan(strip: u8, bus: u8) -> Vec<Command> {
    vec![
        Command::Route {
            strip,
            bus,
            send: RouteSend {
                enabled: false,
                gain_db: 0.0,
                pre_fader: false,
            },
        },
        Command::BusSink {
            bus,
            sink: SinkEndpoint::Unassigned,
            channels: 2,
        },
        Command::StripSource {
            strip,
            source: SourceEndpoint::Unassigned,
            channels: 2,
        },
    ]
}

/// What the Send card can capture, best first: the microphone, everything this computer plays,
/// one program, a virtual input.
pub fn send_sources(
    caps: platform_io::Capabilities,
    devices: &[DeviceInfo],
    apps: &[AppInfo],
) -> Vec<(String, SourceEndpoint, u8)> {
    let mut v = Vec::new();
    let mut inputs: Vec<&DeviceInfo> = devices
        .iter()
        .filter(|d| d.kind == DeviceKind::Input)
        .collect();
    inputs.sort_by_key(|d| !d.is_default);
    if caps.hardware_input {
        for d in inputs {
            let name = if d.is_default {
                format!("Microphone: {} (default)", d.name)
            } else {
                format!("Microphone: {}", d.name)
            };
            v.push((
                name,
                SourceEndpoint::HardwareInput {
                    device_id: FixedStr::from_str_trunc(&d.id),
                },
                d.channels.clamp(1, 2),
            ));
        }
    }
    if caps.device_loopback {
        let mut outs: Vec<&DeviceInfo> = devices
            .iter()
            .filter(|d| d.kind == DeviceKind::Output)
            .collect();
        outs.sort_by_key(|d| !d.is_default);
        if let Some(d) = outs.first() {
            v.push((
                "All sound from this computer".to_owned(),
                SourceEndpoint::DeviceLoopback {
                    device_id: FixedStr::from_str_trunc(&d.id),
                },
                2,
            ));
        }
    }
    if caps.process_loopback {
        for a in apps {
            v.push((
                format!("Program: {}", a.name),
                SourceEndpoint::ProcessLoopback {
                    pid: a.pid,
                    exe_name: FixedStr::from_str_trunc(&a.name),
                    mode: ProcessLoopbackMode::IncludeTree,
                },
                2,
            ));
        }
    }
    for n in 0..caps.virtual_inputs {
        v.push((
            format!("Virtual input {}", n + 1),
            SourceEndpoint::VirtualInput { index: n },
            2,
        ));
    }
    v
}

/// Where the Receive card can play: sound outputs, then virtual microphones.
pub fn receive_outputs(
    caps: platform_io::Capabilities,
    devices: &[DeviceInfo],
) -> Vec<(String, SinkEndpoint, u8)> {
    let mut v = Vec::new();
    let mut outs: Vec<&DeviceInfo> = devices
        .iter()
        .filter(|d| d.kind == DeviceKind::Output)
        .collect();
    outs.sort_by_key(|d| !d.is_default);
    if caps.hardware_output {
        for d in outs {
            let name = if d.is_default {
                format!("{} (default)", d.name)
            } else {
                d.name.clone()
            };
            v.push((
                name,
                SinkEndpoint::HardwareOutput {
                    device_id: FixedStr::from_str_trunc(&d.id),
                },
                d.channels.clamp(1, 2),
            ));
        }
    }
    for n in 0..caps.virtual_mics {
        v.push((
            format!("Virtual microphone {}", n + 1),
            SinkEndpoint::VirtualMic { index: n },
            2,
        ));
    }
    v
}

/// Streams the Receive card can listen to: what arrived and was not assigned, then "any sender".
pub fn receive_streams(
    snap: &StateSnapshot,
    buffer: NetworkBufferPreset,
) -> Vec<(String, LanRxConfig, u8)> {
    let mut v = Vec::new();
    for s in &snap.seen_senders {
        v.push((
            format!(
                "{} from {}.{}.{}.{}",
                s.stream_name, s.ip[0], s.ip[1], s.ip[2], s.ip[3]
            ),
            LanRxConfig {
                source_ipv4: s.ip,
                stream_name: FixedStr::from_str_trunc(&s.stream_name),
                buffer,
                adaptive: true,
            },
            u8::try_from(s.channels).unwrap_or(2).clamp(1, 8),
        ));
    }
    v.push((
        "Any sender, stream \"Stream1\"".to_owned(),
        LanRxConfig {
            source_ipv4: [0, 0, 0, 0],
            stream_name: FixedStr::from_str_trunc("Stream1"),
            buffer,
            adaptive: true,
        },
        2,
    ));
    v
}
// !SECTION: Planning (pure)

// SECTION: The two cards
pub fn simple(ui: &mut Ui, cx: &mut Ctx<'_>) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // ? Stacked, not side by side: `horizontal_wrapped` put the second card at a stray x/y
            // ? offset and clipped it at the window edge (seen in a real window, not in kittest).
            card(ui, cx.narrow, |ui| send_card(ui, cx));
            ui.add_space(8.0);
            card(ui, cx.narrow, |ui| receive_card(ui, cx));
        });
}

fn card(ui: &mut Ui, narrow: bool, add: impl FnOnce(&mut Ui)) {
    let w = (ui.available_width() - 12.0).clamp(200.0, if narrow { f32::MAX } else { 560.0 });
    egui::Frame::group(ui.style())
        .inner_margin(12.0)
        .show(ui, |ui| {
            ui.set_width(w - 30.0);
            add(ui);
        });
}

fn pick(ui: &mut Ui, id: &str, labels: &[String], index: &mut usize) {
    *index = (*index).min(labels.len().saturating_sub(1));
    ComboBox::from_id_salt(id)
        .width(ui.available_width().min(320.0))
        .selected_text(
            labels
                .get(*index)
                .cloned()
                .unwrap_or_else(|| "(nothing available)".into()),
        )
        .show_ui(ui, |ui| {
            for (i, l) in labels.iter().enumerate() {
                ui.selectable_value(index, i, l);
            }
        });
}

fn send_card(ui: &mut Ui, cx: &mut Ctx<'_>) {
    ui.heading("Send");
    widgets::caption(ui, "Play this computer's sound on another machine");
    let (Some(strip), Some(bus)) = (
        find_strip(cx.snap, SEND_LABEL),
        find_bus(cx.snap, SEND_LABEL),
    ) else {
        ui.label("All strips or buses are in use. Free one in Expert mode.");
        return;
    };
    let connected = is_connected(cx.snap, strip, bus);

    ui.add_space(6.0);
    ui.label("Send");
    let sources = send_sources(cx.snap.capabilities, cx.devices, cx.apps);
    let labels: Vec<String> = sources.iter().map(|s| s.0.clone()).collect();
    let mut idx = cx.view.simple.send_source;
    ui.add_enabled_ui(!connected, |ui| {
        pick(ui, "simple-send-source", &labels, &mut idx)
    });
    cx.view.simple.send_source = idx;

    ui.add_space(4.0);
    ui.label("to");
    let targets = lan_targets(cx);
    let mut tlabels: Vec<String> = targets
        .iter()
        .map(|t| format!("{} ({})", t.0, t.1.ip()))
        .collect();
    tlabels.push("another machine (type its address)".into());
    let mut tidx = cx.view.simple.send_target;
    ui.add_enabled_ui(!connected, |ui| {
        pick(ui, "simple-send-target", &tlabels, &mut tidx)
    });
    cx.view.simple.send_target = tidx;
    let typing = tidx >= targets.len();
    let mut ip = cx.view.simple.send_ip.clone();
    if typing {
        ui.horizontal(|ui| {
            let r = ui.add_enabled(
                !connected,
                egui::TextEdit::singleline(&mut ip)
                    .hint_text("192.168.1.20")
                    .desired_width(140.0),
            );
            widgets::name(&r, WidgetType::TextEdit, "Receiver address");
        });
        cx.view.simple.send_ip.clone_from(&ip);
    }

    ui.add_space(8.0);
    if connected {
        let b = ui.add(Button::new("Disconnect").fill(Color32::from_rgb(150, 60, 50)));
        widgets::name(&b, WidgetType::Button, "Disconnect send");
        if b.clicked() {
            cx.out.commands.extend(disconnect_plan(strip, bus));
        }
        status(ui, cx, strip, bus);
    } else {
        let b = ui.add_enabled(
            !sources.is_empty(),
            Button::new("Connect").fill(Color32::from_rgb(50, 120, 190)),
        );
        widgets::name(&b, WidgetType::Button, "Connect send");
        if b.clicked() {
            let addr = if typing {
                parse_ipv4(&ip).map(|a| (a, audio_types::VBAN_DEFAULT_PORT))
            } else {
                targets.get(tidx).map(|t| (t.1.ip().octets(), t.1.port()))
            };
            match (sources.get(idx), addr) {
                (None, _) => cx.out.say("nothing to send: no input is available"),
                (_, None) => cx.out.say("the receiver address is not an IPv4 address"),
                (Some((_, src, ch)), Some((peer, port))) => {
                    let sink = SinkEndpoint::LanOut(LanTxConfig {
                        peer_ipv4: peer,
                        port,
                        ..LanTxConfig::default()
                    });
                    cx.out
                        .commands
                        .extend(connect_plan(strip, bus, SEND_LABEL, *src, *ch, sink, 2));
                }
            }
        }
    }
}

fn receive_card(ui: &mut Ui, cx: &mut Ctx<'_>) {
    ui.heading("Receive");
    widgets::caption(ui, "Play another machine's sound here");
    let (Some(strip), Some(bus)) = (
        find_strip(cx.snap, RECEIVE_LABEL),
        find_bus(cx.snap, RECEIVE_LABEL),
    ) else {
        ui.label("All strips or buses are in use. Free one in Expert mode.");
        return;
    };
    let connected = is_connected(cx.snap, strip, bus);

    ui.add_space(6.0);
    ui.label("Listen to");
    let streams = receive_streams(cx.snap, cx.view.simple.recv_buffer);
    let labels: Vec<String> = streams.iter().map(|s| s.0.clone()).collect();
    let mut idx = cx.view.simple.recv_stream;
    ui.add_enabled_ui(!connected, |ui| {
        pick(ui, "simple-recv-stream", &labels, &mut idx)
    });
    cx.view.simple.recv_stream = idx;

    ui.add_space(4.0);
    ui.label("and play it on");
    let outs = receive_outputs(cx.snap.capabilities, cx.devices);
    let olabels: Vec<String> = outs.iter().map(|o| o.0.clone()).collect();
    let mut oidx = cx.view.simple.recv_output;
    ui.add_enabled_ui(!connected, |ui| {
        pick(ui, "simple-recv-output", &olabels, &mut oidx)
    });
    cx.view.simple.recv_output = oidx;

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label("Delay for the network");
        let mut buf = cx.view.simple.recv_buffer;
        ui.add_enabled_ui(!connected, |ui| {
            ComboBox::from_id_salt("simple-recv-buffer")
                .selected_text(preset_name(buf))
                .show_ui(ui, |ui| {
                    for p in PRESETS {
                        ui.selectable_value(&mut buf, p, preset_name(p));
                    }
                });
        });
        cx.view.simple.recv_buffer = buf;
    });
    // ? Choosing the preset from measured jitter (PLAN §9.1) needs the adaptive target (NOTES N3,
    // ? S9); until then the user picks, with Fast (12 ms) for a good 5 GHz Wi-Fi or wired link.

    ui.add_space(8.0);
    if connected {
        let b = ui.add(Button::new("Disconnect").fill(Color32::from_rgb(150, 60, 50)));
        widgets::name(&b, WidgetType::Button, "Disconnect receive");
        if b.clicked() {
            cx.out.commands.extend(disconnect_plan(strip, bus));
        }
        status(ui, cx, strip, bus);
    } else {
        let b = ui.add_enabled(
            !outs.is_empty(),
            Button::new("Connect").fill(Color32::from_rgb(50, 120, 190)),
        );
        widgets::name(&b, WidgetType::Button, "Connect receive");
        if b.clicked() {
            if let (Some((_, rx, rch)), Some((_, sink, sch))) = (streams.get(idx), outs.get(oidx)) {
                cx.out.commands.extend(connect_plan(
                    strip,
                    bus,
                    RECEIVE_LABEL,
                    SourceEndpoint::LanIn(*rx),
                    *rch,
                    *sink,
                    *sch,
                ));
            }
        }
    }
}

/// One line about the running connection, from the real link states and counters.
fn status(ui: &mut Ui, cx: &Ctx<'_>, strip: u8, bus: u8) {
    let s = &cx.snap.strips[usize::from(strip)];
    let b = &cx.snap.buses[usize::from(bus)];
    for link in [&s.link, &b.link] {
        if let LinkState::Failed(why) = link {
            ui.label(RichText::new(format!("⚠ {why}")).color(Color32::from_rgb(235, 110, 90)));
            return;
        }
    }
    ui.label(describe_source(&s.config.source, cx.devices));
    if let Some(l) = cx.strip_levels(usize::from(strip)) {
        let peak = l.peak_db.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        ui.label(
            RichText::new(format!("level {} dB", db_text(peak)))
                .small()
                .weak(),
        );
    }
    if let (SourceEndpoint::LanIn(_), Some(l)) = (s.config.source, cx.lan_rx(usize::from(strip))) {
        if l.last_packet_ms_ago == u32::MAX {
            ui.label(RichText::new("waiting for the other machine...").weak());
        } else {
            ui.label(
                RichText::new(format!(
                    "buffer {:.0} ms, lost {}, underruns {}",
                    l.buffer_ms, l.packets_lost, l.underruns
                ))
                .small(),
            );
        }
    }
    if let Some(t) = b.tx {
        ui.label(RichText::new(format!("{} packets sent", t.packets)).small());
    }
}
// !SECTION: The two cards

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::sample_snapshot;
    use platform_io::Capabilities;

    fn caps() -> Capabilities {
        sample_snapshot().capabilities
    }

    fn dev(id: &str, kind: DeviceKind, default: bool) -> DeviceInfo {
        DeviceInfo {
            id: id.into(),
            name: id.into(),
            kind,
            channels: 2,
            is_default: default,
        }
    }

    #[test]
    fn a_card_takes_the_first_free_slot_then_keeps_it_by_label() {
        let mut s = sample_snapshot();
        assert_eq!(find_strip(&s, SEND_LABEL), Some(0));
        s.strips[0].config.source = SourceEndpoint::VirtualInput { index: 0 };
        assert_eq!(find_strip(&s, SEND_LABEL), Some(1), "strip 0 is in use");
        s.strips[3].config.label = FixedStr::from_str_trunc(SEND_LABEL);
        s.strips[3].config.source = SourceEndpoint::VirtualInput { index: 1 };
        assert_eq!(find_strip(&s, SEND_LABEL), Some(3), "the label wins");
        // Disconnected again (source cleared): the label still finds the same strip.
        s.strips[3].config.source = SourceEndpoint::Unassigned;
        assert_eq!(find_strip(&s, SEND_LABEL), Some(3));
    }

    #[test]
    fn no_free_slot_is_none_not_a_panic() {
        let mut s = sample_snapshot();
        for st in &mut s.strips {
            st.config.source = SourceEndpoint::VirtualInput { index: 0 };
        }
        assert_eq!(find_strip(&s, SEND_LABEL), None);
    }

    #[test]
    fn connect_then_disconnect_are_ordered_safely() {
        let sink = SinkEndpoint::VirtualMic { index: 0 };
        let c = connect_plan(
            2,
            1,
            SEND_LABEL,
            SourceEndpoint::VirtualInput { index: 0 },
            2,
            sink,
            2,
        );
        assert!(matches!(c[2], Command::StripSource { strip: 2, .. }));
        assert!(matches!(c[3], Command::BusSink { bus: 1, .. }));
        assert!(
            matches!(c.last(), Some(Command::Route { send, .. }) if send.enabled),
            "route last"
        );
        let d = disconnect_plan(2, 1);
        assert!(
            matches!(d[0], Command::Route { send, .. } if !send.enabled),
            "silence first"
        );
        assert!(matches!(
            d[2],
            Command::StripSource {
                source: SourceEndpoint::Unassigned,
                ..
            }
        ));
    }

    #[test]
    fn connected_needs_both_ends_and_the_send() {
        let mut s = sample_snapshot();
        assert!(!is_connected(&s, 0, 0));
        s.strips[0].config.source = SourceEndpoint::VirtualInput { index: 0 };
        s.buses[0].config.sink = SinkEndpoint::VirtualMic { index: 0 };
        assert!(!is_connected(&s, 0, 0), "no send yet");
        s.strips[0].config.sends[0].enabled = true;
        assert!(is_connected(&s, 0, 0));
    }

    #[test]
    fn the_default_microphone_is_listed_first() {
        let devices = [
            dev("usb", DeviceKind::Input, false),
            dev("onboard", DeviceKind::Input, true),
            dev("speakers", DeviceKind::Output, true),
        ];
        let v = send_sources(caps(), &devices, &[]);
        assert!(v[0].0.contains("onboard"), "{:?}", v[0].0);
        assert!(v.iter().any(|s| s.0 == "All sound from this computer"));
        assert!(v.iter().any(|s| s.0.contains("Virtual input 1")));
    }

    #[test]
    fn apps_are_offered_only_when_the_backend_can_capture_them() {
        let apps = [AppInfo {
            pid: 7,
            name: "player".into(),
            playing: true,
        }];
        assert!(
            send_sources(caps(), &[], &apps)
                .iter()
                .any(|s| s.0.contains("player"))
        );
        let mut c = caps();
        c.process_loopback = false;
        assert!(
            !send_sources(c, &[], &apps)
                .iter()
                .any(|s| s.0.contains("player"))
        );
    }

    #[test]
    fn receive_always_offers_any_sender() {
        let s = sample_snapshot();
        let v = receive_streams(&s, NetworkBufferPreset::Fast);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].1.source_ipv4, [0, 0, 0, 0]);
    }
}
