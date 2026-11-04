//! Console Strips (PLAN.md §9.2): strips on the left, buses on the right, one horizontal scroll.

use crate::view::{Ctx, PILLAR_W, STRIP_W, Target, bus_name, describe_sink, describe_source};
use crate::widgets::{self, db_text};
use app_core::types::{Command, LinkState};
use audio_types::{
    FixedStr, LanRxConfig, LanTxConfig, NetworkBufferPreset, ProcessLoopbackMode, RouteSend,
    SinkEndpoint, SourceEndpoint,
};
use egui::{
    Align, Button, Color32, ComboBox, Layout, RichText, Sense, Ui, UiBuilder, WidgetType, vec2,
};

const MUTE_COLOR: Color32 = Color32::from_rgb(190, 60, 50);
const SOLO_COLOR: Color32 = Color32::from_rgb(200, 170, 40);
const ON_COLOR: Color32 = Color32::from_rgb(50, 120, 190);

// SECTION: The console
/// Draw all strips, the "+" columns and the buses. The open inspector (if any) sits right
/// after its strip or bus, so it reads as attached to it.
pub fn console(ui: &mut Ui, cx: &mut Ctx<'_>) {
    let height = ui.available_height().max(240.0);
    egui::ScrollArea::horizontal()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                for i in 0..cx.view.strips_shown {
                    strip_column(ui, cx, i, height);
                    if !cx.narrow && cx.view.drawer == Some(Target::Strip(i as u8)) {
                        crate::inspector::drawer(ui, cx, Target::Strip(i as u8), height);
                    }
                }
                if cx.view.strips_shown < audio_types::MAX_STRIPS {
                    let b = ui.add_sized([24.0, 24.0], Button::new("+"));
                    widgets::name(&b, WidgetType::Button, "Show another strip");
                    if b.clicked() {
                        cx.view.strips_shown += 1;
                    }
                }
                ui.separator();
                for i in 0..cx.view.buses_shown {
                    bus_column(ui, cx, i, height);
                    if !cx.narrow && cx.view.drawer == Some(Target::Bus(i as u8)) {
                        crate::inspector::drawer(ui, cx, Target::Bus(i as u8), height);
                    }
                }
                if cx.view.buses_shown < audio_types::MAX_BUSES {
                    let b = ui.add_sized([24.0, 24.0], Button::new("+"));
                    widgets::name(&b, WidgetType::Button, "Show another bus");
                    if b.clicked() {
                        cx.view.buses_shown += 1;
                    }
                }
            });
        });
}
// !SECTION: The console

fn toggle_drawer(cx: &mut Ctx<'_>, t: Target) {
    cx.view.drawer = if cx.view.drawer == Some(t) {
        None
    } else {
        Some(t)
    };
}

/// Allocate a column of exactly `w` x `height`, give it an accessible name (the tests measure it),
/// paint its background and return the child `Ui` to fill it.
fn column(ui: &mut Ui, w: f32, height: f32, name: &str) -> Ui {
    let (rect, resp) = ui.allocate_exact_size(vec2(w, height), Sense::hover());
    widgets::name(&resp, WidgetType::Other, name);
    ui.painter()
        .rect_filled(rect, 4.0, ui.visuals().faint_bg_color);
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(rect.shrink2(vec2(if w > PILLAR_W + 1.0 { 4.0 } else { 2.0 }, 3.0)))
            .layout(Layout::top_down(Align::Center)),
    );
    child.spacing_mut().item_spacing = vec2(3.0, 4.0);
    child.spacing_mut().button_padding = vec2(3.0, 1.0);
    child
}

fn small_button(text: &str, on: Option<Color32>) -> Button<'_> {
    let b = Button::new(RichText::new(text).size(12.0)).min_size(vec2(26.0, 20.0));
    match on {
        Some(c) => b.fill(c),
        None => b,
    }
}

