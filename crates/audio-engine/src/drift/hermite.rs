//! Our own variable-ratio cubic Hermite (Catmull-Rom) resampler.
//!
//! * 4-point cubic: output between input samples `x1` and `x2` at fraction `t`
//!   `y = ((a·t + b)·t + c)·t + d` with `a = -½x0 + 3/2x1 - 3/2x2 + ½x3`, `b = x0 - 5/2x1 + 2x2 - ½x3`,
//!   `c = ½(x2 - x0)`, `d = x1`. Reproduces polynomials up to degree 2 exactly.
//! * Latency: 2 frames at ratio 1 (0.042 ms @ 48 kHz), well under the 0.1 ms budget.
//! * Accuracy: error ≈ 0.04·ω⁴ at the worst fraction, i.e. about -95 dB for a 1 kHz tone
//!   at 48 kHz. Fine for drift correction (ratio within 0.1 %); NOT a general-purpose
//!   sample-rate converter for high frequencies.

use super::{DriftResampler, MAX_RATIO_OFFSET, WORK_FRAMES, Work};
use crate::dsp::Planar;
use audio_types::{ENGINE_BLOCK_FRAMES, MAX_CHANNELS};

/// Frames of history kept in front of the new input: at least 3 (`x0` of the first output
/// needs one frame before the interpolation interval, and the interval itself is two frames).
const HIST_MIN: usize = 3;
/// ... and at most 4. A pull ends with `floor(p_next) - floor(p_last) ∈ {0, 1, 2}`; when the
/// position barely advanced (ratio just below 1 and a tiny fractional phase) one more frame
/// than `HIST_MIN` has to be carried over, or that frame is lost and the stream skips a sample.
const HIST_MAX: usize = 4;
/// A pull of at most 128 outputs at |ratio - 1| <= 1 % moves the fractional offset by at
/// most 1.28, so it crosses a whole-sample boundary at most twice -> at most 3 segments.
const MAX_SEGMENTS: usize = 4;

#[derive(Clone, Copy, Default)]
struct Segment {
    /// First output index of the segment.
    start: usize,
    /// Number of outputs.
    len: usize,
    /// Index in the work buffer of `x0` for the segment's first output.
    first_x0: usize,
    /// Fraction `t` of the first output.
    t0: f32,
}

pub struct HermiteResampler {
    channels: usize,
    /// `[history (`hist` frames) | new input]` per channel.
    buf: [[f32; HIST_MAX + WORK_FRAMES]; MAX_CHANNELS],
    /// Valid history frames at the front of `buf` (3 or 4).
    hist: usize,
    /// Position of the next output in `buf` coordinates; always in `[1, 2)` between pulls.
    pos: f64,
}

impl Default for HermiteResampler {
    fn default() -> Self {
        Self::new()
    }
}

impl HermiteResampler {
    pub fn new() -> Self {
        Self {
            channels: 2,
            buf: [[0.0; HIST_MAX + WORK_FRAMES]; MAX_CHANNELS],
            hist: HIST_MIN,
            pos: 1.0,
        }
    }

    #[inline]
    fn clamp_ratio(ratio: f64) -> f64 {
        if ratio.is_finite() {
            ratio.clamp(1.0 - MAX_RATIO_OFFSET, 1.0 + MAX_RATIO_OFFSET)
        } else {
            1.0
        }
    }

    /// Total `buf` length (history included) a pull of `n` outputs needs, and the position
    /// after the pull. Single source of truth for `input_frames_needed` and `process`.
    #[inline]
    fn plan(p0: f64, r: f64, n: usize) -> (usize, f64) {
        let p_last = p0 + n.saturating_sub(1) as f64 * r;
        let p_next = p0 + n as f64 * r;
        // The last output reads up to index floor(p_last) + 2, so the buffer must reach
        // floor(p_last) + 3 frames. The history kept for the next pull starts at
        // floor(p_next) - 1 and runs to the END of the buffer (3 frames, or 4 when the
        // position barely advanced), so the buffer must also reach floor(p_next) + 2.
        let need = (p_last.floor() as usize + 3).max(p_next.floor() as usize + 2);
        (need, p_next)
    }
}

