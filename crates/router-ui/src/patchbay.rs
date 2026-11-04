//! Patchbay Cards (PLAN.md §9.4): sources on the left, buses on the right, wires between them.
//!
//! * A card is 52 px high: handle, kind badge, label, fader, Mute/Solo, expand arrow, and a
//!   socket on its inner edge. The meter is the card's background.
//! * Drag from a source socket to a bus socket to make the send. Right-click a wire for its gain.
//! * `▾` opens the card's drawer in place (sources, send faders, pan); "Inspector" opens the same
//!   inspector the strips view uses, in a column on the right.
//! * Drag a source card by its handle to reorder. RichMotion springs the other cards aside and
//!   tilts the held card with the pointer; EcoReducedMotion moves them instantly.
//!
//! Wires and cards are drawn from the newest snapshot. A gesture only collects `Command`s in
//! `cx.out` (CLAUDE.md law 5): nothing here touches the engine.

use crate::motion::{self, Spring};
use crate::strips::{link_line, sink_picker, source_picker, truncate};
use crate::view::{Ctx, Target, bus_name};
use crate::widgets::{self, FADER_MAX_DB, FADER_MIN_DB, db_text, meter_fill};
use app_core::types::Command;
use audio_types::{MAX_BUSES, MAX_STRIPS, RouteSend, SourceEndpoint};
use egui::{
    Align, Button, Color32, Layout, Pos2, Rect, RichText, Sense, Shape, Slider, Stroke, TextEdit,
    Ui, UiBuilder, WidgetType, pos2, vec2,
};

/// Height of a collapsed card.
pub const CARD_H: f32 = 52.0;
const GAP: f32 = 6.0;
const ROW_H: f32 = 26.0;
const SOCKET_R: f32 = 6.5;
/// How close a wire end has to be to a socket to connect, and a click to a wire to pick it.
const SOCKET_HIT: f32 = 16.0;
const WIRE_HIT: f32 = 8.0;
const MIN_COL: f32 = 300.0;
const MAX_COL: f32 = 420.0;
const MIN_WIRE_SPACE: f32 = 40.0;
const WIRE_COLOR: Color32 = Color32::from_rgb(80, 150, 220);
const SELECTED_COLOR: Color32 = Color32::from_rgb(245, 245, 250);
const MUTE_COLOR: Color32 = Color32::from_rgb(190, 60, 50);
const SOLO_COLOR: Color32 = Color32::from_rgb(200, 170, 40);
/// A strip quieter than this has nothing flowing down its wires.
const ACTIVE_DB: f32 = -50.0;

/// The narrowest window the two columns and the wires between them fit in.
pub const MIN_WIDTH: f32 = 2.0 * MIN_COL + MIN_WIRE_SPACE;

// SECTION: State
/// What the Patchbay remembers between frames. UI only: none of it goes to the daemon.
#[derive(Clone, Debug, Default)]
pub struct PatchState {
    /// Each source card's top edge, springing toward its slot.
    tops: [Spring; MAX_STRIPS],
    placed: [bool; MAX_STRIPS],
    /// The card being dragged by its handle, and where its top edge is.
    held: Option<usize>,
    held_top: f32,
    /// The strip whose socket a new wire is being dragged from.
    wiring: Option<u8>,
    /// The wire whose gain is open: (strip, bus).
    pub selected: Option<(u8, u8)>,
    /// Where each bus socket was last frame: the targets of a wire being dropped.
    bus_sockets: Vec<(u8, Pos2)>,
    /// A label being typed into.
    editing: Option<(Target, String)>,
}

impl PatchState {
    /// Is a card being dragged or a wire being pulled? (The tests ask.)
    pub fn busy(&self) -> bool {
        self.held.is_some() || self.wiring.is_some()
    }
}
// !SECTION: State

// SECTION: Sizes
fn drawer_h_strip(buses: usize) -> f32 {
    12.0 + 44.0 + buses as f32 * ROW_H + ROW_H + 34.0
}

const DRAWER_H_BUS: f32 = 12.0 + 44.0 + 34.0;

fn strip_h(cx: &Ctx<'_>, i: usize) -> f32 {
    if cx.snap.ui.strips[i].expanded_in_cards {
        CARD_H + drawer_h_strip(cx.view.buses_shown)
    } else {
        CARD_H
    }
}

fn bus_h(cx: &Ctx<'_>, b: usize) -> f32 {
    if cx.snap.ui.buses[b].expanded_in_cards {
        CARD_H + DRAWER_H_BUS
    } else {
        CARD_H
    }
}
// !SECTION: Sizes

