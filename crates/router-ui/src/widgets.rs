//! Custom-drawn controls: fader, level meter, knob. Each one reports itself to the accessibility
//! tree (`widget_info`) with a unique label such as "Fader Strip 1", which is also how the
//! headless tests find them.

use egui::{
    Align2, Color32, CornerRadius, FontId, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2,
    WidgetInfo, WidgetType, epaint::TextShape, vec2,
};

pub const FADER_MIN_DB: f32 = -60.0;
pub const FADER_MAX_DB: f32 = 12.0;
pub const METER_FLOOR_DB: f32 = -60.0;

/// Fader position (0 = bottom, 1 = top) for a gain.
pub fn db_to_pos(db: f32) -> f32 {
    ((db - FADER_MIN_DB) / (FADER_MAX_DB - FADER_MIN_DB)).clamp(0.0, 1.0)
}

pub fn pos_to_db(pos: f32) -> f32 {
    FADER_MIN_DB + pos.clamp(0.0, 1.0) * (FADER_MAX_DB - FADER_MIN_DB)
}

/// Level-meter fill (0..1) for a level in dB.
pub fn meter_fill(db: f32) -> f32 {
    if db.is_nan() {
        return 0.0;
    }
    ((db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0)
}

fn meter_color(db: f32) -> Color32 {
    if db >= -1.0 {
        Color32::from_rgb(230, 60, 50)
    } else if db >= -12.0 {
        Color32::from_rgb(235, 190, 50)
    } else {
        Color32::from_rgb(70, 200, 100)
    }
}

/// "-6.0 dB", or "-inf" at the bottom of the fader.
pub fn db_text(db: f32) -> String {
    if db <= FADER_MIN_DB {
        "-inf".to_owned()
    } else {
        format!("{db:+.1}")
    }
}

/// Give a response a unique accessible name and role without changing how it draws.
pub fn name(response: &Response, kind: WidgetType, label: impl Into<String>) {
    let label = label.into();
    response.widget_info(|| WidgetInfo::labeled(kind, true, label.clone()));
}

// SECTION: Fader
/// A vertical fader. Drag to set, double-click for 0 dB. Returns the response, with
/// `changed()` set when `db` was modified. `grab` is the handle height: 44 px on touch screens.
pub fn fader(ui: &mut Ui, label: &str, db: &mut f32, height: f32, grab: f32) -> Response {
    let size = vec2(30.0, height.max(60.0));
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click_and_drag());
    let travel = rect.height() - grab;

    // ? No `if a && let Some(..)`: let-chains need Rust 1.88 and the workspace MSRV is 1.85.
    if response.dragged() || response.clicked() {
        if let Some(p) = response.interact_pointer_pos() {
            // ? The handle centre follows the pointer, so the usable travel is the height minus
            // ? one handle. This is also why a drag from the very top lands exactly on +12 dB.
            let pos = 1.0 - ((p.y - rect.top() - grab / 2.0) / travel.max(1.0));
            let new = pos_to_db(pos);
            if (new - *db).abs() > f32::EPSILON {
                *db = new;
                response.mark_changed();
            }
        }
    }
    if response.double_clicked() {
        *db = 0.0;
        response.mark_changed();
    }
    response.widget_info(|| WidgetInfo::slider(true, f64::from(*db), label));

    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let slot = Rect::from_center_size(rect.center(), vec2(4.0, travel + grab - 8.0));
        p.rect_filled(slot, CornerRadius::same(2), Color32::from_gray(25));
        // 0 dB tick.
        let y0 = rect.bottom() - grab / 2.0 - db_to_pos(0.0) * travel;
        p.line_segment(
            [
                Pos2::new(rect.left() + 2.0, y0),
                Pos2::new(rect.right() - 2.0, y0),
            ],
            Stroke::new(1.0_f32, Color32::from_gray(110)),
        );
        let y = rect.bottom() - grab / 2.0 - db_to_pos(*db) * travel;
        let handle =
            Rect::from_center_size(Pos2::new(rect.center().x, y), vec2(26.0, grab.min(22.0)));
        let fill = if response.dragged() {
            Color32::from_gray(190)
        } else if response.hovered() {
            Color32::from_gray(165)
        } else {
            Color32::from_gray(140)
        };
        p.rect_filled(handle, CornerRadius::same(3), fill);
        p.line_segment(
            [
                Pos2::new(handle.left() + 3.0, y),
                Pos2::new(handle.right() - 3.0, y),
            ],
            Stroke::new(2.0_f32, Color32::from_gray(30)),
        );
    }
    response
}
// !SECTION: Fader

