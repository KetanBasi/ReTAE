//! The top toolbar (PLAN.md §9.6): mode, layout, graphics profile, peers, profiles, record,
//! and the engine status.

use crate::governor::Rate;
use crate::strips::lan_targets;
use crate::view::Ctx;
use crate::widgets;
use app_core::types::Command;
use app_core::ui_state::{GraphicsProfile, GraphicsSettings, LayoutMode, UiMode};
use audio_types::{LanTxConfig, SinkEndpoint};
use egui::{Button, Color32, DragValue, RichText, Ui, WidgetType};

pub fn toolbar(ui: &mut Ui, cx: &mut Ctx<'_>) {
    ui.horizontal_wrapped(|ui| {
        mode_toggle(ui, cx);
        if cx.snap.ui.mode == UiMode::Expert {
            ui.separator();
            layout_toggle(ui, cx);
        }
        ui.separator();
        graphics_menu(ui, cx);
        peers_menu(ui, cx);
        profile_menu(ui, cx);
        ui.separator();
        record_button(ui, cx);
        ui.separator();
        engine_status(ui, cx);
    });
}

// SECTION: Record
/// `m:ss` (or `h:mm:ss`) for a take length.
pub fn clock(seconds: f32) -> String {
    let s = seconds.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// One button for the whole mixer: starts a take on every bus that has an output, or stops all
/// the takes that are running. Per-bus control is the small `Rec` button on each bus.
fn record_button(ui: &mut Ui, cx: &mut Ctx<'_>) {
    let running: Vec<u8> = (0..cx.snap.buses.len())
        .filter(|&b| cx.snap.buses[b].config.record)
        .map(|b| b as u8)
        .collect();
    if !running.is_empty() {
        let secs = cx
            .snap
            .buses
            .iter()
            .filter_map(|b| b.recording.as_ref())
            .fold(0.0f32, |m, r| m.max(r.seconds));
        let text =
            RichText::new(format!("⏹ Stop recording {}", clock(secs))).color(Color32::LIGHT_RED);
        if ui
            .add(Button::new(text))
            .on_hover_text("Stop every running recording")
            .clicked()
        {
            for bus in running {
                cx.out.send(Command::BusRecord {
                    bus,
                    enabled: false,
                });
            }
        }
        return;
    }
    let targets: Vec<u8> = (0..cx.snap.buses.len())
        .filter(|&b| cx.snap.buses[b].config.sink != SinkEndpoint::Unassigned)
        .map(|b| b as u8)
        .collect();
    let rec = ui.add_enabled(!targets.is_empty(), Button::new("⏺ Record"));
    let hover = if targets.is_empty() {
        "Give a bus an output first"
    } else {
        "Record every bus that has an output"
    };
    if rec
        .on_hover_text(hover)
        .on_disabled_hover_text(hover)
        .clicked()
    {
        for bus in targets {
            cx.out.send(Command::BusRecord { bus, enabled: true });
        }
    }
}
// !SECTION: Record

fn mode_toggle(ui: &mut Ui, cx: &mut Ctx<'_>) {
    let mut mode = cx.snap.ui.mode;
    ui.selectable_value(&mut mode, UiMode::Simple, "Simple");
    ui.selectable_value(&mut mode, UiMode::Expert, "Expert");
    if mode != cx.snap.ui.mode {
        cx.out.edit_layout(cx.snap, |l| l.mode = mode);
    }
}

fn layout_toggle(ui: &mut Ui, cx: &mut Ctx<'_>) {
    let mut layout = cx.snap.ui.layout;
    ui.selectable_value(&mut layout, LayoutMode::ConsoleStrips, "Strips");
    ui.selectable_value(&mut layout, LayoutMode::Matrix, "Matrix");
    ui.selectable_value(&mut layout, LayoutMode::PatchbayCards, "Patchbay");
    if layout != cx.snap.ui.layout {
        cx.out.edit_layout(cx.snap, |l| l.layout = layout);
    }
}

fn graphics_menu(ui: &mut Ui, cx: &mut Ctx<'_>) {
    ui.menu_button("Graphics ▾", |ui| {
        let cur = cx.gfx;
        for (name, preset) in [
            ("Rich", GraphicsSettings::RICH),
            ("Eco", GraphicsSettings::ECO),
        ] {
            if ui
                .selectable_label(cur.profile == preset.profile, name)
                .clicked()
            {
                cx.out.edit_layout(cx.snap, |l| l.graphics = Some(preset));
            }
        }
        let custom = cur.profile == GraphicsProfile::Custom;
        if ui.selectable_label(custom, "Custom").clicked() && !custom {
            cx.out.edit_layout(cx.snap, |l| {
                l.graphics = Some(GraphicsSettings {
                    profile: GraphicsProfile::Custom,
                    ..cur
                });
            });
        }
        if custom {
            let mut g = cur;
            let mut changed = false;
            ui.horizontal(|ui| {
                ui.label("Active fps");
                changed |= ui
                    .add(DragValue::new(&mut g.active_fps).range(1..=240))
                    .changed();
            });
            ui.horizontal(|ui| {
                ui.label("Quiet fps");
                changed |= ui
                    .add(DragValue::new(&mut g.silent_fps).range(1..=240))
                    .changed();
            });
            changed |= ui
                .checkbox(&mut g.meter_peak_hold, "Meter peak hold")
                .changed();
            changed |= ui
                .checkbox(&mut g.smooth_transitions, "Smooth transitions")
                .changed();
            // ? The two below only matter for the Patchbay view (S9); stored so a profile keeps them.
            changed |= ui
                .checkbox(&mut g.card_spring_physics, "Card spring physics")
                .changed();
            changed |= ui
                .checkbox(&mut g.wire_particles, "Wire particles")
                .changed();
            if changed {
                cx.out
                    .edit_layout(cx.snap, |l| l.graphics = Some(g.sanitized()));
            }
        }
    });
}

fn peers_menu(ui: &mut Ui, cx: &mut Ctx<'_>) {
    ui.menu_button("Peers ▾", |ui| {
        let mut beacon = cx.snap.beacon;
        if ui.checkbox(&mut beacon, "Search for devices").changed() {
            cx.out.send(Command::Beacon { on: beacon });
        }
        ui.separator();
        let targets = lan_targets(cx);
        if targets.is_empty() {
            ui.label(RichText::new("No machines found yet").weak());
        }
        for (name, addr) in targets {
            if ui
                .button(format!("{name}  ({})", addr.ip()))
                .on_hover_text("Send the next free output to this machine")
                .clicked()
            {
                quick_connect(cx, addr);
                ui.close();
            }
        }
    });
}

/// Point the first output that has nothing on it at `addr`.
fn quick_connect(cx: &mut Ctx<'_>, addr: std::net::SocketAddrV4) {
    let free = cx
        .snap
        .buses
        .iter()
        .position(|b| b.config.sink == SinkEndpoint::Unassigned);
    match free {
        None => cx.out.say("every output is in use"),
        Some(b) => {
            cx.view.buses_shown = cx.view.buses_shown.max(b + 1);
            let sink = SinkEndpoint::LanOut(LanTxConfig {
                peer_ipv4: addr.ip().octets(),
                port: addr.port(),
                ..LanTxConfig::default()
            });
            cx.out.send(Command::BusSink {
                bus: b as u8,
                sink,
                channels: 2,
            });
        }
    }
}

fn profile_menu(ui: &mut Ui, cx: &mut Ctx<'_>) {
    let title = match &cx.snap.profile {
        Some(p) => format!("Profile: {p} ▾"),
        None => "Profile ▾".to_owned(),
    };
    ui.menu_button(title, |ui| {
        for p in &cx.snap.profiles {
            ui.horizontal(|ui| {
                if ui.button(p).clicked() {
                    cx.out.send(Command::LoadProfile { name: p.clone() });
                    ui.close();
                }
                if ui.small_button("🗑").on_hover_text("Delete").clicked() {
                    cx.out.send(Command::DeleteProfile { name: p.clone() });
                }
            });
        }
        if cx.snap.profiles.is_empty() {
            ui.label(RichText::new("No saved profiles").weak());
        }
        ui.separator();
        ui.horizontal(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut cx.view.profile_name)
                    .hint_text("name")
                    .desired_width(110.0),
            );
            widgets::name(&r, WidgetType::TextEdit, "Profile name");
            let name = cx.view.profile_name.trim().to_owned();
            if ui
                .add_enabled(!name.is_empty(), Button::new("Save"))
                .clicked()
            {
                cx.out.send(Command::SaveProfile { name });
                ui.close();
            }
        });
    });
}

