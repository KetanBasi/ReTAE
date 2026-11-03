//! VBAN audio packets: PCM <-> `f32`, packet parsing, and the TX packetizer.
//!
//! * RX decodes every PCM type of PLAN.md §4.1 (Byte8, Int16, Int24, Int32, Float32,
//!   Float64). 10/12-bit packed formats and the VBCA/VBCV codecs are rejected with a
//!   distinct error so the caller can count them.
//! * TX produces Int16, Int24 and Float32 only.
//! * The 28-byte header itself is `audio_types::VbanAudioHeader` (explicit LE, no
//!   `#[repr(packed)]`); this module only adds the payload on top of it.
// ? Nothing here allocates: parsing returns slices into the caller's receive buffer, and the
// ? packetizer writes into a caller-provided `[u8; VBAN_MAX_PACKET_BYTES]`.

use audio_types::{
    FixedStr, MAX_CHANNELS, OPUS_USER_TAG, VBAN_HEADER_SIZE, VBAN_MAX_PACKET_BYTES,
    VbanAudioHeader, VbanCodec, VbanDataType, VbanParseError, vban_max_frames,
};

/// Default frames per packet (PLAN.md §4.2): 2.67 ms at 48 kHz.
pub const DEFAULT_FRAMES_PER_PACKET: usize = 128;

/// Most `f32` samples one packet can hold after decoding: 256 frames x 8 channels.
// ? A packet is also bounded by its 1436-byte payload, which is smaller for every type but
// ? Byte8 (1436 samples). This constant is the buffer size, not a protocol limit.
pub const MAX_PACKET_SAMPLES: usize = 256 * MAX_CHANNELS;

/// Why a packet was not turned into audio. Each variant has its own counter on the RX side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketError {
    /// Bad magic, too short, not audio, unknown sample-rate index or codec.
    Header(VbanParseError),
    /// 10/12-bit packed samples, or the VBCA/VBCV codecs.
    UnsupportedFormat,
    /// More channels than the engine's 8, or zero.
    UnsupportedChannels(u16),
    /// A `User` codec packet that does not start with `OPUS_USER_TAG` (another app's data).
    ForeignUser,
    /// Payload shorter than `frames * channels * bytes_per_sample`.
    Truncated,
}

impl From<VbanParseError> for PacketError {
    fn from(e: VbanParseError) -> Self {
        Self::Header(e)
    }
}

/// The audio bytes of a parsed packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RxPayload<'a> {
    /// Exactly `frames * channels * bytes_per_sample` bytes.
    Pcm {
        data_type: VbanDataType,
        bytes: &'a [u8],
    },
    /// One Opus packet (the `OPUS_USER_TAG` is already stripped).
    Opus(&'a [u8]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParsedAudio<'a> {
    pub header: VbanAudioHeader,
    pub payload: RxPayload<'a>,
}

/// Validate a UDP datagram that starts with the audio sub-protocol.
// ? Returns slices that borrow `pkt`; the borrow checker guarantees the receive buffer is
// ? not reused while the slices are still alive.
pub fn parse_audio_packet(pkt: &[u8]) -> Result<ParsedAudio<'_>, PacketError> {
    let header = VbanAudioHeader::decode(pkt)?;
    if header.channels as usize > MAX_CHANNELS {
        return Err(PacketError::UnsupportedChannels(header.channels));
    }
    let body = &pkt[VBAN_HEADER_SIZE..];
    match header.codec {
        VbanCodec::Pcm => {
            let bps = header
                .data_type
                .bytes_per_sample()
                .ok_or(PacketError::UnsupportedFormat)?;
            let need = header.frames as usize * header.channels as usize * bps;
            if body.len() < need {
                return Err(PacketError::Truncated);
            }
            Ok(ParsedAudio {
                header,
                payload: RxPayload::Pcm {
                    data_type: header.data_type,
                    // ? Extra trailing bytes are ignored rather than rejected: some senders pad.
                    bytes: &body[..need],
                },
            })
        }
        VbanCodec::User => match body.strip_prefix(&OPUS_USER_TAG) {
            Some(opus) if !opus.is_empty() => Ok(ParsedAudio {
                header,
                payload: RxPayload::Opus(opus),
            }),
            Some(_) => Err(PacketError::Truncated),
            None => Err(PacketError::ForeignUser),
        },
        VbanCodec::Vbca | VbanCodec::Vbcv => Err(PacketError::UnsupportedFormat),
    }
}

