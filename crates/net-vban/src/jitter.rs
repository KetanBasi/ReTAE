//! Jitter-buffer building blocks (PLAN.md §5.2, CLAUDE.md law 4).
//!
//! The jitter buffer *is* the strip's input ring: its fill level is the jitter depth, and the
//! drift controller (S2) steers that fill to the target. Who does what:
//!
//! | Event | Where it is handled |
//! |---|---|
//! | Reordering, duplicates, late packets | `reorder::ReorderWindow` |
//! | A lost packet | [`Concealer`]: ~3 ms fade-out of the last sample, silence after, then a short fade-in on the next real packet |
//! | Fill > 4 x target (consumer not draining) | [`PlayoutFifo`] refuses new audio and counts it |
//! | Underrun (fill ran dry) -> fade + rebuffer | `audio_engine::ClockDomainAdapter`: it owns the consumer side, so it is the only place that can see the ring run dry |
//! | Overrun (fill > 2 x target) -> crossfade-drop | the same adapter, for the same reason |
//! | Target depth | [`target_ms`], optionally raised/lowered by [`AdaptiveTarget`] |
// ? The producer (RX thread) can never remove audio it already pushed, so "drop the oldest"
// ? and "fade out when empty" can only live on the consumer side. Duplicating that here would
// ? mean two components fighting over the same ring.

use audio_types::{ENGINE_SAMPLE_RATE, NetworkBufferPreset};
use rtrb::Producer;

/// Fade-out used when a packet is lost: 3 ms at 48 kHz.
pub const CONCEAL_FADE_FRAMES: usize = 144;
/// Fade-in applied to the first real packet after a concealed gap: 2 ms.
pub const FADE_IN_FRAMES: usize = 96;
/// Frames per millisecond at the engine rate.
pub const FRAMES_PER_MS: f32 = ENGINE_SAMPLE_RATE as f32 / 1000.0;

/// Receiver-side jitter target in ms for a stream whose packets last `packet_ms`.
/// ? A thin wrapper so the RX side and the control plane (which sends
/// ? `ClockDomain::External { target_ms }` to the engine) always agree.
pub fn target_ms(preset: NetworkBufferPreset, packet_ms: f32) -> f32 {
    preset.effective_target_ms(packet_ms)
}

pub fn ms_to_frames(ms: f32) -> usize {
    (ms * FRAMES_PER_MS).round().max(0.0) as usize
}

// SECTION: Play-out FIFO
/// The producer half of a strip-input ring with a *logical* capacity of 4 x target.
// ? The physical ring is 4096 frames x 8 ch no matter what; the logical cap keeps a stuck
// ? consumer from letting seconds of audio pile up (and a later resume from playing them).
pub struct PlayoutFifo {
    producer: Producer<f32>,
    channels: usize,
    cap_frames: usize,
}

impl PlayoutFifo {
    pub fn new(producer: Producer<f32>, channels: usize) -> Self {
        let channels = channels.max(1);
        let cap_frames = producer.buffer().capacity() / channels;
        Self {
            producer,
            channels,
            cap_frames,
        }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Set the logical capacity to `4 x target_frames`, bounded by the physical ring.
    pub fn set_target_frames(&mut self, target_frames: usize) {
        let ring = self.producer.buffer().capacity() / self.channels;
        self.cap_frames = (target_frames.saturating_mul(4)).clamp(1, ring);
    }

    pub fn cap_frames(&self) -> usize {
        self.cap_frames
    }

    /// Frames waiting for the consumer.
    pub fn fill_frames(&self) -> usize {
        let cap = self.producer.buffer().capacity();
        (cap - self.producer.slots()) / self.channels
    }

    /// Push whole frames, or nothing: `false` if the logical capacity would be exceeded.
    pub fn push(&mut self, samples: &[f32]) -> bool {
        debug_assert_eq!(samples.len() % self.channels, 0);
        let frames = samples.len() / self.channels;
        if self.fill_frames() + frames > self.cap_frames {
            return false;
        }
        self.producer.push_entire_slice(samples).is_ok()
    }

    /// Give the ring back (stream removed).
    pub fn into_producer(self) -> Producer<f32> {
        self.producer
    }
}
// !SECTION: Play-out FIFO

// SECTION: Concealment
/// Packet-loss concealment for PCM: continue the waveform as a 3 ms fade-out, then silence.
/// The next real packet fades in so the join is click-free.
// ? This is deliberately simple. A pitch-repetition (WSOLA-style) concealer sounds better on
// ? sustained vowels, but at 1 % loss on a LAN it is a rare event and the fade is inaudible
// ? for most material. Opus streams use the codec's own PLC instead.
#[derive(Clone, Debug, Default)]
pub struct Concealer {
    last: [f32; 8],
    have_last: bool,
    fade_in_pending: bool,
}

impl Concealer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget everything (stream restarted, format changed).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// True while the next real packet will be faded in.
    pub fn fade_in_pending(&self) -> bool {
        self.fade_in_pending
    }

