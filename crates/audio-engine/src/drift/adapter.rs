//! `ClockDomainAdapter` = FIFO + resampler + controller (PLAN.md §5.1, S2).
//!
//! The adapter *borrows* the ring's `Consumer` on each `pull`, so it works wherever the
//! ring is consumed: inside the mixer for a strip input, or on a device thread for a bus
//! output (the ring then holds mixer-rate audio and the device callback pulls from it).
//!
//! # States
//! ```text
//!  Priming ──(fill ≥ target)──> Running ──(not enough input)──> Rebuffering
//!                                  ^                                 │
//!                                  └────────(fill ≥ target)──────────┘
//! ```
//! * `Priming` / `Rebuffering`: output silence and leave the ring alone so it can fill.
//! * Underrun (the resampler needs more input than the ring holds): the block is a short
//!   fade-out of the last output value, the underrun is counted once, and we rebuffer.
//! * Overrun (fill above 2× target): the excess is dropped with a crossfade (never by
//!   resampling hard to catch up).
//! * Restarting always fades in over 3 ms, so no state change can click.

use super::{DriftController, DriftResampler, HermiteResampler, WORK_FRAMES, Work};
use crate::dsp::Planar;
use crate::dsp::util::SAMPLE_RATE;
use crate::ports::deinterleave;
use audio_types::{ENGINE_BLOCK_FRAMES, MAX_CHANNELS};
use rtrb::Consumer;

/// Fade length after (re)starting: 3 ms.
const FADE_IN_FRAMES: usize = 144;
/// Length of the fade-out on an underrun. Shorter than one block so it always completes.
const FADE_OUT_FRAMES: usize = 96;
/// Smallest FIFO target, in frames. Below about one resampler pull the ring would underrun
/// on every block.
const MIN_TARGET_FRAMES: usize = 192;
/// Accepted range for the configured target.
const MIN_TARGET_MS: f32 = 4.0;
const MAX_TARGET_MS: f32 = 500.0;

type Staging = [f32; MAX_CHANNELS * WORK_FRAMES];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Priming,
    Running,
    Rebuffering,
}

/// What happened during one `pull`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AdapterReport {
    /// The ring ran dry: this block is a fade-out and the adapter is now rebuffering.
    pub underrun: bool,
    /// The ring was too full: the excess was dropped with a crossfade.
    pub overrun: bool,
    /// The block contains real audio (false while priming / rebuffering).
    pub running: bool,
}

pub struct ClockDomainAdapter<R: DriftResampler = HermiteResampler> {
    resampler: R,
    controller: DriftController,
    channels: usize,
    target_ms: f32,
    state: State,
    fade_in_left: usize,
    /// Final output frame of the previous block, per channel (start of an underrun fade-out).
    last_out: [f32; MAX_CHANNELS],
    underruns: u32,
    overruns: u32,
    work: Work,
    /// Second block, used only while crossfading after an overrun.
    alt: Planar,
    staging: Staging,
}

impl Default for ClockDomainAdapter<HermiteResampler> {
    fn default() -> Self {
        Self::new(HermiteResampler::new())
    }
}

