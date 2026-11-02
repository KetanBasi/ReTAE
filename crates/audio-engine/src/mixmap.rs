//! Pan laws and strip -> bus channel mapping (PLAN.md §7: "constant-power pan or channel map").
//!
//! * Mono strip  -> stereo(+) bus : constant-power pan (centre = -3.01 dB per side).
//! * Stereo(+) strip -> stereo(+) bus : channel `c` goes to channel `c`; `pan` acts as a
//!   *balance* (the quieter side is attenuated, the other side stays at unity).
//! * Anything -> mono bus : average of the channels (balance applied to the first two).
//! * Channels beyond `min(strip, bus)` are dropped (down) or left silent (up).

use crate::dsp::Planar;
use crate::dsp::smooth::mix_ramp;
use core::f32::consts::{FRAC_PI_2, FRAC_PI_4};

/// `(left, right)` gains for `pan` in `[-1, 1]` (−1 = hard left).
pub fn pan_gains(strip_channels: usize, pan: f32) -> (f32, f32) {
    if strip_channels <= 1 {
        // Constant power: gl² + gr² == 1 for every pan position.
        let theta = (pan + 1.0) * FRAC_PI_4;
        (theta.cos(), theta.sin())
    } else {
        // Balance: unity on the favoured side, cos-law attenuation on the other.
        (
            (pan.max(0.0) * FRAC_PI_2).cos(),
            ((-pan).max(0.0) * FRAC_PI_2).cos(),
        )
    }
}

/// Gains for one send, each as `(start_of_block, end_of_block)` so they can be ramped.
#[derive(Clone, Copy, Debug)]
pub struct SendGains {
    pub left: (f32, f32),
    pub right: (f32, f32),
    pub flat: (f32, f32),
}

#[inline]
fn scaled(g: (f32, f32), s: f32) -> (f32, f32) {
    (g.0 * s, g.1 * s)
}

