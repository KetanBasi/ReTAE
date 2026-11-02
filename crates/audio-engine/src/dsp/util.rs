//! Small numeric helpers: dB conversion, input sanitising, one-pole coefficients.

use audio_types::ENGINE_SAMPLE_RATE;

pub const SAMPLE_RATE: f32 = ENGINE_SAMPLE_RATE as f32;

/// Meter / detector floor. Anything quieter than this is reported as silence.
pub const SILENCE_DB: f32 = -120.0;
/// `10^(SILENCE_DB / 20)`.
pub const SILENCE_LIN: f32 = 1.0e-6;

/// Faders at or below this are treated as −∞ (exact zero gain).
pub const FADER_MIN_DB: f32 = -100.0;
pub const FADER_MAX_DB: f32 = 24.0;

#[inline]
pub fn db_to_lin(db: f32) -> f32 {
    // ? `exp` is cheaper than `powf(10, x)` and gives the same result.
    (db * (core::f32::consts::LN_10 / 20.0)).exp()
}

#[inline]
pub fn lin_to_db(lin: f32) -> f32 {
    if lin <= SILENCE_LIN {
        SILENCE_DB
    } else {
        20.0 * lin.log10()
    }
}

// SECTION: Fast log2 / exp2
// * Used only where ~1e-5 dB accuracy is plenty: the control-rate dynamics and the meters.
/// `20·log10(2)`: dB per octave of amplitude.
pub const DB_PER_LOG2: f32 = 6.020_6;
/// `1 / DB_PER_LOG2`.
pub const LOG2_PER_DB: f32 = 0.166_096_4;

/// `log2(x)` for positive normal `x`. Absolute error < 4e-6 (≈ 2.4e-5 dB): a few f32 ulps.
// ? Split `x = m · 2^e` with m in [√½, √2) by reading the float's bit pattern, then use the
// ? series ln(m) = 2·(s + s³/3 + s⁵/5 + s⁷/7) with s = (m-1)/(m+1), |s| ≤ 0.172. No fitted
// ? constants, and about 4x cheaper than libm's `log10`. Callers must pass x ≥ SILENCE_LIN.
#[inline]
pub fn fast_log2(x: f32) -> f32 {
    let bits = x.to_bits();
    let exp = ((bits >> 23) & 0xFF) as i32 - 127;
    let mant = f32::from_bits((bits & 0x007F_FFFF) | 0x3F80_0000); // [1, 2)
    let (m, e) = if mant > core::f32::consts::SQRT_2 {
        (mant * 0.5, exp + 1)
    } else {
        (mant, exp)
    };
    let s = (m - 1.0) / (m + 1.0);
    let s2 = s * s;
    let ln_m = 2.0 * s * (1.0 + s2 * (1.0 / 3.0 + s2 * (1.0 / 5.0 + s2 * (1.0 / 7.0))));
    e as f32 + ln_m * core::f32::consts::LOG2_E
}

/// `2^x`, relative error < 2e-5 (≈ 2e-4 dB). Input is clamped to ±125.
// ? Same idea in reverse: 2^x = 2^floor(x) · 2^frac(x). The integer part is built directly in
// ? the exponent bits of an f32; the fractional part is a 6th-order Taylor series.
#[inline]
pub fn fast_exp2(x: f32) -> f32 {
    let x = x.clamp(-125.0, 125.0);
    let n = x.floor();
    let t = (x - n) * core::f32::consts::LN_2; // [0, 0.693)
    let p = 1.0
        + t * (1.0
            + t * (0.5
                + t * (1.0 / 6.0 + t * (1.0 / 24.0 + t * (1.0 / 120.0 + t * (1.0 / 720.0))))));
    p * f32::from_bits(((n as i32 + 127) as u32) << 23)
}
// !SECTION: Fast log2 / exp2

/// Linear gain for a fader/send position. `≤ FADER_MIN_DB` is exact silence.
#[inline]
pub fn fader_to_lin(db: f32) -> f32 {
    if db <= FADER_MIN_DB {
        0.0
    } else {
        db_to_lin(db.min(FADER_MAX_DB))
    }
}