// SECTION: Strip
fn strip_column(ui: &mut Ui, cx: &mut Ctx<'_>, i: usize, height: f32) {
    let collapsed = cx.strip_collapsed(i);
    let w = if collapsed { PILLAR_W } else { STRIP_W };
    let mut ui = column(ui, w, height, &format!("Strip {} column", i + 1));
    let ui = &mut ui;
    let cfg = cx.snap.strips[i].config;
    let n = i + 1;

    if collapsed {
        pillar(
            ui,
            cx,
            Target::Strip(i as u8),
            cfg.label.as_str(),
            cfg.mute,
            cfg.channels,
            n,
        );
        return;
    }

    // Header: collapse, name (right-click = inspector), FX.
    ui.horizontal(|ui| {
        let c = ui.add(Button::new("«").min_size(vec2(20.0, 18.0)));
        widgets::name(&c, WidgetType::Button, format!("Collapse strip {n}"));
        if c.clicked() {
            cx.toggle_strip_collapsed(i);
        }
        let title = ui.add(
            egui::Label::new(RichText::new(truncate(cfg.label.as_str(), 9)).strong())
                .sense(Sense::click()),
        );
        widgets::name(&title, WidgetType::Button, format!("Name of strip {n}"));
        if title.secondary_clicked() || title.double_clicked() {
            toggle_drawer(cx, Target::Strip(i as u8));
        }
        let fx = ui.add(
            Button::new("FX")
                .small()
                .selected(cx.view.drawer == Some(Target::Strip(i as u8))),
        );
        widgets::name(&fx, WidgetType::Button, format!("Open inspector strip {n}"));
        if fx.clicked() {
            toggle_drawer(cx, Target::Strip(i as u8));
        }
    });

    source_picker(ui, cx, i);
    link_line(ui, &cx.snap.strips[i].link);

    // Meter + fader fill the middle.
    let mid = (ui.available_height() - 150.0).max(70.0);
    let mut gain = cfg.gain_db;
    ui.horizontal(|ui| {
        let levels = cx
            .strip_levels(i)
            .map(|l| (&l.peak_db, &l.rms_db, l.clipping));
        let hold = cx.peak_hold_on();
        let holds = |ch: usize| if hold { cx.strip_hold.db(i, ch) } else { None };
        widgets::meter(
            ui,
            &format!("Level strip {n}"),
            levels,
            &holds,
            usize::from(cfg.channels),
            mid,
        );
        let f = widgets::fader(ui, &format!("Fader strip {n}"), &mut gain, mid, cx.grab());
        if f.changed() {
            cx.out.send(Command::StripGain {
                strip: i as u8,
                gain_db: gain,
            });
        }
    });
    ui.label(
        RichText::new(format!("{} dB", db_text(gain)))
            .monospace()
            .small(),
    );

    ui.horizontal(|ui| {
        let m = ui.add(small_button("M", cfg.mute.then_some(MUTE_COLOR)));
        widgets::name(&m, WidgetType::Button, format!("Mute strip {n}"));
        if m.clicked() {
            cx.out.send(Command::StripMute {
                strip: i as u8,
                mute: !cfg.mute,
            });
        }
        let s = ui.add(small_button("S", cfg.solo.then_some(SOLO_COLOR)));
        widgets::name(&s, WidgetType::Button, format!("Solo strip {n}"));
        if s.clicked() {
            cx.out.send(Command::StripSolo {
                strip: i as u8,
                solo: !cfg.solo,
            });
        }
    });

    let mut pan = cfg.pan;
    let p = ui.add_sized(
        [100.0, 16.0],
        egui::Slider::new(&mut pan, -1.0..=1.0).show_value(false),
    );
    widgets::name(&p, WidgetType::Slider, format!("Pan strip {n}"));
    if p.changed() {
        cx.out.send(Command::StripPan {
            strip: i as u8,
            pan,
        });
    }

    routing_buttons(ui, cx, i);
}