impl DriftResampler for HermiteResampler {
    fn reset(&mut self, channels: usize) {
        self.channels = channels.clamp(1, MAX_CHANNELS);
        self.buf = [[0.0; HIST_MAX + WORK_FRAMES]; MAX_CHANNELS];
        self.hist = HIST_MIN;
        self.pos = 1.0;
    }

    fn latency_frames(&self) -> f32 {
        2.0
    }

    fn input_frames_needed(&mut self, ratio: f64, out_frames: usize) -> usize {
        let n = out_frames.min(ENGINE_BLOCK_FRAMES);
        if n == 0 {
            return 0;
        }
        let (need, _) = Self::plan(self.pos, Self::clamp_ratio(ratio), n);
        need.saturating_sub(self.hist)
    }

    fn process(
        &mut self,
        ratio: f64,
        input: &Work,
        in_frames: usize,
        output: &mut Planar,
        out_frames: usize,
    ) {
        let n = out_frames.min(ENGINE_BLOCK_FRAMES);
        if n == 0 {
            return;
        }
        let r = Self::clamp_ratio(ratio);
        let dt = r - 1.0;
        let p0 = self.pos;
        let (need, p_next) = Self::plan(p0, r, n);
        let hist = self.hist;
        // Exactly `need - hist` new frames are consumed. Extra frames from a caller that
        // ignored `input_frames_needed` are ignored; missing ones read as silence (never as
        // stale memory).
        let wanted = need.saturating_sub(hist);
        let take = in_frames.min(wanted).min(WORK_FRAMES);
        let channels = self.channels;
        for (row, src) in self.buf.iter_mut().zip(input.iter()).take(channels) {
            row[hist..hist + take].copy_from_slice(&src[..take]);
            if take < wanted {
                row[hist + take..hist + wanted.min(WORK_FRAMES)].fill(0.0);
            }
        }

        // Split the block into segments in which the whole-sample offset is constant.
        // Output k sits at P_k = p0 + k·r = (p0 + k·dt) + k, so with q_k = p0 + k·dt:
        // x0 index = floor(q_k) + k - 1 and fraction t = q_k - floor(q_k).
        let mut segs = [Segment::default(); MAX_SEGMENTS];
        let mut n_segs = 0;
        let mut k = 0;
        while k < n && n_segs < MAX_SEGMENTS {
            let q = p0 + k as f64 * dt;
            let s = q.floor();
            let t0 = q - s;
            let count = if dt > 0.0 {
                ((1.0 - t0) / dt).ceil()
            } else if dt < 0.0 {
                (t0 / -dt).floor() + 1.0
            } else {
                f64::INFINITY
            };
            let m = (count.max(1.0).min((n - k) as f64)) as usize;
            // floor(q) + k - 1 >= 0 always holds because P_k >= 1 (see `pos` invariant).
            let first_x0 = (s as isize + k as isize - 1).max(0) as usize;
            segs[n_segs] = Segment {
                start: k,
                len: m,
                first_x0,
                t0: t0 as f32,
            };
            n_segs += 1;
            k += m;
        }
        // !! Unreachable with the clamped ratio (see MAX_SEGMENTS). If it ever happened the
        // !! unwritten outputs keep their old contents: wrong audio, but never a panic on the
        // !! RT thread (extending a segment would index past the input window).
        debug_assert!(k >= n, "resampler segment table overflow");

        let dtf = dt as f32;
        for (row_in, row_out) in self.buf.iter().zip(output.iter_mut()).take(channels) {
            for seg in &segs[..n_segs] {
                cubic_segment(
                    row_in,
                    seg.first_x0,
                    seg.len,
                    seg.t0,
                    dtf,
                    &mut row_out[seg.start..seg.start + seg.len],
                );
            }
        }

        // Carry over everything from the first frame the next pull needs to the end of what we
        // were given (3 or 4 frames), and renormalise the position. Dropping the 4th frame
        // here is the bug the `..._is_not_lost` regression test pins down.
        let shift = p_next.floor() as usize - 1;
        let keep = need - shift;
        debug_assert!(
            (HIST_MIN..=HIST_MAX).contains(&keep),
            "history of {keep} frames"
        );
        for row in self.buf.iter_mut().take(channels) {
            row.copy_within(shift..need, 0);
        }
        self.hist = keep.clamp(HIST_MIN, HIST_MAX);
        self.pos = p_next - shift as f64;
    }
}

