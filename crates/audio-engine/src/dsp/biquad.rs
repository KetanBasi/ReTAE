//! RBJ-cookbook biquads and the 3-band strip/bus EQ (PLAN.md §7).
//!
//! * Band convention (the `audio-types` contract only has `[EqBand; 3]`):
//!   band 0 = low shelf, band 1 = peaking, band 2 = high shelf. UIs label them accordingly.

use super::Planar;
use super::util::{SAMPLE_RATE, flush_tiny};
use audio_types::{ENGINE_BLOCK_FRAMES, EqBand, MAX_CHANNELS};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BandKind {
    LowShelf,
    Peaking,
    HighShelf,
}

pub const BAND_KINDS: [BandKind; 3] = [BandKind::LowShelf, BandKind::Peaking, BandKind::HighShelf];

// SECTION: Coefficients
/// Normalised biquad: `H(z) = (b0 + b1 z⁻¹ + b2 z⁻²) / (1 + a1 z⁻¹ + a2 z⁻²)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Coeffs {
    pub b0: f32,
    pub b1: f32,
    pub b2: f32,
    pub a1: f32,
    pub a2: f32,
}

impl Coeffs {
    pub const IDENTITY: Self = Self {
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
        a1: 0.0,
        a2: 0.0,
    };

    /// RBJ design. Inputs are clamped to a stable range, so any `f32` gives a stable filter.
    // ? Maths is done in f64 and rounded to f32 at the end: low-frequency shelves at 48 kHz
    // ? have poles very close to z = 1, where f32 intermediate rounding would visibly
    // ? shift the corner frequency.
    pub fn design(kind: BandKind, freq_hz: f32, gain_db: f32, q: f32) -> Self {
        let fs = f64::from(SAMPLE_RATE);
        let f0 = f64::from(freq_hz).clamp(20.0, 0.45 * fs);
        let gain_db = f64::from(gain_db).clamp(-24.0, 24.0);
        let q = f64::from(q).clamp(0.1, 18.0);

        let a = 10f64.powf(gain_db / 40.0);
        let w0 = core::f64::consts::TAU * f0 / fs;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * q);

        let (b0, b1, b2, a0, a1, a2) = match kind {
            BandKind::Peaking => (
                1.0 + alpha * a,
                -2.0 * cos_w0,
                1.0 - alpha * a,
                1.0 + alpha / a,
                -2.0 * cos_w0,
                1.0 - alpha / a,
            ),
            BandKind::LowShelf => {
                let t = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) - (a - 1.0) * cos_w0 + t),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cos_w0),
                    a * ((a + 1.0) - (a - 1.0) * cos_w0 - t),
                    (a + 1.0) + (a - 1.0) * cos_w0 + t,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cos_w0),
                    (a + 1.0) + (a - 1.0) * cos_w0 - t,
                )
            }
            BandKind::HighShelf => {
                let t = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) + (a - 1.0) * cos_w0 + t),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w0),
                    a * ((a + 1.0) + (a - 1.0) * cos_w0 - t),
                    (a + 1.0) - (a - 1.0) * cos_w0 + t,
                    2.0 * ((a - 1.0) - (a + 1.0) * cos_w0),
                    (a + 1.0) - (a - 1.0) * cos_w0 - t,
                )
            }
        };

        Self {
            b0: (b0 / a0) as f32,
            b1: (b1 / a0) as f32,
            b2: (b2 / a0) as f32,
            a1: (a1 / a0) as f32,
            a2: (a2 / a0) as f32,
        }
    }

    /// Magnitude response in dB at `freq_hz` (for tests and for UI response curves).
    pub fn magnitude_db(&self, freq_hz: f32) -> f32 {
        let w = core::f64::consts::TAU * f64::from(freq_hz) / f64::from(SAMPLE_RATE);
        let (s1, c1) = w.sin_cos();
        let (s2, c2) = (2.0 * w).sin_cos();
        // z⁻¹ = e^{-jw} = cos w - j sin w
        let num_re = f64::from(self.b0) + f64::from(self.b1) * c1 + f64::from(self.b2) * c2;
        let num_im = -f64::from(self.b1) * s1 - f64::from(self.b2) * s2;
        let den_re = 1.0 + f64::from(self.a1) * c1 + f64::from(self.a2) * c2;
        let den_im = -f64::from(self.a1) * s1 - f64::from(self.a2) * s2;
        let mag2 = (num_re * num_re + num_im * num_im) / (den_re * den_re + den_im * den_im);
        (10.0 * mag2.log10()) as f32
    }
}
// !SECTION: Coefficients

