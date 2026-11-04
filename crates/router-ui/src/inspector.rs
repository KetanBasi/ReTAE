//! The inline inspector (PLAN.md §9.2, §9.5): a drawer attached to a strip or a bus. Never a
//! popup window (CLAUDE.md law 5). On a narrow window it takes the whole width.
//!
//! * Strip: name, channels, gate, 3-band EQ, compressor, and for LAN strips the stream inspector.
//! * Bus: name, channels, 3-band EQ, and for LAN buses the send settings and counters.

use crate::strips::truncate;
use crate::view::{Ctx, PRESETS, Target, bus_name, parse_ipv4, preset_name};
use crate::widgets;
use app_core::types::{Command, LinkState};
use audio_types::{
    EqBand, FixedStr, LanRxConfig, LanTxConfig, NetworkBufferPreset, OpusFrame, SinkEndpoint,
    SourceEndpoint, StripDsp, VbanDataType, WireCodec,
};
use egui::{
    Align, Button, Color32, ComboBox, DragValue, Layout, ProgressBar, RichText, Sense, Slider, Ui,
    UiBuilder, WidgetType, vec2,
};

pub const DRAWER_W: f32 = 260.0;

pub fn drawer(ui: &mut Ui, cx: &mut Ctx<'_>, t: Target, height: f32) {
    let width = if cx.narrow {
        ui.available_width().max(DRAWER_W)
    } else {
        DRAWER_W
    };
    let (rect, resp) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let (kind, n) = match t {
        Target::Strip(i) => ("strip", usize::from(i) + 1),
        Target::Bus(i) => ("bus", usize::from(i) + 1),
    };
    widgets::name(&resp, WidgetType::Other, format!("Inspector {kind} {n}"));
    ui.painter()
        .rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(rect.shrink(6.0))
            .layout(Layout::top_down(Align::LEFT)),
    );
    child.spacing_mut().item_spacing = vec2(6.0, 5.0);
    reload_buffers(cx, t);
    egui::ScrollArea::vertical()
        .id_salt(("inspector", kind, n))
        .auto_shrink([false, false])
        .show(&mut child, |ui| match t {
            Target::Strip(i) => strip_inspector(ui, cx, i),
            Target::Bus(i) => bus_inspector(ui, cx, i),
        });
}

/// Load the text boxes from the snapshot the first time a target is shown.
fn reload_buffers(cx: &mut Ctx<'_>, t: Target) {
    if cx.view.edit.loaded_for == Some(t) {
        return;
    }
    let e = &mut cx.view.edit;
    e.loaded_for = Some(t);
    match t {
        Target::Strip(i) => {
            let c = cx.snap.strips[usize::from(i)].config;
            e.label = c.label.as_str().to_owned();
            e.rx_channels = c.channels;
            e.rx = match c.source {
                SourceEndpoint::LanIn(r) => r,
                _ => LanRxConfig {
                    stream_name: FixedStr::from_str_trunc("Stream1"),
                    ..LanRxConfig::default()
                },
            };
            e.rx_name = e.rx.stream_name.as_str().to_owned();
            e.rx_ip = ip_text(e.rx.source_ipv4);
        }
        Target::Bus(i) => {
            let c = cx.snap.buses[usize::from(i)].config;
            e.label = c.label.as_str().to_owned();
            e.tx_channels = c.channels;
            e.tx = match c.sink {
                SinkEndpoint::LanOut(t) => t,
                _ => LanTxConfig::default(),
            };
            e.tx_name = e.tx.stream_name.as_str().to_owned();
            e.tx_ip = ip_text(e.tx.peer_ipv4);
            e.tx_port = e.tx.port.to_string();
        }
    }
}

fn ip_text(ip: [u8; 4]) -> String {
    if ip == [0, 0, 0, 0] {
        String::new()
    } else {
        format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
    }
}

