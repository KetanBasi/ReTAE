//! The arithmetic behind the Patchbay Cards (PLAN.md §9.4). Pure: no egui widgets, no clock, so
//! every rule here is a plain unit test. `patchbay.rs` draws with it.
//!
//! * Wires are cubic Béziers from a source socket to a bus socket.
//! * Cards sit in a column; their order is `ChannelViewState::card_order`, and a card being
//!   dragged makes the others move aside.
//! * RichMotion moves a card with a spring and tilts it with the pointer speed. EcoReducedMotion
//!   uses `Spring::snap`, so the same code path gives instant movement.

use egui::{Pos2, Vec2, pos2, vec2};

// SECTION: Wires
/// A wire from `from` (a source socket) to `to` (a bus socket): four Bézier control points.
// ? The handles run horizontally out of each socket, so a wire leaves a card sideways and arrives
// ? sideways even when the two sockets are at very different heights.
pub fn wire(from: Pos2, to: Pos2) -> [Pos2; 4] {
    let reach = ((to.x - from.x).abs() * 0.5).max(40.0);
    [
        from,
        pos2(from.x + reach, from.y),
        pos2(to.x - reach, to.y),
        to,
    ]
}

/// The point at `t` (0 = the source end, 1 = the bus end) on a cubic Bézier.
pub fn bezier_at(p: &[Pos2; 4], t: f32) -> Pos2 {
    let u = 1.0 - t;
    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    pos2(
        a * p[0].x + b * p[1].x + c * p[2].x + d * p[3].x,
        a * p[0].y + b * p[1].y + c * p[2].y + d * p[3].y,
    )
}

/// How far `point` is from the wire, in points. Measured against a polyline of 32 pieces, which
/// is within a pixel of the curve at the sizes this UI draws.
pub fn distance_to_wire(p: &[Pos2; 4], point: Pos2) -> f32 {
    const PIECES: usize = 32;
    let mut best = f32::INFINITY;
    let mut prev = p[0];
    for k in 1..=PIECES {
        let next = bezier_at(p, k as f32 / PIECES as f32);
        best = best.min(distance_to_segment(prev, next, point));
        prev = next;
    }
    best
}

fn distance_to_segment(a: Pos2, b: Pos2, p: Pos2) -> f32 {
    let ab = b - a;
    let len2 = ab.length_sq();
    if len2 <= f32::EPSILON {
        return (p - a).length();
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    (p - (a + ab * t)).length()
}

/// Positions (0..1 along the wire) of `count` particles at time `time_s`. They all move toward
/// the bus at `speed` wire-lengths per second and are spread evenly, so the wire looks the same
/// at every moment but flows.
pub fn particle_positions(time_s: f64, count: usize, speed: f32) -> impl Iterator<Item = f32> {
    let base = (time_s * f64::from(speed)).rem_euclid(1.0) as f32;
    (0..count).map(move |k| (base + k as f32 / count.max(1) as f32).rem_euclid(1.0))
}
// !SECTION: Wires

// SECTION: Spring
/// A one-dimensional spring for a card's height. `step` moves it toward a target.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Spring {
    pub pos: f32,
    pub vel: f32,
}

/// Stiffness and damping ratio. 0.75 overshoots by about 3 %: enough to feel like a spring, not
/// enough to bounce.
const STIFFNESS: f32 = 220.0;
const DAMPING_RATIO: f32 = 0.75;
/// Settled when this close and this slow, in points and points per second.
const REST_DISTANCE: f32 = 0.4;
const REST_SPEED: f32 = 4.0;

impl Spring {
    pub fn at(pos: f32) -> Self {
        Self { pos, vel: 0.0 }
    }

    /// Jump to `target` with no motion (Eco profile, first frame, a card being held).
    pub fn snap(&mut self, target: f32) {
        *self = Self::at(target);
    }

    pub fn settled(&self, target: f32) -> bool {
        (self.pos - target).abs() < REST_DISTANCE && self.vel.abs() < REST_SPEED
    }

    /// Advance by `dt` seconds. Returns the new position.
    // ? Semi-implicit Euler in steps of at most 1/240 s: stable at any frame rate, because a
    // ? slow frame (dt = 0.1) is cut into small steps instead of being integrated in one go.
    pub fn step(&mut self, target: f32, dt: f32) -> f32 {
        let dt = if dt.is_finite() {
            dt.clamp(0.0, 0.1)
        } else {
            0.0
        };
        let damping = 2.0 * DAMPING_RATIO * STIFFNESS.sqrt();
        let steps = (dt / (1.0 / 240.0)).ceil().max(1.0) as usize;
        let h = dt / steps as f32;
        for _ in 0..steps {
            let accel = STIFFNESS * (target - self.pos) - damping * self.vel;
            self.vel += accel * h;
            self.pos += self.vel * h;
        }
        if self.settled(target) {
            self.snap(target);
        }
        self.pos
    }
}
// !SECTION: Spring

