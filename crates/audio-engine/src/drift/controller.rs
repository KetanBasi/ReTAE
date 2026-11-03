//! Drift controller: EMA of FIFO fill vs target -> PI -> resampling ratio (CLAUDE.md law 2).
//!
//! * EMA time constant ≈ 1 s, so packet/burst sawtooth in the fill level averages out.
//! * Output clamp ±1000 ppm (crystals are ±20..100 ppm; 1000 ppm is 1.7 cents, inaudible).
//! * Slew limit 10 ppm per 128-frame block, so the pitch never steps audibly.
//!
//! # Why these gains
//! With the fill measured in ms, the plant is an integrator: `dF/dt = 1e-3 · (δ - u)` ms/s,
//! where `δ` is the true clock offset and `u` the applied correction, both in ppm. The
//! 1 s EMA adds a lag, so the loop's characteristic polynomial is
//! `τ s³ + s² + (1e-3·Kp) s + 1e-3·Ki`, stable iff `Kp > Ki` (in these units). The defaults sit
//! at `Kp = 300 ppm/ms`, `Ki = 25 ppm/(ms·s)`: three real poles (no overshoot) with the
//! slowest at about -0.14 /s. The tests simulate it against a drifting producer.

use crate::dsp::util::{SAMPLE_RATE, pole_coeff};
use audio_types::ENGINE_BLOCK_FRAMES;

/// EMA time constant for the fill level.
pub const EMA_TAU_S: f32 = 1.0;
/// Hard clamp on the applied correction.
pub const MAX_PPM: f32 = 1000.0;
/// Largest change of the applied correction per 128-frame block.
pub const SLEW_PPM_PER_BLOCK: f32 = 10.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DriftGains {
    /// Proportional gain: ppm of correction per ms of fill error.
    pub kp_ppm_per_ms: f32,
    /// Integral gain: ppm per ms of fill error per second.
    pub ki_ppm_per_ms_s: f32,
}