fn routing_buttons(ui: &mut Ui, cx: &mut Ctx<'_>, i: usize) {
    let n = i + 1;
    let cfg = cx.snap.strips[i].config;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(3.0, 3.0);
        for b in 0..cx.view.buses_shown {
            let send = cfg.sends[b];
            let open = cx.view.send_open == Some((i as u8, b as u8));
            let text = bus_name(cx.snap, b);
            let mut button = small_button(&text, send.enabled.then_some(ON_COLOR));
            if open {
                button = button.stroke(egui::Stroke::new(1.5_f32, Color32::WHITE));
            }
            let r = ui.add(button);
            widgets::name(
                &r,
                WidgetType::Button,
                format!("Route strip {n} to bus {}", b + 1),
            );
            if r.clicked() {
                cx.out.send(Command::Route {
                    strip: i as u8,
                    bus: b as u8,
                    send: RouteSend {
                        enabled: !send.enabled,
                        ..send
                    },
                });
            }
            if r.secondary_clicked() {
                cx.view.send_open = if open { None } else { Some((i as u8, b as u8)) };
            }
        }
    });

    // The inline send knob + pre/post toggle (right-click a routing button).
    if let Some((s, b)) = cx.view.send_open {
        if usize::from(s) == i && usize::from(b) < cx.view.buses_shown {
            let bu = usize::from(b);
            let send = cfg.sends[bu];
            ui.group(|ui| {
                ui.set_width(STRIP_W - 24.0);
                widgets::caption(ui, &format!("Send to {}", bus_name(cx.snap, bu)));
                let mut gain = send.gain_db;
                let k = widgets::knob(
                    ui,
                    &format!("Send gain strip {n} to bus {}", bu + 1),
                    &mut gain,
                    (-60.0, 12.0),
                    0.0,
                    40.0,
                );
                ui.label(
                    RichText::new(format!("{} dB", db_text(gain)))
                        .monospace()
                        .small(),
                );
                let mut pre = send.pre_fader;
                let t = ui.checkbox(&mut pre, "pre-fader");
                widgets::name(
                    &t,
                    WidgetType::Checkbox,
                    format!("Pre-fader strip {n} to bus {}", bu + 1),
                );
                if k.changed() || t.changed() {
                    cx.out.send(Command::Route {
                        strip: s,
                        bus: b,
                        send: RouteSend {
                            gain_db: gain,
                            pre_fader: pre,
                            ..send
                        },
                    });
                }
            });
        }
    }
}

/// The 28 px pillar: expand, mute, a vertical meter and the name written upwards.
fn pillar(
    ui: &mut Ui,
    cx: &mut Ctx<'_>,
    t: Target,
    label: &str,
    mute: bool,
    channels: u8,
    n: usize,
) {
    let (kind, idx) = match t {
        Target::Strip(i) => ("strip", usize::from(i)),
        Target::Bus(i) => ("bus", usize::from(i)),
    };
    ui.spacing_mut().item_spacing = vec2(2.0, 3.0);
    let e = ui.add(Button::new("»").min_size(vec2(20.0, 18.0)));
    widgets::name(&e, WidgetType::Button, format!("Expand {kind} {n}"));
    if e.clicked() {
        match t {
            Target::Strip(_) => cx.toggle_strip_collapsed(idx),
            Target::Bus(_) => cx.toggle_bus_collapsed(idx),
        }
    }
    let m = ui.add(
        Button::new(RichText::new("M").size(11.0))
            .min_size(vec2(20.0, 18.0))
            .fill(if mute {
                MUTE_COLOR
            } else {
                Color32::TRANSPARENT
            }),
    );
    widgets::name(&m, WidgetType::Button, format!("Mute {kind} {n}"));
    if m.clicked() {
        cx.out.send(match t {
            Target::Strip(i) => Command::StripMute {
                strip: i,
                mute: !mute,
            },
            Target::Bus(i) => Command::BusMute {
                bus: i,
                mute: !mute,
            },
        });
    }
    if m.secondary_clicked() {
        toggle_drawer(cx, t);
    }
    let levels = match t {
        Target::Strip(_) => cx.strip_levels(idx),
        Target::Bus(_) => cx.bus_levels(idx),
    };
    let levels = levels.map(|l| (&l.peak_db, &l.rms_db, l.clipping));
    let hold = cx.peak_hold_on();
    let holds = |ch: usize| {
        if !hold {
            return None;
        }
        match t {
            Target::Strip(_) => cx.strip_hold.db(idx, ch),
            Target::Bus(_) => cx.bus_hold.db(idx, ch),
        }
    };
    let meter_h = (ui.available_height() * 0.45).max(40.0);
    widgets::meter(
        ui,
        &format!("Level {kind} {n}"),
        levels,
        &holds,
        usize::from(channels).min(2),
        meter_h,
    );
    let rest = ui.available_height().max(20.0);
    widgets::vertical_label(ui, label, rest);
}
// !SECTION: Strip

