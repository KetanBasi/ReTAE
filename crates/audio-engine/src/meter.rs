//! Level meters with engine-side ballistics, published through `audio_types::MeterAtomics`.
//!
//! * The UI reads at ~30 Hz but the engine runs 375 blocks/s. If we published the raw
//!   per-block peak, the UI would usually miss short peaks. So the *engine* holds each
//!   peak and lets it fall at `PEAK_FALL_DB_PER_S`; the UI just reads the latest value.
//! * `peak_db` / `rms_db` use the contract's floor of -120 dB for silence.
//! * `clipping` is **sticky**: the engine only ever sets it. The UI clears it
//!   (`swap(false)`) when it has shown the indicator.

use crate::dsp::Planar;
use crate::dsp::util::{
    DB_PER_LOG2, SAMPLE_RATE, SILENCE_DB, SILENCE_LIN, fast_log2, peak_and_energy,
};
use audio_types::{MAX_CHANNELS, MeterAtomics, store_f32};
use core::sync::atomic::Ordering;

pub const PEAK_FALL_DB_PER_S: f32 = 20.0;
pub const RMS_TAU_MS: f32 = 300.0;
/// Mean-square floor, `SILENCE_LIN²`.
const MS_FLOOR: f32 = SILENCE_LIN * SILENCE_LIN;
/// Largest peak the meter will ever report (+120 dBFS).
const METER_PEAK_LIMIT: f32 = 1.0e6;

/// Per-block-size constants, recomputed only when the block size changes.
#[derive(Clone, Copy, Debug)]
pub struct MeterConsts {
    /// Linear multiplier applied to the held peak every block.
    fall: f32,
    /// One-pole coefficient for the mean-square (RMS) integrator.
    rms_k: f32,
}