// SECTION: Filter state
/// Transposed direct-form II state (best numerics for f32).
#[derive(Clone, Copy, Debug, Default)]
pub struct State {
    z1: f32,
    z2: f32,
}

#[inline(always)]
fn tick(c: &Coeffs, s: &mut State, x: f32) -> f32 {
    let y = c.b0 * x + s.z1;
    // ? Parenthesised so the loop-carried dependency (through `y`) is as short as possible.
    s.z1 = (c.b1 * x + s.z2) - c.a1 * y;
    s.z2 = c.b2 * x - c.a2 * y;
    y
}
// !SECTION: Filter state

// SECTION: 3-band EQ chain
/// Three RBJ bands in series, one independent state per channel.
#[derive(Clone, Debug)]
pub struct EqChain {
    coeffs: [Coeffs; 3],
    on: [bool; 3],
    // * Indexed [channel][band] (not by position in the active list) so enabling one band
    // * never swaps the state of another.
    state: [[State; 3]; MAX_CHANNELS],
}

impl Default for EqChain {
    fn default() -> Self {
        Self::new()
    }
}

impl EqChain {
    pub const fn new() -> Self {
        Self {
            coeffs: [Coeffs::IDENTITY; 3],
            on: [false; 3],
            state: [[State { z1: 0.0, z2: 0.0 }; 3]; MAX_CHANNELS],
        }
    }

    /// Apply new band settings. Filter state is kept, so tweaking a knob never clicks.
    pub fn set_bands(&mut self, bands: &[EqBand; 3]) {
        for (i, band) in bands.iter().enumerate() {
            // ? A band at 0 dB is a perfect pass-through, so it is skipped entirely.
            let active = band.enabled && band.gain_db.abs() >= 0.01;
            if active {
                self.coeffs[i] = Coeffs::design(BAND_KINDS[i], band.freq_hz, band.gain_db, band.q);
            } else if self.on[i] {
                // Leaving the chain: clear stale state so a later re-enable starts clean.
                for ch in self.state.iter_mut() {
                    ch[i] = State::default();
                }
            }
            self.on[i] = active;
        }
    }

    pub fn is_bypassed(&self) -> bool {
        !self.on.iter().any(|&b| b)
    }

    pub fn reset(&mut self) {
        self.state = [[State::default(); 3]; MAX_CHANNELS];
    }

    /// Indices of the active bands (compacted) and how many there are.
    fn active(&self) -> ([usize; 3], usize) {
        let mut idx = [0usize; 3];
        let mut n = 0;
        for (b, &on) in self.on.iter().enumerate() {
            if on {
                idx[n] = b;
                n += 1;
            }
        }
        (idx, n)
    }

    pub fn process(&mut self, buf: &mut Planar, channels: usize, frames: usize) {
        let (idx, n) = self.active();
        let channels = channels.min(MAX_CHANNELS);
        match n {
            0 => {}
            1 => self.run::<1>(idx, buf, channels, frames),
            2 => self.run::<2>(idx, buf, channels, frames),
            _ => self.run::<3>(idx, buf, channels, frames),
        }
    }

    // ? `B` is the number of active bands. A const generic lets the compiler fully unroll the
    // ? band loop, so the per-sample code is straight-line arithmetic with no branches.
    // ? Channels are processed in pairs inside ONE loop: a biquad is a recursive filter (each
    // ? sample waits for the previous one), so two independent channels in the same loop
    // ? fill the CPU's idle execution slots instead of running one after another.
    fn run<const B: usize>(
        &mut self,
        idx: [usize; 3],
        buf: &mut Planar,
        channels: usize,
        frames: usize,
    ) {
        let coef: [Coeffs; B] = core::array::from_fn(|k| self.coeffs[idx[k]]);
        let mut c = 0;
        while c + 1 < channels {
            // ? `split_at_mut` hands out two non-overlapping `&mut` rows at once; the borrow
            // ? checker would reject two `&mut buf[..]` borrows taken the normal way.
            let (head, tail) = buf.split_at_mut(c + 1);
            let l = &mut head[c][..frames];
            let r = &mut tail[0][..frames];
            let mut sl: [State; B] = core::array::from_fn(|k| self.state[c][idx[k]]);
            let mut sr: [State; B] = core::array::from_fn(|k| self.state[c + 1][idx[k]]);
            for (xl, xr) in l.iter_mut().zip(r.iter_mut()) {
                let mut vl = *xl;
                let mut vr = *xr;
                for k in 0..B {
                    vl = tick(&coef[k], &mut sl[k], vl);
                    vr = tick(&coef[k], &mut sr[k], vr);
                }
                *xl = vl;
                *xr = vr;
            }
            for k in 0..B {
                self.state[c][idx[k]] = flush_state(sl[k]);
                self.state[c + 1][idx[k]] = flush_state(sr[k]);
            }
            c += 2;
        }
        if c < channels {
            let x = &mut buf[c][..frames];
            let mut s: [State; B] = core::array::from_fn(|k| self.state[c][idx[k]]);
            for v in x.iter_mut() {
                let mut y = *v;
                for k in 0..B {
                    y = tick(&coef[k], &mut s[k], y);
                }
                *v = y;
            }
            for k in 0..B {
                self.state[c][idx[k]] = flush_state(s[k]);
            }
        }
    }
}