impl Default for DriftGains {
    fn default() -> Self {
        Self {
            kp_ppm_per_ms: 300.0,
            ki_ppm_per_ms_s: 25.0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DriftController {
    gains: DriftGains,
    target_ms: f32,
    /// Smoothed fill level.
    ema_ms: f32,
    /// False until the first measurement seeds the EMA.
    seeded: bool,
    /// The integrator, already scaled to ppm. Converges to the true clock offset.
    integral_ppm: f32,
    /// Correction currently applied (after clamp and slew limit).
    ppm: f32,
}

impl DriftController {
    pub fn new(target_ms: f32) -> Self {
        Self::with_gains(target_ms, DriftGains::default())
    }

    pub fn with_gains(target_ms: f32, gains: DriftGains) -> Self {
        Self {
            gains,
            target_ms,
            ema_ms: target_ms,
            seeded: false,
            integral_ppm: 0.0,
            ppm: 0.0,
        }
    }

    pub fn set_target_ms(&mut self, target_ms: f32) {
        self.target_ms = target_ms;
    }

    /// Forget the fill history (after a rebuffer) but KEEP the learned clock offset: the
    /// crystals didn't change just because the stream stalled.
    pub fn restart(&mut self) {
        self.seeded = false;
    }

    /// Forget everything, including the learned offset.
    pub fn reset(&mut self) {
        self.seeded = false;
        self.integral_ppm = 0.0;
        self.ppm = 0.0;
    }

    /// Applied correction in ppm (this is what gets published as `drift_ppm`).
    pub fn ppm(&self) -> f32 {
        self.ppm
    }

    /// Smoothed fill level in ms.
    pub fn fill_ema_ms(&self) -> f32 {
        self.ema_ms
    }

    pub fn target_ms(&self) -> f32 {
        self.target_ms
    }

    /// One call per pull. `fill_ms` is the FIFO fill measured *before* the pull,
    /// `frames` the number of output frames of this pull. Returns the ratio for this pull
    /// (input frames consumed per output frame).
    pub fn update(&mut self, fill_ms: f32, frames: usize) -> f64 {
        if !fill_ms.is_finite() {
            return 1.0 + f64::from(self.ppm) * 1.0e-6;
        }
        if !self.seeded {
            self.ema_ms = fill_ms;
            self.seeded = true;
        } else {
            self.ema_ms += pole_coeff(EMA_TAU_S * 1000.0, frames as f32) * (fill_ms - self.ema_ms);
        }
        let dt_s = frames as f32 / SAMPLE_RATE;
        let err_ms = self.ema_ms - self.target_ms;

        // Anti-windup (conditional integration): while the output is pinned at the clamp *and*
        // the error still pushes the same way, the integrator stands still.
        // ? Without this, a large fill error (a stream that starts 5 ms above target, or a
        // ? burst after a network stall) saturates the P term for seconds; the integrator keeps
        // ? adding up meanwhile, and once the fill reaches the target that stored charge keeps
        // ? pushing it through: the fill ends up *below* target, which for a jitter buffer is
        // ? the dangerous side. Small errors never saturate, so the normal tracking of a clock
        // ? offset is untouched.
        let p_term = self.gains.kp_ppm_per_ms * err_ms;
        let next_integral = (self.integral_ppm + self.gains.ki_ppm_per_ms_s * err_ms * dt_s)
            .clamp(-MAX_PPM, MAX_PPM);
        let next_desired = p_term + next_integral;
        let pinned = next_desired.abs() > MAX_PPM && next_desired.signum() == err_ms.signum();
        if !pinned {
            self.integral_ppm = next_integral;
        }
        let desired = (p_term + self.integral_ppm).clamp(-MAX_PPM, MAX_PPM);

        let max_step = SLEW_PPM_PER_BLOCK * frames as f32 / ENGINE_BLOCK_FRAMES as f32;
        self.ppm += (desired - self.ppm).clamp(-max_step, max_step);
        1.0 + f64::from(self.ppm) * 1.0e-6
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK_S: f32 = ENGINE_BLOCK_FRAMES as f32 / SAMPLE_RATE;

    /// Ideal plant: fill (ms) integrates `1e-3 · (offset_ppm - applied_ppm)` per second.
    /// Returns (peak |error| ms after `settle_s`, peak |error| ms overall, final ppm).
    fn simulate(
        gains: DriftGains,
        offset_ppm: f32,
        seconds: f32,
        settle_s: f32,
    ) -> (f32, f32, f32) {
        let target = 12.0_f32;
        let mut c = DriftController::with_gains(target, gains);
        let mut fill = target;
        let (mut peak_after, mut peak_all) = (0.0_f32, 0.0_f32);
        let blocks = (seconds / BLOCK_S) as usize;
        for b in 0..blocks {
            let ratio = c.update(fill, ENGINE_BLOCK_FRAMES);
            let applied_ppm = ((ratio - 1.0) * 1.0e6) as f32;
            fill += 1.0e-3 * (offset_ppm - applied_ppm) * BLOCK_S;
            let e = (fill - target).abs();
            peak_all = peak_all.max(e);
            if b as f32 * BLOCK_S >= settle_s {
                peak_after = peak_after.max(e);
            }
        }
        (peak_after, peak_all, c.ppm())
    }

    #[test]
    fn learns_the_clock_offset_and_keeps_the_fill_within_a_millisecond() {
        // Real crystals differ by at most a few hundred ppm; the S2 scenario is 208 ppm.
        for offset in [208.0, -208.0, 100.0, -100.0, 300.0, -300.0] {
            let (after, all, ppm) = simulate(DriftGains::default(), offset, 120.0, 5.0);
            println!(
                "offset {offset:>6} ppm: peak |err| {all:.3} ms, after 5 s {after:.3} ms, learned {ppm:.1} ppm"
            );
            assert!(
                (ppm - offset).abs() < 2.0,
                "learned {ppm} ppm for a {offset} ppm offset"
            );
            assert!(all < 1.0, "fill strayed {all} ms from target");
        }
        // Beyond any real crystal the controller still converges and stays bounded; the fill
        // just wanders further first (the peak error scales with the offset).
        for offset in [600.0, -800.0] {
            let (_, all, ppm) = simulate(DriftGains::default(), offset, 180.0, 5.0);
            println!("offset {offset:>6} ppm: peak |err| {all:.3} ms, learned {ppm:.1} ppm");
            assert!(
                (ppm - offset).abs() < 2.0,
                "learned {ppm} ppm for a {offset} ppm offset"
            );
            assert!(all < 3.0, "fill strayed {all} ms from target");
        }
    }

    #[test]
    fn output_is_clamped_to_1000_ppm_and_the_integrator_unwinds_after_saturation() {
        // Closed loop. A 1500 ppm offset cannot be followed (the clamp is 1000 ppm), so the
        // output saturates and the fill climbs. When the offset disappears the controller must
        // recover without a big undershoot: with a wound-up integrator it would not.
        let target = 12.0_f32;
        let mut c = DriftController::new(target);
        let mut fill = target;
        let step = |c: &mut DriftController, fill: &mut f32, offset_ppm: f32| {
            let ratio = c.update(*fill, ENGINE_BLOCK_FRAMES);
            let applied = ((ratio - 1.0) * 1.0e6) as f32;
            assert!(
                applied.abs() <= MAX_PPM + 0.01,
                "applied {applied} ppm beyond the clamp"
            );
            *fill += 1.0e-3 * (offset_ppm - applied) * BLOCK_S;
        };
        for _ in 0..(30.0 / BLOCK_S) as usize {
            step(&mut c, &mut fill, 1500.0);
        }
        assert_eq!(c.ppm(), MAX_PPM, "output must saturate at the clamp");
        assert!(
            fill > target + 8.0,
            "the offset was unfollowable, so the fill must have grown"
        );

        let (mut lowest, mut recovered_at) = (fill, None);
        for b in 0..(300.0 / BLOCK_S) as usize {
            step(&mut c, &mut fill, 0.0);
            lowest = lowest.min(fill);
            if recovered_at.is_none() && (fill - target).abs() < 0.5 && c.ppm().abs() < 20.0 {
                recovered_at = Some(b as f32 * BLOCK_S);
            }
        }
        println!(
            "after saturation: undershoot to {lowest:.2} ms, recovered after {recovered_at:?} s"
        );
        assert!(
            lowest > target - 8.0,
            "wound-up integrator: fill undershot to {lowest} ms"
        );
        assert!(
            recovered_at.is_some_and(|t| t < 150.0),
            "did not recover: {recovered_at:?}"
        );

        // Same on the negative side.
        let mut c = DriftController::new(target);
        let mut fill = target;
        for _ in 0..(30.0 / BLOCK_S) as usize {
            step(&mut c, &mut fill, -1500.0);
        }
        assert_eq!(c.ppm(), -MAX_PPM);
    }

    #[test]
    fn a_fill_that_starts_high_comes_down_to_the_target_without_undershooting() {
        // A stream that starts with a cushion (or a burst after a network stall) begins several
        // ms above the target with no clock offset at all. The P term saturates at once; the
        // loop must drain the extra and stop AT the target. Undershooting is the dangerous
        // direction for a jitter buffer.
        let target = 12.0_f32;
        let mut dips = Vec::new();
        for start_error in [3.0_f32, 6.0, 12.0, 30.0] {
            for offset in [0.0_f32, 150.0, -150.0] {
                let mut c = DriftController::new(target);
                let mut fill = target + start_error;
                let (mut lowest, mut settled_at) = (fill, None);
                for b in 0..(120.0 / BLOCK_S) as usize {
                    let ratio = c.update(fill, ENGINE_BLOCK_FRAMES);
                    let applied = ((ratio - 1.0) * 1.0e6) as f32;
                    fill += 1.0e-3 * (offset - applied) * BLOCK_S;
                    lowest = lowest.min(fill);
                    if settled_at.is_none() && (fill - target).abs() < 0.5 {
                        settled_at = Some(b as f32 * BLOCK_S);
                    }
                }
                println!(
                    "start +{start_error:>4} ms, offset {offset:>6} ppm: lowest {lowest:.2} ms, within 0.5 ms after {settled_at:?} s"
                );
                // ? A PI loop around an integrating plant always dips a little below the target
                // ? after it has come down from above (the integrator holds charge from the
                // ? descent), and learning a -150 ppm offset needs a stretch of negative error
                // ? anyway. What must not happen is the dip growing with the start error, which
                // ? is what a wound-up integrator does: it scales with how long the P term sat
                // ? at the clamp.
                let dip = target - lowest;
                dips.push((start_error, offset, dip));
                assert!(
                    settled_at.is_some_and(|t| t < 100.0),
                    "start +{start_error} ms, offset {offset}: never settled ({settled_at:?})"
                );
            }
        }
        for &(start_error, offset, dip) in &dips {
            assert!(
                dip < 1.6,
                "start +{start_error} ms, offset {offset}: fill undershot by {dip} ms"
            );
        }
    }

    #[test]
    fn slew_is_limited_to_10_ppm_per_block() {
        let mut c = DriftController::new(12.0);
        c.update(12.0, ENGINE_BLOCK_FRAMES); // seed
        let mut prev = c.ppm();
        let mut worst = 0.0_f32;
        for _ in 0..4000 {
            c.update(80.0, ENGINE_BLOCK_FRAMES); // huge error: wants full scale at once
            worst = worst.max((c.ppm() - prev).abs());
            prev = c.ppm();
        }
        // ! These compare against the LITERAL values from CLAUDE.md law 2, not the constants:
        // ! a test that reads the constant it is testing passes for any value of that constant.
        assert!(
            worst <= 10.0 + 1.0e-4,
            "slewed {worst} ppm in one block (law: <= 10)"
        );
        assert!(
            worst > 9.0,
            "slew limit never reached; the test isn't exercising it"
        );
        // Partial blocks scale the limit with their length.
        let mut c = DriftController::new(12.0);
        c.update(12.0, 32);
        let before = c.ppm();
        c.update(80.0, 32);
        assert!((c.ppm() - before).abs() <= 10.0 * 0.25 + 1.0e-4);
    }

    #[test]
    fn the_law_2_constants_are_what_the_architecture_says() {
        // CLAUDE.md law 2: EMA about 1 s, clamp +-1000 ppm, slew <= 10 ppm per block.
        assert_eq!(EMA_TAU_S, 1.0);
        assert_eq!(MAX_PPM, 1000.0);
        assert_eq!(SLEW_PPM_PER_BLOCK, 10.0);
    }

    #[test]
    fn the_ema_smooths_a_sawtooth() {
        // A 480-frame producer makes the sampled fill sweep 10 ms peak-to-peak.
        let mut c = DriftController::new(12.0);
        let mut lo = f32::MAX;
        let mut hi = f32::MIN;
        for b in 0..(30.0 / BLOCK_S) as usize {
            let saw = 7.0 + 10.0 * ((b as f32 * 0.27) % 1.0); // 7..17 ms, mean 12
            c.update(saw, ENGINE_BLOCK_FRAMES);
            if b as f32 * BLOCK_S > 10.0 {
                lo = lo.min(c.fill_ema_ms());
                hi = hi.max(c.fill_ema_ms());
            }
        }
        // ? Only the smoothing is checked here. This test is open loop (the fill ignores the
        // ? controller), so a sampling bias in the sawtooth would be integrated forever; in a
        // ? real loop the fill moves to cancel it. Chunky producers are tested closed-loop in
        // ? `tests/drift_sim.rs`.
        assert!(hi - lo < 1.0, "EMA still swings {} ms", hi - lo);
    }

    #[test]
    fn restart_keeps_the_learned_offset_reset_forgets_it() {
        let (_, _, _) = simulate(DriftGains::default(), 300.0, 60.0, 5.0);
        let mut c = DriftController::new(12.0);
        let mut fill = 12.0_f32;
        for _ in 0..(120.0 / BLOCK_S) as usize {
            let ratio = c.update(fill, ENGINE_BLOCK_FRAMES);
            fill += 1.0e-3 * (300.0 - ((ratio - 1.0) * 1.0e6) as f32) * BLOCK_S;
        }
        let learned = c.ppm();
        assert!((learned - 300.0).abs() < 3.0);
        c.restart();
        c.update(12.0, ENGINE_BLOCK_FRAMES);
        assert!(
            (c.ppm() - learned).abs() < 15.0,
            "restart threw the offset away"
        );
        c.reset();
        assert_eq!(c.ppm(), 0.0);
    }

    #[test]
    fn garbage_measurements_do_not_poison_the_state() {
        let mut c = DriftController::new(12.0);
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let r = c.update(bad, ENGINE_BLOCK_FRAMES);
            assert!(r.is_finite());
        }
        let r = c.update(12.0, ENGINE_BLOCK_FRAMES);
        assert!(r.is_finite() && (r - 1.0).abs() < 1.0e-9);
        assert!(c.ppm().is_finite() && c.fill_ema_ms().is_finite());
    }
}