/// Add `src` (a strip with `strip_ch` channels) into `dst` (a bus with `bus_ch` channels).
pub fn mix_into_bus(
    dst: &mut Planar,
    bus_ch: usize,
    src: &Planar,
    strip_ch: usize,
    g: &SendGains,
    frames: usize,
) {
    let bus_ch = bus_ch.min(dst.len());
    let strip_ch = strip_ch.min(src.len());
    if bus_ch == 0 || strip_ch == 0 {
        return;
    }
    if bus_ch == 1 {
        // Down to mono: average. For 1 -> 1 that is a plain copy at the send gain.
        let inv = 1.0 / strip_ch as f32;
        for (c, row) in src.iter().take(strip_ch).enumerate() {
            let gc = match (strip_ch, c) {
                (1, _) => g.flat,
                (_, 0) => g.left,
                (_, 1) => g.right,
                _ => g.flat,
            };
            let gc = scaled(gc, inv);
            mix_ramp(&mut dst[0][..frames], &row[..frames], gc.0, gc.1);
        }
    } else if strip_ch == 1 {
        // Mono -> stereo(+): pan between channels 0 and 1.
        mix_ramp(&mut dst[0][..frames], &src[0][..frames], g.left.0, g.left.1);
        mix_ramp(
            &mut dst[1][..frames],
            &src[0][..frames],
            g.right.0,
            g.right.1,
        );
    } else {
        for c in 0..strip_ch.min(bus_ch) {
            let gc = match c {
                0 => g.left,
                1 => g.right,
                _ => g.flat,
            };
            mix_ramp(&mut dst[c][..frames], &src[c][..frames], gc.0, gc.1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::SILENT;

    fn unity() -> SendGains {
        SendGains {
            left: (1.0, 1.0),
            right: (1.0, 1.0),
            flat: (1.0, 1.0),
        }
    }

    #[test]
    fn mono_pan_is_constant_power() {
        for i in 0..=20 {
            let pan = -1.0 + i as f32 * 0.1;
            let (l, r) = pan_gains(1, pan);
            assert!((l * l + r * r - 1.0).abs() < 1.0e-6, "pan {pan}");
        }
        let (l, r) = pan_gains(1, 0.0);
        assert!((l - r).abs() < 1.0e-6);
        assert!(
            (20.0 * l.log10() + 3.0103).abs() < 0.01,
            "centre must be -3.01 dB"
        );
        assert!(pan_gains(1, -1.0).1.abs() < 1.0e-6);
        assert!((pan_gains(1, -1.0).0 - 1.0).abs() < 1.0e-6);
        assert!(pan_gains(1, 1.0).0.abs() < 1.0e-6);
    }

    #[test]
    fn stereo_balance_is_unity_at_centre() {
        assert_eq!(pan_gains(2, 0.0), (1.0, 1.0));
        let (l, r) = pan_gains(2, -1.0);
        assert!((l - 1.0).abs() < 1.0e-6 && r.abs() < 1.0e-6);
        let (l, r) = pan_gains(2, 1.0);
        assert!(l.abs() < 1.0e-6 && (r - 1.0).abs() < 1.0e-6);
        // Monotonic: moving right never raises the left gain.
        let mut prev = 2.0;
        for i in 0..=20 {
            let l = pan_gains(2, -1.0 + i as f32 * 0.1).0;
            assert!(l <= prev + 1.0e-6);
            prev = l;
        }
    }

    fn src_with(rows: &[f32]) -> Planar {
        let mut s = SILENT;
        for (c, v) in rows.iter().enumerate() {
            s[c][..4].fill(*v);
        }
        s
    }

    #[test]
    fn mono_to_stereo_uses_pan() {
        let src = src_with(&[1.0]);
        let mut dst = SILENT;
        let (l, r) = pan_gains(1, -0.5);
        let g = SendGains {
            left: (l, l),
            right: (r, r),
            flat: (1.0, 1.0),
        };
        mix_into_bus(&mut dst, 2, &src, 1, &g, 4);
        assert!((dst[0][0] - l).abs() < 1.0e-6 && (dst[1][0] - r).abs() < 1.0e-6);
        assert_eq!(dst[2][0], 0.0);
    }

    #[test]
    fn stereo_to_stereo_is_identity_at_unity() {
        let src = src_with(&[0.25, -0.75]);
        let mut dst = SILENT;
        mix_into_bus(&mut dst, 2, &src, 2, &unity(), 4);
        assert_eq!((dst[0][3], dst[1][3]), (0.25, -0.75));
    }

    #[test]
    fn down_to_mono_averages() {
        let src = src_with(&[0.5, 1.0]);
        let mut dst = SILENT;
        mix_into_bus(&mut dst, 1, &src, 2, &unity(), 4);
        assert!((dst[0][0] - 0.75).abs() < 1.0e-6);
        // 1 -> 1 keeps the signal at the send gain.
        let mut dst = SILENT;
        mix_into_bus(&mut dst, 1, &src_with(&[0.5]), 1, &unity(), 4);
        assert_eq!(dst[0][0], 0.5);
        // 8 channels of 0.8 -> mono 0.8.
        let mut dst = SILENT;
        mix_into_bus(&mut dst, 1, &src_with(&[0.8; 8]), 8, &unity(), 4);
        assert!((dst[0][0] - 0.8).abs() < 1.0e-6);
    }

    #[test]
    fn surround_maps_by_index_and_drops_extras() {
        let src = src_with(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        // 6 ch -> 4 ch bus: first four channels, 5 and 6 dropped.
        let mut dst = SILENT;
        mix_into_bus(&mut dst, 4, &src, 6, &unity(), 4);
        assert_eq!(
            [dst[0][0], dst[1][0], dst[2][0], dst[3][0], dst[4][0]],
            [1.0, 2.0, 3.0, 4.0, 0.0]
        );
        // 2 ch -> 8 ch bus: only L/R are fed.
        let mut dst = SILENT;
        mix_into_bus(&mut dst, 8, &src_with(&[1.0, 2.0]), 2, &unity(), 4);
        assert_eq!([dst[0][0], dst[1][0], dst[2][0]], [1.0, 2.0, 0.0]);
    }

    #[test]
    fn mixing_accumulates_instead_of_overwriting() {
        let src = src_with(&[0.5, 0.5]);
        let mut dst = SILENT;
        mix_into_bus(&mut dst, 2, &src, 2, &unity(), 4);
        mix_into_bus(&mut dst, 2, &src, 2, &unity(), 4);
        assert_eq!(dst[0][0], 1.0);
    }
}
