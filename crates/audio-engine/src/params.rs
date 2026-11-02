//! Validation of parameters that arrive through `EngineCommand`.
//!
//! * A command is applied **whole or not at all**: if any field is NaN/infinite the entire
//!   command is rejected (and counted). Finite-but-silly values are clamped.
//! * ? Ranges here are the engine's safety limits, not UI limits. A UI is free to offer less.

use crate::dsp::util::finite_clamp;
use audio_types::{Compressor, EqBand, Gate, StripDsp};

pub fn sanitize_band(b: &EqBand) -> Option<EqBand> {
    Some(EqBand {
        enabled: b.enabled,
        freq_hz: finite_clamp(b.freq_hz, 20.0, 20_000.0)?,
        gain_db: finite_clamp(b.gain_db, -24.0, 24.0)?,
        q: finite_clamp(b.q, 0.1, 18.0)?,
    })
}

pub fn sanitize_bands(b: &[EqBand; 3]) -> Option<[EqBand; 3]> {
    Some([
        sanitize_band(&b[0])?,
        sanitize_band(&b[1])?,
        sanitize_band(&b[2])?,
    ])
}

pub fn sanitize_gate(g: &Gate) -> Option<Gate> {
    Some(Gate {
        enabled: g.enabled,
        threshold_db: finite_clamp(g.threshold_db, -100.0, 0.0)?,
        attack_ms: finite_clamp(g.attack_ms, 0.1, 1000.0)?,
        release_ms: finite_clamp(g.release_ms, 1.0, 5000.0)?,
    })
}

pub fn sanitize_comp(c: &Compressor) -> Option<Compressor> {
    Some(Compressor {
        enabled: c.enabled,
        threshold_db: finite_clamp(c.threshold_db, -80.0, 0.0)?,
        ratio: finite_clamp(c.ratio, 1.0, 100.0)?,
        attack_ms: finite_clamp(c.attack_ms, 0.1, 500.0)?,
        release_ms: finite_clamp(c.release_ms, 5.0, 5000.0)?,
        makeup_db: finite_clamp(c.makeup_db, -24.0, 24.0)?,
    })
}

pub fn sanitize_dsp(d: &StripDsp) -> Option<StripDsp> {
    Some(StripDsp {
        gate: sanitize_gate(&d.gate)?,
        eq: sanitize_bands(&d.eq)?,
        comp: sanitize_comp(&d.comp)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_pass_unchanged() {
        let d = StripDsp::default();
        assert_eq!(sanitize_dsp(&d), Some(d));
    }

    #[test]
    fn one_nan_rejects_the_whole_command() {
        let mut d = StripDsp::default();
        d.comp.ratio = f32::NAN;
        assert_eq!(sanitize_dsp(&d), None);
        let mut d = StripDsp::default();
        d.eq[2].freq_hz = f32::INFINITY;
        assert_eq!(sanitize_dsp(&d), None);
    }

    #[test]
    fn silly_values_are_clamped() {
        let mut d = StripDsp::default();
        d.comp.ratio = 0.0;
        d.gate.attack_ms = -5.0;
        d.eq[0].gain_db = 1000.0;
        d.eq[1].q = 0.0;
        let s = sanitize_dsp(&d).unwrap();
        assert_eq!(s.comp.ratio, 1.0);
        assert_eq!(s.gate.attack_ms, 0.1);
        assert_eq!(s.eq[0].gain_db, 24.0);
        assert_eq!(s.eq[1].q, 0.1);
    }
}