// SECTION: Bus
fn bus_column(ui: &mut Ui, cx: &mut Ctx<'_>, i: usize, height: f32) {
    let collapsed = cx.bus_collapsed(i);
    let w = if collapsed { PILLAR_W } else { STRIP_W };
    let mut ui = column(ui, w, height, &format!("Bus {} column", i + 1));
    let ui = &mut ui;
    let cfg = cx.snap.buses[i].config;
    let n = i + 1;

    if collapsed {
        pillar(
            ui,
            cx,
            Target::Bus(i as u8),
            cfg.label.as_str(),
            cfg.mute,
            cfg.channels,
            n,
        );
        return;
    }

    ui.horizontal(|ui| {
        let c = ui.add(Button::new("«").min_size(vec2(20.0, 18.0)));
        widgets::name(&c, WidgetType::Button, format!("Collapse bus {n}"));
        if c.clicked() {
            cx.toggle_bus_collapsed(i);
        }
        let title = ui.add(
            egui::Label::new(
                RichText::new(truncate(
                    &format!("{} {}", bus_name(cx.snap, i), cfg.label.as_str()),
                    9,
                ))
                .strong(),
            )
            .sense(Sense::click()),
        );
        widgets::name(&title, WidgetType::Button, format!("Name of bus {n}"));
        if title.secondary_clicked() || title.double_clicked() {
            toggle_drawer(cx, Target::Bus(i as u8));
        }
        let fx = ui.add(
            Button::new("FX")
                .small()
                .selected(cx.view.drawer == Some(Target::Bus(i as u8))),
        );
        widgets::name(&fx, WidgetType::Button, format!("Open inspector bus {n}"));
        if fx.clicked() {
            toggle_drawer(cx, Target::Bus(i as u8));
        }
    });

    sink_picker(ui, cx, i);
    link_line(ui, &cx.snap.buses[i].link);

    let mid = (ui.available_height() - 70.0).max(70.0);
    let mut gain = cfg.gain_db;
    ui.horizontal(|ui| {
        let levels = cx
            .bus_levels(i)
            .map(|l| (&l.peak_db, &l.rms_db, l.clipping));
        let hold = cx.peak_hold_on();
        let holds = |ch: usize| if hold { cx.bus_hold.db(i, ch) } else { None };
        widgets::meter(
            ui,
            &format!("Level bus {n}"),
            levels,
            &holds,
            usize::from(cfg.channels),
            mid,
        );
        let f = widgets::fader(ui, &format!("Fader bus {n}"), &mut gain, mid, cx.grab());
        if f.changed() {
            cx.out.send(Command::BusGain {
                bus: i as u8,
                gain_db: gain,
            });
        }
    });
    ui.label(
        RichText::new(format!("{} dB", db_text(gain)))
            .monospace()
            .small(),
    );
    ui.horizontal(|ui| {
        let m = ui.add(small_button("M", cfg.mute.then_some(MUTE_COLOR)));
        widgets::name(&m, WidgetType::Button, format!("Mute bus {n}"));
        if m.clicked() {
            cx.out.send(Command::BusMute {
                bus: i as u8,
                mute: !cfg.mute,
            });
        }
        // ? Recording is S9 (NOTES C4): the engine fills the recorder ring but nothing drains it.
        // ? The button is shown so the layout is final, and disabled so it cannot promise a file.
        let r = ui.add_enabled(false, small_button("Rec", None));
        r.on_disabled_hover_text("Recording arrives in a later session");
    });
}
// !SECTION: Bus

