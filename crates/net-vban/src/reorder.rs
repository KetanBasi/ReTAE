//! The 4-slot reorder window (PLAN.md §5.2 step 1, CLAUDE.md law 4).
//!
//! Packets arrive out of order, duplicated, late or not at all. This type puts them back in
//! `frame_counter` order and reports what happened to the *timeline*:
//!
//! * [`Emit::Packet`]: the next packet in order.
//! * [`Emit::Lost`]: the next packet never came; the caller must fill its duration (the
//!   jitter module does it with a fade-out, see `jitter::Concealer`).
//!
//! It is pure sequencing logic: no sockets, no clock, no ring. *When* to give up on a missing
//! packet is the caller's decision (`give_up_head`), because it depends on how much audio is
//! still queued for playback, which only the caller can see.
// ? Why 4 slots indexed by `counter % 4`: a packet can only be reordered past a few
// ? neighbours on a LAN. Four slots cover a reordering depth of 3 packets (8 ms at 128 frames)
// ? with zero allocation, and the modulo makes "which slot?" a single AND.

/// Reorder depth in packets.
pub const WINDOW_SLOTS: usize = 4;
/// A packet this far ahead of / behind the expected counter is a restarted or resumed stream,
/// not a gap to conceal. 32 packets = 85 ms at 128 frames.
pub const RESYNC_DISTANCE: u32 = 32;
/// Samples one slot can hold: 256 frames x 8 channels.
pub const SLOT_SAMPLES: usize = 256 * 8;

/// What the window tells the caller to do with the timeline, in play-out order.
#[derive(Debug, PartialEq)]
pub enum Emit<'a> {
    /// The next packet. `samples` is mutable so the caller can apply a fade-in in place.
    Packet {
        counter: u32,
        frames: usize,
        samples: &'a mut [f32],
    },
    /// Packet `counter` is lost. `frames` is the duration to conceal (the size of the
    /// neighbour that follows it, else of the most recent packet).
    Lost { counter: u32, frames: usize },
}

/// Monotonic event counts. The RX thread mirrors these into atomics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReorderStats {
    /// Packets accepted into the window (excludes duplicates and late arrivals).
    pub received: u32,
    /// Counters declared missing (gap in `frame_counter`). Includes the jump on a resync.
    pub lost: u32,
    /// Arrived after being declared lost.
    pub late: u32,
    /// Arrived twice (packet duplication on TX, or a network duplicate).
    pub duplicate: u32,
    /// Counter jumped by >= `RESYNC_DISTANCE` (or backwards): treated as a new run.
    pub resyncs: u32,
}

struct Slot {
    counter: u32,
    frames: usize,
    /// Valid samples in `data` (`frames * channels`).
    len: usize,
    present: bool,
    data: Vec<f32>,
}

pub struct ReorderWindow {
    slots: [Slot; WINDOW_SLOTS],
    /// Next counter to play out.
    expected: u32,
    started: bool,
    /// Bit `k` set <=> counter `expected - 1 - k` was delivered (not concealed). Separates
    /// duplicates from late packets for the 64 most recent counters.
    delivered: u64,
    /// Duration of the most recent packet, used to size a concealed gap.
    last_frames: usize,
    stats: ReorderStats,
}

impl Default for ReorderWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl ReorderWindow {
    /// Allocates the four slot buffers. Call on the control thread, not while streaming.
    pub fn new() -> Self {
        Self {
            slots: core::array::from_fn(|_| Slot {
                counter: 0,
                frames: 0,
                len: 0,
                present: false,
                data: vec![0.0; SLOT_SAMPLES],
            }),
            expected: 0,
            started: false,
            delivered: 0,
            last_frames: 128,
            stats: ReorderStats::default(),
        }
    }

    pub fn stats(&self) -> ReorderStats {
        self.stats
    }

    pub fn expected(&self) -> u32 {
        self.expected
    }

    /// Forget the current run (format change, stream removed). Counters are kept.
    pub fn clear(&mut self) {
        for s in &mut self.slots {
            s.present = false;
        }
        self.started = false;
        self.delivered = 0;
    }

    /// True when the packet at the head of the line is missing but a later one has arrived,
    /// i.e. playback is stalled behind a hole.
    pub fn is_blocked(&self) -> bool {
        self.started && !self.head_present() && self.slots.iter().any(|s| s.present)
    }

    /// Number of packets currently held back.
    pub fn held(&self) -> usize {
        self.slots.iter().filter(|s| s.present).count()
    }

    fn head_present(&self) -> bool {
        let s = &self.slots[self.expected as usize % WINDOW_SLOTS];
        s.present && s.counter == self.expected
    }

