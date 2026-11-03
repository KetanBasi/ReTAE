//! One incoming LAN stream: parsed packet -> f32 -> reorder window -> concealment -> the
//! strip's input ring (PLAN.md §5.2).
//!
//! This is the whole receive path *minus* the socket and the clock, so it can be driven by the
//! RX thread in production and by a virtual clock in the simulations.
//!
//! # When to give up on a missing packet
//! The ring's fill is the time until playback runs dry. A hole at the head of the reorder
//! window holds later packets back, so as long as the ring still has comfortable fill we keep
//! waiting for the straggler. Once the fill drops below `guard` we stop waiting, conceal the
//! hole and release the packets behind it. That adds **no** latency of its own: the wait is
//! paid out of audio that was already buffered.
//!
//! ```text
//!   fill  ──────────────┐
//!   (frames)            │  wait for the late packet here (it may still arrive)
//!                guard ─┼─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─
//!                       │  give up: conceal + release the held packets
//!                  0 ───┘  (underrun; the engine's adapter fades out and rebuffers)
//! ```
// ? guard = one engine block + one packet. The block is what the mixer takes in a single
// ? pull; the packet is how far the fill can fall between two of our checks (we run once per
// ? arrival and once per 5 ms socket timeout).

use crate::jitter::{self, Concealer, PlayoutFifo, ms_to_frames};
use crate::packet::{self, MAX_PACKET_SAMPLES, ParsedAudio, RxPayload};
use crate::reorder::{Emit, ReorderWindow};
use audio_types::{
    ENGINE_BLOCK_FRAMES, ENGINE_SAMPLE_RATE, NetworkBufferPreset, Telemetry, store_f32,
};
use rtrb::Producer;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

/// Everything the control thread / UI may want to know about one stream. Written only by the
/// RX thread; all loads and stores are `Relaxed`.
#[derive(Default)]
pub struct StreamCounters {
    /// Packets that entered the reorder window.
    pub received: AtomicU32,
    pub lost: AtomicU32,
    pub late: AtomicU32,
    pub duplicate: AtomicU32,
    pub resyncs: AtomicU32,
    /// Frames of concealment inserted for lost packets.
    pub concealed_frames: AtomicU32,
    /// Packets concealed *before* anything showed they were missing, because the ring was
    /// about to run dry (see `StreamRx::tick`). Each also counts in `lost`; if the packet
    /// does turn up afterwards it counts in `late` as well.
    pub predicted: AtomicU32,
    /// Frames refused because the play-out FIFO was at 4 x target (consumer not draining).
    pub dropped_frames: AtomicU32,
    /// Packets at a sample rate other than the engine's 48 kHz.
    pub wrong_rate: AtomicU32,
    /// Opus packets with the `opus` feature off, or a codec/format we cannot decode.
    pub unsupported: AtomicU32,
    /// Payload that did not decode to the advertised size.
    pub bad_payload: AtomicU32,
    // * Observed format, for the stream inspector (PLAN.md §9.5).
    pub sample_rate: AtomicU32,
    pub channels: AtomicU32,
    pub frames_per_packet: AtomicU32,
    /// `VbanDataType` byte, or 0xF0 for Opus.
    pub format: AtomicU32,
    /// f32 bits: the jitter target for this stream. The control plane sends it to the engine
    /// as `ClockDomain::External { target_ms }`.
    pub target_ms: AtomicU32,
}

impl StreamCounters {
    pub fn target_ms(&self) -> f32 {
        f32::from_bits(self.target_ms.load(Relaxed))
    }
    pub fn get(c: &AtomicU32) -> u32 {
        c.load(Relaxed)
    }
}

/// Scratch + sinks needed to emit audio. Split from `StreamRx` so the borrow checker can see
/// that the reorder window and the output side are disjoint.
struct Output<'a> {
    fifo: &'a mut PlayoutFifo,
    concealer: &'a mut Concealer,
    conceal_buf: &'a mut [f32],
    counters: &'a StreamCounters,
    /// Don't pad the ring with concealment beyond this fill (2 x target = the point where
    /// the engine's adapter starts dropping).
    conceal_cap_frames: usize,
    #[cfg(feature = "opus")]
    opus: &'a mut Option<crate::opus_codec::OpusRx>,
}