// SECTION: PCM conversion
const I16_SCALE: f32 = 32_768.0;
const I24_SCALE: f32 = 8_388_608.0;
const I32_SCALE: f64 = 2_147_483_648.0;

/// Replace NaN/inf (hostile or corrupt packets) with silence.
#[inline]
fn finite(x: f32) -> f32 {
    if x.is_finite() { x } else { 0.0 }
}

/// Convert wire bytes to `f32` in `[-1, 1)` (floats pass through unclamped).
/// Returns the number of samples written, or `None` for the packed 10/12-bit types.
/// ? Whole samples only; a trailing partial sample is ignored, and `out` bounds the count.
pub fn decode_pcm(data_type: VbanDataType, bytes: &[u8], out: &mut [f32]) -> Option<usize> {
    fn run<const N: usize>(bytes: &[u8], out: &mut [f32], f: impl Fn([u8; N]) -> f32) -> usize {
        let mut n = 0;
        for (dst, src) in out.iter_mut().zip(bytes.chunks_exact(N)) {
            // ? `chunks_exact(N)` guarantees the length, so this conversion cannot fail.
            *dst = finite(f(src.try_into().expect("chunk is N bytes")));
            n += 1;
        }
        n
    }
    Some(match data_type {
        // * VBAN Byte8 is unsigned 8-bit, centred on 128 (like WAV).
        VbanDataType::Byte8 => run::<1>(bytes, out, |b| (b[0] as f32 - 128.0) / 128.0),
        VbanDataType::Int16 => run::<2>(bytes, out, |b| i16::from_le_bytes(b) as f32 / I16_SCALE),
        VbanDataType::Int24 => run::<3>(bytes, out, |b| {
            // ? Sign-extend by placing the 24 bits in the top of an i32, then shifting back.
            let v = i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8;
            v as f32 / I24_SCALE
        }),
        VbanDataType::Int32 => run::<4>(bytes, out, |b| {
            (i32::from_le_bytes(b) as f64 / I32_SCALE) as f32
        }),
        VbanDataType::Float32 => run::<4>(bytes, out, f32::from_le_bytes),
        VbanDataType::Float64 => run::<8>(bytes, out, |b| f64::from_le_bytes(b) as f32),
        VbanDataType::Bits12 | VbanDataType::Bits10 => return None,
    })
}

/// True for the types we can send (PLAN.md §4.1: TX is Int16/Int24/Float32).
pub const fn tx_supported(data_type: VbanDataType) -> bool {
    matches!(
        data_type,
        VbanDataType::Int16 | VbanDataType::Int24 | VbanDataType::Float32
    )
}

/// Convert `samples` to wire bytes. Integer types round to nearest and saturate; NaN becomes 0.
/// Returns the number of bytes written, or `None` if the type is not a TX type or `out` is
/// too small.
pub fn encode_pcm(data_type: VbanDataType, samples: &[f32], out: &mut [u8]) -> Option<usize> {
    let bps = data_type
        .bytes_per_sample()
        .filter(|_| tx_supported(data_type))?;
    let total = samples.len() * bps;
    let out = out.get_mut(..total)?;
    match data_type {
        VbanDataType::Int16 => {
            for (dst, &s) in out.chunks_exact_mut(2).zip(samples) {
                // ? `as i16` saturates and maps NaN to 0 (Rust float->int casts), so the
                // ? clamp is built into the cast; `round` picks nearest instead of truncating.
                let v = (s * I16_SCALE).round() as i16;
                dst.copy_from_slice(&v.to_le_bytes());
            }
        }
        VbanDataType::Int24 => {
            for (dst, &s) in out.chunks_exact_mut(3).zip(samples) {
                let v = ((s * I24_SCALE).round() as i32).clamp(-8_388_608, 8_388_607);
                dst.copy_from_slice(&v.to_le_bytes()[..3]);
            }
        }
        VbanDataType::Float32 => {
            for (dst, &s) in out.chunks_exact_mut(4).zip(samples) {
                dst.copy_from_slice(&finite(s).to_le_bytes());
            }
        }
        _ => return None,
    }
    Some(total)
}
// !SECTION: PCM conversion