/// Returns `x` clamped into `[lo, hi]`, or `None` when it is NaN/infinite.
// !! Commands cross a thread boundary and may carry garbage. One NaN that reaches a filter
// !! state or a smoother would poison the output forever, so every float parameter goes
// !! through here first and a bad command is rejected instead of applied.
#[inline]
pub fn finite_clamp(x: f32, lo: f32, hi: f32) -> Option<f32> {
    if x.is_finite() {
        Some(x.clamp(lo, hi))
    } else {
        None
    }
}

/// Coefficient `k` of a one-pole smoother `y += k * (target - y)` that is stepped every
/// `step_samples` samples and has time constant `tau_ms`.
#[inline]
pub fn pole_coeff(tau_ms: f32, step_samples: f32) -> f32 {
    if tau_ms <= 0.0 {
        1.0
    } else {
        1.0 - (-step_samples / (SAMPLE_RATE * tau_ms * 0.001)).exp()
    }
}

/// Largest `|x|` in the slice.
// ? A plain `for` loop that keeps one running maximum is a serial dependency chain (each
// ? step waits for the previous one). Four independent lanes let the compiler turn this
// ? into SIMD `maxps`, and the final fold is only 4 values. Same idea in `peak_and_energy`.
#[inline]
pub fn peak_abs(x: &[f32]) -> f32 {
    let mut lanes = [0.0_f32; 4];
    let mut chunks = x.chunks_exact(4);
    for c in &mut chunks {
        for i in 0..4 {
            let a = c[i].abs();
            if a > lanes[i] {
                lanes[i] = a;
            }
        }
    }
    let mut p = lanes[0].max(lanes[1]).max(lanes[2].max(lanes[3]));
    for &v in chunks.remainder() {
        p = p.max(v.abs());
    }
    p
}

/// `(max |x|, Σ x²)` in one pass.
#[inline]
pub fn peak_and_energy(x: &[f32]) -> (f32, f32) {
    let mut pk = [0.0_f32; 4];
    let mut en = [0.0_f32; 4];
    let mut chunks = x.chunks_exact(4);
    for c in &mut chunks {
        for i in 0..4 {
            let a = c[i].abs();
            if a > pk[i] {
                pk[i] = a;
            }
            en[i] += c[i] * c[i];
        }
    }
    let mut p = pk[0].max(pk[1]).max(pk[2].max(pk[3]));
    let mut e = (en[0] + en[1]) + (en[2] + en[3]);
    for &v in chunks.remainder() {
        p = p.max(v.abs());
        e += v * v;
    }
    (p, e)
}