    /// Call with every real packet *before* it is queued: applies the pending fade-in in place.
    pub fn begin_packet(&mut self, samples: &mut [f32], channels: usize) {
        if !self.fade_in_pending {
            return;
        }
        self.fade_in_pending = false;
        for (f, frame) in samples
            .chunks_exact_mut(channels)
            .take(FADE_IN_FRAMES)
            .enumerate()
        {
            let g = (f + 1) as f32 / FADE_IN_FRAMES as f32;
            frame.iter_mut().for_each(|s| *s *= g);
        }
    }

    /// Call with every real packet *after* `begin_packet`: remembers the last frame.
    /// ? Uses the faded values only if the packet was shorter than the fade, which is the
    /// ? honest "what was last played".
    pub fn observe(&mut self, samples: &[f32], channels: usize) {
        if let Some(frame) = samples.chunks_exact(channels).next_back() {
            self.last = [0.0; 8];
            self.last[..channels.min(8)].copy_from_slice(&frame[..channels.min(8)]);
            self.have_last = true;
        }
    }

    /// Fill `out` (`frames * channels` samples) with the concealment for one lost packet.
    pub fn conceal(&mut self, out: &mut [f32], channels: usize) {
        let channels = channels.clamp(1, 8);
        // ? A lost packet shorter than 3 ms (the 128-frame default is 2.67 ms) finishes its
        // ? fade inside the gap, so the join with the next packet starts from silence rather
        // ? than from a level the fade never reached.
        let fade = CONCEAL_FADE_FRAMES.min(out.len() / channels).max(1);
        for (f, frame) in out.chunks_exact_mut(channels).enumerate() {
            let g = if self.have_last && f < fade {
                1.0 - (f + 1) as f32 / fade as f32
            } else {
                0.0
            };
            for (s, &l) in frame.iter_mut().zip(&self.last) {
                *s = l * g;
            }
        }
        // ? After a fade-out the signal is at zero; a second consecutive loss stays silent
        // ? rather than restarting the fade from the old value.
        self.last = [0.0; 8];
        self.fade_in_pending = true;
    }
}
// !SECTION: Concealment

// SECTION: Adaptive target
/// Optional "adaptive mode" for the jitter target (PLAN.md §5.2 step 4). Presets stay the
/// floor; the target rises when the stream underruns repeatedly and drifts back down after a
/// clean minute.
///
/// * More than one underrun inside 60 s: raise by 50 % (at least +2 ms), up to `max_ms`.
/// * 60 s without an underrun or a change: lower by 10 % (at least 1 ms), down to the floor.
///
/// Pure state machine: feed it the engine's cumulative underrun counter and the time.
#[derive(Clone, Debug)]
pub struct AdaptiveTarget {
    floor_ms: f32,
    max_ms: f32,
    current_ms: f32,
    seen_total: u32,
    /// Times (s) of the most recent underruns, oldest first.
    recent: [f64; 4],
    n_recent: usize,
    /// Last underrun or last change, whichever is later.
    last_event_s: f64,
}

pub const ADAPT_WINDOW_S: f64 = 60.0;

impl AdaptiveTarget {
    pub fn new(floor_ms: f32, max_ms: f32, now_s: f64) -> Self {
        let floor_ms = floor_ms.max(1.0);
        Self {
            floor_ms,
            max_ms: max_ms.max(floor_ms),
            current_ms: floor_ms,
            seen_total: 0,
            recent: [0.0; 4],
            n_recent: 0,
            last_event_s: now_s,
        }
    }