// SECTION: Meter
/// Bars for `channels` channels, RMS dim and peak bright, an optional held-peak tick, and a red
/// clip light at the top. `levels` is None while no meters have arrived.
pub fn meter(
    ui: &mut Ui,
    label: &str,
    levels: Option<(&[f32; 8], &[f32; 8], bool)>,
    holds: &dyn Fn(usize) -> Option<f32>,
    channels: usize,
    height: f32,
) -> Response {
    let channels = channels.clamp(1, 8);
    let bar_w = if channels <= 2 { 9.0 } else { 5.0 };
    let width = channels as f32 * (bar_w + 1.0) + 1.0;
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let peak_db = levels.map_or(f32::NEG_INFINITY, |(p, _, _)| {
        p.iter()
            .take(channels)
            .copied()
            .fold(f32::NEG_INFINITY, f32::max)
    });
    response.widget_info(|| {
        let mut info = WidgetInfo::labeled(WidgetType::ProgressIndicator, true, label);
        info.value = Some(f64::from(peak_db.max(-120.0)));
        info
    });
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(2), Color32::from_gray(18));
    let clip = levels.is_some_and(|(_, _, c)| c);
    for ch in 0..channels {
        let x = rect.left() + 1.0 + ch as f32 * (bar_w + 1.0);
        let bar = Rect::from_min_max(
            Pos2::new(x, rect.top() + 6.0),
            Pos2::new(x + bar_w, rect.bottom() - 1.0),
        );
        if let Some((peak, rms, _)) = levels {
            let fill = |db: f32, dim: bool| {
                let h = bar.height() * meter_fill(db);
                let r = Rect::from_min_max(Pos2::new(bar.left(), bar.bottom() - h), bar.max);
                let mut c = meter_color(db);
                if dim {
                    c = c.gamma_multiply(0.55);
                }
                p.rect_filled(r, CornerRadius::ZERO, c);
            };
            fill(peak[ch], true);
            fill(rms[ch], false);
        }
        if let Some(db) = holds(ch) {
            let y = bar.bottom() - bar.height() * meter_fill(db);
            p.line_segment(
                [Pos2::new(bar.left(), y), Pos2::new(bar.right(), y)],
                Stroke::new(1.5_f32, Color32::from_gray(235)),
            );
        }
    }
    let light = Rect::from_min_max(
        Pos2::new(rect.left() + 1.0, rect.top() + 1.0),
        Pos2::new(rect.right() - 1.0, rect.top() + 5.0),
    );
    p.rect_filled(
        light,
        CornerRadius::same(1),
        if clip {
            Color32::from_rgb(240, 40, 40)
        } else {
            Color32::from_gray(45)
        },
    );
    response
}
// !SECTION: Meter

// SECTION: Knob
/// A round knob over `min..=max`. Drag up/down to change, double-click for `reset`.
pub fn knob(
    ui: &mut Ui,
    label: &str,
    value: &mut f32,
    (min, max): (f32, f32),
    reset: f32,
    diameter: f32,
) -> Response {
    let (rect, mut response) =
        ui.allocate_exact_size(Vec2::splat(diameter), Sense::click_and_drag());
    if response.dragged() {
        let per_px = (max - min) / 150.0;
        let fine = ui.input(|i| i.modifiers.shift);
        let step = -response.drag_delta().y * per_px * if fine { 0.1 } else { 1.0 };
        let new = (*value + step).clamp(min, max);
        if (new - *value).abs() > f32::EPSILON {
            *value = new;
            response.mark_changed();
        }
    }
    if response.double_clicked() {
        *value = reset.clamp(min, max);
        response.mark_changed();
    }
    response.widget_info(|| WidgetInfo::slider(true, f64::from(*value), label));

    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let c = rect.center();
        let r = diameter / 2.0 - 2.0;
        p.circle_filled(c, r, Color32::from_gray(45));
        p.circle_stroke(c, r, Stroke::new(1.0_f32, Color32::from_gray(110)));
        // Sweep from 7 o'clock to 5 o'clock (270 degrees).
        let t = if (max - min).abs() < f32::EPSILON {
            0.0
        } else {
            (*value - min) / (max - min)
        };
        let angle = (-225.0_f32 + 270.0 * t).to_radians();
        let tip = c + vec2(angle.cos(), angle.sin()) * (r - 3.0);
        p.line_segment(
            [c, tip],
            Stroke::new(2.5_f32, Color32::from_rgb(120, 190, 255)),
        );
    }
    response
}
// !SECTION: Knob

/// Text drawn upright-rotated (reading bottom to top), for the 28 px pillar.
pub fn vertical_label(ui: &mut Ui, text: &str, height: f32) {
    let font = FontId::proportional(12.0);
    let color = ui.visuals().text_color();
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, color);
    let size = vec2(galley.size().y, height);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    // ? A rotated galley is drawn around its top-left corner, so start at the bottom of the box
    // ? and turn it a quarter turn anticlockwise to read upwards.
    let pos = Pos2::new(rect.left(), rect.bottom().min(rect.top() + galley.size().x));
    let shape = TextShape::new(pos, galley, color).with_angle(-std::f32::consts::FRAC_PI_2);
    ui.painter().add(shape);
}

/// A small caption in the quiet colour.
pub fn caption(ui: &mut Ui, text: &str) {
    ui.label(egui::RichText::new(text).small().weak());
}

/// Centered, clipped text on a rectangle (used by the matrix cells).
pub fn cell_text(ui: &Ui, rect: Rect, text: &str, color: Color32) {
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::proportional(11.0),
        color,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fader_mapping_round_trips_and_clamps() {
        for db in [-60.0, -30.0, 0.0, 6.0, 12.0] {
            assert!((pos_to_db(db_to_pos(db)) - db).abs() < 1e-4, "{db}");
        }
        assert_eq!(pos_to_db(-1.0), FADER_MIN_DB);
        assert_eq!(pos_to_db(2.0), FADER_MAX_DB);
        assert_eq!(db_to_pos(-200.0), 0.0);
    }

    #[test]
    fn meter_fill_is_bounded_and_nan_safe() {
        assert_eq!(meter_fill(-120.0), 0.0);
        assert_eq!(meter_fill(0.0), 1.0);
        assert_eq!(meter_fill(6.0), 1.0);
        assert_eq!(meter_fill(f32::NAN), 0.0);
        assert!((meter_fill(-30.0) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn gain_text_reads_naturally() {
        assert_eq!(db_text(0.0), "+0.0");
        assert_eq!(db_text(-6.04), "-6.0");
        assert_eq!(db_text(-60.0), "-inf");
    }
}
