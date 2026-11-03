//! Opus in the VBAN `User` codec slot (feature `opus`; PLAN.md §4.2, CLAUDE.md law 3).
//!
//! Wire format of one packet: the usual 28-byte VBAN header with `codec = User` and the data
//! type nibble set to Int16 (VBAN requires one; it is unused), `frames` = the Opus frame
//! length in samples per channel (120, 240 or 480), then `OPUS_USER_TAG` ("O1") and exactly
//! one Opus packet. Receivers that don't know the tag ignore the packet.
//!
//! * Encoder: `OPUS_APPLICATION_RESTRICTED_LOWDELAY` (CELT only, 2.5 ms algorithmic delay).
//!   That mode has no in-band FEC, so loss resilience is the codec's PLC plus the optional
//!   packet duplication of `LanTxConfig::duplicate`.
//! * Mono or stereo only (that is what the safe `opus` wrapper exposes).
// ? This whole file is compiled out without the feature, so the default build needs no C
// ? toolchain. With it, the `opus` crate builds libopus from its bundled source via cmake.

use crate::packet::map_channels;
use audio_types::{
    ENGINE_SAMPLE_RATE, FixedStr, OPUS_USER_TAG, OpusFrame, VBAN_HEADER_SIZE,
    VBAN_MAX_PACKET_BYTES, VbanAudioHeader, VbanCodec, VbanDataType,
};
use opus::{Application, Bitrate, Channels, Decoder, Encoder};

/// Samples per channel for an [`OpusFrame`] at 48 kHz.
pub const fn frame_samples(frame: OpusFrame) -> usize {
    match frame {
        OpusFrame::Ms2_5 => 120,
        OpusFrame::Ms5 => 240,
        OpusFrame::Ms10 => 480,
    }
}

fn channels_of(n: usize) -> Option<Channels> {
    match n {
        1 => Some(Channels::Mono),
        2 => Some(Channels::Stereo),
        _ => None,
    }
}

// SECTION: Receive
pub struct OpusRx {
    dec: Decoder,
    channels: usize,
    /// PLC output before it is mapped to the strip's channel count.
    scratch: Vec<f32>,
}

impl OpusRx {
    /// `None` for anything but mono/stereo.
    pub fn new(channels: usize) -> Option<Self> {
        Some(Self {
            dec: Decoder::new(ENGINE_SAMPLE_RATE, channels_of(channels)?).ok()?,
            channels,
            scratch: vec![0.0; 480 * 2],
        })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Decode one packet into `out`; returns frames per channel. `out` must hold at least
    /// 10 ms (480 frames x channels), which is also the longest frame we accept: libopus
    /// reports an error for a (hostile) packet that would need more room.
    pub fn decode(&mut self, packet: &[u8], out: &mut [f32]) -> Option<usize> {
        if packet.is_empty() {
            return None;
        }
        let room = (480 * self.channels).min(out.len());
        self.dec.decode_float(packet, &mut out[..room], false).ok()
    }

    /// Packet-loss concealment for `frames` samples, mapped to `out_ch` channels.
    /// Returns false if the codec could not produce it (the caller falls back to the PCM fade).
    pub fn conceal(&mut self, out: &mut [f32], out_ch: usize, frames: usize) -> bool {
        let n = frames * self.channels;
        if n == 0 || n > self.scratch.len() {
            return false;
        }
        // ? An empty input slice is how libopus is asked for PLC.
        match self.dec.decode_float(&[], &mut self.scratch[..n], false) {
            Ok(got) if got == frames => {
                map_channels(&self.scratch[..n], self.channels, out, out_ch, frames);
                true
            }
            _ => false,
        }
    }
}
// !SECTION: Receive

// SECTION: Transmit
pub struct OpusPacketizer {
    enc: Encoder,
    header: VbanAudioHeader,
    frame: usize,
    channels: usize,
}

impl OpusPacketizer {
    pub fn new(
        channels: usize,
        bitrate_kbps: u16,
        frame: OpusFrame,
        stream_name: FixedStr<16>,
    ) -> Option<Self> {
        let mut enc = Encoder::new(
            ENGINE_SAMPLE_RATE,
            channels_of(channels)?,
            Application::LowDelay,
        )
        .ok()?;
        enc.set_bitrate(Bitrate::Bits(i32::from(bitrate_kbps) * 1000))
            .ok()?;
        let frame_n = frame_samples(frame);
        Some(Self {
            enc,
            header: VbanAudioHeader {
                sample_rate: ENGINE_SAMPLE_RATE,
                // ! The header field is one byte (1..=256) and a 10 ms frame is 480 samples,
                // ! so this is informational only. Receivers take the real duration from the
                // ! Opus packet itself (see `OpusRx::decode`).
                frames: frame_n.min(256) as u16,
                channels: channels as u16,
                data_type: VbanDataType::Int16,
                codec: VbanCodec::User,
                stream_name,
                frame_counter: 0,
            },
            frame: frame_n,
            channels,
        })
    }

    pub fn frames(&self) -> usize {
        self.frame
    }
    pub fn samples_per_packet(&self) -> usize {
        self.frame * self.channels
    }
    pub fn next_counter(&self) -> u32 {
        self.header.frame_counter
    }

    /// Encode exactly one frame (`frames * channels` interleaved `f32`) into a datagram.
    pub fn build(&mut self, pcm: &[f32], out: &mut [u8; VBAN_MAX_PACKET_BYTES]) -> Option<usize> {
        if pcm.len() != self.samples_per_packet() {
            return None;
        }
        let (head, body) = out.split_at_mut(VBAN_HEADER_SIZE);
        self.header.encode(head.try_into().ok()?)?;
        body[..OPUS_USER_TAG.len()].copy_from_slice(&OPUS_USER_TAG);
        let n = self
            .enc
            .encode_float(pcm, &mut body[OPUS_USER_TAG.len()..])
            .ok()?;
        self.header.frame_counter = self.header.frame_counter.wrapping_add(1);
        Some(VBAN_HEADER_SIZE + OPUS_USER_TAG.len() + n)
    }
}
// !SECTION: Transmit

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{RxPayload, parse_audio_packet};