// SECTION: Channel mapping
/// Map `frames` interleaved frames from `src_ch` to `dst_ch` channels.
///
/// - equal: copy.
/// - mono source: duplicated to every output channel.
/// - mono destination: average of the source channels.
/// - otherwise: the first `min` channels are copied and the rest are silent.
// ? PLAN.md §4.1 "downmix/upmix map per strip". A full speaker-layout matrix is a later
// ? polish item; this keeps the common mono/stereo/8-ch cases predictable.
pub fn map_channels(src: &[f32], src_ch: usize, dst: &mut [f32], dst_ch: usize, frames: usize) {
    debug_assert!(src_ch > 0 && dst_ch > 0);
    debug_assert!(src.len() >= frames * src_ch && dst.len() >= frames * dst_ch);
    if src_ch == dst_ch {
        dst[..frames * dst_ch].copy_from_slice(&src[..frames * src_ch]);
        return;
    }
    let src_frames = src.chunks_exact(src_ch).take(frames);
    let dst_frames = dst.chunks_exact_mut(dst_ch).take(frames);
    for (s, d) in src_frames.zip(dst_frames) {
        if src_ch == 1 {
            d.fill(s[0]);
        } else if dst_ch == 1 {
            d[0] = s.iter().sum::<f32>() / src_ch as f32;
        } else {
            let n = src_ch.min(dst_ch);
            d[..n].copy_from_slice(&s[..n]);
            d[n..].fill(0.0);
        }
    }
}
// !SECTION: Channel mapping

// SECTION: Packetizer (TX)
/// Frames per packet for a TX config: `requested == 0` means auto.
/// Auto = the largest count <= 128 that fits one packet; an explicit request is clamped to
/// what fits (`vban_max_frames`) and to at least 1.
pub fn plan_frames(requested: u16, channels: usize, bytes_per_sample: usize) -> usize {
    let fit = vban_max_frames(channels.max(1), bytes_per_sample.max(1)).max(1);
    match requested {
        0 => fit.min(DEFAULT_FRAMES_PER_PACKET),
        n => (n as usize).min(fit).max(1),
    }
}

/// Builds PCM packets for one outgoing stream and numbers them.
#[derive(Clone, Debug)]
pub struct PcmPacketizer {
    header: VbanAudioHeader,
    frames: usize,
    channels: usize,
    bytes_per_sample: usize,
}

impl PcmPacketizer {
    /// `None` if the type is not a TX type, the rate has no VBAN index, or the channel count
    /// is out of range.
    pub fn new(
        sample_rate: u32,
        channels: usize,
        data_type: VbanDataType,
        requested_frames: u16,
        stream_name: FixedStr<16>,
    ) -> Option<Self> {
        if !tx_supported(data_type) || channels == 0 || channels > MAX_CHANNELS {
            return None;
        }
        let bps = data_type.bytes_per_sample()?;
        let frames = plan_frames(requested_frames, channels, bps);
        let header = VbanAudioHeader {
            sample_rate,
            frames: frames as u16,
            channels: channels as u16,
            data_type,
            codec: VbanCodec::Pcm,
            stream_name,
            frame_counter: 0,
        };
        // ? Validate the header once here so `build` can not fail on it later.
        header.encode(&mut [0; VBAN_HEADER_SIZE])?;
        Some(Self {
            header,
            frames,
            channels,
            bytes_per_sample: bps,
        })
    }