/// Is there room for the Patchbay at this width? (Inspector column included.)
pub fn fits(available_width: f32, inspector_open: bool) -> bool {
    let reserved = if inspector_open {
        crate::inspector::DRAWER_W + 8.0
    } else {
        0.0
    };
    available_width - reserved >= MIN_WIDTH
}

// SECTION: The view
pub fn patchbay(ui: &mut Ui, cx: &mut Ctx<'_>) {
    let inspector_open = cx.view.drawer.is_some();
    if cx.narrow || !fits(ui.available_width(), inspector_open) {
        ui.label(
            RichText::new(format!(
                "The Patchbay needs a window about {} px wide; showing the matrix.",
                MIN_WIDTH as u32
            ))
            .weak(),
        );
        crate::matrix::matrix(ui, cx);
        return;
    }
    drop_stale_selection(cx);

    // The background takes right-clicks on wires. It is created first, so every card and socket
    // is above it and takes its own clicks.
    let area = ui.available_rect_before_wrap();
    let bg = ui.interact(area, ui.id().with("patchbay-bg"), Sense::click());
    let wire_bar_h = wire_bar(ui, cx);
    let area = Rect::from_min_max(pos2(area.min.x, area.min.y + wire_bar_h), area.max);

    let inspector_w = if inspector_open {
        crate::inspector::DRAWER_W + 8.0
    } else {
        0.0
    };
    let usable = area.width() - inspector_w;
    let col_w = ((usable - 2.0 * MIN_WIRE_SPACE) / 2.0).clamp(MIN_COL, MAX_COL);
    let left = Rect::from_min_size(area.min, vec2(col_w, area.height()));
    let right = Rect::from_min_size(
        pos2(area.min.x + usable - col_w, area.min.y),
        vec2(col_w, area.height()),
    );

    let mut sources: Vec<(u8, Pos2)> = Vec::new();
    ui.scope_builder(UiBuilder::new().max_rect(left), |ui| {
        egui::ScrollArea::vertical()
            .id_salt("patchbay-sources")
            .auto_shrink([false, false])
            .show(ui, |ui| source_column(ui, cx, col_w, &mut sources));
    });
    let mut buses: Vec<(u8, Pos2)> = Vec::new();
    ui.scope_builder(UiBuilder::new().max_rect(right), |ui| {
        egui::ScrollArea::vertical()
            .id_salt("patchbay-buses")
            .auto_shrink([false, false])
            .show(ui, |ui| bus_column(ui, cx, col_w, &mut buses));
    });

    if let Some(t) = cx.view.drawer {
        let rect = Rect::from_min_size(
            pos2(area.max.x - crate::inspector::DRAWER_W, area.min.y),
            vec2(crate::inspector::DRAWER_W, area.height()),
        );
        ui.scope_builder(UiBuilder::new().max_rect(rect), |ui| {
            crate::inspector::drawer(ui, cx, t, area.height());
        });
    }

    let wires = Rect::from_min_max(
        pos2(area.min.x, area.min.y),
        pos2(area.min.x + usable, area.max.y),
    );
    draw_wires(ui, cx, wires, &sources, &buses);
    cx.view.patch.bus_sockets = buses;

    // Clicking a wire (in the gap, where nothing else is) picks it; clicking empty space drops it.
    if bg.secondary_clicked() {
        if let Some(p) = ui.input(|i| i.pointer.interact_pos()) {
            cx.view.patch.selected = wire_at(cx, &sources, p);
        }
    } else if bg.clicked() {
        cx.view.patch.selected = None;
    }

    // ? A gesture whose widget vanished (a card scrolled away, the window lost the mouse) would
    // ? otherwise leave a wire or a card stuck to the pointer.
    if !ui.input(|i| i.pointer.any_down()) {
        cx.view.patch.wiring = None;
        cx.view.patch.held = None;
    }
}

/// The selected wire may have been switched off (by someone else, or by the bar): forget it.
fn drop_stale_selection(cx: &mut Ctx<'_>) {
    if let Some((s, b)) = cx.view.patch.selected {
        let on = cx
            .snap
            .strips
            .get(usize::from(s))
            .and_then(|st| st.config.sends.get(usize::from(b)))
            .is_some_and(|send| send.enabled);
        if !on {
            cx.view.patch.selected = None;
        }
    }
}