    pub fn current_ms(&self) -> f32 {
        self.current_ms
    }

    /// Move the floor (the user picked another preset, or the packet size changed).
    pub fn set_floor_ms(&mut self, floor_ms: f32) {
        self.floor_ms = floor_ms.max(1.0);
        self.max_ms = self.max_ms.max(self.floor_ms);
        self.current_ms = self.current_ms.clamp(self.floor_ms, self.max_ms);
    }

    /// Returns `Some(new_target_ms)` when the target changed.
    pub fn update(&mut self, now_s: f64, total_underruns: u32) -> Option<f32> {
        let new = total_underruns.wrapping_sub(self.seen_total);
        self.seen_total = total_underruns;
        // ? A counter that went backwards (engine reset) wraps to a huge number; treat it
        // ? as "nothing new" rather than a storm.
        let new = if new > 1_000_000 { 0 } else { new };
        for _ in 0..new.min(self.recent.len() as u32) {
            if self.n_recent == self.recent.len() {
                self.recent.copy_within(1.., 0);
                self.n_recent -= 1;
            }
            self.recent[self.n_recent] = now_s;
            self.n_recent += 1;
        }
        if new > 0 {
            self.last_event_s = now_s;
        }
        // Forget underruns older than the window.
        let keep_from = self.recent[..self.n_recent]
            .iter()
            .position(|&t| now_s - t <= ADAPT_WINDOW_S)
            .unwrap_or(self.n_recent);
        self.recent.copy_within(keep_from..self.n_recent, 0);
        self.n_recent -= keep_from;

        let before = self.current_ms;
        if self.n_recent > 1 {
            self.current_ms = (self.current_ms * 1.5)
                .max(self.current_ms + 2.0)
                .min(self.max_ms);
            self.n_recent = 0;
            self.last_event_s = now_s;
        } else if now_s - self.last_event_s >= ADAPT_WINDOW_S && self.current_ms > self.floor_ms {
            self.current_ms = (self.current_ms * 0.9)
                .min(self.current_ms - 1.0)
                .max(self.floor_ms);
            self.last_event_s = now_s;
        }
        (self.current_ms != before).then_some(self.current_ms)
    }
}
// !SECTION: Adaptive target

#[cfg(test)]
mod tests {
    use super::*;
    use rtrb::RingBuffer;

    // SECTION: PlayoutFifo
    #[test]
    fn fifo_counts_frames_and_enforces_the_logical_cap() {
        let (p, mut c) = RingBuffer::<f32>::new(4096 * 2);
        let mut f = PlayoutFifo::new(p, 2);
        assert_eq!(f.fill_frames(), 0);
        f.set_target_frames(100); // cap = 400 frames
        assert_eq!(f.cap_frames(), 400);
        let block = vec![0.5f32; 2 * 128];
        assert!(f.push(&block) && f.push(&block) && f.push(&block));
        assert_eq!(f.fill_frames(), 384);
        assert!(!f.push(&block), "384 + 128 > 400: refuse the whole packet");
        assert_eq!(
            f.fill_frames(),
            384,
            "a refused packet leaves no partial write"
        );
        // Draining makes room again.
        for _ in 0..2 * 128 {
            c.pop().unwrap();
        }
        assert_eq!(f.fill_frames(), 256);
        assert!(f.push(&block));
    }

    #[test]
    fn fifo_cap_never_exceeds_the_physical_ring() {
        let (p, _c) = RingBuffer::<f32>::new(1000);
        let mut f = PlayoutFifo::new(p, 2);
        f.set_target_frames(10_000);
        assert_eq!(f.cap_frames(), 500);
        f.set_target_frames(0);
        assert_eq!(f.cap_frames(), 1);
    }
    // !SECTION: PlayoutFifo

    // SECTION: Concealer
    fn sine(n: usize, ch: usize, start: usize) -> Vec<f32> {
        (0..n * ch)
            .map(|i| {
                let f = start + i / ch;
                0.6 * ((f as f64) * 2.0 * std::f64::consts::PI * 200.0 / 48_000.0).sin() as f32
            })
            .collect()
    }