    pub fn frames(&self) -> usize {
        self.frames
    }
    pub fn channels(&self) -> usize {
        self.channels
    }
    /// Samples (not frames) one packet consumes: `frames * channels`.
    pub fn samples_per_packet(&self) -> usize {
        self.frames * self.channels
    }
    pub fn packet_bytes(&self) -> usize {
        VBAN_HEADER_SIZE + self.samples_per_packet() * self.bytes_per_sample
    }
    pub fn next_counter(&self) -> u32 {
        self.header.frame_counter
    }

    /// Encode exactly one packet from `samples` (`frames * channels` interleaved `f32`) and
    /// advance the frame counter. Returns the packet length in `out`.
    pub fn build(
        &mut self,
        samples: &[f32],
        out: &mut [u8; VBAN_MAX_PACKET_BYTES],
    ) -> Option<usize> {
        if samples.len() != self.samples_per_packet() {
            return None;
        }
        let (head, body) = out.split_at_mut(VBAN_HEADER_SIZE);
        self.header.encode(head.try_into().ok()?)?;
        let n = encode_pcm(self.header.data_type, samples, body)?;
        self.header.frame_counter = self.header.frame_counter.wrapping_add(1);
        Some(VBAN_HEADER_SIZE + n)
    }
}
// !SECTION: Packetizer (TX)

#[cfg(test)]
mod tests {
    use super::*;
    use audio_types::VBAN_MAX_PAYLOAD_BYTES;

    fn name(s: &str) -> FixedStr<16> {
        FixedStr::from_str_trunc(s)
    }

    fn pcm_packet(
        sr_byte: u8,
        frames: u16,
        ch: u16,
        fmt: u8,
        body: &[u8],
        counter: u32,
    ) -> Vec<u8> {
        let mut p = b"VBAN".to_vec();
        p.extend([sr_byte, (frames - 1) as u8, (ch - 1) as u8, fmt]);
        let mut n = b"Stream1".to_vec();
        n.resize(16, 0);
        p.extend(n);
        p.extend(counter.to_le_bytes());
        p.extend(body);
        p
    }

    // SECTION: Byte-exact fixtures
    // * Hand-assembled from the VBAN spec (byte 4 = sr index | protocol, 5 = frames-1,
    // * 6 = channels-1, 7 = codec | data type), NOT produced by our own encoder, so a
    // * symmetric bug in encode+decode cannot hide.
    #[test]
    fn voicemeeter_int16_stereo_256_frames_fixture() {
        // ! 48 kHz audio => byte4 = 0x03; 256 frames => 0xFF; stereo => 0x01; Int16 => 0x01.
        let mut body = Vec::new();
        for f in 0..256i32 {
            let l = (f * 100 - 12_800) as i16; // ramp, hits negative and positive
            let r = if f == 0 {
                i16::MIN
            } else if f == 255 {
                i16::MAX
            } else {
                -l
            };
            body.extend(l.to_le_bytes());
            body.extend(r.to_le_bytes());
        }
        assert_eq!(body.len(), 1024);
        let wire = pcm_packet(0x03, 256, 2, 0x01, &body, 0x0102_0304);
        assert_eq!(wire.len(), 28 + 1024);
        assert_eq!(
            &wire[..28],
            &[
                b'V', b'B', b'A', b'N', 0x03, 0xFF, 0x01, 0x01, // header
                b'S', b't', b'r', b'e', b'a', b'm', b'1', 0, 0, 0, 0, 0, 0, 0, 0, 0, // name
                0x04, 0x03, 0x02, 0x01, // frame counter, little endian
            ]
        );

        let p = parse_audio_packet(&wire).unwrap();
        assert_eq!(p.header.sample_rate, 48_000);
        assert_eq!(p.header.frames, 256);
        assert_eq!(p.header.channels, 2);
        assert_eq!(p.header.stream_name.as_str(), "Stream1");
        assert_eq!(p.header.frame_counter, 0x0102_0304);
        let RxPayload::Pcm { data_type, bytes } = p.payload else {
            panic!("expected PCM")
        };
        assert_eq!(data_type, VbanDataType::Int16);
        let mut out = vec![0.0f32; 512];
        assert_eq!(decode_pcm(data_type, bytes, &mut out), Some(512));
        assert_eq!(out[0], (-12_800.0) / 32_768.0);
        assert_eq!(out[1], -1.0); // i16::MIN
        assert_eq!(out[2 * 255 + 1], 32_767.0 / 32_768.0);
        // Exact inverse: re-encoding the decoded samples gives the original bytes.
        let mut back = vec![0u8; 1024];
        assert_eq!(encode_pcm(VbanDataType::Int16, &out, &mut back), Some(1024));
        assert_eq!(back, body);
    }