fn title(ui: &mut Ui, cx: &mut Ctx<'_>, text: String) {
    ui.horizontal(|ui| {
        ui.heading(text);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let close = ui.add(Button::new("✕").small());
            widgets::name(&close, WidgetType::Button, "Close inspector");
            if close.clicked() {
                cx.view.drawer = None;
            }
        });
    });
}

fn channel_picker(ui: &mut Ui, id: &str, value: &mut u8) -> bool {
    let before = *value;
    ComboBox::from_id_salt(id)
        .selected_text(match *value {
            1 => "Mono".to_owned(),
            2 => "Stereo".to_owned(),
            n => format!("{n} channels"),
        })
        .show_ui(ui, |ui| {
            for n in [1u8, 2, 4, 6, 8] {
                let text = match n {
                    1 => "Mono".to_owned(),
                    2 => "Stereo".to_owned(),
                    n => format!("{n} channels"),
                };
                ui.selectable_value(value, n, text);
            }
        });
    *value != before
}

fn name_box(ui: &mut Ui, buf: &mut String, hint: &str) -> bool {
    let r = ui.add(
        egui::TextEdit::singleline(buf)
            .desired_width(150.0)
            .char_limit(24)
            .hint_text(hint),
    );
    widgets::name(&r, WidgetType::TextEdit, hint);
    r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
}

// SECTION: Strip
fn strip_inspector(ui: &mut Ui, cx: &mut Ctx<'_>, i: u8) {
    let idx = usize::from(i);
    let cfg = cx.snap.strips[idx].config;
    title(
        ui,
        cx,
        format!("Strip {} — {}", idx + 1, truncate(cfg.label.as_str(), 12)),
    );

    ui.horizontal(|ui| {
        ui.label("Name");
        let mut buf = cx.view.edit.label.clone();
        let enter = name_box(ui, &mut buf, "Strip name");
        cx.view.edit.label.clone_from(&buf);
        if enter && buf != cfg.label.as_str() {
            cx.out.send(Command::StripLabel {
                strip: i,
                label: buf,
            });
        }
    });

    if cfg.source != SourceEndpoint::Unassigned {
        ui.horizontal(|ui| {
            ui.label("Channels");
            let mut ch = cfg.channels;
            if channel_picker(ui, "strip-ch", &mut ch) {
                cx.out.send(Command::StripSource {
                    strip: i,
                    source: cfg.source,
                    channels: ch,
                });
            }
        });
    }

    if let SourceEndpoint::LanIn(_) = cfg.source {
        ui.separator();
        lan_rx_inspector(ui, cx, i);
    }

    ui.separator();
    let mut dsp = cfg.dsp;
    let mut changed = false;
    changed |= gate_ui(ui, &mut dsp);
    ui.separator();
    for (k, band) in dsp.eq.iter_mut().enumerate() {
        changed |= eq_band_ui(ui, &format!("EQ {}", k + 1), band);
    }
    ui.separator();
    changed |= comp_ui(ui, &mut dsp);
    if changed {
        cx.out.send(Command::StripDsp { strip: i, dsp });
    }
}

fn gate_ui(ui: &mut Ui, dsp: &mut StripDsp) -> bool {
    let g = &mut dsp.gate;
    let mut c = ui.checkbox(&mut g.enabled, "Gate").changed();
    ui.add_enabled_ui(g.enabled, |ui| {
        c |= ui
            .add(
                Slider::new(&mut g.threshold_db, -80.0..=0.0)
                    .suffix(" dB")
                    .text("threshold"),
            )
            .changed();
        c |= ui
            .add(
                Slider::new(&mut g.attack_ms, 0.1..=100.0)
                    .logarithmic(true)
                    .suffix(" ms")
                    .text("attack"),
            )
            .changed();
        c |= ui
            .add(
                Slider::new(&mut g.release_ms, 5.0..=1000.0)
                    .logarithmic(true)
                    .suffix(" ms")
                    .text("release"),
            )
            .changed();
    });
    c
}