impl Output<'_> {
    fn emit(&mut self, e: Emit<'_>) {
        let ch = self.fifo.channels();
        match e {
            Emit::Packet {
                frames, samples, ..
            } => {
                self.concealer.begin_packet(samples, ch);
                if self.fifo.push(samples) {
                    self.concealer.observe(samples, ch);
                } else {
                    self.counters
                        .dropped_frames
                        .fetch_add(frames as u32, Relaxed);
                }
            }
            Emit::Lost { frames, .. } => {
                if self.fifo.fill_frames() + frames > self.conceal_cap_frames {
                    return;
                }
                let buf = &mut self.conceal_buf[..frames * ch];
                #[cfg(feature = "opus")]
                let done = self
                    .opus
                    .as_mut()
                    .is_some_and(|o| o.conceal(buf, ch, frames));
                #[cfg(not(feature = "opus"))]
                let done = false;
                if done {
                    // ? The codec's own PLC produced the audio; keep the concealer's state in
                    // ? step so a later PCM stream doesn't fade in from stale data.
                    self.concealer.observe(buf, ch);
                } else {
                    self.concealer.conceal(buf, ch);
                }
                if self.fifo.push(buf) {
                    self.counters
                        .concealed_frames
                        .fetch_add(frames as u32, Relaxed);
                } else {
                    self.counters
                        .dropped_frames
                        .fetch_add(frames as u32, Relaxed);
                }
            }
        }
    }
}

/// Most packets in a row that `StreamRx::predict` conceals without hearing from the sender.
/// 4 x 2.67 ms = 10.7 ms: enough to bridge a short loss burst, short of covering a dead sender.
pub const MAX_PREDICTED_RUN: u32 = 4;

pub struct StreamRx {
    fifo: PlayoutFifo,
    window: ReorderWindow,
    concealer: Concealer,
    decode: Vec<f32>,
    mapped: Vec<f32>,
    conceal_buf: Vec<f32>,
    preset: NetworkBufferPreset,
    /// Adaptive-mode override; the larger of this and the preset's effective target wins.
    target_override_ms: Option<f32>,
    packet_frames: usize,
    /// Packets in a row concealed by prediction since the last real arrival.
    predicted_run: u32,
    /// The RX thread's clock in microseconds ([`set_time_us`](Self::set_time_us)) and when the
    /// last packet arrived on it. Both stay 0 until the owner supplies a clock, which turns
    /// prediction off (nothing can ever look overdue).
    now_us: u64,
    last_arrival_us: u64,
    counters: Arc<StreamCounters>,
    telemetry: Option<(Arc<Telemetry>, usize)>,
    /// (wire channels, is_opus): a change restarts the run.
    format: Option<(u16, bool)>,
    #[cfg(feature = "opus")]
    opus: Option<crate::opus_codec::OpusRx>,
}

impl StreamRx {
    /// Build the per-stream state. Allocates (about 50 KB); call on the control thread.
    /// `channels` is the channel count of the strip the ring feeds.
    pub fn new(
        producer: Producer<f32>,
        channels: usize,
        preset: NetworkBufferPreset,
        counters: Arc<StreamCounters>,
    ) -> Self {
        let mut s = Self {
            fifo: PlayoutFifo::new(producer, channels),
            window: ReorderWindow::new(),
            concealer: Concealer::new(),
            decode: vec![0.0; MAX_PACKET_SAMPLES],
            mapped: vec![0.0; MAX_PACKET_SAMPLES],
            conceal_buf: vec![0.0; MAX_PACKET_SAMPLES],
            preset,
            target_override_ms: None,
            packet_frames: ENGINE_BLOCK_FRAMES,
            predicted_run: 0,
            now_us: 0,
            last_arrival_us: 0,
            counters,
            telemetry: None,
            format: None,
            #[cfg(feature = "opus")]
            opus: None,
        };
        s.refresh_target();
        s
    }

    /// Mirror loss/late counters into `Telemetry::lan_rx[index]` (the strip's slot).
    pub fn attach_telemetry(&mut self, telemetry: Arc<Telemetry>, index: usize) {
        self.telemetry = Some((telemetry, index));
    }

    pub fn counters(&self) -> &Arc<StreamCounters> {
        &self.counters
    }

    /// Tell the stream what time it is (any monotonic microsecond clock, same one every call).
    /// Call before [`on_audio`](Self::on_audio) and [`tick`](Self::tick). Only predictive
    /// concealment uses it: a packet is "overdue" once more than 1.5 packet periods have
    /// passed since the last arrival.
    // ? Why the ring's fill alone is not enough: with a sender clock a little off, the fill
    // ? sits just under one block for a moment after every mixer pull and the next packet is
    // ? simply due soon. Only "no packet for 1.5 periods" separates a late/lost packet from
    // ? that normal wobble.
    pub fn set_time_us(&mut self, now_us: u64) {
        self.now_us = now_us;
    }

    pub fn fill_frames(&self) -> usize {
        self.fifo.fill_frames()
    }

    /// A hole is holding later packets back. The RX thread shortens its socket timeout while
    /// this is true, so the give-up rule in [`tick`](Self::tick) is checked often enough.
    // ? The give-up guard is one block + one packet of fill. If the thread only looked every
    // ? 5 ms the fill could fall from just above the guard to almost nothing between two looks,
    // ? and the mixer would underrun before we ever got to conceal. At 1 ms the fill can fall
    // ? by only 48 frames per look, well inside the guard.
    pub fn is_blocked(&self) -> bool {
        self.window.is_blocked()
    }

