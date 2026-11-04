//! What runs inside a device callback: audio between a device's buffers and an engine port ring.
//!
//! Shared by the Linux backends (PipeWire hands over bytes, ALSA through `cpal` hands over
//! `f32` slices). These functions run on the device's real-time thread, so they follow the same
//! rules as the mixer: no allocation, no blocking lock, no I/O. Every buffer they use is
//! allocated when the stream is opened.

use audio_engine::dsp::Planar;
use audio_engine::{ClockDomainAdapter, InputPort, OutputPort};
use audio_types::{ENGINE_BLOCK_FRAMES, MAX_CHANNELS};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

/// Frames converted per ring access when the device hands over bytes.
const STAGE_FRAMES: usize = 256;
const STAGE_SAMPLES: usize = STAGE_FRAMES * MAX_CHANNELS;
const BYTES_PER_SAMPLE: usize = core::mem::size_of::<f32>();

// SECTION: Shared state
/// Counters the callback updates and `AudioStream::stats` reads.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub frames: AtomicU64,
    pub xruns: AtomicU32,
    /// Drift of the output device against the mixer, ppm as `f32` bits.
    pub ppm_bits: AtomicU32,
}

/// The port and buffers of one stream, shared between the control thread (which hands the port
/// back on close) and the device callback.
// ? The callback takes the lock with `try_lock`, which never waits: it either gets the state or
// ? skips that cycle. The only other holder is `close`, after the stream has been stopped, so
// ? the callback never actually loses the race while audio is running. A real `lock()` would
// ? be a way to block the RT thread, so it is not used here.
pub(crate) struct Slot<T> {
    pub state: Mutex<Option<T>>,
    pub counters: Counters,
}

impl<T> Slot<T> {
    pub fn new(state: T) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(Some(state)),
            counters: Counters::default(),
        })
    }

    /// Take the state back (after the stream is gone).
    pub fn take(&self) -> Option<T> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}
// !SECTION: Shared state

// ? Only the control thread calls these, and only when the stream was never started or has been
// ? destroyed, so no callback can hold the lock and the state is always still there.
pub(crate) fn take_input(slot: &Slot<CaptureState>) -> InputPort {
    match slot.take() {
        Some(state) => state.into_port(),
        None => unreachable!("the capture state is only taken once, on this path"),
    }
}

pub(crate) fn take_output(slot: &Slot<PlaybackState>) -> OutputPort {
    match slot.take() {
        Some(state) => state.into_port(),
        None => unreachable!("the playback state is only taken once, on this path"),
    }
}

// SECTION: Capture (device -> ring)
pub(crate) struct CaptureState {
    port: InputPort,
    channels: usize,
    stage: Box<[f32; STAGE_SAMPLES]>,
}

impl CaptureState {
    pub fn new(port: InputPort, channels: usize) -> Self {
        Self {
            port,
            channels: channels.clamp(1, MAX_CHANNELS),
            stage: Box::new([0.0; STAGE_SAMPLES]),
        }
    }

    pub fn into_port(self) -> InputPort {
        self.port
    }

    /// Push interleaved samples into the ring, whole frames only.
    pub fn feed_samples(&mut self, samples: &[f32], counters: &Counters) {
        let ch = self.channels;
        let whole = samples.len() / ch * ch;
        let accepted = self.port.push_frames(&samples[..whole], ch);
        counters.frames.fetch_add((accepted / ch) as u64, Relaxed);
        if accepted < whole {
            // The ring is full: the mixer is not keeping up. The frames are lost.
            counters.xruns.fetch_add(1, Relaxed);
        }
    }

    /// Push `bytes` (interleaved little-endian `f32`) into the ring, whole frames only.
    pub fn feed(&mut self, bytes: &[u8], counters: &Counters) {
        let ch = self.channels;
        let frame_bytes = ch * BYTES_PER_SAMPLE;
        for piece in bytes.chunks(STAGE_FRAMES * frame_bytes) {
            let samples = piece.len() / frame_bytes * ch;
            for (dst, b) in self.stage[..samples]
                .iter_mut()
                .zip(piece.chunks_exact(BYTES_PER_SAMPLE))
            {
                *dst = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            }
            let accepted = self.port.push_frames(&self.stage[..samples], ch);
            counters.frames.fetch_add((accepted / ch) as u64, Relaxed);
            if accepted < samples {
                counters.xruns.fetch_add(1, Relaxed);
            }
        }
    }
}
// !SECTION: Capture (device -> ring)

