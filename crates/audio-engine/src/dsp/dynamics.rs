//! Noise gate and feed-forward compressor (PLAN.md §7 strip chain: gate -> EQ -> comp).
//!
//! * Both detectors work at *control rate*: one decision per `CTRL_STRIDE` (16) samples,
//!   0.33 ms at 48 kHz. The audio is then multiplied by a linear gain ramp between two
//!   consecutive decisions. This keeps `log10`/`exp` (the expensive part) at 8 calls per
//!   block per effect instead of 128, which is what makes the 16x16 bench budget reachable.
//! * Detectors are *linked*: the loudest channel drives one gain for all channels, so a
//!   stereo image never shifts when only one side is loud.

use super::Planar;
use super::smooth::apply_gain_ramp;
use super::util::{
    DB_PER_LOG2, LOG2_PER_DB, SAMPLE_RATE, SILENCE_LIN, db_to_lin, fast_exp2, fast_log2, peak_abs,
    pole_coeff,
};
use audio_types::{Compressor, Gate};

/// Samples per detector decision.
pub const CTRL_STRIDE: usize = 16;

/// The gate closes this far *below* its open threshold (hysteresis, in dB).
pub const GATE_HYSTERESIS_DB: f32 = 4.0;
/// The gate stays open this long after the level last exceeded the close threshold.
pub const GATE_HOLD_MS: f32 = 20.0;
/// Depth of a closed gate. A fully closed gate outputs exact silence.
pub const GATE_RANGE_DB: f32 = 80.0;

/// Peak `|x|` over `[start, end)` on the first `channels` rows.
#[inline]
fn peak(buf: &Planar, channels: usize, start: usize, end: usize) -> f32 {
    let mut p = 0.0_f32;
    for row in buf.iter().take(channels) {
        p = p.max(peak_abs(&row[start..end]));
    }
    p
}

// SECTION: Gate
#[derive(Clone, Copy, Debug)]
pub struct GateState {
    open: bool,
    hold: u32,
    /// Gate attenuation, 0 dB (open) down to `-GATE_RANGE_DB` (closed).
    gain_db: f32,
}

impl Default for GateState {
    fn default() -> Self {
        Self::new()
    }
}