/// The inline bar for the selected wire. Returns its height.
fn wire_bar(ui: &mut Ui, cx: &mut Ctx<'_>) -> f32 {
    let Some((s, b)) = cx.view.patch.selected else {
        return 0.0;
    };
    let send = cx.snap.strips[usize::from(s)].config.sends[usize::from(b)];
    let label = cx.snap.strips[usize::from(s)].config.label;
    let top = ui.cursor().min.y;
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!(
                "{} → {}",
                truncate(label.as_str(), 14),
                bus_name(cx.snap, usize::from(b))
            ))
            .strong(),
        );
        let mut gain = send.gain_db;
        ui.spacing_mut().slider_width = 220.0;
        let r = ui.add(Slider::new(&mut gain, FADER_MIN_DB..=FADER_MAX_DB).suffix(" dB"));
        widgets::name(
            &r,
            WidgetType::Slider,
            format!("Wire gain strip {} bus {}", s + 1, b + 1),
        );
        if r.changed() {
            cx.out.send(Command::Route {
                strip: s,
                bus: b,
                send: RouteSend {
                    gain_db: gain,
                    ..send
                },
            });
        }
        let cut = ui.button("Disconnect");
        widgets::name(
            &cut,
            WidgetType::Button,
            format!("Disconnect strip {} from bus {}", s + 1, b + 1),
        );
        if cut.clicked() {
            cx.out.send(Command::Route {
                strip: s,
                bus: b,
                send: RouteSend {
                    enabled: false,
                    ..send
                },
            });
            cx.view.patch.selected = None;
        }
        if ui.small_button("✕").on_hover_text("Close").clicked() {
            cx.view.patch.selected = None;
        }
    });
    ui.add_space(4.0);
    ui.cursor().min.y - top
}
// !SECTION: The view

// SECTION: Wires
/// All wires as (strip, bus, curve), for the sockets drawn this frame.
fn all_wires(
    cx: &Ctx<'_>,
    sources: &[(u8, Pos2)],
    buses: &[(u8, Pos2)],
) -> Vec<(u8, u8, [Pos2; 4])> {
    let mut out = Vec::new();
    for &(s, from) in sources {
        for &(b, to) in buses {
            if cx.snap.strips[usize::from(s)].config.sends[usize::from(b)].enabled {
                out.push((s, b, motion::wire(from, to)));
            }
        }
    }
    out
}

/// The wire closest to `p`, if one is within reach.
fn wire_at(cx: &Ctx<'_>, sources: &[(u8, Pos2)], p: Pos2) -> Option<(u8, u8)> {
    let buses = &cx.view.patch.bus_sockets;
    all_wires(cx, sources, buses)
        .into_iter()
        .map(|(s, b, w)| (motion::distance_to_wire(&w, p), (s, b)))
        .filter(|(d, _)| *d <= WIRE_HIT)
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, sb)| sb)
}

fn draw_wires(ui: &mut Ui, cx: &Ctx<'_>, clip: Rect, sources: &[(u8, Pos2)], buses: &[(u8, Pos2)]) {
    let painter = ui.painter_at(clip);
    let time = ui.input(|i| i.time);
    for (s, b, curve) in all_wires(cx, sources, buses) {
        let selected = cx.view.patch.selected == Some((s, b));
        let live = strip_peak(cx, usize::from(s)) > ACTIVE_DB;
        let (color, width) = if selected {
            (SELECTED_COLOR, 3.5_f32)
        } else if live {
            (WIRE_COLOR, 2.5_f32)
        } else {
            (WIRE_COLOR.gamma_multiply(0.55), 2.0_f32)
        };
        // ? egui anti-aliases path strokes, so the Eco profile's static wire is smooth too.
        painter.add(Shape::CubicBezier(
            egui::epaint::CubicBezierShape::from_points_stroke(
                curve,
                false,
                Color32::TRANSPARENT,
                Stroke::new(width, color),
            ),
        ));
        if cx.gfx.wire_particles && live {
            for t in motion::particle_positions(time, 3, 0.6) {
                painter.circle_filled(motion::bezier_at(&curve, t), 2.6, Color32::WHITE);
            }
        }
    }
    // The wire being pulled.
    if let Some(s) = cx.view.patch.wiring {
        if let (Some(&(_, from)), Some(p)) = (
            sources.iter().find(|(i, _)| *i == s),
            ui.input(|i| i.pointer.latest_pos()),
        ) {
            let snap_to = nearest_bus(&cx.view.patch.bus_sockets, p);
            let end = snap_to.map_or(p, |(_, at)| at);
            painter.add(Shape::CubicBezier(
                egui::epaint::CubicBezierShape::from_points_stroke(
                    motion::wire(from, end),
                    false,
                    Color32::TRANSPARENT,
                    Stroke::new(2.5_f32, SELECTED_COLOR.gamma_multiply(0.8)),
                ),
            ));
        }
    }
}

fn nearest_bus(buses: &[(u8, Pos2)], p: Pos2) -> Option<(u8, Pos2)> {
    buses
        .iter()
        .map(|&(b, at)| (at.distance(p), (b, at)))
        .filter(|(d, _)| *d <= SOCKET_HIT * 1.5)
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, v)| v)
}

