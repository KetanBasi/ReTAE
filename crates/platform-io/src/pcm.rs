//! Sample-format conversion between a device's byte buffer and the engine's `f32`.
//!
//! Shared-mode WASAPI streams are always `f32` (the system converts for us), so this matters
//! for **exclusive mode**, where the device decides: 16-bit, 24-bit (packed in 3 bytes or in a
//! 32-bit container) or 32-bit integers. Everything is little-endian, as on every Windows
//! machine. No allocation: both functions work on slices the caller owns, so they are safe to
//! call from the stream's real-time thread.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFormat {
    F32,
    /// 32-bit container, which also carries 24 valid bits (left-justified, as WASAPI defines).
    I32,
    /// 24-bit packed in 3 bytes.
    I24,
    I16,
}

impl SampleFormat {
    pub const fn bytes(self) -> usize {
        match self {
            Self::F32 | Self::I32 => 4,
            Self::I24 => 3,
            Self::I16 => 2,
        }
    }
}

/// Convert `bytes` (a whole number of samples) to `f32`, filling `out` from the start.
/// Returns how many samples were written (the smaller of the two lengths).
pub fn read_samples(fmt: SampleFormat, bytes: &[u8], out: &mut [f32]) -> usize {
    let n = (bytes.len() / fmt.bytes()).min(out.len());
    match fmt {
        SampleFormat::F32 => {
            for (o, b) in out.iter_mut().zip(bytes.chunks_exact(4)) {
                *o = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            }
        }
        SampleFormat::I32 => {
            for (o, b) in out.iter_mut().zip(bytes.chunks_exact(4)) {
                *o = i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0;
            }
        }
        SampleFormat::I24 => {
            for (o, b) in out.iter_mut().zip(bytes.chunks_exact(3)) {
                // ? Put the 24 bits in the top of an i32 and shift back down: the arithmetic
                // ? shift copies the sign bit, which is the sign extension.
                let v = i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8;
                *o = v as f32 / 8_388_608.0;
            }
        }
        SampleFormat::I16 => {
            for (o, b) in out.iter_mut().zip(bytes.chunks_exact(2)) {
                *o = f32::from(i16::from_le_bytes([b[0], b[1]])) / 32_768.0;
            }
        }
    }
    n
}

/// Convert `samples` to the device format into `bytes`. Values outside +-1 are clipped and NaN
/// becomes silence, so a bad value in the mixer never reaches a speaker as a full-scale click.
/// Returns how many samples were written.
pub fn write_samples(fmt: SampleFormat, samples: &[f32], bytes: &mut [u8]) -> usize {
    let n = (bytes.len() / fmt.bytes()).min(samples.len());
    let clean = |v: f32| if v.is_nan() { 0.0 } else { v.clamp(-1.0, 1.0) };
    match fmt {
        SampleFormat::F32 => {
            for (b, s) in bytes.chunks_exact_mut(4).zip(samples) {
                b.copy_from_slice(&clean(*s).to_le_bytes());
            }
        }
        SampleFormat::I32 => {
            for (b, s) in bytes.chunks_exact_mut(4).zip(samples) {
                // ? `as` from float to int saturates, so +1.0 cannot wrap to a negative value.
                let v = (f64::from(clean(*s)) * 2_147_483_647.0).round() as i32;
                b.copy_from_slice(&v.to_le_bytes());
            }
        }
        SampleFormat::I24 => {
            for (b, s) in bytes.chunks_exact_mut(3).zip(samples) {
                let v = (clean(*s) * 8_388_607.0).round() as i32;
                b.copy_from_slice(&v.to_le_bytes()[..3]);
            }
        }
        SampleFormat::I16 => {
            for (b, s) in bytes.chunks_exact_mut(2).zip(samples) {
                let v = (clean(*s) * 32_767.0).round() as i16;
                b.copy_from_slice(&v.to_le_bytes());
            }
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [SampleFormat; 4] = [
        SampleFormat::F32,
        SampleFormat::I32,
        SampleFormat::I24,
        SampleFormat::I16,
    ];

    #[test]
    fn every_format_round_trips_within_its_resolution() {
        let src = [0.0, 0.5, -0.5, 1.0, -1.0, 0.123_456, -0.987_654];
        for fmt in ALL {
            let mut bytes = vec![0u8; src.len() * fmt.bytes()];
            assert_eq!(write_samples(fmt, &src, &mut bytes), src.len());
            let mut back = [0.0f32; 7];
            assert_eq!(read_samples(fmt, &bytes, &mut back), src.len());
            let tol = match fmt {
                SampleFormat::I16 => 1.0 / 16_000.0,
                SampleFormat::I24 => 1.0e-6,
                _ => 1.0e-7,
            };
            for (a, b) in src.iter().zip(&back) {
                assert!((a - b).abs() <= tol, "{fmt:?}: {a} came back as {b}");
            }
        }
    }

    #[test]
    fn full_scale_does_not_wrap_around() {
        for fmt in [SampleFormat::I32, SampleFormat::I24, SampleFormat::I16] {
            let mut bytes = vec![0u8; fmt.bytes()];
            write_samples(fmt, &[1.0], &mut bytes);
            let mut out = [0.0f32];
            read_samples(fmt, &bytes, &mut out);
            assert!(out[0] > 0.999, "{fmt:?}: +1.0 became {}", out[0]);
            write_samples(fmt, &[-1.0], &mut bytes);
            read_samples(fmt, &bytes, &mut out);
            assert!(out[0] < -0.999, "{fmt:?}: -1.0 became {}", out[0]);
        }
    }

    #[test]
    fn bad_values_are_clipped_or_silenced() {
        for fmt in ALL {
            let mut bytes = vec![0u8; 3 * fmt.bytes()];
            write_samples(fmt, &[f32::NAN, 5.0, f32::NEG_INFINITY], &mut bytes);
            let mut out = [9.0f32; 3];
            read_samples(fmt, &bytes, &mut out);
            assert_eq!(out[0], 0.0, "{fmt:?}: NaN must be silence");
            assert!(out[1] > 0.999 && out[1] <= 1.0, "{fmt:?}: {}", out[1]);
            assert!(out[2] < -0.999 && out[2] >= -1.0, "{fmt:?}: {}", out[2]);
        }
    }

    #[test]
    fn the_24_bit_layout_is_little_endian_and_sign_extended() {
        // 0x800001 is the most negative 24-bit value plus one.
        let mut out = [0.0f32];
        read_samples(SampleFormat::I24, &[0x01, 0x00, 0x80], &mut out);
        assert!((out[0] + 1.0).abs() < 1.0e-6, "{}", out[0]);
        read_samples(SampleFormat::I24, &[0xFF, 0xFF, 0x7F], &mut out);
        assert!((out[0] - 1.0).abs() < 1.0e-6, "{}", out[0]);
    }

    #[test]
    fn short_buffers_are_handled_without_panicking() {
        let mut out = [0.0f32; 4];
        // 5 bytes hold one whole 16-bit sample pair... that is 2 samples; the odd byte is ignored.
        assert_eq!(
            read_samples(SampleFormat::I16, &[0, 0, 0, 0, 7], &mut out),
            2
        );
        let mut bytes = [0u8; 3];
        assert_eq!(
            write_samples(SampleFormat::I16, &[0.5, 0.5, 0.5], &mut bytes),
            1
        );
    }
}