fn engine_status(ui: &mut Ui, cx: &Ctx<'_>) {
    let Some(m) = cx.meters else {
        ui.label(RichText::new("engine: —").weak());
        return;
    };
    let e = m.engine;
    let bad = e.rt_overruns > 0 || e.input_overruns > 0 || e.output_overruns > 0;
    let text = format!(
        "{} · RT {:.1}% · overruns {}",
        cx.snap.backend,
        f64::from(e.rt_load_permille) / 10.0,
        e.rt_overruns + e.input_overruns + e.output_overruns
    );
    let color = if bad {
        Color32::from_rgb(235, 190, 50)
    } else {
        ui.visuals().weak_text_color()
    };
    ui.label(RichText::new(text).color(color).small());
    if e.rejected_commands > 0 {
        // ? Shown, not hidden (NOTES E4): the engine dropped commands because its queue was full.
        ui.label(
            RichText::new(format!("{} commands rejected", e.rejected_commands))
                .small()
                .color(Color32::from_rgb(235, 110, 90)),
        );
    }
}

/// "60 fps", "15 fps (quiet)", "paused": what the governor is doing, for the status bar.
pub fn rate_text(rate: Rate, fps: Option<u32>) -> String {
    match (rate, fps) {
        (Rate::Paused, _) => "paused".to_owned(),
        (Rate::Silent, Some(f)) => format!("{f} fps (quiet)"),
        (Rate::Active, Some(f)) => format!("{f} fps"),
        (_, None) => "—".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_text_names_each_state() {
        assert_eq!(rate_text(Rate::Paused, None), "paused");
        assert_eq!(rate_text(Rate::Silent, Some(15)), "15 fps (quiet)");
        assert_eq!(rate_text(Rate::Active, Some(60)), "60 fps");
    }
}