impl GateState {
    // * Starts open at unity so enabling the gate on a live signal does not click.
    pub const fn new() -> Self {
        Self {
            open: true,
            hold: 0,
            gain_db: 0.0,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Current attenuation in dB (0 = open, `-GATE_RANGE_DB` = closed).
    pub fn gain_db(&self) -> f32 {
        self.gain_db
    }

    // ? Thresholds are compared as linear amplitudes, so the gate never needs a `log` at all.
    #[inline]
    fn decide(&mut self, level: f32, open_level: f32, close_level: f32, hold_steps: u32) {
        if level >= open_level {
            self.open = true;
            self.hold = hold_steps;
        } else if self.open {
            if level >= close_level {
                // Inside the hysteresis band: stay open and keep the hold timer fresh.
                self.hold = hold_steps;
            } else if self.hold > 0 {
                self.hold -= 1;
            } else {
                self.open = false;
            }
        }
        // ? A closed gate only re-opens at `open_level`. Levels between `close_level` and
        // ? `open_level` change nothing, which is exactly the hysteresis.
    }

    pub fn process(&mut self, p: &Gate, buf: &mut Planar, channels: usize, frames: usize) {
        let step_s = CTRL_STRIDE as f32 / SAMPLE_RATE;
        // ? The gate moves linearly in dB: `attack_ms` / `release_ms` are the time for the
        // ? FULL 0 <-> -80 dB travel. That makes "closed" a definite state (exact silence)
        // ? rather than an exponential that never quite arrives.
        let open_slew = GATE_RANGE_DB * step_s / (p.attack_ms.max(0.01) * 0.001);
        let close_slew = GATE_RANGE_DB * step_s / (p.release_ms.max(0.01) * 0.001);
        let hold_steps = (GATE_HOLD_MS * 0.001 / step_s).ceil() as u32;
        let open_level = db_to_lin(p.threshold_db);
        let close_level = db_to_lin(p.threshold_db - GATE_HYSTERESIS_DB);

        let mut start = 0;
        while start < frames {
            let end = (start + CTRL_STRIDE).min(frames);
            self.decide(
                peak(buf, channels, start, end),
                open_level,
                close_level,
                hold_steps,
            );

            let g0 = gate_lin(self.gain_db);
            self.gain_db = if self.open {
                (self.gain_db + open_slew).min(0.0)
            } else {
                (self.gain_db - close_slew).max(-GATE_RANGE_DB)
            };
            let g1 = gate_lin(self.gain_db);
            if g0 != 1.0 || g1 != 1.0 {
                for row in buf.iter_mut().take(channels) {
                    apply_gain_ramp(&mut row[start..end], g0, g1);
                }
            }
            start = end;
        }
    }
}

#[inline]
fn gate_lin(gain_db: f32) -> f32 {
    if gain_db <= -GATE_RANGE_DB {
        0.0
    } else if gain_db >= 0.0 {
        1.0
    } else {
        db_to_lin(gain_db)
    }
}
// !SECTION: Gate

// SECTION: Compressor
/// Feed-forward, hard-knee, peak-detecting compressor.
/// ? Gain reduction (GR) is smoothed in the dB domain *after* the static curve (the
/// ? "log-domain, decoupled" topology), so attack/release behave identically at any level.
#[derive(Clone, Copy, Debug)]
pub struct CompState {
    /// Current gain reduction in dB (>= 0).
    gr_db: f32,
    /// Linear gain applied at the end of the previous control step.
    gain: f32,
}

impl Default for CompState {
    fn default() -> Self {
        Self::new()
    }
}

impl CompState {
    pub const fn new() -> Self {
        Self {
            gr_db: 0.0,
            gain: 1.0,
        }
    }

    /// Current gain reduction in dB (for a future GR meter).
    pub fn gain_reduction_db(&self) -> f32 {
        self.gr_db
    }

    pub fn process(&mut self, p: &Compressor, buf: &mut Planar, channels: usize, frames: usize) {
        let attack_k = pole_coeff(p.attack_ms, CTRL_STRIDE as f32);
        let release_k = pole_coeff(p.release_ms, CTRL_STRIDE as f32);
        let slope = 1.0 - 1.0 / p.ratio.max(1.0);

        let mut start = 0;
        while start < frames {
            let end = (start + CTRL_STRIDE).min(frames);
            // ? Level in dB via the cheap log2 (`peak` is floored so log2 never sees 0).
            let level_db =
                DB_PER_LOG2 * fast_log2(peak(buf, channels, start, end).max(SILENCE_LIN));
            let over = level_db - p.threshold_db;
            let target_gr = if over > 0.0 { over * slope } else { 0.0 };

            let k = if target_gr > self.gr_db {
                attack_k
            } else {
                release_k
            };
            self.gr_db += k * (target_gr - self.gr_db);
            if target_gr == 0.0 && self.gr_db < 1.0e-3 {
                self.gr_db = 0.0;
            }

            let g0 = self.gain;
            let g1 = fast_exp2((p.makeup_db - self.gr_db) * LOG2_PER_DB);
            self.gain = g1;
            if g0 != 1.0 || g1 != 1.0 {
                for row in buf.iter_mut().take(channels) {
                    apply_gain_ramp(&mut row[start..end], g0, g1);
                }
            }
            start = end;
        }
    }
}
// !SECTION: Compressor

#[cfg(test)]
mod tests {
    use super::super::util::lin_to_db;
    use super::*;
    use audio_types::ENGINE_BLOCK_FRAMES;

    /// Constant-magnitude AC signal: |x| == amp for every sample (so detector level is exact).
    fn square(amp: f32, channels: usize, start: usize) -> Planar {
        let mut b = [[0.0; ENGINE_BLOCK_FRAMES]; audio_types::MAX_CHANNELS];
        for row in b.iter_mut().take(channels) {
            for (i, v) in row.iter_mut().enumerate() {
                *v = if ((start + i) / 24) % 2 == 0 {
                    amp
                } else {
                    -amp
                };
            }
        }
        b
    }

    fn sine(amp: f32, freq: f32, start: usize) -> Planar {
        let mut b = [[0.0; ENGINE_BLOCK_FRAMES]; audio_types::MAX_CHANNELS];
        for row in b.iter_mut().take(2) {
            for (i, v) in row.iter_mut().enumerate() {
                let t = (start + i) as f32 / SAMPLE_RATE;
                *v = amp * (core::f32::consts::TAU * freq * t).sin();
            }
        }
        b
    }

    fn blocks_for(ms: f32) -> usize {
        (ms * 0.001 * SAMPLE_RATE / ENGINE_BLOCK_FRAMES as f32).ceil() as usize
    }

    fn peak_db(b: &Planar) -> f32 {
        lin_to_db(peak(b, 2, 0, ENGINE_BLOCK_FRAMES))
    }

    // ---- compressor ---------------------------------------------------------------

    fn comp_settled_out_db(p: &Compressor, in_db: f32) -> f32 {
        let mut s = CompState::new();
        let amp = db_to_lin(in_db);
        let mut out = 0.0;
        for blk in 0..blocks_for(1500.0) {
            let mut b = square(amp, 2, blk * ENGINE_BLOCK_FRAMES);
            s.process(p, &mut b, 2, ENGINE_BLOCK_FRAMES);
            out = peak_db(&b);
        }
        out
    }

    #[test]
    fn compressor_static_curve() {
        let p = Compressor {
            enabled: true,
            threshold_db: -20.0,
            ratio: 4.0,
            attack_ms: 5.0,
            release_ms: 100.0,
            makeup_db: 0.0,
        };
        // (input dB, expected output dB): below/at threshold = unity, above = thr + (x-thr)/ratio
        for (x, want) in [
            (-60.0, -60.0),
            (-40.0, -40.0),
            (-20.0, -20.0),
            (-10.0, -17.5),
            (0.0, -15.0),
        ] {
            let got = comp_settled_out_db(&p, x);
            assert!(
                (got - want).abs() < 0.1,
                "in {x} dB: got {got:.3}, want {want:.3}"
            );
        }
    }

    #[test]
    fn compressor_makeup_and_extreme_ratio() {
        let mut p = Compressor {
            enabled: true,
            threshold_db: -20.0,
            ratio: 100.0,
            attack_ms: 1.0,
            release_ms: 50.0,
            makeup_db: 6.0,
        };
        // ratio 100 is a near-limiter: -20 + 10/100 = -19.9, plus 6 dB makeup.
        let got = comp_settled_out_db(&p, -10.0);
        assert!((got - (-19.9 + 6.0)).abs() < 0.1, "{got}");
        // Below threshold only the makeup applies.
        p.ratio = 4.0;
        let got = comp_settled_out_db(&p, -50.0);
        assert!((got - (-50.0 + 6.0)).abs() < 0.1, "{got}");
        // ratio < 1 is treated as 1 (no expansion, no NaN).
        p.ratio = 0.2;
        p.makeup_db = 0.0;
        let got = comp_settled_out_db(&p, -10.0);
        assert!((got - -10.0).abs() < 0.1, "{got}");
    }

    #[test]
    fn compressor_attack_and_release_time_constants() {
        let p = Compressor {
            enabled: true,
            threshold_db: -20.0,
            ratio: 4.0,
            attack_ms: 5.0,
            release_ms: 100.0,
            makeup_db: 0.0,
        };
        let loud = db_to_lin(-10.0); // final GR = 10 dB over * 0.75 = 7.5 dB
        let mut s = CompState::new();
        // Attack: GR after one attack time constant is 1 - 1/e = 63.2 % of the final value.
        let mut gr_at_attack = 0.0;
        for blk in 0..blocks_for(60.0) {
            let mut b = square(loud, 2, blk * ENGINE_BLOCK_FRAMES);
            let steps = ENGINE_BLOCK_FRAMES / CTRL_STRIDE;
            for step in 0..steps {
                // Re-run one control step at a time to sample GR at exactly 5 ms.
                let lo = step * CTRL_STRIDE;
                let mut one = [[0.0; ENGINE_BLOCK_FRAMES]; audio_types::MAX_CHANNELS];
                for ch in 0..2 {
                    one[ch][..CTRL_STRIDE].copy_from_slice(&b[ch][lo..lo + CTRL_STRIDE]);
                }
                s.process(&p, &mut one, 2, CTRL_STRIDE);
                let t_ms = ((blk * steps + step + 1) * CTRL_STRIDE) as f32 / SAMPLE_RATE * 1000.0;
                if (t_ms - 5.0).abs() < 0.17 {
                    gr_at_attack = s.gain_reduction_db();
                }
            }
            let _ = &mut b;
        }
        let want = 7.5 * (1.0 - (-1.0_f32).exp());
        assert!(
            (gr_at_attack - want).abs() < 0.2,
            "GR@5ms = {gr_at_attack:.2} dB, want {want:.2}"
        );
        assert!((s.gain_reduction_db() - 7.5).abs() < 0.05);

        // Release: feed silence, GR falls to 1/e of 7.5 dB after the release time.
        let mut gr_at_release = 0.0;
        let silence = [[0.0; ENGINE_BLOCK_FRAMES]; audio_types::MAX_CHANNELS];
        for blk in 0..blocks_for(150.0) {
            let mut b = silence;
            s.process(&p, &mut b, 2, ENGINE_BLOCK_FRAMES);
            let t_ms = ((blk + 1) * ENGINE_BLOCK_FRAMES) as f32 / SAMPLE_RATE * 1000.0;
            if (t_ms - 98.7).abs() < 1.4 {
                gr_at_release = s.gain_reduction_db();
            }
        }
        let want = 7.5 * (-1.0_f32).exp();
        assert!(
            (gr_at_release - want).abs() < 0.3,
            "GR@100ms = {gr_at_release:.2}, want {want:.2}"
        );
    }

    #[test]
    fn compressor_links_channels() {
        // Loud left, quiet right: both get the same gain reduction (no image shift).
        let p = Compressor {
            enabled: true,
            threshold_db: -20.0,
            ratio: 4.0,
            attack_ms: 1.0,
            release_ms: 50.0,
            makeup_db: 0.0,
        };
        let mut s = CompState::new();
        let (l, r) = (db_to_lin(-6.0), db_to_lin(-36.0));
        let mut ratio = 0.0;
        for blk in 0..blocks_for(500.0) {
            let mut b = square(l, 2, blk * ENGINE_BLOCK_FRAMES);
            for (i, v) in b[1].iter_mut().enumerate() {
                *v = if ((blk * ENGINE_BLOCK_FRAMES + i) / 24) % 2 == 0 {
                    r
                } else {
                    -r
                };
            }
            s.process(&p, &mut b, 2, ENGINE_BLOCK_FRAMES);
            ratio = b[0][0].abs() / l / (b[1][0].abs() / r);
        }
        assert!((ratio - 1.0).abs() < 1.0e-3, "{ratio}");
    }

    // ---- gate ---------------------------------------------------------------------

    fn gate_params() -> Gate {
        Gate {
            enabled: true,
            threshold_db: -30.0,
            attack_ms: 2.0,
            release_ms: 80.0,
        }
    }

    /// Feed a 500 Hz sine at `level_db` for `ms`, return (in-peak dB, out-peak dB) of the last block.
    fn feed(g: &mut GateState, p: &Gate, level_db: f32, ms: f32, t0: &mut usize) -> (f32, f32) {
        let amp = db_to_lin(level_db);
        let mut last = (0.0, 0.0);
        for _ in 0..blocks_for(ms) {
            let mut b = sine(amp, 500.0, *t0);
            let dry = peak_db(&b);
            g.process(p, &mut b, 2, ENGINE_BLOCK_FRAMES);
            last = (dry, peak_db(&b));
            *t0 += ENGINE_BLOCK_FRAMES;
        }
        last
    }

    #[test]
    fn gate_closes_below_threshold_and_is_exactly_silent() {
        let p = gate_params();
        let mut g = GateState::new();
        let mut t = 0;
        let (_, out) = feed(&mut g, &p, -50.0, 300.0, &mut t);
        assert!(!g.is_open());
        assert_eq!(
            out,
            super::super::util::SILENCE_DB,
            "closed gate must output exact silence"
        );
    }

    #[test]
    fn gate_opens_above_threshold_within_attack() {
        let p = gate_params();
        let mut g = GateState::new();
        let mut t = 0;
        feed(&mut g, &p, -50.0, 300.0, &mut t);
        assert!(!g.is_open());
        let (dry, out) = feed(&mut g, &p, -20.0, 10.0, &mut t);
        assert!(g.is_open());
        assert!((dry - out).abs() < 0.1, "dry {dry:.2} out {out:.2}");
    }

    #[test]
    fn gate_hysteresis() {
        let p = gate_params(); // open at -30 dB, close below -34 dB
        let mut g = GateState::new();
        let mut t = 0;
        feed(&mut g, &p, -50.0, 300.0, &mut t);
        feed(&mut g, &p, -20.0, 50.0, &mut t);
        assert!(g.is_open());

        // 1) Open gate, level drops into the hysteresis band (-32 dB): must STAY open.
        let (dry, out) = feed(&mut g, &p, -32.0, 600.0, &mut t);
        assert!(g.is_open(), "gate chattered inside the hysteresis band");
        assert!((dry - out).abs() < 0.1);

        // 2) Level falls below the close threshold (-36 dB): closes.
        let (_, out) = feed(&mut g, &p, -36.0, 400.0, &mut t);
        assert!(!g.is_open());
        assert_eq!(out, super::super::util::SILENCE_DB);

        // 3) Closed gate, level rises into the band (-32 dB): must STAY closed.
        let (_, out) = feed(&mut g, &p, -32.0, 300.0, &mut t);
        assert!(!g.is_open(), "closed gate opened below its open threshold");
        assert_eq!(out, super::super::util::SILENCE_DB);

        // 4) Above the open threshold: opens again.
        feed(&mut g, &p, -20.0, 50.0, &mut t);
        assert!(g.is_open());
    }

    #[test]
    fn gate_release_takes_the_configured_time() {
        let p = gate_params(); // release 80 ms for the full travel, plus 20 ms hold
        let mut g = GateState::new();
        let mut t = 0;
        feed(&mut g, &p, -20.0, 50.0, &mut t);
        // Signal disappears. After hold (20 ms) + release (80 ms) the gate is fully shut,
        // but 60 ms in it must still be passing signal.
        let sixty = feed(&mut g, &p, -80.0, 60.0, &mut t);
        assert!(
            g.gain_db() > -GATE_RANGE_DB,
            "closed too early: {}",
            g.gain_db()
        );
        let _ = sixty;
        feed(&mut g, &p, -80.0, 60.0, &mut t);
        assert_eq!(g.gain_db(), -GATE_RANGE_DB);
    }
}