    /// Insert one packet and emit everything that is now playable, in order.
    /// `samples.len()` must be a multiple of `frames` and at most `SLOT_SAMPLES`.
    pub fn push(
        &mut self,
        counter: u32,
        frames: usize,
        samples: &[f32],
        sink: &mut impl FnMut(Emit<'_>),
    ) {
        if frames == 0 || samples.is_empty() || samples.len() > SLOT_SAMPLES {
            return;
        }
        if !self.started {
            self.expected = counter;
            self.started = true;
            self.delivered = 0;
        }

        let ahead = counter.wrapping_sub(self.expected) as i32;
        if ahead < 0 {
            // * Behind the play-out point.
            let back = ahead.unsigned_abs();
            if back > RESYNC_DISTANCE {
                self.resync(counter);
            } else {
                if (self.delivered >> (back - 1)) & 1 == 1 {
                    self.stats.duplicate += 1;
                } else {
                    self.stats.late += 1;
                }
                return;
            }
        } else if ahead as u32 >= RESYNC_DISTANCE {
            // ? A long gap is an outage, not a glitch: concealing it would pile seconds of
            // ? silence in front of the audio that resumes. Count it and jump.
            self.stats.lost = self.stats.lost.saturating_add(ahead as u32);
            self.resync(counter);
        }

        // * Ahead by 4 or more: the window can't hold it, so the head is overdue.
        while counter.wrapping_sub(self.expected) as usize >= WINDOW_SLOTS {
            self.give_up_head(sink);
        }

        let slot = &mut self.slots[counter as usize % WINDOW_SLOTS];
        if slot.present {
            // ? Same index and within [expected, expected+3] => the very same counter.
            debug_assert_eq!(slot.counter, counter);
            self.stats.duplicate += 1;
            return;
        }
        slot.counter = counter;
        slot.frames = frames;
        slot.len = samples.len();
        slot.present = true;
        slot.data[..samples.len()].copy_from_slice(samples);
        self.stats.received += 1;
        self.drain(sink);
    }

    /// Declare the head-of-line packet lost and emit everything that unblocks.
    /// No-op unless [`is_blocked`](Self::is_blocked) (or the window must advance to fit a
    /// far-ahead packet, which `push` handles).
    pub fn give_up_head(&mut self, sink: &mut impl FnMut(Emit<'_>)) {
        if !self.started {
            return;
        }
        if self.head_present() {
            self.drain(sink);
            return;
        }
        // ? Conceal with the size of the nearest packet that follows (best guess for a hole),
        // ? falling back to the last emitted size.
        let frames = self
            .slots
            .iter()
            .filter(|s| s.present)
            .min_by_key(|s| s.counter.wrapping_sub(self.expected))
            .map_or(self.last_frames, |s| s.frames);
        self.stats.lost += 1;
        sink(Emit::Lost {
            counter: self.expected,
            frames,
        });
        self.advance(false);
        self.drain(sink);
    }

    fn drain(&mut self, sink: &mut impl FnMut(Emit<'_>)) {
        while self.head_present() {
            let slot = &mut self.slots[self.expected as usize % WINDOW_SLOTS];
            sink(Emit::Packet {
                counter: slot.counter,
                frames: slot.frames,
                samples: &mut slot.data[..slot.len],
            });
            slot.present = false;
            self.last_frames = slot.frames;
            self.advance(true);
        }
    }

    fn advance(&mut self, delivered: bool) {
        self.delivered = (self.delivered << 1) | delivered as u64;
        self.expected = self.expected.wrapping_add(1);
    }

    fn resync(&mut self, counter: u32) {
        self.stats.resyncs += 1;
        for s in &mut self.slots {
            s.present = false;
        }
        self.expected = counter;
        self.delivered = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the sink saw, flattened for easy assertions.
    #[derive(Debug, PartialEq, Clone)]
    enum Ev {
        P(u32, f32), // counter, first sample (we write the counter into the payload)
        L(u32, usize),
    }

    struct Rig {
        w: ReorderWindow,
        log: Vec<Ev>,
    }

    impl Rig {
        fn new() -> Self {
            Self {
                w: ReorderWindow::new(),
                log: vec![],
            }
        }
        fn push(&mut self, counter: u32) {
            self.push_frames(counter, 4);
        }
        fn push_frames(&mut self, counter: u32, frames: usize) {
            // The payload carries its own counter so a mis-routed slot is visible.
            let data = vec![counter as f32; frames * 2];
            let log = &mut self.log;
            self.w.push(counter, frames, &data, &mut |e| match e {
                Emit::Packet {
                    counter, samples, ..
                } => log.push(Ev::P(counter, samples[0])),
                Emit::Lost { counter, frames } => log.push(Ev::L(counter, frames)),
            });
        }
        fn give_up(&mut self) {
            let log = &mut self.log;
            self.w.give_up_head(&mut |e| match e {
                Emit::Packet {
                    counter, samples, ..
                } => log.push(Ev::P(counter, samples[0])),
                Emit::Lost { counter, frames } => log.push(Ev::L(counter, frames)),
            });
        }
        fn take(&mut self) -> Vec<Ev> {
            std::mem::take(&mut self.log)
        }
    }

    fn p(c: u32) -> Ev {
        Ev::P(c, c as f32)
    }

    #[test]
    fn in_order_packets_pass_straight_through() {
        let mut r = Rig::new();
        for c in 100..110 {
            r.push(c);
        }
        assert_eq!(r.take(), (100..110).map(p).collect::<Vec<_>>());
        assert_eq!(
            r.w.stats(),
            ReorderStats {
                received: 10,
                ..Default::default()
            }
        );
        assert!(!r.w.is_blocked());
        assert_eq!(r.w.held(), 0);
    }

    #[test]
    fn a_swapped_pair_is_put_back_in_order() {
        let mut r = Rig::new();
        r.push(0);
        r.push(2);
        assert_eq!(r.take(), vec![p(0)], "2 must wait behind the hole at 1");
        assert!(r.w.is_blocked());
        assert_eq!(r.w.held(), 1);
        r.push(1);
        assert_eq!(r.take(), vec![p(1), p(2)]);
        assert!(!r.w.is_blocked());
        assert_eq!(r.w.stats().lost, 0);
    }

    #[test]
    fn reordering_three_deep_is_absorbed() {
        let mut r = Rig::new();
        r.push(0);
        r.take();
        for c in [4, 3, 2] {
            r.push(c);
        }
        assert!(r.take().is_empty(), "everything is stuck behind packet 1");
        r.push(1);
        assert_eq!(r.take(), (1..=4).map(p).collect::<Vec<_>>());
        assert_eq!(r.w.stats().lost, 0);
    }

    #[test]
    fn give_up_conceals_the_hole_then_releases_what_waited() {
        let mut r = Rig::new();
        r.push(0);
        r.push(2);
        r.push(3);
        r.take();
        r.give_up();
        assert_eq!(r.take(), vec![Ev::L(1, 4), p(2), p(3)]);
        assert_eq!(r.w.stats().lost, 1);
        // The missing packet turns up after all: late, not duplicate, and not replayed.
        r.push(1);
        assert!(r.take().is_empty());
        let s = r.w.stats();
        assert_eq!((s.late, s.duplicate), (1, 0));
    }

    #[test]
    fn window_overflow_forces_the_hole_out_without_being_asked() {
        let mut r = Rig::new();
        r.push(0);
        r.push(2);
        r.push(3);
        r.push(4);
        // 1 is missing, 2..4 held. Packet 5 does not fit in 4 slots anywhere past 1.
        assert_eq!(r.take(), vec![p(0)]);
        r.push(5);
        assert_eq!(r.take(), vec![Ev::L(1, 4), p(2), p(3), p(4), p(5)]);
        assert_eq!(r.w.stats().lost, 1);
    }

    #[test]
    fn a_short_burst_loss_conceals_each_missing_packet() {
        let mut r = Rig::new();
        r.push(0);
        r.take();
        r.push(7); // 1..6 missing
        let ev = r.take();
        assert_eq!(
            ev,
            vec![Ev::L(1, 4), Ev::L(2, 4), Ev::L(3, 4)],
            "the window advances only until packet 7 fits (expected = 4); 4..6 stay holes"
        );
        assert!(r.w.is_blocked());
        for _ in 0..3 {
            r.give_up();
        }
        assert_eq!(r.take(), vec![Ev::L(4, 4), Ev::L(5, 4), Ev::L(6, 4), p(7)]);
        assert_eq!(r.w.stats().lost, 6);
    }

    #[test]
    fn duplicates_are_dropped_and_counted_whether_held_or_already_played() {
        let mut r = Rig::new();
        r.push(0);
        r.push(0); // already played
        r.push(2);
        r.push(2); // already held
        r.push(1);
        assert_eq!(r.take(), vec![p(0), p(1), p(2)]);
        r.push(1); // played two packets ago
        assert!(r.take().is_empty());
        let s = r.w.stats();
        assert_eq!((s.duplicate, s.late, s.lost, s.received), (3, 0, 0, 3));
    }

    #[test]
    fn tx_duplication_recovers_a_lost_original() {
        // Packet N is lost, but TX sent N again right after N+1 (one packet apart).
        let mut r = Rig::new();
        r.push(0);
        r.push(2); // 1 lost in transit
        r.push(1); // its duplicate arrives one packet later
        assert_eq!(r.take(), vec![p(0), p(1), p(2)]);
        assert_eq!(r.w.stats().lost, 0);
    }

    #[test]
    fn counter_wraparound_is_seamless() {
        let mut r = Rig::new();
        let start = u32::MAX - 2;
        for i in 0..8u32 {
            r.push(start.wrapping_add(i));
        }
        let want: Vec<_> = (0..8u32).map(|i| p(start.wrapping_add(i))).collect();
        assert_eq!(r.take(), want);
        // And a swap across the wrap point.
        let mut r = Rig::new();
        r.push(u32::MAX);
        r.push(1);
        r.push(0);
        assert_eq!(r.take(), vec![p(u32::MAX), p(0), p(1)]);
        assert_eq!(r.w.stats().lost, 0);
    }

    #[test]
    fn an_outage_resyncs_instead_of_concealing_it() {
        let mut r = Rig::new();
        r.push(0);
        r.take();
        r.push(500); // 499 packets vanished
        assert_eq!(r.take(), vec![p(500)], "no concealment for an outage");
        let s = r.w.stats();
        assert_eq!((s.resyncs, s.lost), (1, 500 - 1));
        assert!(!r.w.is_blocked());
    }

    #[test]
    fn a_restarted_sender_resyncs_without_counting_loss() {
        let mut r = Rig::new();
        for c in 5_000_000..5_000_004 {
            r.push(c);
        }
        r.take();
        r.push(0); // sender restarted: counter back to 0
        r.push(1);
        assert_eq!(r.take(), vec![p(0), p(1)]);
        let s = r.w.stats();
        assert_eq!((s.resyncs, s.lost, s.late), (1, 0, 0));
    }

    #[test]
    fn a_resync_drops_stale_held_packets() {
        let mut r = Rig::new();
        r.push(0);
        r.push(2); // held
        r.take();
        r.push(900);
        assert_eq!(r.take(), vec![p(900)]);
        assert_eq!(r.w.held(), 0, "packet 2 belonged to the previous run");
    }

    #[test]
    fn packet_sizes_may_change_and_a_hole_takes_its_neighbours_size() {
        let mut r = Rig::new();
        r.push_frames(0, 4);
        r.push_frames(2, 6);
        r.take();
        r.give_up();
        assert_eq!(r.take(), vec![Ev::L(1, 6), p(2)]);
    }

    #[test]
    fn nothing_is_emitted_for_garbage_pushes() {
        let mut r = Rig::new();
        r.w.push(0, 0, &[1.0], &mut |_| panic!("zero frames"));
        r.w.push(0, 1, &[], &mut |_| panic!("no samples"));
        r.w.push(0, 1, &vec![0.0; SLOT_SAMPLES + 1], &mut |_| {
            panic!("too big")
        });
        r.give_up(); // not started: no-op
        assert!(r.take().is_empty());
    }

    #[test]
    fn clear_forgets_the_run_but_keeps_counters() {
        let mut r = Rig::new();
        r.push(0);
        r.push(2);
        r.w.clear();
        assert_eq!(r.w.held(), 0);
        r.push(77);
        assert_eq!(r.take(), vec![p(0), p(77)]);
        assert_eq!(r.w.stats().received, 3);
        assert_eq!(
            r.w.stats().resyncs,
            0,
            "clear() restarts without counting a resync"
        );
    }

    #[test]
    fn random_impairment_never_reorders_or_repeats_the_output() {
        // Property: whatever the network does, the emitted counters are strictly
        // increasing and every counter is either played or reported lost, exactly once.
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut rnd = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for trial in 0..50 {
            let n = 400u32;
            // Build a delivery order: each packet slides up to 3 places, 3 % are dropped,
            // 2 % are sent twice.
            let mut order: Vec<(u64, u32)> = vec![];
            for c in 0..n {
                if rnd() % 100 < 3 {
                    continue;
                }
                let t = c as u64 * 10 + rnd() % 30;
                order.push((t, c));
                if rnd() % 100 < 2 {
                    order.push((t + 10 + rnd() % 10, c));
                }
            }
            order.sort();
            let mut r = Rig::new();
            for (i, &(_, c)) in order.iter().enumerate() {
                r.push(c);
                // The caller gives up on a hole every so often (as the fill-based rule would).
                if i % 7 == 6 && r.w.is_blocked() {
                    r.give_up();
                }
            }
            while r.w.is_blocked() {
                r.give_up();
            }
            let mut last: Option<u32> = None;
            let mut seen = std::collections::BTreeSet::new();
            for e in &r.log {
                let c = match *e {
                    Ev::P(c, v) => {
                        assert_eq!(v, c as f32, "payload landed in the wrong slot");
                        c
                    }
                    Ev::L(c, _) => c,
                };
                assert!(
                    last.is_none_or(|l| c == l + 1),
                    "trial {trial}: {last:?} then {c}"
                );
                assert!(seen.insert(c), "counter {c} emitted twice");
                last = Some(c);
            }
        }
    }
}