#[inline]
fn flush_state(s: State) -> State {
    State {
        z1: flush_tiny(s.z1),
        z2: flush_tiny(s.z2),
    }
}
// !SECTION: 3-band EQ chain

// SECTION: Four stereo chains at once (8 SIMD lanes)
// ? Why this exists: one biquad is a *recursive* filter (each sample waits for the previous
// ? one), so a single stereo chain mostly stalls the CPU. Running 4 independent stereo chains
// ? (= 8 channels) in lockstep fills those stalls, and the compiler turns the 8-wide
// ? element-wise loops into SSE instructions. Measured on the 32-chain A/B bench:
// ? ~27 µs for 32 single chains -> ~9 µs through this kernel (3x).
// ? No `unsafe` and no CPU-specific intrinsics: plain `[f32; 8]` loops, which also become NEON
// ? on ARM (Android). A hand-written 4-lane struct was tried first and was 17 % SLOWER than
// ? the single chains, because LLVM's SLP pass mangled it. Always measure these.

/// Lanes per quad kernel: 4 members x (L, R).
pub const QUAD_LANES: usize = 8;
/// The lane-major scratch the quad kernel transposes into: one `[f32; 8]` per frame.
pub type QuadLanes = [[f32; QUAD_LANES]; ENGINE_BLOCK_FRAMES];

/// One band's coefficients for all 8 lanes. Lane = `member * 2 + channel`.
#[derive(Clone, Copy)]
struct LaneCoeffs {
    b0: [f32; QUAD_LANES],
    b1: [f32; QUAD_LANES],
    b2: [f32; QUAD_LANES],
    a1: [f32; QUAD_LANES],
    a2: [f32; QUAD_LANES],
}

impl LaneCoeffs {
    // ? y = 1·x + z1; z1' = (0·x + z2) - 0·y; z2' = 0·x - 0·y. With zero state that is EXACTLY
    // ? `y = x` forever, so a lane with no band (or no strip) passes through unchanged.
    const IDENTITY: Self = Self {
        b0: [1.0; QUAD_LANES],
        b1: [0.0; QUAD_LANES],
        b2: [0.0; QUAD_LANES],
        a1: [0.0; QUAD_LANES],
        a2: [0.0; QUAD_LANES],
    };

    fn set_member(&mut self, member: usize, c: &Coeffs) {
        for lane in [member * 2, member * 2 + 1] {
            self.b0[lane] = c.b0;
            self.b1[lane] = c.b1;
            self.b2[lane] = c.b2;
            self.a1[lane] = c.a1;
            self.a2[lane] = c.a2;
        }
    }
}

#[derive(Clone, Copy)]
struct LaneState {
    z1: [f32; QUAD_LANES],
    z2: [f32; QUAD_LANES],
}

impl EqChain {
    /// Run the EQ of up to four strips (or buses) in one go.
    ///
    /// * `present[m]` says whether member `m` exists this block (`false` = skip it entirely).
    /// * Members that are present, stereo and not bypassed share the 8-lane kernel (when at
    ///   least two qualify; a lone member is faster through `process`).
    /// * Everything else (mono, 3..8 channels, bypassed) takes the ordinary `process` path.
    ///
    /// The result is **bit-identical** to calling `process` on each member (the tests check
    /// `==`), so callers never need to care which path ran.
    pub fn process_quad(
        eqs: [&mut Self; 4],
        bufs: [&mut Planar; 4],
        channels: [usize; 4],
        present: [bool; 4],
        lanes: &mut QuadLanes,
        frames: usize,
    ) {
        let mut eqs = eqs;
        let mut bufs = bufs;
        let frames = frames.min(ENGINE_BLOCK_FRAMES);

        let mut fast = [false; 4];
        let (mut n_fast, mut bands) = (0, 0);
        for m in 0..4 {
            if present[m] && channels[m] == 2 {
                let (_, n) = eqs[m].active();
                if n > 0 {
                    fast[m] = true;
                    n_fast += 1;
                    bands = bands.max(n);
                }
            }
        }

        let use_kernel = n_fast >= 2;
        for m in 0..4 {
            if present[m] && !(use_kernel && fast[m]) {
                eqs[m].process(bufs[m], channels[m], frames);
            }
        }
        if use_kernel {
            match bands {
                1 => run_quad::<1>(&mut eqs, &mut bufs, fast, lanes, frames),
                2 => run_quad::<2>(&mut eqs, &mut bufs, fast, lanes, frames),
                _ => run_quad::<3>(&mut eqs, &mut bufs, fast, lanes, frames),
            }
        }
    }
}