fn comp_ui(ui: &mut Ui, dsp: &mut StripDsp) -> bool {
    let k = &mut dsp.comp;
    let mut c = ui.checkbox(&mut k.enabled, "Compressor").changed();
    ui.add_enabled_ui(k.enabled, |ui| {
        c |= ui
            .add(
                Slider::new(&mut k.threshold_db, -60.0..=0.0)
                    .suffix(" dB")
                    .text("threshold"),
            )
            .changed();
        c |= ui
            .add(
                Slider::new(&mut k.ratio, 1.0..=20.0)
                    .logarithmic(true)
                    .text("ratio"),
            )
            .changed();
        c |= ui
            .add(
                Slider::new(&mut k.attack_ms, 0.1..=100.0)
                    .logarithmic(true)
                    .suffix(" ms")
                    .text("attack"),
            )
            .changed();
        c |= ui
            .add(
                Slider::new(&mut k.release_ms, 5.0..=1000.0)
                    .logarithmic(true)
                    .suffix(" ms")
                    .text("release"),
            )
            .changed();
        c |= ui
            .add(
                Slider::new(&mut k.makeup_db, 0.0..=24.0)
                    .suffix(" dB")
                    .text("makeup"),
            )
            .changed();
    });
    c
}

pub fn eq_band_ui(ui: &mut Ui, label: &str, b: &mut EqBand) -> bool {
    let mut c = ui.checkbox(&mut b.enabled, label).changed();
    ui.add_enabled_ui(b.enabled, |ui| {
        c |= ui
            .add(
                Slider::new(&mut b.freq_hz, 20.0..=20_000.0)
                    .logarithmic(true)
                    .suffix(" Hz")
                    .text("freq"),
            )
            .changed();
        c |= ui
            .add(
                Slider::new(&mut b.gain_db, -18.0..=18.0)
                    .suffix(" dB")
                    .text("gain"),
            )
            .changed();
        c |= ui
            .add(
                Slider::new(&mut b.q, 0.1..=10.0)
                    .logarithmic(true)
                    .text("Q"),
            )
            .changed();
    });
    c
}
// !SECTION: Strip