    /// The thread should look at this stream every millisecond: either a hole is being waited
    /// on, or the ring is low enough that the *next* mixer pull could leave it below one block
    /// with the expected packet still missing (see [`tick`](Self::tick)).
    // ? Fill only ever falls at a mixer pull, by one block. So "fill < guard" now means "one
    // ? pull from the danger line"; a 5 ms socket timeout could then sleep through it.
    pub fn wants_fast_tick(&self) -> bool {
        self.window.is_blocked()
            || (self.window.is_started()
                && self.predicted_run < MAX_PREDICTED_RUN
                && self.fifo.fill_frames() < self.guard_frames())
    }

    pub fn target_ms(&self) -> f32 {
        self.counters.target_ms()
    }

    pub fn set_preset(&mut self, preset: NetworkBufferPreset) {
        self.preset = preset;
        self.refresh_target();
    }

    /// Adaptive mode: raise the target above the preset. `None` goes back to the preset.
    pub fn set_target_override_ms(&mut self, ms: Option<f32>) {
        self.target_override_ms = ms;
        self.refresh_target();
    }

    /// Give the ring back (stream removed).
    pub fn into_producer(self) -> Producer<f32> {
        self.fifo.into_producer()
    }

    fn packet_ms(&self) -> f32 {
        self.packet_frames as f32 / jitter::FRAMES_PER_MS
    }

    fn refresh_target(&mut self) {
        let mut t = jitter::target_ms(self.preset, self.packet_ms());
        if let Some(o) = self.target_override_ms {
            t = t.max(o);
        }
        self.fifo.set_target_frames(ms_to_frames(t));
        store_f32_bits(&self.counters.target_ms, t);
    }

    /// More than 1.5 packet periods since the last arrival (see `set_time_us`).
    fn overdue(&self) -> bool {
        let period_us = self.packet_frames as u64 * 1_000_000 / u64::from(ENGINE_SAMPLE_RATE);
        let k = (1 + self.predicted_run) as u64; // each predicted packet pushes the deadline out
        self.now_us.saturating_sub(self.last_arrival_us) > period_us * (2 * k + 1) / 2
    }

    fn guard_frames(&self) -> usize {
        ENGINE_BLOCK_FRAMES + self.packet_frames
    }

    /// Process one parsed audio packet.
    pub fn on_audio(&mut self, parsed: &ParsedAudio<'_>) {
        let h = &parsed.header;
        if h.sample_rate != ENGINE_SAMPLE_RATE {
            // ? A resampler for 44.1 kHz and friends is future work; count it so the UI can
            // ? say "stream is 44.1 kHz, unsupported" instead of showing silence.
            self.counters.wrong_rate.fetch_add(1, Relaxed);
            self.counters.sample_rate.store(h.sample_rate, Relaxed);
            return;
        }
        let src_ch = h.channels as usize;
        let is_opus = matches!(parsed.payload, RxPayload::Opus(_));

        if self.format != Some((h.channels, is_opus)) {
            // A different channel count or codec is a different stream: start a new run.
            self.window.clear();
            self.concealer.reset();
            self.format = Some((h.channels, is_opus));
            #[cfg(feature = "opus")]
            {
                self.opus = None;
            }
        }

        // Decode into `self.decode`; `frames` is the real duration of this packet.
        let frames = match parsed.payload {
            RxPayload::Pcm { data_type, bytes } => {
                let frames = h.frames as usize;
                let n = frames * src_ch;
                match packet::decode_pcm(data_type, bytes, &mut self.decode[..n]) {
                    Some(m) if m == n => frames,
                    _ => {
                        self.counters.bad_payload.fetch_add(1, Relaxed);
                        return;
                    }
                }
            }
            // ? Opus: the header's `frames` is informational (10 ms = 480 doesn't fit its
            // ? one-byte field), so the decoder is the authority on the packet's duration.
            RxPayload::Opus(bytes) => match self.decode_opus(bytes, src_ch) {
                Some(frames) => frames,
                None => return,
            },
        };
        let n = frames * src_ch;

        if frames != self.packet_frames {
            self.packet_frames = frames;
            self.refresh_target();
        }
        self.counters.sample_rate.store(h.sample_rate, Relaxed);
        self.counters.channels.store(h.channels as u32, Relaxed);
        self.counters
            .frames_per_packet
            .store(frames as u32, Relaxed);
        self.counters
            .format
            .store(if is_opus { 0xF0 } else { h.data_type as u32 }, Relaxed);

        let out_ch = self.fifo.channels();
        packet::map_channels(
            &self.decode[..n],
            src_ch,
            &mut self.mapped[..frames * out_ch],
            out_ch,
            frames,
        );

        // A packet arrived, so the sender is alive: prediction may start over.
        self.predicted_run = 0;
        self.last_arrival_us = self.now_us;
        let mut out = Output {
            fifo: &mut self.fifo,
            concealer: &mut self.concealer,
            conceal_buf: &mut self.conceal_buf,
            counters: &self.counters,
            conceal_cap_frames: 2 * ms_to_frames(self.counters.target_ms()),
            #[cfg(feature = "opus")]
            opus: &mut self.opus,
        };
        self.window.push(
            h.frame_counter,
            frames,
            &self.mapped[..frames * out_ch],
            &mut |e| out.emit(e),
        );
        self.tick();
    }