    #[test]
    fn concealment_continues_from_the_last_sample_and_fades_to_zero() {
        let mut c = Concealer::new();
        let pkt = sine(128, 2, 0);
        c.observe(&pkt, 2);
        let last = [pkt[254], pkt[255]];
        let mut out = vec![9.0f32; 2 * 256];
        c.conceal(&mut out, 2);
        // First sample is within one fade step of the last real one: no step.
        assert!((out[0] - last[0]).abs() <= last[0].abs() / CONCEAL_FADE_FRAMES as f32 + 1e-6);
        assert!((out[1] - last[1]).abs() <= last[1].abs() / CONCEAL_FADE_FRAMES as f32 + 1e-6);
        // Monotone magnitude decay to exactly zero at 3 ms, silence after.
        for f in 1..CONCEAL_FADE_FRAMES {
            assert!(out[2 * f].abs() <= out[2 * (f - 1)].abs() + 1e-9);
        }
        assert_eq!(out[2 * (CONCEAL_FADE_FRAMES - 1)], 0.0);
        assert!(out[2 * CONCEAL_FADE_FRAMES..].iter().all(|&s| s == 0.0));
        assert!(c.fade_in_pending());
    }

    #[test]
    fn a_second_consecutive_loss_stays_silent_and_the_next_packet_fades_in() {
        let mut c = Concealer::new();
        c.observe(&sine(128, 1, 0), 1);
        let mut a = vec![0.0f32; 128];
        c.conceal(&mut a, 1);
        let mut b = vec![1.0f32; 128];
        c.conceal(&mut b, 1);
        assert!(b.iter().all(|&s| s == 0.0));

        let mut next = vec![1.0f32; 256];
        c.begin_packet(&mut next, 1);
        assert!(!c.fade_in_pending());
        assert!(
            (next[0] - 1.0 / FADE_IN_FRAMES as f32).abs() < 1e-6,
            "starts near zero"
        );
        assert!(
            next.windows(2)
                .take(FADE_IN_FRAMES - 1)
                .all(|w| w[1] > w[0]),
            "monotone ramp"
        );
        assert_eq!(
            next[FADE_IN_FRAMES - 1],
            1.0,
            "full level at the end of the fade"
        );
        assert_eq!(next[FADE_IN_FRAMES], 1.0, "untouched afterwards");
        // Packets that follow a clean packet are never touched.
        let mut clean = vec![1.0f32; 128];
        c.begin_packet(&mut clean, 1);
        assert!(clean.iter().all(|&s| s == 1.0));
    }

    #[test]
    fn a_short_lost_packet_still_ends_at_silence() {
        // 128 frames (2.67 ms) is shorter than the 3 ms fade: the fade is compressed to fit.
        let mut c = Concealer::new();
        c.observe(&[0.8, 0.8], 2);
        let mut out = vec![0.0f32; 2 * 128];
        c.conceal(&mut out, 2);
        assert!(out[0] > 0.78, "starts at the last level");
        assert_eq!(
            out[2 * 127],
            0.0,
            "and reaches zero by the end of the packet"
        );
    }