// SECTION: Card order
/// The strips to show, in card order: `card_order` first, the strip number to break ties.
pub fn card_order(shown: usize, orders: &[u8]) -> Vec<usize> {
    let mut v: Vec<usize> = (0..shown).collect();
    v.sort_by_key(|&i| (orders.get(i).copied().unwrap_or(0), i));
    v
}

/// Where each card's top edge is, in the order given, when card `i` is `height(i)` tall and
/// `gap` apart.
pub fn slot_tops(order: &[usize], height: &dyn Fn(usize) -> f32, gap: f32) -> Vec<(usize, f32)> {
    let mut y = 0.0;
    order
        .iter()
        .map(|&i| {
            let top = y;
            y += height(i) + gap;
            (i, top)
        })
        .collect()
}

/// The order while card `held` is being dragged and its centre is at `center_y`: the card goes
/// before the first other card whose centre is below it.
// ? The other cards' centres are taken from the layout *before* the drag (`order`), not from the
// ? layout without the held card. Otherwise the cards would shift under the held one as it
// ? moves, and a card dragged only a few points would already swap places.
pub fn order_while_dragging(
    order: &[usize],
    held: usize,
    center_y: f32,
    height: &dyn Fn(usize) -> f32,
    gap: f32,
) -> Vec<usize> {
    let mut out: Vec<usize> = order.iter().copied().filter(|&i| i != held).collect();
    let before = slot_tops(order, height, gap)
        .into_iter()
        .find(|&(i, top)| i != held && center_y < top + height(i) / 2.0)
        .map(|(i, _)| i);
    let at = before
        .and_then(|i| out.iter().position(|&o| o == i))
        .unwrap_or(out.len());
    out.insert(at, held);
    out
}

/// `card_order` values that make `order` stick: each strip's rank.
pub fn ranks(order: &[usize], total: usize) -> Vec<u8> {
    let mut r = vec![u8::MAX; total];
    for (rank, &i) in order.iter().enumerate() {
        if let Some(slot) = r.get_mut(i) {
            *slot = rank as u8;
        }
    }
    r
}
// !SECTION: Card order

// SECTION: Tilt
/// Largest tilt, in radians (about 2 degrees).
pub const MAX_TILT: f32 = 0.035;

/// The tilt of a held card for a pointer moving sideways at `velocity_x` points per second.
pub fn tilt(velocity_x: f32) -> f32 {
    if velocity_x.is_finite() {
        (velocity_x / 3000.0).clamp(-MAX_TILT, MAX_TILT)
    } else {
        0.0
    }
}