    /// Decode one Opus packet into `self.decode`; returns frames per channel.
    #[cfg(feature = "opus")]
    fn decode_opus(&mut self, bytes: &[u8], src_ch: usize) -> Option<usize> {
        if self.opus.as_ref().is_none_or(|o| o.channels() != src_ch) {
            self.opus = crate::opus_codec::OpusRx::new(src_ch);
            if self.opus.is_none() {
                self.counters.unsupported.fetch_add(1, Relaxed); // e.g. 6-channel Opus
                return None;
            }
        }
        let frames = self.opus.as_mut()?.decode(bytes, &mut self.decode);
        if frames.is_none() {
            self.counters.bad_payload.fetch_add(1, Relaxed);
        }
        frames
    }

    /// Without the `opus` feature the packet is counted and dropped.
    #[cfg(not(feature = "opus"))]
    fn decode_opus(&mut self, _: &[u8], _: usize) -> Option<usize> {
        self.counters.unsupported.fetch_add(1, Relaxed);
        None
    }

    /// Run the "give up on the head-of-line hole" rule and publish counters. Call after every
    /// packet and on every socket timeout.
    pub fn tick(&mut self) {
        let guard = self.guard_frames();
        while self.window.is_blocked() && self.fifo.fill_frames() < guard {
            let mut out = Output {
                fifo: &mut self.fifo,
                concealer: &mut self.concealer,
                conceal_buf: &mut self.conceal_buf,
                counters: &self.counters,
                conceal_cap_frames: 2 * ms_to_frames(self.counters.target_ms()),
                #[cfg(feature = "opus")]
                opus: &mut self.opus,
            };
            self.window.give_up_head(&mut |e| out.emit(e));
        }
        self.predict();
        self.publish();
    }

    /// Predictive concealment. Nothing is held behind the expected packet (so the rule above
    /// has nothing to go on), yet the ring has fallen below one engine block: the next mixer
    /// pull would underrun. Waiting any longer can only turn into a fade-out and a long
    /// rebuffer, so treat the expected packet as lost *now* and play the concealment instead.
    ///
    /// This is what lets the Optimal preset (about two packets deep) survive two lost packets
    /// in a row: the second hole is never revealed by a later arrival.
    // ? The trade-off: a packet that is merely very late (not lost) is concealed too, and
    // ? then dropped as `late` when it finally arrives. That costs a short dip, against the
    // ? alternative of an underrun that costs the dip *plus* a rebuffer. The run is capped so
    // ? a sender that stopped for good gets a real underrun (clean fade-out) and not endless
    // ? synthetic silence.
    fn predict(&mut self) {
        // ? `overdue` is re-read each pass: after one prediction the *next* packet counts from
        // ? the same last arrival, so a long outage keeps being bridged (up to the cap).
        while self.window.is_started()
            && !self.window.is_blocked()
            && self.predicted_run < MAX_PREDICTED_RUN
            && self.fifo.fill_frames() < ENGINE_BLOCK_FRAMES
            && self.overdue()
        {
            let mut out = Output {
                fifo: &mut self.fifo,
                concealer: &mut self.concealer,
                conceal_buf: &mut self.conceal_buf,
                counters: &self.counters,
                conceal_cap_frames: 2 * ms_to_frames(self.counters.target_ms()),
                #[cfg(feature = "opus")]
                opus: &mut self.opus,
            };
            self.window.give_up_head(&mut |e| out.emit(e));
            self.predicted_run += 1;
            self.counters.predicted.fetch_add(1, Relaxed);
        }
    }

    fn publish(&self) {
        let s = self.window.stats();
        let c = &self.counters;
        c.received.store(s.received, Relaxed);
        c.lost.store(s.lost, Relaxed);
        c.late.store(s.late, Relaxed);
        c.duplicate.store(s.duplicate, Relaxed);
        c.resyncs.store(s.resyncs, Relaxed);
        if let Some((tel, i)) = &self.telemetry {
            let lan = &tel.lan_rx[*i];
            lan.packets_lost.store(s.lost, Relaxed);
            lan.packets_late.store(s.late, Relaxed);
        }
    }

    /// Publish how long ago the last packet arrived (the RX thread owns the clock).
    pub fn publish_age_ms(&self, ms: u32) {
        if let Some((tel, i)) = &self.telemetry {
            tel.lan_rx[*i].last_packet_ms_ago.store(ms, Relaxed);
        }
    }
}