    #[test]
    fn voicemeeter_float32_8ch_44_frames_fixture() {
        // ! 44 frames x 8 ch x 4 B = 1408 B, the most a Float32 8-channel packet can carry.
        let mut body = Vec::new();
        for f in 0..44usize {
            for c in 0..8usize {
                body.extend((f as f32 * 0.01 + c as f32 * 0.1 - 0.5).to_le_bytes());
            }
        }
        assert_eq!(body.len(), 1408);
        let wire = pcm_packet(0x03, 44, 8, 0x04, &body, 7);
        assert_eq!(&wire[4..8], &[0x03, 43, 7, 0x04]);
        assert_eq!(wire.len(), 28 + 1408);
        assert!(wire.len() - 28 <= VBAN_MAX_PAYLOAD_BYTES);
        assert_eq!(vban_max_frames(8, 4), 44);

        let p = parse_audio_packet(&wire).unwrap();
        assert_eq!((p.header.frames, p.header.channels), (44, 8));
        let RxPayload::Pcm { data_type, bytes } = p.payload else {
            panic!()
        };
        assert_eq!(data_type, VbanDataType::Float32);
        let mut out = vec![0.0f32; 44 * 8];
        assert_eq!(decode_pcm(data_type, bytes, &mut out), Some(352));
        for f in 0..44usize {
            for c in 0..8usize {
                assert_eq!(out[f * 8 + c], f as f32 * 0.01 + c as f32 * 0.1 - 0.5);
            }
        }

        // And our packetizer produces the identical datagram.
        let mut tx =
            PcmPacketizer::new(48_000, 8, VbanDataType::Float32, 0, name("Stream1")).unwrap();
        assert_eq!(tx.frames(), 44);
        while tx.next_counter() < 7 {
            tx.build(&out, &mut [0; VBAN_MAX_PACKET_BYTES]).unwrap();
        }
        let mut buf = [0u8; VBAN_MAX_PACKET_BYTES];
        let n = tx.build(&out, &mut buf).unwrap();
        assert_eq!(&buf[..n], &wire[..]);
    }

    #[test]
    fn packetizer_matches_the_int16_fixture_byte_for_byte() {
        let mut samples = vec![0.0f32; 512];
        for (i, s) in samples.iter_mut().enumerate() {
            *s = ((i as i32 * 37 % 2000) - 1000) as f32 / 32_768.0;
        }
        let mut tx =
            PcmPacketizer::new(48_000, 2, VbanDataType::Int16, 256, name("Stream1")).unwrap();
        let mut buf = [0u8; VBAN_MAX_PACKET_BYTES];
        let n = tx.build(&samples, &mut buf).unwrap();
        let mut body = vec![0u8; 1024];
        encode_pcm(VbanDataType::Int16, &samples, &mut body).unwrap();
        assert_eq!(&buf[..n], &pcm_packet(0x03, 256, 2, 0x01, &body, 0)[..]);
    }
    // !SECTION: Byte-exact fixtures

    // SECTION: Sample conversion
    #[test]
    fn every_int16_value_round_trips_exactly() {
        let all: Vec<u8> = (i16::MIN..=i16::MAX)
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let mut f = vec![0.0f32; 65_536];
        assert_eq!(decode_pcm(VbanDataType::Int16, &all, &mut f), Some(65_536));
        let mut back = vec![0u8; all.len()];
        encode_pcm(VbanDataType::Int16, &f, &mut back).unwrap();
        assert_eq!(back, all);
    }