    fn sine(frames: usize, ch: usize, start: usize) -> Vec<f32> {
        (0..frames * ch)
            .map(|i| {
                let f = (start + i / ch) as f64;
                (0.5 * (f * 2.0 * std::f64::consts::PI * 440.0 / 48_000.0).sin()) as f32
            })
            .collect()
    }

    #[test]
    fn round_trip_preserves_the_tone_for_every_frame_size() {
        for frame in [OpusFrame::Ms2_5, OpusFrame::Ms5, OpusFrame::Ms10] {
            for ch in [1usize, 2] {
                let mut tx =
                    OpusPacketizer::new(ch, 128, frame, FixedStr::from_str_trunc("o")).unwrap();
                let mut rx = OpusRx::new(ch).unwrap();
                let n = tx.frames();
                let total = 48_000 / n;
                let mut src = Vec::new();
                let mut dst = Vec::new();
                let mut wire = [0u8; VBAN_MAX_PACKET_BYTES];
                let mut buf = vec![0.0f32; n * ch];
                for k in 0..total {
                    let pcm = sine(n, ch, k * n);
                    let len = tx.build(&pcm, &mut wire).unwrap();
                    let p = parse_audio_packet(&wire[..len]).unwrap();
                    assert_eq!(p.header.frames as usize, n.min(256), "informational");
                    assert_eq!(p.header.codec, VbanCodec::User);
                    let RxPayload::Opus(bytes) = p.payload else {
                        panic!("not opus")
                    };
                    assert_eq!(rx.decode(bytes, &mut buf), Some(n));
                    src.extend_from_slice(&pcm);
                    dst.extend_from_slice(&buf);
                }
                // Best alignment within +-10 ms (codec delay), then the correlation.
                let (mut best, mut best_lag) = (-1.0f64, 0isize);
                for lag in -480isize..=480 {
                    let (mut dot, mut ea, mut eb) = (0.0f64, 1e-12f64, 1e-12f64);
                    for (k, &a) in src[4800..src.len() - 4800].iter().enumerate() {
                        let j = ((4800 + k) as isize + lag * ch as isize) as usize;
                        let (a, b) = (f64::from(a), f64::from(dst[j]));
                        dot += a * b;
                        ea += a * a;
                        eb += b * b;
                    }
                    let r = dot / (ea * eb).sqrt();
                    if r > best {
                        best = r;
                        best_lag = lag;
                    }
                }
                assert!(
                    best > 0.99,
                    "{frame:?} ch={ch}: corr {best} at lag {best_lag}"
                );
            }
        }
    }

    #[test]
    fn counter_increments_and_rejects_wrong_sizes() {
        let mut tx =
            OpusPacketizer::new(2, 96, OpusFrame::Ms5, FixedStr::from_str_trunc("o")).unwrap();
        let mut wire = [0u8; VBAN_MAX_PACKET_BYTES];
        let pcm = sine(240, 2, 0);
        tx.build(&pcm, &mut wire).unwrap();
        tx.build(&pcm, &mut wire).unwrap();
        assert_eq!(tx.next_counter(), 2);
        assert!(tx.build(&pcm[..100], &mut wire).is_none());
        assert_eq!(tx.next_counter(), 2);
        assert!(
            OpusPacketizer::new(3, 96, OpusFrame::Ms5, FixedStr::from_str_trunc("o")).is_none()
        );
        assert!(OpusRx::new(8).is_none());
    }

    #[test]
    fn plc_continues_the_tone_instead_of_dropping_to_silence() {
        let mut tx =
            OpusPacketizer::new(1, 128, OpusFrame::Ms5, FixedStr::from_str_trunc("o")).unwrap();
        let mut rx = OpusRx::new(1).unwrap();
        let mut wire = [0u8; VBAN_MAX_PACKET_BYTES];
        let mut out = vec![0.0f32; 240];
        // Warm the decoder with 40 frames (200 ms) of tone.
        for k in 0..40 {
            let len = tx.build(&sine(240, 1, k * 240), &mut wire).unwrap();
            let p = parse_audio_packet(&wire[..len]).unwrap();
            let RxPayload::Opus(b) = p.payload else {
                panic!()
            };
            rx.decode(b, &mut out).unwrap();
        }
        let mut plc = vec![0.0f32; 240];
        assert!(rx.conceal(&mut plc, 1, 240));
        let rms = (plc.iter().map(|s| s * s).sum::<f32>() / 240.0).sqrt();
        assert!(
            rms > 0.15,
            "PLC rms {rms}: should still carry the 0.35-rms tone"
        );
        // Mapping the PLC output into a stereo strip duplicates it.
        let mut st = vec![0.0f32; 480];
        assert!(rx.conceal(&mut st, 2, 240));
        assert!(st.chunks(2).all(|f| f[0] == f[1]));
    }

    #[test]
    fn garbage_opus_packets_do_not_panic() {
        let mut rx = OpusRx::new(2).unwrap();
        let mut out = vec![0.0f32; 480 * 2];
        assert_eq!(rx.decode(&[], &mut out), None);
        let mut x = 0x1234_5678_9ABC_DEF0u64;
        for _ in 0..500 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = 1 + (x % 200) as usize;
            let pkt: Vec<u8> = (0..len).map(|i| (x >> (i % 56)) as u8).collect();
            let _ = rx.decode(&pkt, &mut out);
        }
    }
}