// !SECTION: Wires

// SECTION: Source cards
fn source_column(ui: &mut Ui, cx: &mut Ctx<'_>, col_w: f32, sockets: &mut Vec<(u8, Pos2)>) {
    let shown = cx.view.strips_shown;
    let orders: Vec<u8> = cx.snap.ui.strips.iter().map(|c| c.card_order).collect();
    let heights: Vec<f32> = (0..MAX_STRIPS)
        .map(|i| if i < shown { strip_h(cx, i) } else { 0.0 })
        .collect();
    let height = |i: usize| heights[i];
    let order = motion::card_order(shown, &orders);
    let held = cx.view.patch.held.filter(|&h| h < shown);
    let display = match held {
        Some(h) => motion::order_while_dragging(
            &order,
            h,
            cx.view.patch.held_top + heights[h] / 2.0,
            &height,
            GAP,
        ),
        None => order.clone(),
    };
    let tops = motion::slot_tops(&display, &height, GAP);
    let content_h = tops.last().map_or(0.0, |&(i, t)| t + heights[i] + GAP) + 32.0;
    let (area, _) = ui.allocate_exact_size(vec2(col_w, content_h), Sense::hover());

    // Move every card toward its slot.
    let dt = ui.input(|i| i.stable_dt);
    let springy = cx.gfx.card_spring_physics;
    let mut moving = false;
    for &(i, target) in &tops {
        let st = &mut cx.view.patch;
        if held == Some(i) {
            let top = st.held_top;
            st.tops[i].snap(top);
        } else if !st.placed[i] || !springy {
            st.tops[i].snap(target);
            st.placed[i] = true;
        } else {
            st.tops[i].step(target, dt);
            moving |= !st.tops[i].settled(target);
        }
    }
    if moving {
        // ? A spring that has not come to rest needs its next frame now; the governor's timer
        // ? may be 200 ms away. This ends by itself when every card is settled, and it only
        // ? starts after a gesture, so it is not the unconditional repaint law 5 forbids.
        ui.ctx().request_repaint();
    }

    // Draw in slot order, the held card last so it floats above the others.
    let mut draw_order: Vec<usize> = display.clone();
    if let Some(h) = held {
        draw_order.retain(|&i| i != h);
        draw_order.push(h);
    }
    let mut dropped: Option<Vec<usize>> = None;
    for i in draw_order {
        let top = cx.view.patch.tops[i].pos;
        let rect = Rect::from_min_size(area.min + vec2(0.0, top), vec2(col_w, heights[i]));
        let outcome = strip_card(ui, cx, i, rect, held == Some(i), sockets);
        if outcome.dropped {
            dropped = Some(display.clone());
        }
    }
    if let Some(new_order) = dropped {
        let ranks = motion::ranks(&new_order, MAX_STRIPS);
        cx.out.edit_layout(cx.snap, |l| {
            for &i in &new_order {
                l.strips[i].card_order = ranks[i];
            }
        });
    }

    // "+" for another strip.
    if shown < MAX_STRIPS {
        let rect = Rect::from_min_size(pos2(area.min.x, area.max.y - 28.0), vec2(28.0, 24.0));
        let b = ui.put(rect, Button::new("+"));
        widgets::name(&b, WidgetType::Button, "Show another strip");
        if b.clicked() {
            cx.view.strips_shown += 1;
        }
    }
}

#[derive(Default)]
struct CardOutcome {
    dropped: bool,
}

fn strip_peak(cx: &Ctx<'_>, i: usize) -> f32 {
    let ch = usize::from(cx.snap.strips[i].config.channels).clamp(1, 8);
    cx.strip_levels(i).map_or(-120.0, |l| {
        l.peak_db[..ch].iter().copied().fold(-120.0, f32::max)
    })
}

fn bus_peak(cx: &Ctx<'_>, b: usize) -> f32 {
    let ch = usize::from(cx.snap.buses[b].config.channels).clamp(1, 8);
    cx.bus_levels(b).map_or(-120.0, |l| {
        l.peak_db[..ch].iter().copied().fold(-120.0, f32::max)
    })
}

fn level_color(db: f32) -> Color32 {
    if db >= -1.0 {
        Color32::from_rgb(230, 60, 50)
    } else if db >= -12.0 {
        Color32::from_rgb(235, 190, 50)
    } else {
        Color32::from_rgb(70, 200, 100)
    }
}