/// `out[j] = cubic(x[first+j .. first+j+3], t0 + j·dt)` for `j` in `0..len`.
// ? Four shifted windows into the same slice: for consecutive `j` they are contiguous, so the
// ? loop body has no gathers and the compiler vectorises it (4 samples per SSE instruction).
#[inline]
fn cubic_segment(w: &[f32], first: usize, len: usize, t0: f32, dt: f32, out: &mut [f32]) {
    let x0 = &w[first..first + len];
    let x1 = &w[first + 1..first + 1 + len];
    let x2 = &w[first + 2..first + 2 + len];
    let x3 = &w[first + 3..first + 3 + len];
    for (j, (((o, &a0), &a1), (&a2, &a3))) in out
        .iter_mut()
        .zip(x0)
        .zip(x1)
        .zip(x2.iter().zip(x3))
        .enumerate()
    {
        let t = t0 + j as f32 * dt;
        let a = 1.5 * (a1 - a2) + 0.5 * (a3 - a0);
        let b = a0 - 2.5 * a1 + 2.0 * a2 - 0.5 * a3;
        let c = 0.5 * (a2 - a0);
        *o = ((a * t + b) * t + c) * t + a1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::util::SAMPLE_RATE;

    /// Pull `blocks` blocks of `n` frames out of the input stream `stream(frame_index)`
    /// (one channel). Returns the outputs and the absolute *stream* position of each output
    /// (stream frame 0 is the first frame ever handed to the resampler).
    fn run(
        rs: &mut HermiteResampler,
        ratio: impl Fn(usize) -> f64,
        n: usize,
        blocks: usize,
        stream: impl Fn(usize) -> f32,
    ) -> (Vec<f32>, Vec<f64>) {
        let mut out_all = Vec::new();
        let mut pos_all = Vec::new();
        let mut handed_over = 0usize; // stream frames given to the resampler so far
        let mut base = -(HIST_MIN as f64); // stream index of buf[0] (3 frames of zero history first)
        for blk in 0..blocks {
            let r = ratio(blk);
            let r_eff = HermiteResampler::clamp_ratio(r);
            let need = rs.input_frames_needed(r, n);
            let mut work: Work = [[0.0; WORK_FRAMES]; MAX_CHANNELS];
            for (i, v) in work[0][..need].iter_mut().enumerate() {
                *v = stream(handed_over + i);
            }
            let p0 = rs.pos;
            let mut out: Planar = [[0.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS];
            rs.process(r, &work, need, &mut out, n);
            for (k, &y) in out[0][..n].iter().enumerate() {
                out_all.push(y);
                pos_all.push(base + p0 + k as f64 * r_eff);
            }
            // `process` renormalised `pos`; the difference is how far buf[0] moved in the stream.
            base += p0 + n as f64 * r_eff - rs.pos;
            handed_over += need;
        }
        (out_all, pos_all)
    }

    #[test]
    fn ratio_one_is_a_pure_two_sample_delay() {
        let mut rs = HermiteResampler::new();
        rs.reset(1);
        let x = |i: usize| ((i * 7919) % 1000) as f32 / 500.0 - 1.0; // arbitrary but deterministic
        let (out, _) = run(&mut rs, |_| 1.0, 128, 6, x);
        for (k, &y) in out.iter().enumerate() {
            let want = if k >= 2 { x(k - 2) } else { 0.0 };
            assert_eq!(y, want, "output {k}");
        }
        assert!(
            rs.latency_frames() / SAMPLE_RATE * 1000.0 < 0.1,
            "latency budget"
        );
    }

    #[test]
    fn quadratics_are_reproduced_exactly_at_any_ratio() {
        // Catmull-Rom is third-order: degree <= 2 signals pass through unchanged, whatever
        // the fractional positions are, and across block boundaries.
        let poly = |t: f64| 0.3 + 0.02 * t - 0.0001 * t * t;
        for ratio in [1.0, 1.0007, 0.9993, 1.004, 0.996] {
            let mut rs = HermiteResampler::new();
            rs.reset(1);
            let (out, pos) = run(&mut rs, |_| ratio, 128, 40, |i| poly(i as f64) as f32);
            for (k, (&y, &p)) in out.iter().zip(&pos).enumerate().skip(8) {
                let want = poly(p) as f32;
                // Relative: |poly| reaches ~170 here, where one f32 ulp is already 1.5e-5.
                assert!(
                    (y - want).abs() < 2.0e-6 * want.abs().max(1.0),
                    "ratio {ratio} out {k}: {y} vs {want}"
                );
            }
        }
    }

    #[test]
    fn needed_input_matches_the_ratio_and_never_drifts() {
        let mut rs = HermiteResampler::new();
        rs.reset(1);
        let ratio = 1.0006;
        let (mut consumed, mut produced) = (0usize, 0usize);
        for blk in 0..4000 {
            let n = [128, 128, 64, 128, 37, 128][blk % 6];
            consumed += rs.input_frames_needed(ratio, n);
            let work: Work = [[0.0; WORK_FRAMES]; MAX_CHANNELS];
            let mut out: Planar = [[0.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS];
            let need = rs.input_frames_needed(ratio, n);
            rs.process(ratio, &work, need, &mut out, n);
            produced += n;
        }
        let measured = consumed as f64 / produced as f64;
        assert!(
            (measured - ratio).abs() < 2.0e-6,
            "consumed/produced = {measured}"
        );
    }

    /// THD+N of a sine through the resampler: LS-fit the fundamental at the *output*
    /// frequency, return residual/fundamental in dB.
    fn thd_n_db(freq_hz: f64, ratio: f64) -> f64 {
        let fs = f64::from(SAMPLE_RATE);
        let mut rs = HermiteResampler::new();
        rs.reset(1);
        let x = |i: usize| (0.5 * (core::f64::consts::TAU * freq_hz * i as f64 / fs).sin()) as f32;
        let (out, _) = run(&mut rs, |_| ratio, 128, 400, x);
        let w = core::f64::consts::TAU * freq_hz * ratio / fs; // radians per OUTPUT sample
        let seg = &out[256..]; // skip the start-up transient
        // Least-squares fit y ≈ a·cos + b·sin. The two columns are not exactly orthogonal over
        // a non-integer number of cycles, so solve the full 2x2 normal equations: at a -90 dB
        // residual the cross term would otherwise be a visible error in the measurement itself.
        let (mut cc, mut cs, mut ss, mut yc, mut ys, mut yy) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        for (i, &y) in seg.iter().enumerate() {
            let (s, c) = (w * (i + 256) as f64).sin_cos();
            let y = f64::from(y);
            cc += c * c;
            cs += c * s;
            ss += s * s;
            yc += y * c;
            ys += y * s;
            yy += y * y;
        }
        let det = cc * ss - cs * cs;
        let a = (yc * ss - ys * cs) / det;
        let b = (ys * cc - yc * cs) / det;
        let fundamental = a * yc + b * ys; // energy explained by the fitted sinusoid
        let residual = (yy - fundamental).max(1.0e-30);
        10.0 * (residual / fundamental).log10()
    }

    #[test]
    fn thd_plus_n_of_a_1khz_sine_is_below_minus_80_db() {
        for ratio in [1.0, 1.0002, 0.9998, 1.0005, 0.9995, 1.001, 0.999] {
            let d = thd_n_db(1000.0, ratio);
            println!("1 kHz, ratio {ratio:.4}: THD+N = {d:.1} dB");
            assert!(d < -80.0, "ratio {ratio}: THD+N {d:.1} dB");
        }
        // For the record (not asserted): cubic degrades with frequency, as expected.
        for f in [100.0, 5000.0, 10_000.0] {
            println!(
                "{f:>7} Hz, ratio 1.0005: THD+N = {:.1} dB",
                thd_n_db(f, 1.0005)
            );
        }
    }

    #[test]
    fn a_moving_ratio_never_makes_a_step() {
        // Sweep the ratio across the whole +-1000 ppm range while a 440 Hz tone plays; the
        // output must stay smooth (no discontinuity at segment or block boundaries).
        let fs = f64::from(SAMPLE_RATE);
        let mut rs = HermiteResampler::new();
        rs.reset(1);
        let amp = 0.5;
        let x = |i: usize| (amp * (core::f64::consts::TAU * 440.0 * i as f64 / fs).sin()) as f32;
        let ratio = |blk: usize| 1.0 + 0.001 * (blk as f64 * 0.05).sin();
        let (out, _) = run(&mut rs, ratio, 128, 600, x);
        let max_step = 2.0 * core::f64::consts::PI * 440.0 / fs * amp * 1.01;
        let worst = out
            .windows(2)
            .skip(8)
            .map(|w| f64::from((w[1] - w[0]).abs()))
            .fold(0.0, f64::max);
        assert!(
            worst <= max_step * 1.02,
            "largest step {worst} vs smooth bound {max_step}"
        );
    }

    /// Slow tone: Catmull-Rom's own error is ~1e-9 here, so any larger mismatch against the
    /// exact sine at the output's true stream position means a wrong sample position.
    fn slow_tone(i: usize) -> f32 {
        (0.5 * (core::f64::consts::TAU * 50.0 * i as f64 / f64::from(SAMPLE_RATE)).sin()) as f32
    }
    fn exact_slow_tone(p: f64) -> f64 {
        0.5 * (core::f64::consts::TAU * 50.0 * p / f64::from(SAMPLE_RATE)).sin()
    }

    #[test]
    fn the_fourth_history_frame_is_not_lost() {
        // REGRESSION. With ratio < 1 and a tiny fractional phase, a pull advances the position
        // by less than a whole sample, so the carried-over history is 4 frames, not 3. An early
        // version kept 3, dropped the newest frame, and the stream skipped one input sample
        // (a click) -- only ~0.03 % of blocks at 0.9997, so random tests rarely hit it.
        // Here the alignment is forced: ratio 0.999, p_last = 128.0002.
        let ratio = 0.999;
        let mut rs = HermiteResampler::new();
        rs.reset(1);
        rs.pos = 1.0 + (0.0002 - 0.873_f64).rem_euclid(1.0); // p0 = 1.1272
        let (need, p_next) = HermiteResampler::plan(rs.pos, ratio, 128);
        assert_eq!(
            p_next.floor() as usize,
            (rs.pos + 127.0 * ratio).floor() as usize,
            "test setup: the position must barely advance"
        );
        assert_eq!(
            need - (p_next.floor() as usize - 1),
            4,
            "test setup: 4 history frames expected"
        );

        let (out, pos) = run(&mut rs, |_| ratio, 128, 8, slow_tone);
        for (k, (&y, &p)) in out.iter().zip(&pos).enumerate().skip(8) {
            let err = (f64::from(y) - exact_slow_tone(p)).abs();
            assert!(
                err < 1.0e-5,
                "output {k} (block {}): off by {err} -> a sample was lost",
                k / 128
            );
        }
        assert_eq!(rs.hist, HIST_MIN.max(rs.hist)); // hist stays within 3..=4
    }

    #[test]
    fn random_ratios_over_many_blocks_stay_on_the_exact_position() {
        // Every block uses a different ratio across the full +-1 % range and a different
        // block length, so the fractional phase visits every alignment (including the
        // edge cases above) many times over.
        let mut seed = 0x1234_5678_u32;
        let mut next = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f64 / f64::from(1u32 << 24)
        };
        let mut rs = HermiteResampler::new();
        rs.reset(1);
        let ratios: Vec<f64> = (0..20_000).map(|_| 1.0 + (next() - 0.5) * 0.02).collect();
        let sizes: Vec<usize> = (0..20_000)
            .map(|_| 1 + (next() * 128.0) as usize % 128)
            .collect();
        // `run` takes a fixed block size, so drive it block by block here.
        let mut base = -(HIST_MIN as f64);
        let mut handed_over = 0usize;
        let mut worst = 0.0_f64;
        let mut four_history_blocks = 0;
        for (r, &n) in ratios.iter().zip(&sizes) {
            let r_eff = HermiteResampler::clamp_ratio(*r);
            let need = rs.input_frames_needed(*r, n);
            let mut work: Work = [[0.0; WORK_FRAMES]; MAX_CHANNELS];
            for (i, v) in work[0][..need].iter_mut().enumerate() {
                *v = slow_tone(handed_over + i);
            }
            let p0 = rs.pos;
            let mut out: Planar = [[0.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS];
            rs.process(*r, &work, need, &mut out, n);
            four_history_blocks += (rs.hist == HIST_MAX) as usize;
            for (k, &y) in out[0][..n].iter().enumerate() {
                let p = base + p0 + k as f64 * r_eff;
                if p > 4.0 {
                    worst = worst.max((f64::from(y) - exact_slow_tone(p)).abs());
                }
            }
            base += p0 + n as f64 * r_eff - rs.pos;
            handed_over += need;
        }
        assert!(
            four_history_blocks > 50,
            "the 4-frame case was hit only {four_history_blocks} times"
        );
        assert!(
            worst < 1.0e-5,
            "worst deviation from the exact tone: {worst}"
        );
    }

    #[test]
    fn hostile_ratios_are_clamped_not_trusted() {
        let mut rs = HermiteResampler::new();
        rs.reset(2);
        for r in [f64::NAN, f64::INFINITY, -3.0, 0.0, 50.0] {
            let need = rs.input_frames_needed(r, 128);
            assert!(
                (126..=135).contains(&need),
                "ratio {r} -> {need} input frames"
            );
            let work: Work = [[0.25; WORK_FRAMES]; MAX_CHANNELS];
            let mut out: Planar = [[0.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS];
            rs.process(r, &work, need, &mut out, 128);
            assert!(out[0].iter().all(|v| v.is_finite()));
        }
        // Too little input: the missing tail is silence, not stale memory.
        rs.reset(1);
        let work: Work = [[1.0; WORK_FRAMES]; MAX_CHANNELS];
        let mut out: Planar = [[9.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS];
        rs.process(1.0, &work, 10, &mut out, 128);
        assert!(out[0].iter().all(|v| v.is_finite() && v.abs() <= 1.5));
    }

    #[test]
    fn channels_are_independent_and_extra_channels_untouched() {
        let mut rs = HermiteResampler::new();
        rs.reset(2);
        let need = rs.input_frames_needed(1.0003, 128);
        let mut work: Work = [[0.0; WORK_FRAMES]; MAX_CHANNELS];
        let [r0, r1, r2, ..] = &mut work;
        for (i, ((a, b), c)) in r0
            .iter_mut()
            .zip(r1.iter_mut())
            .zip(r2.iter_mut())
            .enumerate()
            .take(need)
        {
            *a = (i as f32 * 0.01).sin();
            *b = -(i as f32 * 0.02).sin();
            *c = 7.0; // channel 3 is outside `channels` and must be ignored
        }
        let mut out: Planar = [[5.5; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS];
        rs.process(1.0003, &work, need, &mut out, 128);
        assert!(out[0][100] != out[1][100]);
        assert!(
            out[2].iter().all(|&v| v == 5.5),
            "channel beyond `channels` was written"
        );
    }
}