// ? `B` = the largest number of active bands among the members. A member with fewer bands
// ? keeps the identity coefficients for the rest, which pass the signal through exactly.
fn run_quad<const B: usize>(
    eqs: &mut [&mut EqChain; 4],
    bufs: &mut [&mut Planar; 4],
    fast: [bool; 4],
    lanes: &mut QuadLanes,
    frames: usize,
) {
    // Gather coefficients and filter state for every fast member.
    let mut co = [LaneCoeffs::IDENTITY; B];
    let mut st = [LaneState {
        z1: [0.0; QUAD_LANES],
        z2: [0.0; QUAD_LANES],
    }; B];
    let mut idx = [[0usize; 3]; 4];
    let mut nb = [0usize; 4];
    for m in 0..4 {
        if !fast[m] {
            continue;
        }
        let (ix, n) = eqs[m].active();
        idx[m] = ix;
        nb[m] = n;
        for k in 0..n {
            co[k].set_member(m, &eqs[m].coeffs[ix[k]]);
            for ch in 0..2 {
                st[k].z1[m * 2 + ch] = eqs[m].state[ch][ix[k]].z1;
                st[k].z2[m * 2 + ch] = eqs[m].state[ch][ix[k]].z2;
            }
        }
    }

    // 1) Transpose to lane-major. No recursion in this loop, so LLVM vectorises it.
    //    ? Members that are absent or not on the fast path are read too (stale but finite
    //    ? audio) and simply never written back; lanes don't interact.
    {
        let [b0, b1, b2, b3] = &*bufs;
        let r: [&[f32]; QUAD_LANES] = [
            &b0[0][..frames],
            &b0[1][..frames],
            &b1[0][..frames],
            &b1[1][..frames],
            &b2[0][..frames],
            &b2[1][..frames],
            &b3[0][..frames],
            &b3[1][..frames],
        ];
        for (i, lane) in lanes[..frames].iter_mut().enumerate() {
            *lane = [
                r[0][i], r[1][i], r[2][i], r[3][i], r[4][i], r[5][i], r[6][i], r[7][i],
            ];
        }
    }

    // 2) The recursive filter: every statement is an independent element-wise loop over the
    //    8 lanes, in exactly the arithmetic order of the single-chain `tick`.
    for lane in lanes[..frames].iter_mut() {
        let mut v = *lane;
        for k in 0..B {
            let c = &co[k];
            let s = &mut st[k];
            let mut y = [0.0_f32; QUAD_LANES];
            for l in 0..QUAD_LANES {
                y[l] = c.b0[l] * v[l] + s.z1[l];
            }
            for l in 0..QUAD_LANES {
                s.z1[l] = (c.b1[l] * v[l] + s.z2[l]) - c.a1[l] * y[l];
            }
            for l in 0..QUAD_LANES {
                s.z2[l] = c.b2[l] * v[l] - c.a2[l] * y[l];
            }
            v = y;
        }
        *lane = v;
    }

    // 3) Transpose back and store the state, fast members only.
    for m in 0..4 {
        if !fast[m] {
            continue;
        }
        let (left, right) = bufs[m].split_at_mut(1);
        let (l, r) = (&mut left[0][..frames], &mut right[0][..frames]);
        for (i, lane) in lanes[..frames].iter().enumerate() {
            l[i] = lane[m * 2];
            r[i] = lane[m * 2 + 1];
        }
        for k in 0..nb[m] {
            for ch in 0..2 {
                eqs[m].state[ch][idx[m][k]] = flush_state(State {
                    z1: st[k].z1[m * 2 + ch],
                    z2: st[k].z2[m * 2 + ch],
                });
            }
        }
    }
}
// !SECTION: Four stereo chains at once (8 SIMD lanes)

#[cfg(test)]
mod tests {
    use super::*;
    use audio_types::ENGINE_BLOCK_FRAMES;

