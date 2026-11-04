//! Routing Matrix (PLAN.md §9.3): strips are rows, buses are columns. Click a cell to switch the
//! send on or off, drag it up/down to set the send gain, right-click to put it back to 0 dB.

use crate::strips::truncate;
use crate::view::{Ctx, bus_name};
use crate::widgets::{self, db_text};
use app_core::types::Command;
use audio_types::RouteSend;
use egui::{Color32, Sense, Ui, WidgetType};

const CELL: egui::Vec2 = egui::vec2(52.0, 30.0);
const OFF: Color32 = Color32::from_gray(38);
const ON: Color32 = Color32::from_rgb(50, 120, 190);

/// dB per pixel of drag; holding Shift makes it fine.
const DRAG_DB_PER_PX: f32 = 0.3;
const DRAG_DB_PER_PX_FINE: f32 = 0.03;

pub fn matrix(ui: &mut Ui, cx: &mut Ctx<'_>) {
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(4.0);
            egui::Grid::new("routing-matrix")
                .spacing([4.0, 4.0])
                .show(ui, |ui| {
                    ui.label("");
                    for b in 0..cx.view.buses_shown {
                        ui.vertical_centered(|ui| {
                            ui.label(egui::RichText::new(bus_name(cx.snap, b)).strong());
                            widgets::caption(
                                ui,
                                &truncate(cx.snap.buses[b].config.label.as_str(), 8),
                            );
                        });
                    }
                    ui.end_row();
                    for i in 0..cx.view.strips_shown {
                        let label = cx.snap.strips[i].config.label;
                        ui.label(truncate(label.as_str(), 12));
                        for b in 0..cx.view.buses_shown {
                            cell(ui, cx, i, b);
                        }
                        ui.end_row();
                    }
                });
            ui.add_space(8.0);
            widgets::caption(
                ui,
                "Click: on/off    Drag up/down: send gain (Shift = fine)    Right-click: 0 dB",
            );
        });
}

fn cell(ui: &mut Ui, cx: &mut Ctx<'_>, i: usize, b: usize) {
    let send = cx.snap.strips[i].config.sends[b];
    let (rect, resp) = ui.allocate_exact_size(CELL, Sense::click_and_drag());
    widgets::name(
        &resp,
        WidgetType::Button,
        format!("Route strip {} to bus {}", i + 1, b + 1),
    );
    let cmd = |send: RouteSend| Command::Route {
        strip: i as u8,
        bus: b as u8,
        send,
    };

    if resp.clicked() {
        cx.out.send(cmd(RouteSend {
            enabled: !send.enabled,
            ..send
        }));
    }
    if resp.dragged() {
        let fine = ui.input(|inp| inp.modifiers.shift);
        let per_px = if fine {
            DRAG_DB_PER_PX_FINE
        } else {
            DRAG_DB_PER_PX
        };
        let gain = (send.gain_db - resp.drag_delta().y * per_px).clamp(-60.0, 12.0);
        // ? Dragging a cell that is off switches it on: setting a gain on a silent send would
        // ? otherwise change nothing the user can hear.
        if (gain - send.gain_db).abs() > f32::EPSILON || !send.enabled {
            cx.out.send(cmd(RouteSend {
                enabled: true,
                gain_db: gain,
                ..send
            }));
        }
    }
    if resp.secondary_clicked() {
        cx.out.send(cmd(RouteSend {
            gain_db: 0.0,
            ..send
        }));
    }

    if ui.is_rect_visible(rect) {
        let fill = if send.enabled { ON } else { OFF };
        ui.painter().rect_filled(rect, 4.0, fill);
        if resp.hovered() {
            ui.painter().rect_stroke(
                rect,
                4.0,
                egui::Stroke::new(1.0_f32, Color32::WHITE),
                egui::StrokeKind::Inside,
            );
        }
        let text = if send.enabled {
            db_text(send.gain_db)
        } else {
            "·".to_owned()
        };
        widgets::cell_text(
            ui,
            rect,
            &text,
            if send.enabled {
                Color32::WHITE
            } else {
                Color32::from_gray(110)
            },
        );
    }
    resp.on_hover_text(format!(
        "{} → {}: {}",
        cx.snap.strips[i].config.label.as_str(),
        bus_name(cx.snap, b),
        if send.enabled {
            format!("{} dB", db_text(send.gain_db))
        } else {
            "off".into()
        }
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_steps_are_sane() {
        // 100 px of drag is 30 dB normally and 3 dB when fine.
        assert!((100.0 * DRAG_DB_PER_PX - 30.0).abs() < 1e-4);
        assert!((100.0 * DRAG_DB_PER_PX_FINE - 3.0).abs() < 1e-4);
    }
}