/// Card background: the meter as a bar growing from the left edge.
fn card_background(ui: &Ui, header: Rect, peak_db: f32, tilt: Option<f32>) {
    let painter = ui.painter();
    let base = Color32::from_gray(34);
    if let Some(angle) = tilt {
        let corners = [
            header.left_top(),
            header.right_top(),
            header.right_bottom(),
            header.left_bottom(),
        ];
        let turned = motion::rotate_around(header.center(), angle, corners);
        painter.add(Shape::convex_polygon(
            turned.to_vec(),
            Color32::from_gray(48),
            Stroke::new(1.0_f32, Color32::from_gray(120)),
        ));
        return;
    }
    painter.rect_filled(header, 6.0, base);
    let fill = meter_fill(peak_db);
    if fill > 0.0 {
        let bar = Rect::from_min_size(header.min, vec2(header.width() * fill, header.height()));
        painter.rect_filled(bar, 6.0, level_color(peak_db).gamma_multiply(0.28));
    }
}

fn kind_badge(src: &SourceEndpoint) -> (&'static str, Color32) {
    match src {
        SourceEndpoint::Unassigned => ("–", Color32::from_gray(90)),
        SourceEndpoint::HardwareInput { .. } => ("M", Color32::from_rgb(70, 140, 200)),
        SourceEndpoint::DeviceLoopback { .. } => ("L", Color32::from_rgb(120, 110, 200)),
        SourceEndpoint::ProcessLoopback { .. } => ("A", Color32::from_rgb(190, 120, 60)),
        SourceEndpoint::VirtualInput { .. } => ("V", Color32::from_rgb(110, 170, 130)),
        SourceEndpoint::AndroidMic { .. } | SourceEndpoint::AndroidPlaybackCapture { .. } => {
            ("P", Color32::from_rgb(70, 140, 200))
        }
        SourceEndpoint::LanIn(_) => ("N", Color32::from_rgb(60, 170, 170)),
    }
}

fn badge(ui: &mut Ui, text: &str, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 11.0, color);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(12.0),
        Color32::WHITE,
    );
}

fn socket(
    ui: &mut Ui,
    at: Pos2,
    id: impl std::hash::Hash,
    name: String,
    lit: bool,
) -> egui::Response {
    let rect = Rect::from_center_size(at, vec2(SOCKET_HIT * 1.4, SOCKET_HIT * 1.4));
    let resp = ui.interact(rect, ui.id().with(id), Sense::click_and_drag());
    widgets::name(&resp, WidgetType::Button, name);
    let fill = if lit || resp.hovered() || resp.dragged() {
        SELECTED_COLOR
    } else {
        Color32::from_gray(150)
    };
    ui.painter()
        .circle_filled(at, SOCKET_R, Color32::from_gray(20));
    ui.painter()
        .circle_stroke(at, SOCKET_R, Stroke::new(2.0_f32, fill));
    resp
}

/// A label that turns into a text box on double-click. Enter keeps the edit, Escape drops it.
/// Returns the new text when one was committed.
fn label_edit(
    ui: &mut Ui,
    cx: &mut Ctx<'_>,
    t: Target,
    shown: &str,
    n: usize,
    kind: &str,
) -> Option<String> {
    let editing_here = cx.view.patch.editing.as_ref().is_some_and(|(e, _)| *e == t);
    if editing_here {
        let mut done = None;
        if let Some((_, buf)) = cx.view.patch.editing.as_mut() {
            let r = ui.add(
                TextEdit::singleline(buf)
                    .desired_width(ui.available_width().max(40.0))
                    .char_limit(24),
            );
            widgets::name(&r, WidgetType::TextEdit, format!("Label of {kind} {n}"));
            if !r.has_focus() && !r.lost_focus() {
                r.request_focus();
            }
            let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
            if escape {
                done = Some(None);
            } else if r.lost_focus() {
                done = Some(Some(buf.trim().to_owned()));
            }
        }
        if let Some(result) = done {
            cx.view.patch.editing = None;
            return result.filter(|s| !s.is_empty());
        }
        None
    } else {
        let l = ui.add(
            egui::Label::new(RichText::new(truncate(shown, 16)).strong()).sense(Sense::click()),
        );
        widgets::name(&l, WidgetType::Button, format!("Name of {kind} {n}"));
        if l.double_clicked() {
            cx.view.patch.editing = Some((t, shown.to_owned()));
        }
        None
    }
}

fn small_toggle(text: &str, on: Option<Color32>) -> Button<'_> {
    let b = Button::new(RichText::new(text).size(12.0)).min_size(vec2(24.0, 20.0));
    match on {
        Some(c) => b.fill(c),
        None => b,
    }
}