    /// Independent reference: the analog RBJ prototype evaluated at the pre-warped frequency.
    /// ? The bilinear transform maps analog frequency Ω (in units of w0) to
    /// ? tan(w/2)/tan(w0/2), so this does not share any code with `Coeffs::design`.
    fn reference_db(kind: BandKind, f0: f64, gain_db: f64, q: f64, f: f64) -> f64 {
        let fs = f64::from(SAMPLE_RATE);
        let a = 10f64.powf(gain_db / 40.0);
        let omega =
            (core::f64::consts::PI * f / fs).tan() / (core::f64::consts::PI * f0 / fs).tan();
        // s = jΩ, polynomials p(s) = c2 s² + c1 s + c0 -> (c0 - c2 Ω², c1 Ω)
        let poly = |c2: f64, c1: f64, c0: f64| (c0 - c2 * omega * omega, c1 * omega);
        let (num, den, scale) = match kind {
            BandKind::Peaking => (poly(1.0, a / q, 1.0), poly(1.0, 1.0 / (a * q), 1.0), 1.0),
            BandKind::LowShelf => (poly(1.0, a.sqrt() / q, a), poly(a, a.sqrt() / q, 1.0), a),
            BandKind::HighShelf => (poly(a, a.sqrt() / q, 1.0), poly(1.0, a.sqrt() / q, a), a),
        };
        let mag = scale * (num.0.hypot(num.1)) / (den.0.hypot(den.1));
        20.0 * mag.log10()
    }

    #[test]
    fn matches_analog_prototype_within_0_1_db() {
        let cases = [
            (BandKind::LowShelf, 200.0_f32, 9.0_f32, 0.707_f32),
            (BandKind::LowShelf, 120.0, -12.0, 1.2),
            (BandKind::Peaking, 1000.0, 9.0, 1.0),
            (BandKind::Peaking, 3000.0, -15.0, 2.5),
            (BandKind::HighShelf, 6000.0, 8.0, 0.707),
            (BandKind::HighShelf, 9000.0, -10.0, 0.9),
        ];
        for (kind, f0, g, q) in cases {
            let c = Coeffs::design(kind, f0, g, q);
            // 3 probe frequencies per case: well below, at, and well above the corner.
            for f in [f0 / 8.0, f0, (f0 * 6.0).min(20_000.0)] {
                let got = f64::from(c.magnitude_db(f));
                let want = reference_db(
                    kind,
                    f64::from(f0),
                    f64::from(g),
                    f64::from(q),
                    f64::from(f),
                );
                assert!(
                    (got - want).abs() < 0.1,
                    "{kind:?} f0={f0} g={g} q={q} @ {f} Hz: got {got:.3} dB, want {want:.3} dB"
                );
            }
        }
    }

    #[test]
    fn landmark_gains() {
        // Peaking: exactly the requested gain at its centre.
        let p = Coeffs::design(BandKind::Peaking, 1000.0, 9.0, 1.0);
        assert!((p.magnitude_db(1000.0) - 9.0).abs() < 0.05);
        // Low shelf: full gain at DC-ish, half the dB gain at the corner, flat far above.
        let ls = Coeffs::design(BandKind::LowShelf, 200.0, 12.0, 0.707);
        assert!((ls.magnitude_db(20.0) - 12.0).abs() < 0.5);
        assert!((ls.magnitude_db(200.0) - 6.0).abs() < 0.1);
        assert!(ls.magnitude_db(10_000.0).abs() < 0.2);
        // High shelf mirrors that.
        let hs = Coeffs::design(BandKind::HighShelf, 5000.0, -12.0, 0.707);
        assert!((hs.magnitude_db(5000.0) + 6.0).abs() < 0.1);
        assert!((hs.magnitude_db(23_000.0) + 12.0).abs() < 0.5);
        assert!(hs.magnitude_db(100.0).abs() < 0.2);
    }