// SECTION: Playback (ring -> device)
pub(crate) struct PlaybackState {
    port: OutputPort,
    channels: usize,
    adapter: Box<ClockDomainAdapter>,
    planar: Box<Planar>,
}

impl PlaybackState {
    /// `target_ms` is how much audio the output keeps queued between the mixer and the device.
    pub fn new(port: OutputPort, channels: usize, target_ms: f32) -> Self {
        let channels = channels.clamp(1, MAX_CHANNELS);
        let mut adapter = Box::<ClockDomainAdapter>::default();
        adapter.configure(channels, target_ms);
        Self {
            port,
            channels,
            adapter,
            planar: Box::new([[0.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS]),
        }
    }

    pub fn into_port(self) -> OutputPort {
        self.port
    }

    /// Fill `out` (interleaved, `frames` frames) for the device.
    pub fn render_samples(&mut self, out: &mut [f32], frames: usize, counters: &Counters) {
        let ch = self.channels;
        let frames = frames.min(out.len() / ch);
        self.fill(frames, counters, |at, v| out[at] = v);
        // A device that asked for more than it gave room for still gets silence, not stale data.
        out[frames * ch..].fill(0.0);
    }

    /// Fill `bytes` (interleaved little-endian `f32`, `frames` frames) for the device.
    pub fn render(&mut self, bytes: &mut [u8], frames: usize, counters: &Counters) {
        let frames = frames.min(bytes.len() / (self.channels * BYTES_PER_SAMPLE));
        self.fill(frames, counters, |at, v| {
            let b = at * BYTES_PER_SAMPLE;
            bytes[b..b + BYTES_PER_SAMPLE].copy_from_slice(&v.to_le_bytes());
        });
    }

    /// The mixer's audio, resampled by the small amount that keeps the ring at its target
    /// (CLAUDE.md law 2: the device clock is not the mixer clock). `write(i, v)` stores sample
    /// `v` at interleaved index `i`.
    // ? `impl FnMut` is a closure the compiler inlines, so the byte and the `f32` versions share
    // ? this loop without a heap allocation or a dynamic call in the real-time path.
    fn fill(&mut self, frames: usize, counters: &Counters, mut write: impl FnMut(usize, f32)) {
        let ch = self.channels;
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(ENGINE_BLOCK_FRAMES);
            self.adapter
                .pull(&mut self.port.consumer, &mut self.planar, n);
            for i in 0..n {
                for c in 0..ch {
                    let v = self.planar[c][i];
                    // ? NaN fails both comparisons of `clamp`'s contract, so it is zeroed first.
                    // ? A speaker must never get a value outside +-1.
                    let v = if v.is_nan() { 0.0 } else { v.clamp(-1.0, 1.0) };
                    write((done + i) * ch + c, v);
                }
            }
            done += n;
        }
        counters.frames.fetch_add(frames as u64, Relaxed);
        counters.xruns.store(
            self.adapter
                .underruns()
                .wrapping_add(self.adapter.overruns()),
            Relaxed,
        );
        counters
            .ppm_bits
            .store(self.adapter.drift_ppm().to_bits(), Relaxed);
    }
}
// !SECTION: Playback (ring -> device)

#[cfg(test)]
mod tests {
    use super::*;
    use audio_engine::create_engine;