// SECTION: LAN receive inspector (PLAN.md §9.5)
fn lan_rx_inspector(ui: &mut Ui, cx: &mut Ctx<'_>, i: u8) {
    ui.label(RichText::new("LAN stream").strong());
    let mut rx = cx.view.edit.rx;
    let mut name = cx.view.edit.rx_name.clone();
    let mut ip = cx.view.edit.rx_ip.clone();

    ui.horizontal(|ui| {
        ui.label("Stream name");
        name_box(ui, &mut name, "VBAN stream name");
    });
    ui.horizontal(|ui| {
        ui.label("From");
        let r = ui.add(
            egui::TextEdit::singleline(&mut ip)
                .desired_width(120.0)
                .hint_text("any sender"),
        );
        widgets::name(&r, WidgetType::TextEdit, "Sender address");
    });
    ui.horizontal(|ui| {
        ui.label("Network buffer");
        ComboBox::from_id_salt("rx-preset")
            .selected_text(preset_name(rx.buffer))
            .show_ui(ui, |ui| {
                for p in PRESETS {
                    ui.selectable_value(&mut rx.buffer, p, preset_name(p));
                }
                let custom = NetworkBufferPreset::Custom {
                    target_ms: rx.buffer.target_ms().max(2),
                };
                if ui
                    .selectable_label(
                        matches!(rx.buffer, NetworkBufferPreset::Custom { .. }),
                        "Custom",
                    )
                    .clicked()
                {
                    rx.buffer = custom;
                }
            });
    });
    if let NetworkBufferPreset::Custom { target_ms } = &mut rx.buffer {
        ui.horizontal(|ui| {
            ui.label("Custom target");
            ui.add(DragValue::new(target_ms).range(2..=500).suffix(" ms"));
        });
    }
    ui.checkbox(&mut rx.adaptive, "Adaptive").on_hover_text(
        "Raise the delay when the stream keeps underrunning, lower it again after a clean \
         minute. The preset above is the floor.",
    );
    if rx.buffer == NetworkBufferPreset::VerySlow {
        ui.label(
            RichText::new("96 ms is over the 70 ms budget on its own.")
                .small()
                .color(Color32::from_rgb(235, 190, 50)),
        );
    }

    cx.view.edit.rx = rx;
    cx.view.edit.rx_name.clone_from(&name);
    cx.view.edit.rx_ip.clone_from(&ip);

    let apply = ui.add(Button::new("Apply"));
    widgets::name(&apply, WidgetType::Button, "Apply stream settings");
    if apply.clicked() {
        let addr = if ip.trim().is_empty() {
            Some([0, 0, 0, 0])
        } else {
            parse_ipv4(&ip)
        };
        match addr {
            None => cx.out.say("the sender address is not an IPv4 address"),
            Some(_) if name.trim().is_empty() => cx.out.say("the stream name is empty"),
            Some(a) => {
                let cfg = LanRxConfig {
                    source_ipv4: a,
                    stream_name: FixedStr::from_str_trunc(name.trim()),
                    buffer: rx.buffer,
                    adaptive: rx.adaptive,
                };
                let channels = cx.snap.strips[usize::from(i)].config.channels;
                cx.out.send(Command::StripSource {
                    strip: i,
                    source: SourceEndpoint::LanIn(cfg),
                    channels,
                });
            }
        }
    }

    // Live counters.
    ui.add_space(4.0);
    match cx.lan_rx(usize::from(i)) {
        Some(l) if l.last_packet_ms_ago != u32::MAX => {
            let span = (l.target_ms * 2.0).max(1.0);
            ui.add(
                ProgressBar::new((l.buffer_ms / span).clamp(0.0, 1.0)).text(format!(
                    "buffer {:.1} / target {:.1} ms",
                    l.buffer_ms, l.target_ms
                )),
            );
            ui.label(format!("drift {:+.0} ppm", l.drift_ppm));
            ui.label(format!(
                "lost {}   late {}   underruns {}",
                l.packets_lost, l.packets_late, l.underruns
            ));
            ui.label(format!("last packet {} ms ago", l.last_packet_ms_ago));
        }
        _ => {
            ui.label(RichText::new("no packets yet").weak());
        }
    }
    if let Some(s) = cx
        .snap
        .seen_senders
        .iter()
        .find(|s| s.stream_name == rx.stream_name.as_str())
    {
        ui.label(
            RichText::new(format!(
                "sender: {} ch, {} Hz, {} packets",
                s.channels, s.sample_rate, s.packets
            ))
            .small()
            .weak(),
        );
    }
    // ? RSSI and link speed (PLAN §9.5) need the OS Wi-Fi API; nothing provides them yet (TODO).
}
// !SECTION: LAN receive inspector

// SECTION: Bus
fn bus_inspector(ui: &mut Ui, cx: &mut Ctx<'_>, i: u8) {
    let idx = usize::from(i);
    let cfg = cx.snap.buses[idx].config;
    title(
        ui,
        cx,
        format!(
            "{} — {}",
            bus_name(cx.snap, idx),
            truncate(cfg.label.as_str(), 12)
        ),
    );

    ui.horizontal(|ui| {
        ui.label("Name");
        let mut buf = cx.view.edit.label.clone();
        let enter = name_box(ui, &mut buf, "Bus name");
        cx.view.edit.label.clone_from(&buf);
        if enter && buf != cfg.label.as_str() {
            cx.out.send(Command::BusLabel { bus: i, label: buf });
        }
    });

    if cfg.sink != SinkEndpoint::Unassigned {
        ui.horizontal(|ui| {
            ui.label("Channels");
            let mut ch = cfg.channels;
            if channel_picker(ui, "bus-ch", &mut ch) {
                let sink = match cfg.sink {
                    SinkEndpoint::LanOut(t) => {
                        SinkEndpoint::LanOut(LanTxConfig { channels: ch, ..t })
                    }
                    other => other,
                };
                cx.out.send(Command::BusSink {
                    bus: i,
                    sink,
                    channels: ch,
                });
            }
        });
    }

    if let LinkState::Failed(why) = &cx.snap.buses[idx].link {
        ui.label(RichText::new(format!("⚠ {why}")).color(Color32::from_rgb(235, 110, 90)));
    }

    if let SinkEndpoint::LanOut(_) = cfg.sink {
        ui.separator();
        lan_tx_inspector(ui, cx, i);
    }

    ui.separator();
    let mut eq = cfg.eq;
    let mut changed = false;
    for (k, band) in eq.iter_mut().enumerate() {
        changed |= eq_band_ui(ui, &format!("EQ {}", k + 1), band);
    }
    if changed {
        cx.out.send(Command::BusEq { bus: i, eq });
    }
}