    fn sine_block(freq: f32, start: usize) -> Planar {
        let mut b = [[0.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS];
        for ch in b.iter_mut() {
            for (i, v) in ch.iter_mut().enumerate() {
                let t = (start + i) as f32 / SAMPLE_RATE;
                *v = 0.5 * (core::f32::consts::TAU * freq * t).sin();
            }
        }
        b
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    /// Run a sine through the real `EqChain` (real f32 state) and measure the gain in dB.
    fn measured_db(eq: &mut EqChain, freq: f32, channels: usize) -> f32 {
        let settle_blocks = 75; // 0.2 s
        let measure_blocks = 38; // ~0.1 s (4864 samples)
        let mut measured: Vec<f32> = Vec::new();
        let mut input_rms = 0.0;
        for blk in 0..(settle_blocks + measure_blocks) {
            let mut b = sine_block(freq, blk * ENGINE_BLOCK_FRAMES);
            let dry = b[0];
            eq.process(&mut b, channels, ENGINE_BLOCK_FRAMES);
            if blk >= settle_blocks {
                measured.extend_from_slice(&b[0]);
                input_rms += rms(&dry).powi(2);
            }
        }
        let in_rms = (input_rms / measure_blocks as f32).sqrt();
        20.0 * (rms(&measured) / in_rms).log10()
    }

    #[test]
    fn time_domain_response_matches_analysis() {
        let mut eq = EqChain::new();
        let mut bands = [EqBand::default(); 3];
        bands[1] = EqBand {
            enabled: true,
            freq_hz: 1000.0,
            gain_db: 9.0,
            q: 1.0,
        };
        eq.set_bands(&bands);
        let c = Coeffs::design(BandKind::Peaking, 1000.0, 9.0, 1.0);
        for f in [125.0_f32, 1000.0, 8000.0] {
            let got = measured_db(&mut eq, f, 2);
            let want = c.magnitude_db(f);
            assert!(
                (got - want).abs() < 0.1,
                "{f} Hz: measured {got:.3}, analytic {want:.3}"
            );
            eq.reset();
        }
    }

    #[test]
    fn all_three_bands_chain() {
        let mut eq = EqChain::new();
        let bands = [
            EqBand {
                enabled: true,
                freq_hz: 150.0,
                gain_db: 6.0,
                q: 0.707,
            },
            EqBand {
                enabled: true,
                freq_hz: 1500.0,
                gain_db: -8.0,
                q: 1.5,
            },
            EqBand {
                enabled: true,
                freq_hz: 6000.0,
                gain_db: 5.0,
                q: 0.707,
            },
        ];
        eq.set_bands(&bands);
        let want = |f: f32| {
            BAND_KINDS
                .iter()
                .zip(bands.iter())
                .map(|(k, b)| Coeffs::design(*k, b.freq_hz, b.gain_db, b.q).magnitude_db(f))
                .sum::<f32>()
        };
        for f in [100.0_f32, 1500.0, 10_000.0] {
            let got = measured_db(&mut eq, f, 2);
            assert!(
                (got - want(f)).abs() < 0.1,
                "{f} Hz: {got:.3} vs {:.3}",
                want(f)
            );
            eq.reset();
        }
    }

    #[test]
    fn disabled_chain_is_bit_exact_bypass() {
        let mut eq = EqChain::new();
        assert!(eq.is_bypassed());
        let mut b = sine_block(440.0, 0);
        let orig = b;
        eq.process(&mut b, 8, ENGINE_BLOCK_FRAMES);
        assert_eq!(b, orig);
        // Enabled but 0 dB is also a bypass.
        let mut bands = [EqBand::default(); 3];
        bands[0].enabled = true;
        eq.set_bands(&bands);
        assert!(eq.is_bypassed());
    }

    #[test]
    fn odd_and_even_channel_counts_agree() {
        // 3 channels exercises the pair path and the single-channel tail path.
        let mut eq = EqChain::new();
        let mut bands = [EqBand::default(); 3];
        bands[1] = EqBand {
            enabled: true,
            freq_hz: 800.0,
            gain_db: -10.0,
            q: 2.0,
        };
        bands[2] = EqBand {
            enabled: true,
            freq_hz: 5000.0,
            gain_db: 7.0,
            q: 0.707,
        };
        eq.set_bands(&bands);
        for blk in 0..20 {
            let mut b = sine_block(1234.0, blk * ENGINE_BLOCK_FRAMES);
            eq.process(&mut b, 3, ENGINE_BLOCK_FRAMES);
            assert_eq!(b[0], b[1]);
            assert_eq!(b[1], b[2]);
        }
    }

    /// Deterministic pseudo-noise (LCG) so the test needs no RNG crate.
    fn noise_block(seed: &mut u32, channels: usize) -> Planar {
        let mut b = [[0.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS];
        for row in b.iter_mut().take(channels) {
            for v in row.iter_mut() {
                *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *v = ((*seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 1.6;
            }
        }
        b
    }

    fn chain(bands: [(bool, f32, f32, f32); 3]) -> EqChain {
        let mut eq = EqChain::new();
        eq.set_bands(&bands.map(|(enabled, freq_hz, gain_db, q)| EqBand {
            enabled,
            freq_hz,
            gain_db,
            q,
        }));
        eq
    }

    type Bands = [(bool, f32, f32, f32); 3];
    const OFF: (bool, f32, f32, f32) = (false, 0.0, 0.0, 1.0);

    /// Drive four members through `process_quad` and, in parallel, through four ordinary
    /// `process` calls on identical copies. Every output must be bit-for-bit equal, block
    /// after block (so the persistent filter state must also agree).
    fn assert_quad_matches_singles(
        what: &str,
        members: [(Bands, usize, bool); 4], // (bands, channels, present)
        block_sizes: &[usize],
    ) {
        let mut quad_eq = members.map(|(b, _, _)| chain(b));
        let mut ref_eq = members.map(|(b, _, _)| chain(b));
        let channels = members.map(|(_, c, _)| c);
        let present = members.map(|(_, _, p)| p);
        let mut lanes: Box<QuadLanes> = Box::new([[0.0; QUAD_LANES]; ENGINE_BLOCK_FRAMES]);
        let mut seed = 424_242_u32;
        for (blk, &frames) in block_sizes.iter().enumerate() {
            let mut xs: [Planar; 4] = core::array::from_fn(|m| noise_block(&mut seed, channels[m]));
            let mut ys = xs;
            {
                let [e0, e1, e2, e3] = &mut quad_eq;
                let [x0, x1, x2, x3] = &mut xs;
                EqChain::process_quad(
                    [e0, e1, e2, e3],
                    [x0, x1, x2, x3],
                    channels,
                    present,
                    &mut lanes,
                    frames,
                );
            }
            for m in 0..4 {
                if present[m] {
                    ref_eq[m].process(&mut ys[m], channels[m], frames);
                }
            }
            for m in 0..4 {
                assert!(
                    xs[m] == ys[m],
                    "{what}: member {m} differs at block {blk} ({frames} frames)"
                );
            }
        }
    }

    #[test]
    fn quad_kernel_is_bit_identical_to_single_chains() {
        let three_a: Bands = [
            (true, 90.0, 7.0, 0.7),
            (true, 1200.0, -9.0, 2.0),
            (true, 7000.0, 5.0, 0.7),
        ];
        let three_b: Bands = [
            (true, 200.0, -4.0, 1.1),
            (true, 3000.0, 6.0, 0.9),
            (true, 12000.0, -8.0, 0.7),
        ];
        let two_a: Bands = [(true, 90.0, 7.0, 0.7), OFF, (true, 7000.0, 5.0, 0.7)];
        let two_b: Bands = [OFF, (true, 3000.0, 6.0, 0.9), (true, 12000.0, -8.0, 0.7)];
        let one_a: Bands = [OFF, (true, 1200.0, -9.0, 2.0), OFF];
        let one_b: Bands = [(true, 200.0, -4.0, 1.1), OFF, OFF];
        let blocks = [128; 50];

        assert_quad_matches_singles(
            "4 x 3 bands",
            [
                (three_a, 2, true),
                (three_b, 2, true),
                (three_a, 2, true),
                (three_b, 2, true),
            ],
            &blocks,
        );
        assert_quad_matches_singles(
            "4 x 2 bands",
            [
                (two_a, 2, true),
                (two_b, 2, true),
                (two_b, 2, true),
                (two_a, 2, true),
            ],
            &blocks,
        );
        assert_quad_matches_singles(
            "4 x 1 band",
            [
                (one_a, 2, true),
                (one_b, 2, true),
                (one_a, 2, true),
                (one_b, 2, true),
            ],
            &blocks,
        );
        // Different band counts in one quad: the short ones are padded with exact identity bands.
        assert_quad_matches_singles(
            "mixed 3/2/1/3 bands",
            [
                (three_a, 2, true),
                (two_b, 2, true),
                (one_a, 2, true),
                (three_b, 2, true),
            ],
            &blocks,
        );
        assert_quad_matches_singles(
            "mixed 1/3/2/1 bands",
            [
                (one_b, 2, true),
                (three_a, 2, true),
                (two_a, 2, true),
                (one_a, 2, true),
            ],
            &blocks,
        );
    }

    #[test]
    fn quad_kernel_falls_back_cleanly_when_members_are_odd_ones_out() {
        let three: Bands = [
            (true, 90.0, 7.0, 0.7),
            (true, 1200.0, -9.0, 2.0),
            (true, 7000.0, 5.0, 0.7),
        ];
        let other: Bands = [
            (true, 200.0, -4.0, 1.1),
            (true, 3000.0, 6.0, 0.9),
            (true, 12000.0, -8.0, 0.7),
        ];
        let none: Bands = [OFF; 3];
        let blocks = [128; 30];
        // Absent members (present = false) are skipped and never touched.
        assert_quad_matches_singles(
            "two absent",
            [
                (three, 2, true),
                (other, 2, false),
                (three, 2, true),
                (other, 2, false),
            ],
            &blocks,
        );
        assert_quad_matches_singles(
            "one present (single path)",
            [
                (none, 2, false),
                (three, 2, true),
                (none, 2, false),
                (none, 2, false),
            ],
            &blocks,
        );
        assert_quad_matches_singles(
            "none present",
            [
                (three, 2, false),
                (three, 2, false),
                (three, 2, false),
                (three, 2, false),
            ],
            &blocks,
        );
        // Bypassed, mono and multichannel members next to fast ones.
        assert_quad_matches_singles(
            "one bypassed",
            [
                (three, 2, true),
                (none, 2, true),
                (other, 2, true),
                (three, 2, true),
            ],
            &blocks,
        );
        assert_quad_matches_singles(
            "all bypassed",
            [
                (none, 2, true),
                (none, 2, true),
                (none, 2, true),
                (none, 2, true),
            ],
            &blocks,
        );
        assert_quad_matches_singles(
            "mono among stereo",
            [
                (three, 2, true),
                (other, 1, true),
                (three, 2, true),
                (other, 2, true),
            ],
            &blocks,
        );
        assert_quad_matches_singles(
            "8-channel among stereo",
            [
                (three, 8, true),
                (other, 2, true),
                (three, 2, true),
                (other, 3, true),
            ],
            &blocks,
        );
    }

    #[test]
    fn quad_kernel_handles_partial_blocks_and_state_carry_over() {
        let three: Bands = [
            (true, 90.0, 7.0, 0.7),
            (true, 1200.0, -9.0, 2.0),
            (true, 7000.0, 5.0, 0.7),
        ];
        let other: Bands = [
            (true, 200.0, -4.0, 1.1),
            (true, 3000.0, 6.0, 0.9),
            (true, 12000.0, -8.0, 0.7),
        ];
        assert_quad_matches_singles(
            "ragged block sizes",
            [
                (three, 2, true),
                (other, 2, true),
                (other, 2, true),
                (three, 2, true),
            ],
            &[1, 7, 128, 33, 64, 127, 2, 128, 3, 100],
        );
    }

    #[test]
    fn quad_kernel_follows_live_parameter_changes() {
        // Changing a band mid-stream must behave exactly like it does on the single path.
        let three: Bands = [
            (true, 90.0, 7.0, 0.7),
            (true, 1200.0, -9.0, 2.0),
            (true, 7000.0, 5.0, 0.7),
        ];
        let mut quad_eq = [chain(three), chain(three), chain(three), chain(three)];
        let mut ref_eq = quad_eq.clone();
        let mut lanes: Box<QuadLanes> = Box::new([[0.0; QUAD_LANES]; ENGINE_BLOCK_FRAMES]);
        let mut seed = 7_u32;
        for blk in 0..40 {
            if blk == 10 {
                let tweak = [
                    EqBand {
                        enabled: true,
                        freq_hz: 150.0,
                        gain_db: -6.0,
                        q: 0.9,
                    },
                    EqBand {
                        enabled: false,
                        freq_hz: 1200.0,
                        gain_db: -9.0,
                        q: 2.0,
                    },
                    EqBand {
                        enabled: true,
                        freq_hz: 7000.0,
                        gain_db: 5.0,
                        q: 0.7,
                    },
                ];
                quad_eq[1].set_bands(&tweak);
                ref_eq[1].set_bands(&tweak);
            }
            let mut xs: [Planar; 4] = core::array::from_fn(|_| noise_block(&mut seed, 2));
            let mut ys = xs;
            {
                let [e0, e1, e2, e3] = &mut quad_eq;
                let [x0, x1, x2, x3] = &mut xs;
                EqChain::process_quad(
                    [e0, e1, e2, e3],
                    [x0, x1, x2, x3],
                    [2; 4],
                    [true; 4],
                    &mut lanes,
                    ENGINE_BLOCK_FRAMES,
                );
            }
            for m in 0..4 {
                ref_eq[m].process(&mut ys[m], 2, ENGINE_BLOCK_FRAMES);
                assert!(xs[m] == ys[m], "member {m} differs at block {blk}");
            }
        }
    }

    #[test]
    fn extreme_parameters_stay_finite() {
        for (f, g, q) in [
            (0.0, 100.0, 0.0),
            (1.0, -100.0, 1000.0),
            (1.0e9, 24.0, 0.001),
            (f32::MAX, f32::MAX, f32::MAX),
        ] {
            for kind in BAND_KINDS {
                let c = Coeffs::design(kind, f, g, q);
                assert!(c.b0.is_finite() && c.b1.is_finite() && c.b2.is_finite());
                assert!(c.a1.is_finite() && c.a2.is_finite());
                // Stability: poles inside the unit circle <=> |a2| < 1 and |a1| < 1 + a2.
                assert!(
                    c.a2.abs() < 1.0 && c.a1.abs() < 1.0 + c.a2,
                    "{kind:?} {f} {g} {q}: {c:?}"
                );
            }
        }
    }
}