impl MeterConsts {
    pub fn new(frames: usize) -> Self {
        let block_s = frames as f32 / SAMPLE_RATE;
        Self {
            fall: (-PEAK_FALL_DB_PER_S * block_s * core::f32::consts::LN_10 / 20.0).exp(),
            rms_k: 1.0 - (-block_s / (RMS_TAU_MS * 0.001)).exp(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct MeterState {
    peak: [f32; MAX_CHANNELS],
    ms: [f32; MAX_CHANNELS],
    /// True once the floor value has been published, so silence costs no atomic stores.
    floored: [bool; MAX_CHANNELS],
}

impl Default for MeterState {
    fn default() -> Self {
        Self::new()
    }
}

impl MeterState {
    pub const fn new() -> Self {
        Self {
            peak: [0.0; MAX_CHANNELS],
            ms: [0.0; MAX_CHANNELS],
            floored: [false; MAX_CHANNELS],
        }
    }

    /// Measure one block and publish. Returns this block's raw peak (all channels, linear).
    pub fn measure(
        &mut self,
        buf: &Planar,
        channels: usize,
        frames: usize,
        out: &MeterAtomics,
        c: &MeterConsts,
    ) -> f32 {
        let mut block_peak = 0.0_f32;
        let inv_frames = 1.0 / frames.max(1) as f32;
        for (ch, row) in buf.iter().enumerate() {
            let (pk, ms_block) = if ch < channels {
                let (pk, energy) = peak_and_energy(&row[..frames]);
                // !! Clamp what the integrators see. If a pathological block overflows the
                // !! energy sum to infinity, `ms += k * (inf - ms)` would turn into inf - inf
                // !! = NaN and the meter would stay broken forever. `min` also maps NaN to
                // !! the limit. 1e6 = +120 dBFS, far beyond any real signal.
                (
                    pk.min(METER_PEAK_LIMIT),
                    (energy * inv_frames).min(METER_PEAK_LIMIT * METER_PEAK_LIMIT),
                )
            } else {
                (0.0, 0.0)
            };
            // ? Full-scale (|x| == 1.0) is not clipping; only samples beyond it are.
            if pk > 1.0 {
                out.clipping.store(true, Ordering::Relaxed);
            }
            block_peak = block_peak.max(pk);
            self.update_channel(ch, pk, ms_block, out, c);
        }
        block_peak
    }

    /// Same as `measure` for a block of pure silence (idle strips / buses).
    pub fn measure_silence(&mut self, out: &MeterAtomics, c: &MeterConsts) {
        for ch in 0..MAX_CHANNELS {
            self.update_channel(ch, 0.0, 0.0, out, c);
        }
    }

    #[inline]
    fn update_channel(
        &mut self,
        ch: usize,
        pk: f32,
        ms_block: f32,
        out: &MeterAtomics,
        c: &MeterConsts,
    ) {
        self.peak[ch] = (self.peak[ch] * c.fall).max(pk);
        self.ms[ch] += c.rms_k * (ms_block - self.ms[ch]);
        // ? Snap to exact zero below the floor: otherwise the decaying values would crawl
        // ? into denormals and `floored` below would never become true.
        if self.peak[ch] < SILENCE_LIN {
            self.peak[ch] = 0.0;
        }
        if self.ms[ch] < MS_FLOOR {
            self.ms[ch] = 0.0;
        }
        let silent = self.peak[ch] == 0.0 && self.ms[ch] == 0.0;
        if silent && self.floored[ch] {
            return;
        }
        // ? Meters only need ~0.01 dB, so the cheap `fast_log2` (error 2.4e-5 dB) replaces two
        // ? libm `log10` calls per channel per block (128 per block with 16 strips + 16 buses).
        let peak_db = if self.peak[ch] > 0.0 {
            DB_PER_LOG2 * fast_log2(self.peak[ch].max(SILENCE_LIN))
        } else {
            SILENCE_DB
        };
        store_f32(&out.peak_db[ch], peak_db);
        let rms_db = if self.ms[ch] > 0.0 {
            // 10·log10(ms) = (20·log10(2) / 2) · log2(ms)
            0.5 * DB_PER_LOG2 * fast_log2(self.ms[ch].max(MS_FLOOR))
        } else {
            SILENCE_DB
        };
        store_f32(&out.rms_db[ch], rms_db);
        self.floored[ch] = silent;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_types::{ENGINE_BLOCK_FRAMES, load_f32};

    #[test]
    fn peak_falls_at_the_configured_rate() {
        let c = MeterConsts::new(ENGINE_BLOCK_FRAMES);
        let out = MeterAtomics::default();
        let mut m = MeterState::new();
        let mut loud = [[0.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS];
        loud[0][5] = 1.0;
        m.measure(&loud, 1, ENGINE_BLOCK_FRAMES, &out, &c);
        assert!(load_f32(&out.peak_db[0]).abs() < 0.01);
        // 1 s of silence later the peak must have fallen by ~20 dB.
        let silent = [[0.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS];
        let blocks = (SAMPLE_RATE / ENGINE_BLOCK_FRAMES as f32) as usize;
        for _ in 0..blocks {
            m.measure(&silent, 1, ENGINE_BLOCK_FRAMES, &out, &c);
        }
        let db = load_f32(&out.peak_db[0]);
        assert!((db + 20.0).abs() < 0.5, "{db}");
    }

    #[test]
    fn rms_of_full_scale_square_converges_to_zero_db() {
        let c = MeterConsts::new(ENGINE_BLOCK_FRAMES);
        let out = MeterAtomics::default();
        let mut m = MeterState::new();
        let sq = [[1.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS];
        for _ in 0..1500 {
            m.measure(&sq, 2, ENGINE_BLOCK_FRAMES, &out, &c);
        }
        assert!(load_f32(&out.rms_db[0]).abs() < 0.05);
        assert!(
            !out.clipping.load(Ordering::Relaxed),
            "|x| == 1.0 is full scale, not a clip"
        );
        // Unused channels stay at the floor.
        assert_eq!(load_f32(&out.rms_db[5]), SILENCE_DB);
    }

    #[test]
    fn overflowing_energy_cannot_poison_the_meter() {
        let c = MeterConsts::new(ENGINE_BLOCK_FRAMES);
        let out = MeterAtomics::default();
        let mut m = MeterState::new();
        // 3e38 squared overflows f32 -> the raw energy sum is infinity.
        let hot = [[3.0e38; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS];
        for _ in 0..50 {
            m.measure(&hot, 2, ENGINE_BLOCK_FRAMES, &out, &c);
        }
        assert!(load_f32(&out.rms_db[0]).is_finite());
        assert!(load_f32(&out.peak_db[0]).is_finite());
        // ...and it comes back down once the signal is sane again.
        let silent = [[0.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS];
        for _ in 0..60_000 {
            m.measure(&silent, 2, ENGINE_BLOCK_FRAMES, &out, &c);
        }
        assert_eq!(load_f32(&out.peak_db[0]), SILENCE_DB);
        assert_eq!(load_f32(&out.rms_db[0]), SILENCE_DB);
    }

    #[test]
    fn clipping_is_sticky_and_silence_reaches_the_floor() {
        let c = MeterConsts::new(ENGINE_BLOCK_FRAMES);
        let out = MeterAtomics::default();
        let mut m = MeterState::new();
        let mut hot = [[0.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS];
        hot[1][0] = 1.5;
        m.measure(&hot, 2, ENGINE_BLOCK_FRAMES, &out, &c);
        assert!(out.clipping.load(Ordering::Relaxed));
        for _ in 0..10_000 {
            m.measure_silence(&out, &c);
        }
        assert!(
            out.clipping.load(Ordering::Relaxed),
            "only the UI may clear the flag"
        );
        assert_eq!(load_f32(&out.peak_db[1]), SILENCE_DB);
        assert_eq!(load_f32(&out.rms_db[1]), SILENCE_DB);
    }
}