// SECTION: Pickers
fn source_picker(ui: &mut Ui, cx: &mut Ctx<'_>, i: usize) {
    let cfg = cx.snap.strips[i].config;
    let current = cfg.source;
    let text = truncate(&describe_source(&current, cx.devices), 15);
    let mut chosen: Option<(SourceEndpoint, u8)> = None;
    let caps = cx.snap.capabilities;
    let r = ComboBox::from_id_salt(("source", i))
        .selected_text(text)
        .width(STRIP_W - 14.0)
        .show_ui(ui, |ui| {
            let mut pick = |ui: &mut Ui, label: String, src: SourceEndpoint, ch: u8| {
                if ui.selectable_label(src == current, label).clicked() {
                    chosen = Some((src, ch));
                }
            };
            pick(
                ui,
                "— no source —".into(),
                SourceEndpoint::Unassigned,
                cfg.channels,
            );
            if caps.hardware_input {
                heading(ui, "Inputs");
                for d in cx
                    .devices
                    .iter()
                    .filter(|d| d.kind == platform_io::DeviceKind::Input)
                {
                    let src = SourceEndpoint::HardwareInput {
                        device_id: FixedStr::from_str_trunc(&d.id),
                    };
                    pick(ui, d.name.clone(), src, d.channels.clamp(1, 2));
                }
            }
            if caps.device_loopback {
                heading(ui, "What a device plays");
                for d in cx
                    .devices
                    .iter()
                    .filter(|d| d.kind == platform_io::DeviceKind::Output)
                {
                    let src = SourceEndpoint::DeviceLoopback {
                        device_id: FixedStr::from_str_trunc(&d.id),
                    };
                    pick(ui, format!("Loopback: {}", d.name), src, 2);
                }
            }
            if caps.virtual_inputs > 0 {
                heading(ui, "Virtual inputs");
                for v in 0..caps.virtual_inputs {
                    pick(
                        ui,
                        format!("Virtual input {}", v + 1),
                        SourceEndpoint::VirtualInput { index: v },
                        2,
                    );
                }
            }
            if caps.process_loopback && !cx.apps.is_empty() {
                heading(ui, "One program");
                for a in cx.apps {
                    let src = SourceEndpoint::ProcessLoopback {
                        pid: a.pid,
                        exe_name: FixedStr::from_str_trunc(&a.name),
                        mode: ProcessLoopbackMode::IncludeTree,
                    };
                    let mark = if a.playing { "♪ " } else { "" };
                    pick(ui, format!("{mark}{}", a.name), src, 2);
                }
                heading(ui, "Everything except");
                for a in cx.apps {
                    let src = SourceEndpoint::ProcessLoopback {
                        pid: a.pid,
                        exe_name: FixedStr::from_str_trunc(&a.name),
                        mode: ProcessLoopbackMode::ExcludeTree,
                    };
                    pick(ui, format!("all but {}", a.name), src, 2);
                }
            }
            heading(ui, "From the network");
            let any = LanRxConfig {
                source_ipv4: [0, 0, 0, 0],
                stream_name: FixedStr::from_str_trunc("Stream1"),
                buffer: NetworkBufferPreset::Fast,
            };
            pick(
                ui,
                "LAN stream (set in inspector)".into(),
                SourceEndpoint::LanIn(any),
                2,
            );
            for s in &cx.snap.seen_senders {
                let src = SourceEndpoint::LanIn(LanRxConfig {
                    source_ipv4: s.ip,
                    stream_name: FixedStr::from_str_trunc(&s.stream_name),
                    buffer: NetworkBufferPreset::Fast,
                });
                let ch = u8::try_from(s.channels).unwrap_or(2).clamp(1, 8);
                pick(
                    ui,
                    format!(
                        "Incoming: {} @ {}.{}.{}.{}",
                        s.stream_name, s.ip[0], s.ip[1], s.ip[2], s.ip[3]
                    ),
                    src,
                    ch,
                );
            }
        });
    if r.response.clicked() {
        cx.out.refresh_lists = true;
    }
    if let Some((source, channels)) = chosen {
        if source != current {
            cx.out.send(Command::StripSource {
                strip: i as u8,
                source,
                channels,
            });
        }
    }
}