impl<R: DriftResampler> ClockDomainAdapter<R> {
    // ? ~21 KB of buffers. The engine keeps one per strip inside a `Box`, allocated up front on
    // ? the control thread, so nothing here ever allocates once audio is running.
    pub fn new(resampler: R) -> Self {
        let mut a = Self {
            resampler,
            controller: DriftController::new(12.0),
            channels: 2,
            target_ms: 12.0,
            state: State::Priming,
            fade_in_left: 0,
            last_out: [0.0; MAX_CHANNELS],
            underruns: 0,
            overruns: 0,
            work: [[0.0; WORK_FRAMES]; MAX_CHANNELS],
            alt: [[0.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS],
            staging: [0.0; MAX_CHANNELS * WORK_FRAMES],
        };
        a.configure(2, 12.0);
        a
    }

    /// (Re)start for `channels` channels and a FIFO target of `target_ms`. Forgets the learned
    /// clock offset and the counters; call it whenever the source changes.
    pub fn configure(&mut self, channels: usize, target_ms: f32) {
        self.channels = channels.clamp(1, MAX_CHANNELS);
        self.target_ms = if target_ms.is_finite() {
            target_ms.clamp(MIN_TARGET_MS, MAX_TARGET_MS)
        } else {
            12.0
        };
        self.controller = DriftController::new(self.target_ms);
        self.resampler.reset(self.channels);
        self.state = State::Priming;
        self.fade_in_left = 0;
        self.last_out = [0.0; MAX_CHANNELS];
        self.underruns = 0;
        self.overruns = 0;
    }

    /// Change the target while running: no reset, no re-priming, the learned offset stays.
    /// A non-finite value is ignored.
    pub fn set_target_ms(&mut self, target_ms: f32) {
        if target_ms.is_finite() {
            self.target_ms = target_ms.clamp(MIN_TARGET_MS, MAX_TARGET_MS);
            self.controller.set_target_ms(self.target_ms);
        }
    }

    /// Applied ratio correction in ppm (published as `drift_ppm`).
    pub fn drift_ppm(&self) -> f32 {
        self.controller.ppm()
    }
    /// Smoothed FIFO fill in ms (published as `buffer_ms`).
    pub fn fill_ms(&self) -> f32 {
        self.controller.fill_ema_ms()
    }
    pub fn target_ms(&self) -> f32 {
        self.target_ms
    }
    pub fn underruns(&self) -> u32 {
        self.underruns
    }
    pub fn overruns(&self) -> u32 {
        self.overruns
    }
    pub fn is_running(&self) -> bool {
        self.state == State::Running
    }
    pub fn latency_frames(&self) -> f32 {
        self.resampler.latency_frames()
    }

    /// Produce `frames` (<= 128) frames of the consumer's clock into `out` from `ring`.
    /// Always writes all `frames` frames of the first `channels` rows (audio or silence).
    pub fn pull(
        &mut self,
        ring: &mut Consumer<f32>,
        out: &mut Planar,
        frames: usize,
    ) -> AdapterReport {
        let frames = frames.min(ENGINE_BLOCK_FRAMES);
        let ch = self.channels;
        let mut report = AdapterReport::default();
        if frames == 0 {
            return report;
        }

        // The target can't exceed a quarter of the ring, or the high-water mark (2x target)
        // would sit beyond the ring's capacity.
        let capacity_frames = ring.buffer().capacity() / ch;
        let target_frames = ((self.target_ms * SAMPLE_RATE / 1000.0) as usize).clamp(
            MIN_TARGET_FRAMES,
            (capacity_frames / 4).max(MIN_TARGET_FRAMES),
        );
        let fill = ring.slots() / ch;

        if self.state != State::Running {
            if fill < target_frames {
                silence(out, ch, frames);
                self.last_out = [0.0; MAX_CHANNELS];
                return report;
            }
            // Enough audio queued: start with fresh resampler history and fade in.
            self.resampler.reset(ch);
            self.controller.restart();
            self.fade_in_left = FADE_IN_FRAMES;
            self.state = State::Running;
        }

        let fill_ms = fill as f32 * 1000.0 / SAMPLE_RATE;
        let ratio = self.controller.update(fill_ms, frames);
        let need = self.resampler.input_frames_needed(ratio, frames);
        if fill < need {
            self.underruns = self.underruns.wrapping_add(1);
            self.state = State::Rebuffering;
            fade_out(out, &self.last_out, ch, frames);
            self.last_out = [0.0; MAX_CHANNELS];
            report.underrun = true;
            return report;
        }

        self.stage(ring, need);
        self.resampler.process(ratio, &self.work, need, out, frames);

        let high_water = (2 * target_frames).min(capacity_frames * 3 / 4);
        if fill > high_water {
            report.overrun = self.crossfade_drop(ring, out, ratio, frames, target_frames);
        }

        self.apply_fade_in(out, ch, frames);
        for (last, row) in self.last_out.iter_mut().zip(out.iter()).take(ch) {
            *last = row[frames - 1];
        }
        report.running = true;
        report
    }

    /// Pop `need` frames from the ring into the resampler's staging buffer (planar, sanitised).
    fn stage(&mut self, ring: &mut Consumer<f32>, need: usize) {
        let n = need * self.channels;
        if ring.pop_entire_slice(&mut self.staging[..n]).is_ok() {
            deinterleave(&self.staging[..n], self.channels, &mut self.work);
        } else {
            // Cannot happen (callers check the fill first); never feed stale data if it does.
            for row in self.work.iter_mut().take(self.channels) {
                row[..need].fill(0.0);
            }
        }
    }

    /// Overrun handling. `out` already holds the continuation block (A). Drop the surplus
    /// from the ring head and crossfade A -> B, where B is a block from the new head.
    /// Returns whether anything was dropped.
    fn crossfade_drop(
        &mut self,
        ring: &mut Consumer<f32>,
        out: &mut Planar,
        ratio: f64,
        frames: usize,
        target_frames: usize,
    ) -> bool {
        let ch = self.channels;
        // Block B starts from a clean resampler, so it needs exactly this much input.
        self.resampler.reset(ch);
        let need_b = self.resampler.input_frames_needed(ratio, frames);
        let remaining = ring.slots() / ch;
        let keep = target_frames.max(need_b + 8);
        let drop = remaining.saturating_sub(keep);
        if drop == 0 {
            // Nothing to drop after all; the state reset above is harmless but A's history is
            // gone, so just continue from here with a short fade (the next block re-primes).
            self.fade_in_left = self.fade_in_left.max(FADE_IN_FRAMES / 2);
            return false;
        }
        if let Ok(chunk) = ring.read_chunk(drop * ch) {
            chunk.commit_all();
        }
        self.stage(ring, need_b);
        self.resampler
            .process(ratio, &self.work, need_b, &mut self.alt, frames);
        // Linear crossfade over the whole block: A fades out while B (the new head) fades in.
        for (a_row, b_row) in out.iter_mut().zip(self.alt.iter()).take(ch) {
            for (k, (a, &b)) in a_row[..frames].iter_mut().zip(&b_row[..frames]).enumerate() {
                let w = (k + 1) as f32 / frames as f32;
                *a += (b - *a) * w;
            }
        }
        self.overruns = self.overruns.wrapping_add(1);
        // The fill just changed abruptly; let the EMA re-seed instead of "learning" a drift.
        self.controller.restart();
        true
    }

    fn apply_fade_in(&mut self, out: &mut Planar, ch: usize, frames: usize) {
        if self.fade_in_left == 0 {
            return;
        }
        let done = FADE_IN_FRAMES - self.fade_in_left;
        let n = self.fade_in_left.min(frames);
        for row in out.iter_mut().take(ch) {
            for (k, v) in row[..n].iter_mut().enumerate() {
                *v *= (done + k + 1) as f32 / FADE_IN_FRAMES as f32;
            }
        }
        self.fade_in_left -= n;
    }
}

fn silence(out: &mut Planar, channels: usize, frames: usize) {
    for row in out.iter_mut().take(channels) {
        row[..frames].fill(0.0);
    }
}

/// Fade the last output value to zero. ? Not a waveform continuation (we have no more
/// audio), just a short ramp so the stream doesn't step from the signal straight to silence.
fn fade_out(out: &mut Planar, last: &[f32; MAX_CHANNELS], channels: usize, frames: usize) {
    for (row, &l) in out.iter_mut().zip(last).take(channels) {
        for (k, v) in row[..frames].iter_mut().enumerate() {
            *v = l * (1.0 - (k + 1) as f32 / FADE_OUT_FRAMES as f32).max(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtrb::{Producer, RingBuffer};

    const TARGET_MS: f32 = 12.0;
    const FS: f32 = SAMPLE_RATE;

    fn setup(channels: usize) -> (Producer<f32>, Consumer<f32>, ClockDomainAdapter) {
        let (p, c) = RingBuffer::new(8 * 4096);
        let mut a = ClockDomainAdapter::default();
        a.configure(channels, TARGET_MS);
        (p, c, a)
    }

    /// Push `frames` frames of a continuous test tone; returns the new stream index.
    // ? The phase is computed in f64: in f32 the argument reaches ~13 000 rad after five
    // ? seconds, where one ulp is 0.001 rad, and the *test signal itself* gets phase noise
    // ? that looks like a click in the resampler output.
    fn push_tone(p: &mut Producer<f32>, channels: usize, start: usize, frames: usize) -> usize {
        for i in start..start + frames {
            let phase = core::f64::consts::TAU * 440.0 * i as f64 / f64::from(FS);
            let s = (0.5 * phase.sin()) as f32;
            for _ in 0..channels {
                p.push(s).unwrap();
            }
        }
        start + frames
    }

    const MAX_SLOPE: f32 = 0.5 * core::f32::consts::TAU * 440.0 / FS; // of the 440 Hz tone

    fn blank() -> Planar {
        [[0.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS]
    }

    #[test]
    fn stays_silent_until_the_fifo_reaches_its_target_then_starts_without_counting_an_underrun() {
        let (mut p, mut c, mut a) = setup(2);
        let mut out = blank();
        let target_frames = (TARGET_MS * FS / 1000.0) as usize; // 576
        let mut pushed = 0;
        // Fill to just below the target: silence, ring untouched.
        pushed = push_tone(&mut p, 2, pushed, target_frames - 1);
        let r = a.pull(&mut c, &mut out, 128);
        assert!(!r.running && !r.underrun);
        assert!(out[0].iter().all(|&v| v == 0.0));
        assert_eq!(
            c.slots(),
            (target_frames - 1) * 2,
            "priming must not consume"
        );
        // One more frame crosses the target.
        pushed = push_tone(&mut p, 2, pushed, 1);
        let r = a.pull(&mut c, &mut out, 128);
        assert!(r.running && !r.underrun && !r.overrun);
        assert!(a.is_running());
        // The first block fades in from silence.
        assert!(out[0][0].abs() < 0.05);
        assert_eq!(a.underruns(), 0);
        let _ = pushed;
    }

    #[test]
    fn matched_clocks_give_a_clean_continuous_stream() {
        let (mut p, mut c, mut a) = setup(2);
        // Start exactly on target: the loop pushes 128 before every pull, so the fill the
        // adapter sees is 448 + 128 = 576 = 12 ms.
        let mut pushed = push_tone(&mut p, 2, 0, 448);
        let mut all = Vec::new();
        for _ in 0..2000 {
            pushed = push_tone(&mut p, 2, pushed, 128); // producer == consumer rate
            let mut out = blank();
            let r = a.pull(&mut c, &mut out, 128);
            assert!(!r.underrun && !r.overrun);
            all.extend_from_slice(&out[0][..128]);
        }
        assert_eq!(a.underruns(), 0);
        assert_eq!(a.overruns(), 0);
        // After the 3 ms fade-in the tone is smooth: no step above the tone's own slope.
        let worst = all[300..]
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        assert!(
            worst <= MAX_SLOPE * 1.02,
            "step {worst} vs slope {MAX_SLOPE}"
        );
        assert!(
            a.drift_ppm().abs() < 20.0,
            "learned {} ppm from matched clocks",
            a.drift_ppm()
        );
        let level = all[300..].iter().fold(0.0_f32, |m, v| m.max(v.abs()));
        assert!((level - 0.5).abs() < 0.01, "level {level}");
    }

    #[test]
    fn an_underrun_fades_out_counts_once_rebuffers_and_fades_back_in() {
        let (mut p, mut c, mut a) = setup(2);
        let mut pushed = push_tone(&mut p, 2, 0, 700);
        let mut out = blank();
        let mut stream = Vec::new();
        for _ in 0..40 {
            pushed = push_tone(&mut p, 2, pushed, 128);
            a.pull(&mut c, &mut out, 128);
            stream.extend_from_slice(&out[0][..128]);
        }
        // The producer stalls: drain the ring completely, then keep pulling.
        let mut underrun_blocks = 0;
        let mut events = 0;
        for _ in 0..30 {
            let r = a.pull(&mut c, &mut out, 128);
            events += r.underrun as u32;
            underrun_blocks += (!r.running) as u32;
            stream.extend_from_slice(&out[0][..128]);
        }
        assert_eq!(events, 1, "one stall must be reported exactly once");
        assert_eq!(a.underruns(), 1);
        assert!(underrun_blocks >= 25, "should be rebuffering, not running");
        // The stream never steps: the fade-out starts at the last value and reaches zero.
        let worst = stream
            .windows(2)
            .skip(300)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        assert!(
            worst <= MAX_SLOPE * 1.02 + 1.0e-3,
            "step {worst} around the underrun"
        );
        assert!(
            stream[stream.len() - 128..].iter().all(|&v| v == 0.0),
            "rebuffering must be silent"
        );
        // The producer comes back.
        pushed = push_tone(&mut p, 2, pushed, 700);
        let mut resumed = Vec::new();
        for _ in 0..30 {
            pushed = push_tone(&mut p, 2, pushed, 128);
            let r = a.pull(&mut c, &mut out, 128);
            resumed.extend_from_slice(&out[0][..128]);
            assert!(!r.underrun);
        }
        assert!(a.is_running());
        assert_eq!(a.underruns(), 1);
        let worst = resumed
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        assert!(
            worst <= MAX_SLOPE * 1.02 + 1.0e-3,
            "restart clicked: step {worst}"
        );
    }

    #[test]
    fn an_overrun_drops_the_excess_with_a_crossfade_not_a_click() {
        let (mut p, mut c, mut a) = setup(2);
        let mut pushed = push_tone(&mut p, 2, 0, 700);
        let mut out = blank();
        let mut stream = Vec::new();
        for _ in 0..40 {
            pushed = push_tone(&mut p, 2, pushed, 128);
            a.pull(&mut c, &mut out, 128);
            stream.extend_from_slice(&out[0][..128]);
        }
        // A burst lands in the ring: fill jumps to ~60 ms (limit is 2x12 ms).
        let before = c.slots() / 2;
        pushed = push_tone(&mut p, 2, pushed, 2000);
        assert!(c.slots() / 2 > before + 1500);
        let mut dropped_report = false;
        for _ in 0..40 {
            pushed = push_tone(&mut p, 2, pushed, 128);
            let r = a.pull(&mut c, &mut out, 128);
            dropped_report |= r.overrun;
            stream.extend_from_slice(&out[0][..128]);
        }
        assert!(
            dropped_report && a.overruns() == 1,
            "overruns = {}",
            a.overruns()
        );
        assert_eq!(a.underruns(), 0);
        let fill_ms = c.slots() as f32 / 2.0 * 1000.0 / FS;
        assert!(
            fill_ms < 2.0 * TARGET_MS,
            "fill is still {fill_ms} ms after the drop"
        );
        // The splice joins two different phases of the tone; the crossfade keeps the largest
        // step within a small multiple of the tone's own slope.
        let worst = stream
            .windows(2)
            .skip(300)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        assert!(
            worst <= MAX_SLOPE * 3.0,
            "overrun click: step {worst} vs slope {MAX_SLOPE}"
        );
    }

    #[test]
    fn hostile_samples_in_the_ring_are_sanitised() {
        let (mut p, mut c, mut a) = setup(2);
        for i in 0..800 {
            let v = [f32::NAN, f32::INFINITY, 3.0e38, -3.0e38, 0.25][i % 5];
            p.push(v).unwrap();
            p.push(v).unwrap();
        }
        let mut out = blank();
        for _ in 0..4 {
            a.pull(&mut c, &mut out, 128);
            assert!(
                out[0]
                    .iter()
                    .chain(out[1].iter())
                    .all(|v| v.is_finite() && v.abs() <= 1.0e3 * 1.5)
            );
        }
    }

    #[test]
    fn every_channel_count_works_and_untouched_rows_stay_untouched() {
        for channels in [1usize, 2, 3, 6, 8] {
            let (mut p, mut c, mut a) = setup(channels);
            let mut pushed = push_tone(&mut p, channels, 0, 700);
            let mut out = blank();
            out[7] = [4.5; ENGINE_BLOCK_FRAMES];
            for _ in 0..50 {
                pushed = push_tone(&mut p, channels, pushed, 128);
                a.pull(&mut c, &mut out, 128);
            }
            assert!(a.is_running(), "{channels} ch");
            assert_eq!(a.underruns(), 0, "{channels} ch");
            for row in out.iter().take(channels) {
                assert!(
                    row[..128].iter().any(|&v| v.abs() > 0.1),
                    "{channels} ch: silent row"
                );
            }
            if channels < 8 {
                assert!(
                    out[7].iter().all(|&v| v == 4.5),
                    "{channels} ch: wrote beyond its channels"
                );
            }
        }
    }

    #[test]
    fn partial_blocks_and_odd_sizes_are_fine() {
        let (mut p, mut c, mut a) = setup(2);
        let mut pushed = push_tone(&mut p, 2, 0, 700);
        let mut out = blank();
        for n in [1, 7, 64, 128, 33, 100, 5, 128, 128, 77]
            .iter()
            .cycle()
            .take(300)
        {
            pushed = push_tone(&mut p, 2, pushed, *n);
            a.pull(&mut c, &mut out, *n);
        }
        assert_eq!(a.underruns(), 0);
        assert_eq!(a.overruns(), 0);
    }

    #[test]
    fn configure_resets_everything() {
        let (mut p, mut c, mut a) = setup(2);
        let mut pushed = push_tone(&mut p, 2, 0, 700);
        let mut out = blank();
        for _ in 0..30 {
            pushed = push_tone(&mut p, 2, pushed, 128);
            a.pull(&mut c, &mut out, 128);
        }
        assert!(a.is_running());
        a.configure(2, 20.0);
        assert!(!a.is_running());
        assert_eq!((a.underruns(), a.overruns()), (0, 0));
        assert_eq!(a.target_ms(), 20.0);
        assert_eq!(a.drift_ppm(), 0.0);
        // Nonsense targets are clamped, never trusted.
        a.configure(2, f32::NAN);
        assert_eq!(a.target_ms(), 12.0);
        a.configure(2, 1.0e9);
        assert_eq!(a.target_ms(), MAX_TARGET_MS);
        a.configure(2, -5.0);
        assert_eq!(a.target_ms(), MIN_TARGET_MS);
    }

    #[test]
    fn latency_is_inside_the_budget() {
        let a = ClockDomainAdapter::default();
        assert!(
            a.latency_frames() / FS * 1000.0 < 0.1,
            "resampler latency must stay under 0.1 ms"
        );
    }
}