fn strip_card(
    ui: &mut Ui,
    cx: &mut Ctx<'_>,
    i: usize,
    rect: Rect,
    held: bool,
    sockets: &mut Vec<(u8, Pos2)>,
) -> CardOutcome {
    let n = i + 1;
    let cfg = cx.snap.strips[i].config;
    let expanded = cx.snap.ui.strips[i].expanded_in_cards;
    let header = Rect::from_min_size(rect.min, vec2(rect.width(), CARD_H));
    let mut outcome = CardOutcome::default();

    let tilt = (held && cx.gfx.card_spring_physics)
        .then(|| motion::tilt(ui.input(|i| i.pointer.velocity().x)));
    card_background(ui, header, strip_peak(cx, i), tilt);

    // The header row.
    let inner = header.shrink2(vec2(10.0, 0.0));
    // ? `id_salt` matters: without it every card's row gets the same widget ids, so the drag
    // ? handles of different cards would be one widget to egui and a drag could not tell them apart.
    let mut row = ui.new_child(
        UiBuilder::new()
            .id_salt(("patch-strip-row", i))
            .max_rect(inner)
            .layout(Layout::left_to_right(Align::Center)),
    );
    row.spacing_mut().item_spacing = vec2(6.0, 0.0);
    row.spacing_mut().button_padding = vec2(3.0, 1.0);
    let ui2 = &mut row;

    // Handle: drag to reorder.
    // ? The id is spelled out on purpose. egui derives a child ui's automatic widget ids from the
    // ? ORDER the children were made in, and the held card is drawn last, so its automatic id would
    // ? change the moment the drag starts and egui would drop the drag. An explicit id does not move.
    let (hrect, _) = ui2.allocate_exact_size(vec2(14.0, 28.0), Sense::hover());
    let handle = ui2.interact(hrect, egui::Id::new(("patch-handle", i)), Sense::drag());
    widgets::name(&handle, WidgetType::Button, format!("Move card strip {n}"));
    for dy in [-8.0_f32, 0.0, 8.0] {
        for dx in [-3.0_f32, 3.0] {
            ui2.painter().circle_filled(
                hrect.center() + vec2(dx, dy),
                1.4,
                Color32::from_gray(140),
            );
        }
    }
    if handle.drag_started() {
        let st = &mut cx.view.patch;
        st.held = Some(i);
        st.held_top = st.tops[i].pos;
    }
    if handle.dragged() && cx.view.patch.held == Some(i) {
        cx.view.patch.held_top += handle.drag_delta().y;
    }
    if handle.drag_stopped() && cx.view.patch.held == Some(i) {
        cx.view.patch.held = None;
        outcome.dropped = true;
    }

    let (letter, color) = kind_badge(&cfg.source);
    badge(ui2, letter, color);

    // Right-hand controls first, so the label gets whatever is left.
    ui2.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing = vec2(5.0, 0.0);
        ui.add_space(10.0);
        let arrow = if expanded { "▴" } else { "▾" };
        let e = ui.add(small_toggle(arrow, None));
        widgets::name(&e, WidgetType::Button, format!("Expand card strip {n}"));
        if e.clicked() {
            cx.out
                .edit_layout(cx.snap, |l| l.strips[i].expanded_in_cards = !expanded);
        }
        let s = ui.add(small_toggle("S", cfg.solo.then_some(SOLO_COLOR)));
        widgets::name(&s, WidgetType::Button, format!("Solo strip {n}"));
        if s.clicked() {
            cx.out.send(Command::StripSolo {
                strip: i as u8,
                solo: !cfg.solo,
            });
        }
        let m = ui.add(small_toggle("M", cfg.mute.then_some(MUTE_COLOR)));
        widgets::name(&m, WidgetType::Button, format!("Mute strip {n}"));
        if m.clicked() {
            cx.out.send(Command::StripMute {
                strip: i as u8,
                mute: !cfg.mute,
            });
        }
        let mut gain = cfg.gain_db;
        ui.spacing_mut().slider_width = 84.0;
        let f = ui.add(Slider::new(&mut gain, FADER_MIN_DB..=FADER_MAX_DB).show_value(false));
        widgets::name(&f, WidgetType::Slider, format!("Fader strip {n}"));
        f.on_hover_text(format!("{} dB", db_text(gain)));
        if gain != cfg.gain_db {
            cx.out.send(Command::StripGain {
                strip: i as u8,
                gain_db: gain,
            });
        }
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            if let Some(text) = label_edit(
                ui,
                cx,
                Target::Strip(i as u8),
                cfg.label.as_str(),
                n,
                "strip",
            ) {
                cx.out.send(Command::StripLabel {
                    strip: i as u8,
                    label: text,
                });
            }
        });
    });

    // The socket on the card's outer edge.
    let at = pos2(header.right(), header.center().y);
    let lit = cx.view.patch.wiring == Some(i as u8);
    let sock = socket(
        ui,
        at,
        ("pb-src", i),
        format!("Output socket strip {n}"),
        lit,
    );
    if sock.drag_started() {
        cx.view.patch.wiring = Some(i as u8);
    }
    if sock.drag_stopped() && cx.view.patch.wiring == Some(i as u8) {
        cx.view.patch.wiring = None;
        if let Some(p) = ui.input(|inp| inp.pointer.interact_pos()) {
            if let Some((b, _)) = nearest_bus(&cx.view.patch.bus_sockets, p) {
                let current = cfg.sends[usize::from(b)];
                if !current.enabled {
                    cx.out.send(Command::Route {
                        strip: i as u8,
                        bus: b,
                        send: RouteSend {
                            enabled: true,
                            ..current
                        },
                    });
                }
            }
        }
    }
    sockets.push((i as u8, at));

    if expanded {
        let body = Rect::from_min_max(pos2(rect.min.x, header.max.y), rect.max);
        strip_drawer(ui, cx, i, body);
    }
    outcome
}