    fn f32_bytes(samples: &[f32]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn capture_converts_whole_frames_and_counts_what_it_moved() {
        let (_engine, mut h) = create_engine();
        let port = h.ports.acquire_input().unwrap();
        let mut state = CaptureState::new(port, 2);
        let counters = Counters::default();
        // 600 frames: more than one staging chunk (256), so the loop must carry on.
        let samples: Vec<f32> = (0..1200).map(|i| i as f32 * 0.001).collect();
        state.feed(&f32_bytes(&samples), &counters);
        assert_eq!(counters.frames.load(Relaxed), 600);
        assert_eq!(counters.xruns.load(Relaxed), 0);

        // A trailing partial frame (3 samples at 2 channels) is ignored, not half-stored.
        state.feed(&f32_bytes(&[0.5, 0.5, 0.5]), &counters);
        assert_eq!(counters.frames.load(Relaxed), 601);
    }

    #[test]
    fn capture_from_slices_counts_the_same_way() {
        let (_engine, mut h) = create_engine();
        let port = h.ports.acquire_input().unwrap();
        let mut state = CaptureState::new(port, 2);
        let counters = Counters::default();
        state.feed_samples(&vec![0.1_f32; 1200], &counters);
        assert_eq!(counters.frames.load(Relaxed), 600);
        state.feed_samples(&[0.5, 0.5, 0.5], &counters);
        assert_eq!(
            counters.frames.load(Relaxed),
            601,
            "the odd sample is ignored"
        );
        assert_eq!(counters.xruns.load(Relaxed), 0);
    }

    #[test]
    fn capture_counts_a_full_ring_and_never_stores_half_a_frame() {
        let (_engine, mut h) = create_engine();
        let port = h.ports.acquire_input().unwrap();
        let mut state = CaptureState::new(port, 3);
        let counters = Counters::default();
        // Far more than the ring holds: some is dropped, the count says so.
        let big = vec![0.25_f32; 3 * 20_000];
        state.feed(&f32_bytes(&big), &counters);
        assert!(counters.xruns.load(Relaxed) >= 1);
        let frames = counters.frames.load(Relaxed);
        assert!(frames > 0 && frames < 20_000, "{frames}");
        let port = state.into_port();
        let queued = port.producer.buffer().capacity() - port.producer.slots();
        assert_eq!(queued % 3, 0, "the ring holds whole 3-channel frames only");
        assert_eq!(queued as u64, frames * 3);
    }

    fn samples(bytes: &[u8]) -> Vec<f32> {
        bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect()
    }

    #[test]
    fn playback_always_fills_the_whole_buffer_and_primes_before_it_counts_underruns() {
        use audio_types::{BusId, EngineCommand};
        let (mut engine, mut h) = create_engine();
        let port = h.ports.acquire_output().unwrap();
        let slot = port.slot();
        let mut state = PlaybackState::new(port, 2, 8.0);
        h.commands
            .push(EngineCommand::AttachBusOutput {
                bus: BusId(0),
                slot,
                channels: 2,
            })
            .unwrap();
        let counters = Counters::default();

        // Nothing queued yet: the device still gets a full buffer of silence, and priming is
        // not an underrun.
        let mut bytes = vec![0xAA_u8; 300 * 2 * 4]; // 300 frames: more than one engine block
        state.render(&mut bytes, 300, &counters);
        assert!(samples(&bytes).iter().all(|&v| v == 0.0));
        assert_eq!(counters.frames.load(Relaxed), 300);
        assert_eq!(counters.xruns.load(Relaxed), 0);

        // The mixer runs ahead by more than the 8 ms target; the stream starts without noise.
        for _ in 0..6 {
            engine.process_block(ENGINE_BLOCK_FRAMES);
        }
        for _ in 0..20 {
            state.render(&mut bytes, 128, &counters);
            engine.process_block(ENGINE_BLOCK_FRAMES);
        }
        assert!(
            samples(&bytes)
                .iter()
                .all(|v| v.is_finite() && v.abs() <= 1.0)
        );
        assert_eq!(
            counters.xruns.load(Relaxed),
            0,
            "matched clocks never underrun"
        );
        // ? The ring started 8 ms above its target, so the controller speeds playback up, at
        // ? most 10 ppm per block (CLAUDE.md law 2): 20 blocks -> up to 200 ppm. Anything inside
        // ? the +-1000 ppm clamp is correct; the test only checks the report is sane.
        let ppm = f32::from_bits(counters.ppm_bits.load(Relaxed));
        assert!(ppm.is_finite() && ppm.abs() <= 1000.0, "{ppm}");
        drop(state.into_port());
    }

    #[test]
    fn playback_to_a_slice_never_leaves_stale_samples() {
        let (_engine, mut h) = create_engine();
        let port = h.ports.acquire_output().unwrap();
        let mut state = PlaybackState::new(port, 2, 8.0);
        let counters = Counters::default();
        // The device buffer is bigger than the frames asked for: the tail is silence.
        let mut out = vec![9.0_f32; 2 * 64 + 6];
        state.render_samples(&mut out, 64, &counters);
        assert!(
            out.iter().all(|&v| v == 0.0),
            "nothing queued yet: pure silence"
        );
        assert_eq!(counters.frames.load(Relaxed), 64);
        // Asking for more frames than the slice holds is clipped, not a panic.
        let mut small = vec![9.0_f32; 10];
        state.render_samples(&mut small, 1000, &counters);
        assert!(small.iter().all(|&v| v == 0.0));
        assert_eq!(counters.frames.load(Relaxed), 64 + 5);
    }
}