/// The corners of `corners` (top-left, top-right, bottom-right, bottom-left) turned by `angle`
/// radians around `center`.
pub fn rotate_around(center: Pos2, angle: f32, corners: [Pos2; 4]) -> [Pos2; 4] {
    let (s, c) = angle.sin_cos();
    corners.map(|p| {
        let d: Vec2 = p - center;
        center + vec2(d.x * c - d.y * s, d.x * s + d.y * c)
    })
}
// !SECTION: Tilt

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wire_starts_and_ends_on_its_sockets_and_leaves_sideways() {
        let w = wire(pos2(10.0, 20.0), pos2(300.0, 200.0));
        assert_eq!(bezier_at(&w, 0.0), pos2(10.0, 20.0));
        assert_eq!(bezier_at(&w, 1.0), pos2(300.0, 200.0));
        // The handles share the socket's height: the wire starts and ends horizontally.
        assert_eq!(w[1].y, w[0].y);
        assert_eq!(w[2].y, w[3].y);
        // Even a wire between two sockets above each other gets a visible bow.
        let close = wire(pos2(100.0, 0.0), pos2(100.0, 100.0));
        assert!(close[1].x > 100.0 && close[2].x < 100.0);
    }

    #[test]
    fn the_distance_to_a_wire_is_zero_on_it_and_grows_away_from_it() {
        let w = wire(pos2(0.0, 0.0), pos2(200.0, 0.0));
        assert!(distance_to_wire(&w, pos2(100.0, 0.0)) < 0.5);
        assert!((distance_to_wire(&w, pos2(100.0, 30.0)) - 30.0).abs() < 1.0);
        assert!(distance_to_wire(&w, pos2(-20.0, 0.0)) > 19.0);
        // The wire's own midpoint is on it, whatever the shape.
        let s = wire(pos2(10.0, 10.0), pos2(260.0, 190.0));
        assert!(distance_to_wire(&s, bezier_at(&s, 0.5)) < 0.5);
    }

    #[test]
    fn particles_stay_on_the_wire_and_flow_toward_the_bus() {
        let at = |t| particle_positions(t, 4, 0.5).collect::<Vec<_>>();
        for t in [0.0, 0.3, 1.7, 1234.5] {
            for p in at(t) {
                assert!((0.0..1.0).contains(&p), "{p} at {t}");
            }
        }
        assert_eq!(at(0.0), vec![0.0, 0.25, 0.5, 0.75]);
        // 0.2 s later at half a wire per second: every particle moved 0.1 along.
        let (a, b) = (at(0.0), at(0.2));
        for (x, y) in a.iter().zip(&b) {
            assert!(((y - x).rem_euclid(1.0) - 0.1).abs() < 1e-5);
        }
        assert_eq!(particle_positions(5.0, 0, 1.0).count(), 0);
    }

    #[test]
    fn a_spring_settles_on_its_target_without_blowing_up() {
        for dt in [1.0 / 240.0, 1.0 / 60.0, 1.0 / 15.0, 0.1, 5.0] {
            let mut s = Spring::at(0.0);
            let mut peak = 0.0_f32;
            let mut frames = 0;
            while !s.settled(100.0) {
                peak = peak.max(s.step(100.0, dt));
                frames += 1;
                assert!(frames < 5000, "never settled at dt {dt}");
            }
            assert!(peak < 110.0, "overshot to {peak} at dt {dt}");
            assert_eq!(s.pos, 100.0, "a settled spring sits exactly on the target");
        }
    }

    #[test]
    fn a_spring_overshoots_a_little_but_not_much() {
        let mut s = Spring::at(0.0);
        let mut peak = 0.0_f32;
        for _ in 0..600 {
            peak = peak.max(s.step(100.0, 1.0 / 60.0));
        }
        assert!(peak > 100.0, "an underdamped spring passes its target");
        assert!(peak < 106.0, "...by a few percent: {peak}");
    }

    #[test]
    fn a_spring_ignores_bad_time_steps() {
        let mut s = Spring::at(5.0);
        s.step(50.0, f32::NAN);
        assert_eq!(s.pos, 5.0);
        s.step(50.0, -1.0);
        assert_eq!(s.pos, 5.0);
        assert!(s.pos.is_finite());
    }

    #[test]
    fn snap_is_instant() {
        let mut s = Spring::at(0.0);
        s.vel = 500.0;
        s.snap(42.0);
        assert_eq!(s, Spring::at(42.0));
        assert!(s.settled(42.0));
    }

    #[test]
    fn cards_follow_card_order_then_strip_number() {
        assert_eq!(card_order(4, &[0, 0, 0, 0]), vec![0, 1, 2, 3]);
        assert_eq!(card_order(4, &[3, 2, 1, 0]), vec![3, 2, 1, 0]);
        assert_eq!(card_order(3, &[1, 0, 1]), vec![1, 0, 2]);
        assert_eq!(
            card_order(5, &[0, 0]),
            vec![0, 1, 2, 3, 4],
            "missing orders are 0"
        );
    }

    #[test]
    fn slots_stack_by_height() {
        let h = |i: usize| if i == 1 { 200.0 } else { 50.0 };
        let tops = slot_tops(&[0, 1, 2], &h, 10.0);
        assert_eq!(tops, vec![(0, 0.0), (1, 60.0), (2, 270.0)]);
    }

    #[test]
    fn dragging_a_card_past_its_neighbour_swaps_them() {
        let h = |_: usize| 50.0;
        let order = [0, 1, 2, 3];
        // Held card 0, still near the top: nothing changes.
        assert_eq!(
            order_while_dragging(&order, 0, 25.0, &h, 10.0),
            vec![0, 1, 2, 3]
        );
        // Its centre passed card 1's centre (85): it goes after card 1.
        assert_eq!(
            order_while_dragging(&order, 0, 90.0, &h, 10.0),
            vec![1, 0, 2, 3]
        );
        // Dragged below everything.
        assert_eq!(
            order_while_dragging(&order, 0, 500.0, &h, 10.0),
            vec![1, 2, 3, 0]
        );
        // Dragged above everything.
        assert_eq!(
            order_while_dragging(&order, 3, -40.0, &h, 10.0),
            vec![3, 0, 1, 2]
        );
    }

    #[test]
    fn ranks_turn_an_order_back_into_card_order_values() {
        assert_eq!(ranks(&[2, 0, 1], 4), vec![1, 2, 0, u8::MAX]);
        // Round trip: the order read back from the ranks is the order that made them.
        let r = ranks(&[1, 3, 0, 2], 4);
        assert_eq!(card_order(4, &r), vec![1, 3, 0, 2]);
    }

    #[test]
    fn tilt_follows_the_pointer_and_is_capped() {
        assert_eq!(tilt(0.0), 0.0);
        assert!(tilt(300.0) > 0.0 && tilt(-300.0) < 0.0);
        assert_eq!(tilt(1.0e6), MAX_TILT);
        assert_eq!(tilt(-1.0e6), -MAX_TILT);
        assert_eq!(tilt(f32::NAN), 0.0);
    }

    #[test]
    fn rotating_keeps_the_size_of_the_card() {
        let c = pos2(50.0, 25.0);
        let corners = [
            pos2(0.0, 0.0),
            pos2(100.0, 0.0),
            pos2(100.0, 50.0),
            pos2(0.0, 50.0),
        ];
        let r = rotate_around(c, MAX_TILT, corners);
        assert!(((r[1] - r[0]).length() - 100.0).abs() < 1e-3);
        assert!(((r[3] - r[0]).length() - 50.0).abs() < 1e-3);
        assert_eq!(rotate_around(c, 0.0, corners), corners);
    }
}