fn strip_drawer(ui: &mut Ui, cx: &mut Ctx<'_>, i: usize, body: Rect) {
    let n = i + 1;
    ui.painter()
        .rect_filled(body.shrink2(vec2(0.0, 2.0)), 6.0, Color32::from_gray(26));
    let mut d = ui.new_child(
        UiBuilder::new()
            .id_salt(("patch-strip-drawer", i))
            .max_rect(body.shrink2(vec2(12.0, 8.0)))
            .layout(Layout::top_down(Align::LEFT)),
    );
    d.spacing_mut().item_spacing = vec2(6.0, 4.0);
    d.spacing_mut().button_padding = vec2(4.0, 1.0);
    let ui = &mut d;

    source_picker(ui, cx, i);
    link_line(ui, &cx.snap.strips[i].link);

    for b in 0..cx.view.buses_shown {
        let send = cx.snap.strips[i].config.sends[b];
        ui.horizontal(|ui| {
            let mut on = send.enabled;
            let c = ui.checkbox(&mut on, bus_name(cx.snap, b));
            widgets::name(
                &c,
                WidgetType::Checkbox,
                format!("Send strip {n} to bus {}", b + 1),
            );
            if c.changed() {
                cx.out.send(Command::Route {
                    strip: i as u8,
                    bus: b as u8,
                    send: RouteSend {
                        enabled: on,
                        ..send
                    },
                });
            }
            let mut gain = send.gain_db;
            ui.spacing_mut().slider_width = 150.0;
            let g = ui.add_enabled(
                send.enabled,
                Slider::new(&mut gain, FADER_MIN_DB..=FADER_MAX_DB).suffix(" dB"),
            );
            widgets::name(
                &g,
                WidgetType::Slider,
                format!("Send gain strip {n} bus {}", b + 1),
            );
            if g.changed() {
                cx.out.send(Command::Route {
                    strip: i as u8,
                    bus: b as u8,
                    send: RouteSend {
                        gain_db: gain,
                        ..send
                    },
                });
            }
        });
    }
    ui.horizontal(|ui| {
        ui.label("Pan");
        let mut pan = cx.snap.strips[i].config.pan;
        ui.spacing_mut().slider_width = 150.0;
        let p = ui.add(Slider::new(&mut pan, -1.0..=1.0).show_value(false));
        widgets::name(&p, WidgetType::Slider, format!("Pan strip {n}"));
        if p.changed() {
            cx.out.send(Command::StripPan {
                strip: i as u8,
                pan,
            });
        }
    });
    inspector_button(
        ui,
        cx,
        Target::Strip(i as u8),
        format!("Open inspector strip {n}"),
    );
}

fn inspector_button(ui: &mut Ui, cx: &mut Ctx<'_>, t: Target, name: String) {
    let open = cx.view.drawer == Some(t);
    let b = ui.add(Button::new("Inspector ▸").selected(open));
    widgets::name(&b, WidgetType::Button, name);
    if b.clicked() {
        cx.view.drawer = if open { None } else { Some(t) };
    }
}
// !SECTION: Source cards