    #[test]
    fn int24_round_trips_and_sign_extends() {
        let vals = [
            -8_388_608i32,
            -8_388_607,
            -1,
            0,
            1,
            255,
            256,
            65_535,
            8_388_606,
            8_388_607,
        ];
        let bytes: Vec<u8> = vals
            .iter()
            .flat_map(|v| v.to_le_bytes()[..3].to_vec())
            .collect();
        let mut f = vec![0.0f32; vals.len()];
        assert_eq!(
            decode_pcm(VbanDataType::Int24, &bytes, &mut f),
            Some(vals.len())
        );
        for (v, x) in vals.iter().zip(&f) {
            assert_eq!(*x, *v as f32 / 8_388_608.0, "{v}");
        }
        assert_eq!(f[0], -1.0);
        let mut back = vec![0u8; bytes.len()];
        encode_pcm(VbanDataType::Int24, &f, &mut back).unwrap();
        assert_eq!(back, bytes);
    }

    #[test]
    fn int32_float64_and_byte8_decode() {
        let mut f = [0.0f32; 3];
        let b: Vec<u8> = [i32::MIN, 0, 1 << 30]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        decode_pcm(VbanDataType::Int32, &b, &mut f).unwrap();
        assert_eq!(f, [-1.0, 0.0, 0.5]);

        let b: Vec<u8> = [0.25f64, -0.5, 1.5]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        decode_pcm(VbanDataType::Float64, &b, &mut f).unwrap();
        assert_eq!(f, [0.25, -0.5, 1.5]); // floats are not clamped here; the engine limits them

        decode_pcm(VbanDataType::Byte8, &[0, 128, 255], &mut f).unwrap();
        assert_eq!(f, [-1.0, 0.0, 127.0 / 128.0]);
    }

    #[test]
    fn packed_formats_are_refused() {
        let mut f = [0.0f32; 4];
        assert_eq!(decode_pcm(VbanDataType::Bits12, &[0; 6], &mut f), None);
        assert_eq!(decode_pcm(VbanDataType::Bits10, &[0; 5], &mut f), None);
        assert_eq!(
            encode_pcm(VbanDataType::Int32, &f, &mut [0; 16]),
            None,
            "not a TX type"
        );
        assert_eq!(encode_pcm(VbanDataType::Byte8, &f, &mut [0; 16]), None);
        assert_eq!(encode_pcm(VbanDataType::Float64, &f, &mut [0; 32]), None);
        let mut small = [0u8; 7];
        assert_eq!(
            encode_pcm(VbanDataType::Int16, &f, &mut small),
            None,
            "buffer too small"
        );
    }