fn sink_picker(ui: &mut Ui, cx: &mut Ctx<'_>, b: usize) {
    let cfg = cx.snap.buses[b].config;
    let current = cfg.sink;
    let text = truncate(&describe_sink(&current, cx.devices), 15);
    let mut chosen: Option<(SinkEndpoint, u8)> = None;
    let caps = cx.snap.capabilities;
    let r = ComboBox::from_id_salt(("sink", b))
        .selected_text(text)
        .width(STRIP_W - 14.0)
        .show_ui(ui, |ui| {
            let mut pick = |ui: &mut Ui, label: String, sink: SinkEndpoint, ch: u8| {
                if ui.selectable_label(sink == current, label).clicked() {
                    chosen = Some((sink, ch));
                }
            };
            pick(
                ui,
                "— no output —".into(),
                SinkEndpoint::Unassigned,
                cfg.channels,
            );
            if caps.hardware_output {
                heading(ui, "Outputs");
                for d in cx
                    .devices
                    .iter()
                    .filter(|d| d.kind == platform_io::DeviceKind::Output)
                {
                    let sink = SinkEndpoint::HardwareOutput {
                        device_id: FixedStr::from_str_trunc(&d.id),
                    };
                    pick(ui, d.name.clone(), sink, d.channels.clamp(1, 2));
                }
            }
            if caps.virtual_mics > 0 {
                heading(ui, "Virtual microphones");
                for v in 0..caps.virtual_mics {
                    pick(
                        ui,
                        format!("Virtual mic {}", v + 1),
                        SinkEndpoint::VirtualMic { index: v },
                        2,
                    );
                }
            }
            heading(ui, "Send over the network");
            for (name, addr) in lan_targets(cx) {
                let sink = SinkEndpoint::LanOut(LanTxConfig {
                    peer_ipv4: addr.ip().octets(),
                    port: addr.port(),
                    ..LanTxConfig::default()
                });
                pick(ui, format!("→ {name}"), sink, 2);
            }
            pick(
                ui,
                "→ other machine (set in inspector)".into(),
                SinkEndpoint::LanOut(LanTxConfig::default()),
                2,
            );
        });
    if r.response.clicked() {
        cx.out.refresh_lists = true;
    }
    if let Some((sink, channels)) = chosen {
        if sink != current {
            cx.out.send(Command::BusSink {
                bus: b as u8,
                sink,
                channels,
            });
        }
    }
}

/// Machines we could send to: VBAN peers that answered a ping, and mDNS finds.
pub fn lan_targets(cx: &Ctx<'_>) -> Vec<(String, std::net::SocketAddrV4)> {
    let mut v: Vec<(String, std::net::SocketAddrV4)> = Vec::new();
    for p in &cx.snap.peers {
        let name = if p.host_name.is_empty() {
            p.device_name.clone()
        } else {
            p.host_name.clone()
        };
        v.push((name, p.addr));
    }
    for d in &cx.snap.discovered {
        if let Some(a) = d.vban_addr {
            if !v.iter().any(|(_, x)| x.ip() == a.ip()) {
                v.push((d.name.clone(), a));
            }
        }
    }
    v
}

fn heading(ui: &mut Ui, text: &str) {
    ui.add_space(2.0);
    ui.label(RichText::new(text).small().weak());
}

fn link_line(ui: &mut Ui, link: &LinkState) {
    match link {
        LinkState::Idle => {
            ui.add_space(0.0);
        }
        LinkState::Running => {
            ui.label(
                RichText::new("● running")
                    .small()
                    .color(Color32::from_rgb(70, 200, 100)),
            );
        }
        LinkState::Failed(why) => {
            ui.label(
                RichText::new("⚠ failed")
                    .small()
                    .color(Color32::from_rgb(235, 110, 90)),
            )
            .on_hover_text(why);
        }
    }
}

pub fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_owned()
    } else {
        let head: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{head}…")
    }
}
// !SECTION: Pickers

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_counts_characters_not_bytes() {
        assert_eq!(truncate("short", 9), "short");
        assert_eq!(truncate("Küchenlautsprecher", 6), "Küche…");
        assert_eq!(truncate("", 3), "");
    }
}