// SECTION: Bus cards
fn bus_column(ui: &mut Ui, cx: &mut Ctx<'_>, col_w: f32, sockets: &mut Vec<(u8, Pos2)>) {
    let shown = cx.view.buses_shown;
    let heights: Vec<f32> = (0..MAX_BUSES)
        .map(|b| if b < shown { bus_h(cx, b) } else { 0.0 })
        .collect();
    let content_h: f32 = heights.iter().take(shown).map(|h| h + GAP).sum::<f32>() + 32.0;
    let (area, _) = ui.allocate_exact_size(vec2(col_w, content_h), Sense::hover());
    let mut y = 0.0;
    for (b, &h) in heights.iter().enumerate().take(shown) {
        let rect = Rect::from_min_size(area.min + vec2(0.0, y), vec2(col_w, h));
        bus_card(ui, cx, b, rect, sockets);
        y += h + GAP;
    }
    if shown < MAX_BUSES {
        let rect = Rect::from_min_size(pos2(area.min.x, area.max.y - 28.0), vec2(28.0, 24.0));
        let b = ui.put(rect, Button::new("+"));
        widgets::name(&b, WidgetType::Button, "Show another bus");
        if b.clicked() {
            cx.view.buses_shown += 1;
        }
    }
}

fn bus_card(ui: &mut Ui, cx: &mut Ctx<'_>, b: usize, rect: Rect, sockets: &mut Vec<(u8, Pos2)>) {
    let n = b + 1;
    let cfg = cx.snap.buses[b].config;
    let expanded = cx.snap.ui.buses[b].expanded_in_cards;
    let header = Rect::from_min_size(rect.min, vec2(rect.width(), CARD_H));
    card_background(ui, header, bus_peak(cx, b), None);

    let inner = header.shrink2(vec2(10.0, 0.0));
    // ? `id_salt` per card: see `strip_card`.
    let mut row = ui.new_child(
        UiBuilder::new()
            .id_salt(("patch-bus-row", b))
            .max_rect(inner)
            .layout(Layout::left_to_right(Align::Center)),
    );
    row.spacing_mut().item_spacing = vec2(6.0, 0.0);
    row.spacing_mut().button_padding = vec2(3.0, 1.0);
    let ui2 = &mut row;
    ui2.add_space(8.0);
    badge(ui2, &bus_name(cx.snap, b), Color32::from_rgb(70, 110, 160));
    ui2.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing = vec2(5.0, 0.0);
        let arrow = if expanded { "▴" } else { "▾" };
        let e = ui.add(small_toggle(arrow, None));
        widgets::name(&e, WidgetType::Button, format!("Expand card bus {n}"));
        if e.clicked() {
            cx.out
                .edit_layout(cx.snap, |l| l.buses[b].expanded_in_cards = !expanded);
        }
        let m = ui.add(small_toggle("M", cfg.mute.then_some(MUTE_COLOR)));
        widgets::name(&m, WidgetType::Button, format!("Mute bus {n}"));
        if m.clicked() {
            cx.out.send(Command::BusMute {
                bus: b as u8,
                mute: !cfg.mute,
            });
        }
        let mut gain = cfg.gain_db;
        ui.spacing_mut().slider_width = 84.0;
        let f = ui.add(Slider::new(&mut gain, FADER_MIN_DB..=FADER_MAX_DB).show_value(false));
        widgets::name(&f, WidgetType::Slider, format!("Fader bus {n}"));
        f.on_hover_text(format!("{} dB", db_text(gain)));
        if gain != cfg.gain_db {
            cx.out.send(Command::BusGain {
                bus: b as u8,
                gain_db: gain,
            });
        }
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            if let Some(text) =
                label_edit(ui, cx, Target::Bus(b as u8), cfg.label.as_str(), n, "bus")
            {
                cx.out.send(Command::BusLabel {
                    bus: b as u8,
                    label: text,
                });
            }
        });
    });

    let at = pos2(header.left(), header.center().y);
    socket(
        ui,
        at,
        ("pb-bus", b),
        format!("Input socket bus {n}"),
        false,
    );
    sockets.push((b as u8, at));

    if expanded {
        let body = Rect::from_min_max(pos2(rect.min.x, header.max.y), rect.max);
        ui.painter()
            .rect_filled(body.shrink2(vec2(0.0, 2.0)), 6.0, Color32::from_gray(26));
        let mut d = ui.new_child(
            UiBuilder::new()
                .id_salt(("patch-bus-drawer", b))
                .max_rect(body.shrink2(vec2(12.0, 8.0)))
                .layout(Layout::top_down(Align::LEFT)),
        );
        d.spacing_mut().item_spacing = vec2(6.0, 4.0);
        d.spacing_mut().button_padding = vec2(4.0, 1.0);
        sink_picker(&mut d, cx, b);
        link_line(&mut d, &cx.snap.buses[b].link);
        inspector_button(
            &mut d,
            cx,
            Target::Bus(b as u8),
            format!("Open inspector bus {n}"),
        );
    }
}
// !SECTION: Bus cards