    #[test]
    fn concealing_before_any_audio_is_silence() {
        let mut c = Concealer::new();
        let mut out = vec![5.0f32; 64];
        c.conceal(&mut out, 2);
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn loss_in_a_sine_has_no_discontinuity_at_either_edge() {
        // Real packet, lost packet (concealed), real packet: the largest sample-to-sample
        // step across the whole 3-packet stream must stay near the sine's own maximum slope.
        let ch = 1;
        let mut c = Concealer::new();
        let a = sine(128, ch, 0);
        c.observe(&a, ch);
        let mut gap = vec![0.0f32; 128];
        c.conceal(&mut gap, ch);
        let mut b = sine(128, ch, 256);
        c.begin_packet(&mut b, ch);
        let all: Vec<f32> = a.iter().chain(&gap).chain(&b).copied().collect();
        let max_step = all
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        // A 200 Hz sine at 0.6 has a max slope of 0.6*2*pi*200/48000 = 0.0157 per sample.
        assert!(max_step < 0.0157 * 1.5, "click of {max_step}");
        // The fade-in is doing real work: joining the raw packet to the silent gap would
        // be a step of 0.24, i.e. 15x the sine's own maximum slope.
        let raw = sine(128, ch, 256);
        assert!((raw[0] - gap[127]).abs() > 0.2);
    }
    // !SECTION: Concealer

    // SECTION: AdaptiveTarget
    #[test]
    fn target_helper_enforces_two_packets() {
        // Optimal (6 ms) with 256-frame packets (5.33 ms) must become 10.67 ms.
        let t = target_ms(NetworkBufferPreset::Optimal, 256.0 / 48.0);
        assert!((t - 10.666).abs() < 0.01);
        assert_eq!(target_ms(NetworkBufferPreset::Fast, 128.0 / 48.0), 12.0);
        assert_eq!(ms_to_frames(12.0), 576);
    }

    #[test]
    fn two_underruns_inside_a_minute_raise_the_target() {
        let mut a = AdaptiveTarget::new(12.0, 96.0, 0.0);
        assert_eq!(a.update(1.0, 0), None);
        assert_eq!(a.update(10.0, 1), None, "one underrun alone is tolerated");
        assert_eq!(
            a.update(30.0, 2),
            Some(18.0),
            "a second within 60 s raises by 50 %"
        );
        // The window was consumed; one more is again just one.
        assert_eq!(a.update(31.0, 3), None);
        assert_eq!(a.update(32.0, 4), Some(27.0));
        assert_eq!(a.current_ms(), 27.0);
    }

    #[test]
    fn underruns_a_minute_apart_do_not_accumulate() {
        let mut a = AdaptiveTarget::new(12.0, 96.0, 0.0);
        assert_eq!(a.update(5.0, 1), None);
        assert_eq!(
            a.update(70.0, 2),
            None,
            "the first one aged out of the window"
        );
        assert_eq!(a.current_ms(), 12.0);
    }

    #[test]
    fn a_burst_of_underruns_counts_individually() {
        let mut a = AdaptiveTarget::new(12.0, 96.0, 0.0);
        assert_eq!(
            a.update(1.0, 3),
            Some(18.0),
            "three at once is more than one per minute"
        );
    }

    #[test]
    fn clean_minutes_lower_the_target_to_the_floor_and_no_further() {
        let mut a = AdaptiveTarget::new(12.0, 96.0, 0.0);
        a.update(1.0, 2);
        assert_eq!(a.current_ms(), 18.0);
        let mut t = 1.0;
        let mut seen = vec![];
        for _ in 0..10 {
            t += 60.0;
            if let Some(v) = a.update(t, 2) {
                seen.push(v);
            }
        }
        assert!(seen.windows(2).all(|w| w[1] < w[0]), "monotone: {seen:?}");
        assert_eq!(a.current_ms(), 12.0);
        assert!(seen.len() >= 3, "slow descent, not one jump: {seen:?}");
        assert_eq!(a.update(t + 600.0, 2), None);
    }

    #[test]
    fn target_is_capped_and_floor_changes_clamp_it() {
        let mut a = AdaptiveTarget::new(24.0, 40.0, 0.0);
        let mut n = 0;
        for i in 0..20 {
            n += 2;
            a.update(i as f64, n);
        }
        assert_eq!(a.current_ms(), 40.0);
        a.set_floor_ms(60.0);
        assert_eq!(
            a.current_ms(),
            60.0,
            "a higher floor lifts the target (max follows)"
        );
        a.set_floor_ms(6.0);
        assert!(a.current_ms() >= 6.0);
    }

    #[test]
    fn a_counter_reset_is_not_a_storm() {
        let mut a = AdaptiveTarget::new(12.0, 96.0, 0.0);
        a.update(1.0, 500);
        let before = a.current_ms();
        // Engine restarted: cumulative counter back to 0 (wrapping_sub gives ~4 billion).
        assert_eq!(a.update(2.0, 0), None);
        assert_eq!(a.current_ms(), before);
    }
    // !SECTION: AdaptiveTarget
}