/// Flush tiny values to exactly zero so recursive filters never reach denormals, and
/// replace NaN/infinity so a poisoned filter state heals itself.
// ? Denormal floats can make x86 arithmetic 50-100x slower. A decaying filter tail or
// ? envelope walks straight into that range, so we snap it to zero once it is inaudible.
#[inline]
pub fn flush_tiny(x: f32) -> f32 {
    if x.is_nan() || x.is_infinite() || x.abs() < 1.0e-15 {
        0.0
    } else {
        x
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_roundtrip() {
        for db in [-60.0_f32, -20.0, -6.0, 0.0, 6.0, 12.0] {
            assert!((lin_to_db(db_to_lin(db)) - db).abs() < 1.0e-3, "{db}");
        }
        assert!((db_to_lin(-6.0206) - 0.5).abs() < 1.0e-4);
        assert_eq!(lin_to_db(0.0), SILENCE_DB);
    }

    #[test]
    fn fader_floor_is_exact_zero() {
        assert_eq!(fader_to_lin(-100.0), 0.0);
        assert_eq!(fader_to_lin(-300.0), 0.0);
        assert!((fader_to_lin(0.0) - 1.0).abs() < 1.0e-6);
        // ? Above the max it saturates instead of exploding.
        assert!((fader_to_lin(500.0) - db_to_lin(FADER_MAX_DB)).abs() < 1.0e-3);
    }

    #[test]
    fn reductions_match_naive_for_every_length() {
        // Lengths around the 4-lane boundary exercise both the SIMD body and the remainder.
        for len in 0..40 {
            let x: Vec<f32> = (0..len)
                .map(|i| ((i * 37 % 23) as f32 - 11.0) * 0.07)
                .collect();
            let naive_peak = x.iter().fold(0.0_f32, |m, v| m.max(v.abs()));
            let naive_e: f32 = x.iter().map(|v| v * v).sum();
            assert_eq!(peak_abs(&x), naive_peak, "len {len}");
            let (p, e) = peak_and_energy(&x);
            assert_eq!(p, naive_peak, "len {len}");
            assert!(
                (e - naive_e).abs() <= 1.0e-5 * naive_e.max(1.0),
                "len {len}"
            );
        }
    }

    #[test]
    fn fast_log2_and_exp2_track_libm() {
        // Sweep ~28 octaves, 10 000 log-spaced points.
        let mut worst_log = 0.0_f32;
        let mut worst_exp = 0.0_f32;
        for i in 0..10_000 {
            let x = SILENCE_LIN * 10f32.powf(i as f32 * 9.0 / 10_000.0); // 1e-6 .. 1e3
            worst_log = worst_log.max((fast_log2(x) - x.log2()).abs());
            let y = -20.0 + i as f32 * 0.004; // exp2 over -20 .. +20
            worst_exp = worst_exp.max((fast_exp2(y) / y.exp2() - 1.0).abs());
        }
        // ? Results reach |log2| ≈ 10, where one f32 ulp is already 9.5e-7, so a few ulps
        // ? (4e-6 = 2.4e-5 dB) is the best any f32 implementation, libm included, can show.
        assert!(worst_log < 4.0e-6, "log2 error {worst_log}");
        assert!(worst_exp < 2.0e-5, "exp2 error {worst_exp}");
        // Exact powers of two and the clamp.
        assert_eq!(fast_log2(1.0), 0.0);
        assert!((fast_log2(8.0) - 3.0).abs() < 1.0e-6);
        assert!((fast_exp2(0.0) - 1.0).abs() < 1.0e-6);
        assert!(fast_exp2(1.0e9).is_finite() && fast_exp2(-1.0e9) > 0.0);
        // dB constants agree with each other.
        assert!((DB_PER_LOG2 * LOG2_PER_DB - 1.0).abs() < 1.0e-5);
        assert!((DB_PER_LOG2 - 20.0 * 2f32.log10()).abs() < 1.0e-4);
    }

    #[test]
    fn flush_tiny_heals_poisoned_state() {
        assert_eq!(flush_tiny(f32::NAN), 0.0);
        assert_eq!(flush_tiny(f32::INFINITY), 0.0);
        assert_eq!(flush_tiny(f32::NEG_INFINITY), 0.0);
        assert_eq!(flush_tiny(1.0e-20), 0.0);
        assert_eq!(flush_tiny(-1.0e-20), 0.0);
        assert_eq!(flush_tiny(0.25), 0.25);
        assert_eq!(flush_tiny(-3.0e30), -3.0e30);
    }

    #[test]
    fn finite_clamp_rejects_garbage() {
        assert_eq!(finite_clamp(f32::NAN, -1.0, 1.0), None);
        assert_eq!(finite_clamp(f32::INFINITY, -1.0, 1.0), None);
        assert_eq!(finite_clamp(5.0, -1.0, 1.0), Some(1.0));
        assert_eq!(finite_clamp(-5.0, -1.0, 1.0), Some(-1.0));
    }

    #[test]
    fn pole_coeff_is_sane() {
        assert_eq!(pole_coeff(0.0, 16.0), 1.0);
        // 16-sample steps divide 5 ms (240 samples) exactly: 15 steps = one time constant.
        let k = pole_coeff(5.0, 16.0);
        assert!(k > 0.0 && k < 1.0);
        let mut y = 0.0_f32;
        for _ in 0..15 {
            y += k * (1.0 - y);
        }
        // After one time constant a one-pole reaches 1 - 1/e = 63.2 %.
        assert!((y - 0.632).abs() < 0.002, "{y}");
    }
}