fn store_f32_bits(cell: &AtomicU32, v: f32) {
    store_f32(cell, v);
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_types::{FixedStr, VBAN_HEADER_SIZE, VBAN_MAX_PACKET_BYTES, VbanDataType};
    use rtrb::RingBuffer;

    pub(crate) struct Harness {
        pub rx: StreamRx,
        pub cons: rtrb::Consumer<f32>,
        tx: packet::PcmPacketizer,
        pub ch: usize,
    }

    impl Harness {
        /// Float32 on the wire (bit-exact), so sample values can be checked exactly.
        pub fn new(ch: usize, frames: u16, preset: NetworkBufferPreset) -> Self {
            Self::with_type(ch, frames, preset, VbanDataType::Float32)
        }

        pub fn with_type(
            ch: usize,
            frames: u16,
            preset: NetworkBufferPreset,
            dt: VbanDataType,
        ) -> Self {
            let (p, c) = RingBuffer::<f32>::new(8 * 4096);
            let counters = Arc::new(StreamCounters::default());
            Self {
                rx: StreamRx::new(p, ch, preset, counters),
                cons: c,
                tx: packet::PcmPacketizer::new(
                    ENGINE_SAMPLE_RATE,
                    ch,
                    dt,
                    frames,
                    FixedStr::from_str_trunc("t"),
                )
                .unwrap(),
                ch,
            }
        }

        /// Build wire packet number `next_counter` carrying `value` in every sample.
        pub fn packet(&mut self, fill: impl Fn(usize) -> f32) -> Vec<u8> {
            let n = self.tx.samples_per_packet();
            let samples: Vec<f32> = (0..n).map(|i| fill(i / self.ch)).collect();
            let mut buf = [0u8; VBAN_MAX_PACKET_BYTES];
            let len = self.tx.build(&samples, &mut buf).unwrap();
            buf[..len].to_vec()
        }

        pub fn feed(&mut self, wire: &[u8]) {
            let parsed = packet::parse_audio_packet(wire).unwrap();
            self.rx.on_audio(&parsed);
        }

        pub fn drain(&mut self) -> Vec<f32> {
            let n = self.cons.slots();
            let chunk = self.cons.read_chunk(n).unwrap();
            let (a, b) = chunk.as_slices();
            let v = [a, b].concat();
            chunk.commit_all();
            v
        }
    }

    #[test]
    fn in_order_stream_lands_in_the_ring_unchanged() {
        let mut h = Harness::new(2, 128, NetworkBufferPreset::Fast);
        for k in 0..4 {
            let w = h.packet(|f| (k * 128 + f) as f32 * 1e-4);
            h.feed(&w);
        }
        let got = h.drain();
        assert_eq!(got.len(), 4 * 128 * 2);
        for (i, s) in got.iter().enumerate() {
            assert_eq!(*s, (i / 2) as f32 * 1e-4, "sample {i}");
        }
        let c = h.rx.counters();
        assert_eq!(StreamCounters::get(&c.received), 4);
        assert_eq!(StreamCounters::get(&c.channels), 2);
        assert_eq!(StreamCounters::get(&c.frames_per_packet), 128);
        assert_eq!(StreamCounters::get(&c.format), VbanDataType::Float32 as u32);
    }

    #[test]
    fn target_follows_preset_and_packet_size() {
        let mut h = Harness::new(2, 128, NetworkBufferPreset::Optimal);
        // Before any packet: assumes 128 frames. Optimal = 6 ms >= 2 x 2.67.
        assert_eq!(h.rx.target_ms(), 6.0);
        let w = h.packet(|_| 0.0);
        h.feed(&w);
        assert_eq!(h.rx.target_ms(), 6.0);
        // 256-frame packets (5.33 ms): Optimal is clamped to 2 packets = 10.67 ms.
        // ? Int16 stereo fits 256 frames per packet (Float32 only 179), as VoiceMeeter sends.
        let mut h = Harness::with_type(2, 256, NetworkBufferPreset::Optimal, VbanDataType::Int16);
        let w = h.packet(|_| 0.0);
        h.feed(&w);
        assert!(
            (h.rx.target_ms() - 10.6667).abs() < 1e-3,
            "{}",
            h.rx.target_ms()
        );
        h.rx.set_preset(NetworkBufferPreset::Slow);
        assert_eq!(h.rx.target_ms(), 48.0);
        h.rx.set_target_override_ms(Some(60.0));
        assert_eq!(h.rx.target_ms(), 60.0);
        h.rx.set_target_override_ms(Some(10.0));
        assert_eq!(
            h.rx.target_ms(),
            48.0,
            "an override never lowers the preset"
        );
    }

    #[test]
    fn channel_count_mismatch_is_mapped_to_the_strip() {
        // Mono sender into a stereo strip: duplicated.
        let mut h = Harness::new(2, 128, NetworkBufferPreset::Fast);
        let mut mono = packet::PcmPacketizer::new(
            ENGINE_SAMPLE_RATE,
            1,
            VbanDataType::Float32,
            0,
            FixedStr::from_str_trunc("m"),
        )
        .unwrap();
        let mut buf = [0u8; VBAN_MAX_PACKET_BYTES];
        let samples: Vec<f32> = (0..128).map(|i| i as f32 / 128.0).collect();
        let n = mono.build(&samples, &mut buf).unwrap();
        h.feed(&buf[..n]);
        let got = h.drain();
        assert_eq!(got.len(), 256);
        for f in 0..128 {
            assert_eq!(got[2 * f], samples[f]);
            assert_eq!(got[2 * f + 1], samples[f]);
        }
    }

    #[test]
    fn wrong_sample_rate_is_counted_not_played() {
        let mut h = Harness::new(2, 128, NetworkBufferPreset::Fast);
        let mut tx44 = packet::PcmPacketizer::new(
            44_100,
            2,
            VbanDataType::Int16,
            0,
            FixedStr::from_str_trunc("x"),
        )
        .unwrap();
        let mut buf = [0u8; VBAN_MAX_PACKET_BYTES];
        let n = tx44.build(&vec![0.1; 256], &mut buf).unwrap();
        h.feed(&buf[..n]);
        assert!(h.drain().is_empty());
        let c = h.rx.counters();
        assert_eq!(StreamCounters::get(&c.wrong_rate), 1);
        assert_eq!(StreamCounters::get(&c.sample_rate), 44_100);
    }

    #[test]
    fn a_lost_packet_is_concealed_once_the_ring_runs_low_and_not_before() {
        let mut h = Harness::new(2, 128, NetworkBufferPreset::Fast);
        let level = 0.5f32;
        let w0 = h.packet(|_| level);
        let _lost = h.packet(|_| level); // packet 1 never arrives
        let w2 = h.packet(|_| level);
        let w3 = h.packet(|_| level);
        h.feed(&w0);
        h.feed(&w2);
        h.feed(&w3);
        // Fill is 128 frames (< guard 256), so the stream gave up on packet 1 immediately.
        let got = h.drain();
        assert_eq!(got.len(), 4 * 128 * 2, "0, concealment(1), 2, 3");
        assert_eq!(StreamCounters::get(&h.rx.counters().lost), 1);
        assert_eq!(StreamCounters::get(&h.rx.counters().concealed_frames), 128);
        // The concealed block fades out from the level and ends in silence.
        let gap = &got[256..512];
        assert!((gap[0] - level).abs() < 0.01);
        assert!(
            gap[2 * 127..].iter().all(|&s| s == 0.0),
            "fade completes inside the gap"
        );
        assert!(
            gap.windows(2)
                .step_by(2)
                .all(|w| w[1].abs() <= w[0].abs() + 1e-6),
            "no bump"
        );

        // Now with plenty of fill: the same hole is waited on, because there is time.
        let mut h = Harness::new(2, 128, NetworkBufferPreset::Fast);
        let ws: Vec<_> = (0..8).map(|_| h.packet(|_| level)).collect();
        for w in [0, 1, 2, 3, 4] {
            h.feed(&ws[w]);
        } // 640 frames in the ring, well above the 256 guard
        h.feed(&ws[6]); // 5 is missing
        assert_eq!(
            h.rx.fill_frames(),
            5 * 128,
            "6 is held behind the hole, nothing concealed"
        );
        assert_eq!(StreamCounters::get(&h.rx.counters().lost), 0);
        h.feed(&ws[5]); // the straggler shows up
        assert_eq!(h.rx.fill_frames(), 7 * 128);
        assert_eq!(StreamCounters::get(&h.rx.counters().concealed_frames), 0);
        // And it is played, not concealed.
        assert!(h.drain().iter().all(|&s| (s - level).abs() < 1e-6));
    }

    #[test]
    fn a_hole_is_given_up_when_the_ring_drains_to_the_guard() {
        let mut h = Harness::new(2, 128, NetworkBufferPreset::Fast);
        let ws: Vec<_> = (0..12).map(|_| h.packet(|_| 0.25)).collect();
        for w in &ws[..5] {
            h.feed(w);
        }
        h.feed(&ws[6]);
        assert_eq!(StreamCounters::get(&h.rx.counters().lost), 0);
        // The consumer eats one block at a time; the stream keeps waiting for packet 5 while
        // the fill is comfortable (640, 512, 384, 256 frames: none is below the guard of 256).
        for expect_fill in [512, 384, 256] {
            for _ in 0..128 * 2 {
                h.cons.pop().unwrap();
            }
            h.rx.tick();
            assert_eq!(h.rx.fill_frames(), expect_fill);
            assert_eq!(
                StreamCounters::get(&h.rx.counters().lost),
                0,
                "still waiting"
            );
        }
        // One more block taken: fill 128 < guard 256. It gives up, conceals, releases packet 6.
        for _ in 0..128 * 2 {
            h.cons.pop().unwrap();
        }
        h.rx.tick();
        assert_eq!(StreamCounters::get(&h.rx.counters().lost), 1);
        assert_eq!(StreamCounters::get(&h.rx.counters().concealed_frames), 128);
        assert_eq!(
            h.rx.fill_frames(),
            128 + 128 + 128,
            "old fill + concealment + packet 6"
        );
    }

    #[test]
    fn duplicated_and_swapped_packets_come_out_as_one_clean_stream() {
        let mut h = Harness::new(2, 128, NetworkBufferPreset::Fast);
        let ws: Vec<_> = (0..6)
            .map(|k| h.packet(|f| k as f32 + f as f32 * 1e-4))
            .collect();
        for &k in &[0, 1, 1, 3, 2, 4, 4, 5] {
            h.feed(&ws[k]);
        }
        let got = h.drain();
        assert_eq!(got.len(), 6 * 128 * 2);
        for k in 0..6 {
            assert_eq!(got[k * 256], k as f32, "packet {k} in position");
        }
        let c = h.rx.counters();
        assert_eq!(StreamCounters::get(&c.duplicate), 2);
        assert_eq!(StreamCounters::get(&c.lost), 0);
        assert_eq!(StreamCounters::get(&c.concealed_frames), 0);
    }

    #[test]
    fn the_fifo_cap_protects_against_a_stalled_consumer() {
        let mut h = Harness::new(2, 128, NetworkBufferPreset::Optimal);
        // Cap = 4 x 6 ms = 4 x 288 frames = 1152 frames = 9 packets.
        for _ in 0..40 {
            let w = h.packet(|_| 0.1);
            h.feed(&w);
        }
        assert_eq!(h.rx.fill_frames(), 9 * 128);
        let dropped = StreamCounters::get(&h.rx.counters().dropped_frames);
        assert_eq!(dropped, 31 * 128);
    }

    #[test]
    fn telemetry_receives_loss_and_late_counts() {
        let mut h = Harness::new(2, 128, NetworkBufferPreset::Fast);
        let tel = Arc::new(Telemetry::default());
        h.rx.attach_telemetry(tel.clone(), 3);
        let ws: Vec<_> = (0..4).map(|_| h.packet(|_| 0.1)).collect();
        h.feed(&ws[0]);
        h.feed(&ws[2]); // 1 lost (ring is low -> immediate give-up)
        h.feed(&ws[1]); // ... and then it is late
        h.rx.publish_age_ms(7);
        let l = &tel.lan_rx[3];
        assert_eq!(l.packets_lost.load(Relaxed), 1);
        assert_eq!(l.packets_late.load(Relaxed), 1);
        assert_eq!(l.last_packet_ms_ago.load(Relaxed), 7);
        assert_eq!(
            tel.lan_rx[2].packets_lost.load(Relaxed),
            0,
            "only its own slot"
        );
    }

    #[test]
    fn a_format_change_restarts_the_run() {
        let mut h = Harness::new(2, 128, NetworkBufferPreset::Fast);
        let w = h.packet(|_| 0.1);
        h.feed(&w);
        // Same stream name, now 1 channel and counter restarting at 0 (sender reconfigured).
        let mut mono = packet::PcmPacketizer::new(
            ENGINE_SAMPLE_RATE,
            1,
            VbanDataType::Int16,
            0,
            FixedStr::from_str_trunc("t"),
        )
        .unwrap();
        let mut buf = [0u8; VBAN_MAX_PACKET_BYTES];
        let n = mono.build(&vec![0.2; 128], &mut buf).unwrap();
        h.feed(&buf[..n]);
        assert_eq!(StreamCounters::get(&h.rx.counters().channels), 1);
        let got = h.drain();
        assert_eq!(got.len(), 2 * 128 * 2);
        assert_eq!(
            StreamCounters::get(&h.rx.counters().lost),
            0,
            "restart is not a loss"
        );
    }

    #[test]
    fn corrupt_payloads_are_counted_and_never_reach_the_ring() {
        let mut h = Harness::new(2, 128, NetworkBufferPreset::Fast);
        let w = h.packet(|_| 0.1);
        // Opus without the feature (or with a bogus packet) is rejected and counted.
        let mut o = w.clone();
        o[7] = 0xF1;
        o.truncate(VBAN_HEADER_SIZE);
        // ! TOC 0xFF = "code 3, 63 frames": longer than libopus allows, so it is invalid.
        // ! (A lone TOC byte like 0x00 is a *valid* empty frame, which is why not that.)
        o.extend_from_slice(b"O1\xFF\xFF");
        if let Ok(parsed) = packet::parse_audio_packet(&o) {
            h.rx.on_audio(&parsed);
        }
        assert!(h.drain().is_empty());
        let c = h.rx.counters();
        assert!(StreamCounters::get(&c.bad_payload) + StreamCounters::get(&c.unsupported) >= 1);
    }

    // SECTION: predictive concealment
    //
    // ? Time is virtual: `set_time_us` is the only clock the stream sees. One packet is 128
    // ? frames = 2667 us, so "overdue" means more than 4000 us since the last arrival.

    /// Feed two packets (so a run exists), then play out until `left` frames remain.
    fn primed(left_frames: usize) -> Harness {
        let mut h = Harness::new(2, 128, NetworkBufferPreset::Optimal);
        for (i, t) in [0u64, 2667].into_iter().enumerate() {
            h.rx.set_time_us(t);
            let w = h.packet(|_| 0.25 + i as f32 * 0.01);
            h.feed(&w);
        }
        let play = h.rx.fill_frames() - left_frames;
        let chunk = h.cons.read_chunk(play * h.ch).unwrap();
        chunk.commit_all();
        h
    }

    fn tick_at(h: &mut Harness, t: u64) {
        h.rx.set_time_us(t);
        h.rx.tick();
    }

    #[test]
    fn an_overdue_packet_is_concealed_before_the_ring_runs_dry() {
        let mut h = primed(100); // below one block (128): the next pull would underrun
        let c = h.rx.counters().clone();
        // 3000 us after the last arrival: the packet is merely due, not overdue.
        tick_at(&mut h, 2667 + 3000);
        assert_eq!(StreamCounters::get(&c.predicted), 0);
        assert_eq!(h.rx.fill_frames(), 100);
        // 4100 us: more than 1.5 periods. Conceal it.
        tick_at(&mut h, 2667 + 4100);
        assert_eq!(StreamCounters::get(&c.predicted), 1);
        assert_eq!(StreamCounters::get(&c.lost), 1);
        assert_eq!(h.rx.fill_frames(), 100 + 128);
        // The real packet turns up afterwards: dropped as late, not played twice.
        h.rx.set_time_us(2667 + 4500);
        let w = h.packet(|_| 0.5);
        h.feed(&w);
        assert_eq!(StreamCounters::get(&c.late), 1);
        assert_eq!(h.rx.fill_frames(), 100 + 128);
    }

    #[test]
    fn a_ring_with_plenty_of_fill_is_never_predicted_into() {
        let mut h = primed(200); // a whole block available: no danger yet
        let c = h.rx.counters().clone();
        tick_at(&mut h, 2667 + 20_000);
        assert_eq!(StreamCounters::get(&c.predicted), 0);
        assert_eq!(h.rx.fill_frames(), 200);
    }

    #[test]
    fn without_a_clock_nothing_is_ever_overdue() {
        let mut h = Harness::new(2, 128, NetworkBufferPreset::Optimal);
        for _ in 0..2 {
            let w = h.packet(|_| 0.25);
            h.feed(&w);
        }
        let chunk = h.cons.read_chunk(200 * h.ch).unwrap();
        chunk.commit_all();
        h.rx.tick();
        assert_eq!(StreamCounters::get(&h.rx.counters().predicted), 0);
    }

    #[test]
    fn a_dead_sender_gets_a_bounded_run_then_a_real_underrun() {
        let mut h = primed(0);
        let c = h.rx.counters().clone();
        let mut t = 2667;
        for _ in 0..40 {
            t += 1000;
            tick_at(&mut h, t);
            // Mixer keeps playing whatever there is.
            let n = h.cons.slots();
            if n > 0 {
                h.cons.read_chunk(n).unwrap().commit_all();
            }
        }
        assert_eq!(StreamCounters::get(&c.predicted), MAX_PREDICTED_RUN);
        assert!(!h.rx.wants_fast_tick(), "stops polling once it gave up");
        // The sender comes back: prediction is armed again.
        h.rx.set_time_us(t + 5000);
        let w = h.packet(|_| 0.5);
        h.feed(&w);
        assert_eq!(h.rx.predicted_run, 0);
    }

    #[test]
    fn a_burst_of_lost_packets_is_bridged_one_packet_per_period() {
        let mut h = primed(100);
        let c = h.rx.counters().clone();
        let mut t = 2667;
        let mut seen = vec![];
        for _ in 0..30 {
            t += 1000;
            tick_at(&mut h, t);
            // Drain a block whenever one is available, like the mixer does.
            if h.cons.slots() / h.ch >= ENGINE_BLOCK_FRAMES {
                let c2 = h.cons.read_chunk(ENGINE_BLOCK_FRAMES * h.ch).unwrap();
                c2.commit_all();
            }
            seen.push(StreamCounters::get(&c.predicted));
        }
        // Each missing packet is covered once its own deadline (2.5, 3.5, ... periods) passed.
        assert_eq!(*seen.last().unwrap(), MAX_PREDICTED_RUN);
        assert!(seen.windows(2).all(|w| w[1] >= w[0]));
    }
    // !SECTION: predictive concealment
}