fn lan_tx_inspector(ui: &mut Ui, cx: &mut Ctx<'_>, i: u8) {
    ui.label(RichText::new("LAN output").strong());
    let mut tx = cx.view.edit.tx;
    let mut name = cx.view.edit.tx_name.clone();
    let mut ip = cx.view.edit.tx_ip.clone();
    let mut port = cx.view.edit.tx_port.clone();

    ui.horizontal(|ui| {
        ui.label("To");
        let r = ui.add(
            egui::TextEdit::singleline(&mut ip)
                .desired_width(100.0)
                .hint_text("192.168.1.20"),
        );
        widgets::name(&r, WidgetType::TextEdit, "Receiver address");
        let p = ui.add(egui::TextEdit::singleline(&mut port).desired_width(46.0));
        widgets::name(&p, WidgetType::TextEdit, "Receiver port");
    });
    ui.horizontal(|ui| {
        ui.label("Stream name");
        name_box(ui, &mut name, "VBAN stream name");
    });

    // Codec: PCM data type, or Opus (only machines running this program decode it).
    let is_opus = matches!(tx.codec, WireCodec::Opus { .. });
    ui.horizontal(|ui| {
        ui.label("Codec");
        ComboBox::from_id_salt("tx-codec")
            .selected_text(codec_name(tx.codec))
            .show_ui(ui, |ui| {
                for dt in [
                    VbanDataType::Int16,
                    VbanDataType::Int24,
                    VbanDataType::Float32,
                ] {
                    let c = WireCodec::Pcm { data_type: dt };
                    ui.selectable_value(&mut tx.codec, c, codec_name(c));
                }
                let o = WireCodec::Opus {
                    bitrate_kbps: 96,
                    frame: OpusFrame::Ms5,
                };
                if ui
                    .selectable_label(is_opus, "Opus (ReTAE peers only)")
                    .clicked()
                    && !is_opus
                {
                    tx.codec = o;
                }
            });
    });
    if let WireCodec::Opus {
        bitrate_kbps,
        frame,
    } = &mut tx.codec
    {
        ui.horizontal(|ui| {
            ui.add(
                DragValue::new(bitrate_kbps)
                    .range(16..=510)
                    .suffix(" kbit/s"),
            );
            ComboBox::from_id_salt("tx-opus-frame")
                .selected_text(match frame {
                    OpusFrame::Ms2_5 => "2.5 ms",
                    OpusFrame::Ms5 => "5 ms",
                    OpusFrame::Ms10 => "10 ms",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(frame, OpusFrame::Ms2_5, "2.5 ms");
                    ui.selectable_value(frame, OpusFrame::Ms5, "5 ms");
                    ui.selectable_value(frame, OpusFrame::Ms10, "10 ms");
                });
        });
        ui.label(
            RichText::new("needs a build with Opus on both machines")
                .small()
                .weak(),
        );
    }
    ui.horizontal(|ui| {
        ui.label("Frames/packet");
        ui.add(
            DragValue::new(&mut tx.frames_per_packet)
                .range(0..=256)
                .custom_formatter(|v, _| {
                    if v == 0.0 {
                        "auto".to_owned()
                    } else {
                        format!("{v}")
                    }
                }),
        );
    });
    ui.horizontal(|ui| {
        ui.label("DSCP");
        ui.add(DragValue::new(&mut tx.dscp).range(0..=63));
        if ui.small_button("EF 46").clicked() {
            tx.dscp = 46;
        }
        if ui.small_button("CS6 48").clicked() {
            tx.dscp = 48;
        }
        if ui.small_button("off").clicked() {
            tx.dscp = 0;
        }
    });
    let dup = ui.checkbox(&mut tx.duplicate, "Send every packet twice");
    widgets::name(&dup, WidgetType::Checkbox, "Packet duplication");
    if tx.duplicate {
        ui.label(
            RichText::new("doubles the bandwidth; fixes one lost packet at +1 packet of delay")
                .small()
                .weak(),
        );
    }

    cx.view.edit.tx = tx;
    cx.view.edit.tx_name.clone_from(&name);
    cx.view.edit.tx_ip.clone_from(&ip);
    cx.view.edit.tx_port.clone_from(&port);

    let apply = ui.add(Button::new("Apply"));
    widgets::name(&apply, WidgetType::Button, "Apply output settings");
    if apply.clicked() {
        match (parse_ipv4(&ip), port.trim().parse::<u16>()) {
            (None, _) => cx.out.say("the receiver address is not an IPv4 address"),
            (_, Err(_)) => cx.out.say("the port must be a number from 1 to 65535"),
            (Some(_), Ok(0)) => cx.out.say("the port must be a number from 1 to 65535"),
            (Some(_), _) if name.trim().is_empty() => cx.out.say("the stream name is empty"),
            (Some(a), Ok(p)) => {
                let channels = cx.snap.buses[usize::from(i)].config.channels;
                let cfg = LanTxConfig {
                    peer_ipv4: a,
                    port: p,
                    stream_name: FixedStr::from_str_trunc(name.trim()),
                    channels,
                    ..tx
                };
                cx.out.send(Command::BusSink {
                    bus: i,
                    sink: SinkEndpoint::LanOut(cfg),
                    channels,
                });
            }
        }
    }

    if let Some(t) = cx.snap.buses[usize::from(i)].tx {
        ui.add_space(4.0);
        ui.label(format!(
            "{} packets, {:.1} MB",
            t.packets,
            t.bytes as f64 / 1e6
        ));
        ui.label(format!(
            "send errors {}   dropped frames {}",
            t.send_errors, t.dropped_frames
        ));
        ui.label(format!(
            "longest sender wake gap {:.1} ms",
            f64::from(t.max_tick_gap_us) / 1000.0
        ));
    }
}

fn codec_name(c: WireCodec) -> String {
    match c {
        WireCodec::Pcm { data_type } => match data_type {
            VbanDataType::Int16 => "PCM 16-bit".into(),
            VbanDataType::Int24 => "PCM 24-bit".into(),
            VbanDataType::Int32 => "PCM 32-bit".into(),
            VbanDataType::Float32 => "PCM float".into(),
            other => format!("PCM {other:?}"),
        },
        WireCodec::Opus { bitrate_kbps, .. } => format!("Opus {bitrate_kbps} kbit/s"),
    }
}
// !SECTION: Bus

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unset_ip_shows_as_an_empty_box() {
        assert_eq!(ip_text([0, 0, 0, 0]), "");
        assert_eq!(ip_text([10, 0, 0, 7]), "10.0.0.7");
    }

    #[test]
    fn codec_names_are_distinct() {
        let names: Vec<String> = [
            VbanDataType::Int16,
            VbanDataType::Int24,
            VbanDataType::Float32,
        ]
        .into_iter()
        .map(|d| codec_name(WireCodec::Pcm { data_type: d }))
        .collect();
        assert_eq!(names.len(), 3);
        assert!(names[0] != names[1] && names[1] != names[2]);
    }
}
