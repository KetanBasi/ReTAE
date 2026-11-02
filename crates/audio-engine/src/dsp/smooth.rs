//! Block-rate parameter smoothing (PLAN.md §7: one-pole on gain/pan, 5 ms).
//!
//! * The smoother steps once per block and the audio is multiplied by a *linear ramp*
//!   between the old and the new value. That is one `exp` per block instead of one per
//!   sample, and it is still click-free because the ramp has no discontinuity.

/// Time constant for gain / pan / mute smoothing.
pub const PARAM_SMOOTH_MS: f32 = 5.0;

/// Below this distance a smoother jumps straight to its target.
// ? Without the snap, a fader ramping to 0 would decay exponentially into denormals and
// ? never become "settled", so the fast path below would never trigger.
const SNAP: f32 = 1.0e-5;

#[derive(Clone, Copy, Debug)]
pub struct Ramp {
    current: f32,
    target: f32,
}

impl Ramp {
    pub const fn new(value: f32) -> Self {
        Self {
            current: value,
            target: value,
        }
    }

    #[inline]
    pub fn set_target(&mut self, target: f32) {
        self.target = target;
    }

    #[inline]
    pub fn target(&self) -> f32 {
        self.target
    }

    #[inline]
    pub fn current(&self) -> f32 {
        self.current
    }

    /// Jump to the target without ramping (used when a strip is (re)attached).
    #[inline]
    pub fn snap(&mut self) {
        self.current = self.target;
    }

    /// True when both the current value and the target are exactly zero.
    #[inline]
    pub fn is_silent(&self) -> bool {
        self.current == 0.0 && self.target == 0.0
    }

    /// Advance one block. Returns `(start, end)` for a linear ramp across the block.
    #[inline]
    pub fn advance(&mut self, k: f32) -> (f32, f32) {
        let start = self.current;
        if start != self.target {
            let next = start + k * (self.target - start);
            self.current = if (self.target - next).abs() < SNAP {
                self.target
            } else {
                next
            };
        }
        (start, self.current)
    }
}

/// Ramp positions `(i + 1) / 16` for the 16-sample control steps of the dynamics.
// ? A table replaces "convert i to float, add one, multiply" per sample with a plain load.
const RAMP16: [f32; 16] = {
    let mut t = [0.0; 16];
    let mut i = 0;
    while i < 16 {
        t[i] = (i + 1) as f32 / 16.0;
        i += 1;
    }
    t
};

/// `buf *= gain`, with the gain moving linearly from `g0` to `g1` across the slice.
#[inline]
pub fn apply_gain_ramp(buf: &mut [f32], g0: f32, g1: f32) {
    if g0 == g1 {
        if g0 != 1.0 {
            for x in buf.iter_mut() {
                *x *= g0;
            }
        }
        return;
    }
    if let Ok(chunk) = <&mut [f32; 16]>::try_from(&mut *buf) {
        let dg = g1 - g0;
        for (x, r) in chunk.iter_mut().zip(RAMP16.iter()) {
            *x *= g0 + dg * r;
        }
        return;
    }
    let step = (g1 - g0) / buf.len() as f32;
    for (i, x) in buf.iter_mut().enumerate() {
        *x *= g0 + step * (i as f32 + 1.0);
    }
}

/// `dst += src * gain`, with the gain moving linearly from `g0` to `g1`.
#[inline]
pub fn mix_ramp(dst: &mut [f32], src: &[f32], g0: f32, g1: f32) {
    let n = dst.len().min(src.len());
    let (dst, src) = (&mut dst[..n], &src[..n]);
    if g0 == g1 {
        if g0 != 0.0 {
            for (d, s) in dst.iter_mut().zip(src) {
                *d += *s * g0;
            }
        }
        return;
    }
    let step = (g1 - g0) / n as f32;
    for (i, (d, s)) in dst.iter_mut().zip(src).enumerate() {
        *d += *s * (g0 + step * (i as f32 + 1.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramp_converges_and_settles_exactly() {
        let k = 0.4;
        let mut r = Ramp::new(1.0);
        r.set_target(0.0);
        let mut blocks = 0;
        while r.current() != 0.0 {
            r.advance(k);
            blocks += 1;
            assert!(blocks < 100, "never settled");
        }
        assert!(r.is_silent());
        assert_eq!(r.advance(k), (0.0, 0.0));
    }

    #[test]
    fn gain_ramp_is_linear_and_hits_end_value() {
        let mut buf = [1.0_f32; 8];
        apply_gain_ramp(&mut buf, 0.0, 1.0);
        for (i, v) in buf.iter().enumerate() {
            assert!((v - (i as f32 + 1.0) / 8.0).abs() < 1.0e-6);
        }
        assert!((buf[7] - 1.0).abs() < 1.0e-6);
    }

    #[test]
    fn sixteen_sample_table_path_matches_the_generic_formula() {
        let mut fast = [1.0_f32; 16];
        apply_gain_ramp(&mut fast, 0.25, 1.75);
        for (i, v) in fast.iter().enumerate() {
            let want = 0.25 + (1.75 - 0.25) * (i as f32 + 1.0) / 16.0;
            assert!((v - want).abs() < 1.0e-6, "sample {i}: {v} vs {want}");
        }
        // Same through a 15-sample slice (generic path) so both branches stay in sync.
        let mut generic = [1.0_f32; 15];
        apply_gain_ramp(&mut generic, 0.25, 1.75);
        assert!((generic[14] - 1.75).abs() < 1.0e-6);
    }

    #[test]
    fn mix_ramp_accumulates() {
        let mut dst = [0.5_f32; 4];
        mix_ramp(&mut dst, &[1.0; 4], 2.0, 2.0);
        assert_eq!(dst, [2.5; 4]);
        // Zero gain on both ends leaves dst untouched.
        mix_ramp(&mut dst, &[9.0; 4], 0.0, 0.0);
        assert_eq!(dst, [2.5; 4]);
    }
}