    #[test]
    fn encode_saturates_rounds_and_scrubs_non_finite() {
        let xs = [
            2.0,
            -2.0,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            0.5 / 32_768.0 * 3.0,
        ];
        let mut b = [0u8; 12];
        encode_pcm(VbanDataType::Int16, &xs, &mut b).unwrap();
        let v: Vec<i16> = b
            .chunks(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        // 1.5 LSB rounds to 2 (nearest), not 1 (truncation).
        assert_eq!(v, [i16::MAX, i16::MIN, 0, i16::MAX, i16::MIN, 2]);

        let mut b = [0u8; 18];
        encode_pcm(VbanDataType::Int24, &xs, &mut b).unwrap();
        let get =
            |i: usize| i32::from_le_bytes([b[3 * i], b[3 * i + 1], b[3 * i + 2], 0]) << 8 >> 8;
        assert_eq!(get(0), 8_388_607);
        assert_eq!(get(1), -8_388_608);
        assert_eq!(get(2), 0);

        let mut b = [0u8; 24];
        encode_pcm(VbanDataType::Float32, &xs, &mut b).unwrap();
        let g = |i: usize| f32::from_le_bytes(b[4 * i..4 * i + 4].try_into().unwrap());
        assert_eq!(
            (g(0), g(2), g(3), g(4)),
            (2.0, 0.0, 0.0, 0.0),
            "floats pass; NaN/inf -> 0"
        );
    }

    #[test]
    fn decode_scrubs_hostile_floats() {
        let b: Vec<u8> = [f32::NAN, f32::INFINITY, -f32::INFINITY, 0.5]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let mut f = [9.0f32; 4];
        decode_pcm(VbanDataType::Float32, &b, &mut f).unwrap();
        assert_eq!(f, [0.0, 0.0, 0.0, 0.5]);
    }

    #[test]
    fn decode_never_writes_past_the_output() {
        let mut f = [7.0f32; 2];
        let n = decode_pcm(VbanDataType::Int16, &[1, 0, 2, 0, 3, 0], &mut f).unwrap();
        assert_eq!(n, 2);
        // A dangling half-sample at the end is ignored.
        let mut f = [0.0f32; 4];
        assert_eq!(decode_pcm(VbanDataType::Int16, &[1, 0, 2], &mut f), Some(1));
    }
    // !SECTION: Sample conversion

    // SECTION: Parsing
    #[test]
    fn parser_rejects_what_it_should() {
        let good = pcm_packet(0x03, 4, 2, 0x01, &[0; 16], 0);
        assert!(parse_audio_packet(&good).is_ok());

        assert_eq!(
            parse_audio_packet(&good[..20]),
            Err(PacketError::Header(VbanParseError::TooShort))
        );
        let mut bad = good.clone();
        bad[0] = b'X';
        assert_eq!(
            parse_audio_packet(&bad),
            Err(PacketError::Header(VbanParseError::BadMagic))
        );

        // Payload one byte short.
        assert_eq!(
            parse_audio_packet(&good[..good.len() - 1]),
            Err(PacketError::Truncated)
        );
        // Trailing padding is tolerated and trimmed.
        let mut padded = good.clone();
        padded.extend([0xAA; 5]);
        match parse_audio_packet(&padded).unwrap().payload {
            RxPayload::Pcm { bytes, .. } => assert_eq!(bytes.len(), 16),
            _ => panic!(),
        }
        // 12-bit packed, VBCA codec, and 9 channels.
        assert_eq!(
            parse_audio_packet(&pcm_packet(0x03, 4, 2, 0x06, &[0; 32], 0)),
            Err(PacketError::UnsupportedFormat)
        );
        assert_eq!(
            parse_audio_packet(&pcm_packet(0x03, 4, 2, 0x11, &[0; 32], 0)),
            Err(PacketError::UnsupportedFormat)
        );
        assert_eq!(
            parse_audio_packet(&pcm_packet(0x03, 4, 9, 0x01, &[0; 128], 0)),
            Err(PacketError::UnsupportedChannels(9))
        );
        // Sample-rate index 31 does not exist.
        assert_eq!(
            parse_audio_packet(&pcm_packet(0x1F, 4, 2, 0x01, &[0; 16], 0)),
            Err(PacketError::Header(VbanParseError::BadSampleRate(31)))
        );
    }

    #[test]
    fn user_codec_needs_our_tag() {
        let mut body = OPUS_USER_TAG.to_vec();
        body.extend([1, 2, 3]);
        let wire = pcm_packet(0x03, 120, 2, 0xF1, &body, 0);
        let p = parse_audio_packet(&wire).unwrap();
        assert_eq!(p.payload, RxPayload::Opus(&[1, 2, 3]));
        // Another application's USER payload is ignored, not misdecoded.
        assert_eq!(
            parse_audio_packet(&pcm_packet(0x03, 120, 2, 0xF1, b"XX\x01\x02", 0)),
            Err(PacketError::ForeignUser)
        );
        // Tag with nothing behind it.
        assert_eq!(
            parse_audio_packet(&pcm_packet(0x03, 120, 2, 0xF1, &OPUS_USER_TAG, 0)),
            Err(PacketError::Truncated)
        );
    }

    #[test]
    fn parser_survives_garbage() {
        // Deterministic xorshift: every prefix length of random bytes must return, not panic.
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for round in 0..400 {
            let len = (next() % 1500) as usize;
            let mut pkt: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            if round % 2 == 0 && len >= 8 {
                pkt[..4].copy_from_slice(b"VBAN");
                pkt[4] &= 0x1F; // audio
            }
            let _ = parse_audio_packet(&pkt);
        }
    }
    // !SECTION: Parsing

    // SECTION: Frame planning and channel mapping
    #[test]
    fn frames_per_packet_planning() {
        // Auto: 128 where it fits (default), else what fits.
        assert_eq!(plan_frames(0, 2, 2), 128);
        assert_eq!(plan_frames(0, 8, 2), 89);
        assert_eq!(plan_frames(0, 8, 4), 44);
        assert_eq!(plan_frames(0, 2, 4), 128);
        // Explicit: honoured up to what fits, never zero.
        assert_eq!(plan_frames(256, 2, 2), 256);
        assert_eq!(plan_frames(256, 8, 4), 44);
        assert_eq!(plan_frames(64, 2, 2), 64);
        assert_eq!(plan_frames(1, 2, 2), 1);
        for ch in 1..=8 {
            for bps in [2usize, 3, 4] {
                for req in [0u16, 1, 44, 128, 256, 1000] {
                    let f = plan_frames(req, ch, bps);
                    assert!(
                        f >= 1 && f * ch * bps <= VBAN_MAX_PAYLOAD_BYTES,
                        "{ch} {bps} {req}"
                    );
                }
            }
        }
    }

    #[test]
    fn packetizer_numbers_packets_and_wraps() {
        let mut tx = PcmPacketizer::new(48_000, 2, VbanDataType::Int16, 0, name("S")).unwrap();
        assert_eq!(tx.frames(), 128);
        assert_eq!(tx.packet_bytes(), 28 + 512);
        let audio = vec![0.25f32; 256];
        let mut buf = [0u8; VBAN_MAX_PACKET_BYTES];
        for want in 0..3u32 {
            let n = tx.build(&audio, &mut buf).unwrap();
            assert_eq!(n, 540);
            assert_eq!(
                parse_audio_packet(&buf[..n]).unwrap().header.frame_counter,
                want
            );
        }
        // Wrong sample count is refused and does not consume a counter value.
        assert!(tx.build(&audio[..255], &mut buf).is_none());
        assert_eq!(tx.next_counter(), 3);
        tx.header.frame_counter = u32::MAX;
        tx.build(&audio, &mut buf).unwrap();
        assert_eq!(tx.next_counter(), 0);
    }

    #[test]
    fn packetizer_rejects_unsendable_configs() {
        let n = name("S");
        assert!(PcmPacketizer::new(48_000, 2, VbanDataType::Int32, 0, n).is_none());
        assert!(PcmPacketizer::new(48_000, 0, VbanDataType::Int16, 0, n).is_none());
        assert!(PcmPacketizer::new(48_000, 9, VbanDataType::Int16, 0, n).is_none());
        assert!(
            PcmPacketizer::new(47_000, 2, VbanDataType::Int16, 0, n).is_none(),
            "no VBAN index"
        );
    }

    #[test]
    fn channel_mapping_rules() {
        let stereo = [1.0, 2.0, 3.0, 4.0];
        let mut o = [0.0f32; 8];
        map_channels(&stereo, 2, &mut o, 2, 2);
        assert_eq!(&o[..4], &stereo);
        // mono -> stereo duplicates
        map_channels(&[5.0, 6.0], 1, &mut o, 2, 2);
        assert_eq!(&o[..4], &[5.0, 5.0, 6.0, 6.0]);
        // stereo -> mono averages
        map_channels(&stereo, 2, &mut o, 1, 2);
        assert_eq!(&o[..2], &[1.5, 3.5]);
        // 8 -> 2 keeps the first two
        let eight: Vec<f32> = (0..16).map(|i| i as f32).collect();
        map_channels(&eight, 8, &mut o, 2, 2);
        assert_eq!(&o[..4], &[0.0, 1.0, 8.0, 9.0]);
        // 2 -> 4 zero-fills the extra channels (and overwrites stale data)
        o.fill(9.0);
        map_channels(&stereo, 2, &mut o, 4, 2);
        assert_eq!(o, [1.0, 2.0, 0.0, 0.0, 3.0, 4.0, 0.0, 0.0]);
    }
    // !SECTION: Frame planning and channel mapping
}
