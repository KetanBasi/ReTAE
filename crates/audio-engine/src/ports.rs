//! The pre-allocated port-ring pool (CLAUDE.md law 1, PLAN.md §7).
//!
//! * 48 `rtrb` rings of 8 ch x 4096 frames of `f32` (128 KB each, ~6 MB total), allocated
//!   once at startup and **reused, never freed**. Attach/detach therefore never allocates
//!   or frees on the RT thread.
//! * Rings carry *interleaved* samples (a FIFO has to keep frames in order). The channel
//!   count of a ring is chosen per attach, so a stereo port simply holds 16 384 frames.
//!
//! # Slot map
//! ```text
//!  slots  0..16   strip inputs    device/net thread  --producer-->  ring  --consumer-->  engine
//!  slots 16..32   bus outputs     engine  --producer-->  ring  --consumer-->  device/net thread
//!  slots 32..48   bus recorders   engine  --producer-->  ring  --consumer-->  recorder thread
//! ```
// ? Why fixed directions instead of "any slot, any direction"? An `rtrb` ring is split into
// ? exactly one Producer and one Consumer, and each half can live on only one thread. The
// ? engine thread must own its half permanently (the command queue is `Copy`, so a half
// ? can never be sent to it later). Fixing the direction per slot at creation time means
// ? the engine owns the right half from the start and the control plane owns the other.
// ? 16 + 16 + 16 = 48 = the pool size from the plan: one input per strip, one output and
// ? one recorder per bus.

use crate::dsp::Planar;
use audio_types::{BusId, ENGINE_BLOCK_FRAMES, MAX_BUSES, MAX_CHANNELS, MAX_STRIPS, PortSlot};
use rtrb::{Consumer, Producer, RingBuffer};

pub const INPUT_PORTS: usize = MAX_STRIPS;
pub const OUTPUT_PORTS: usize = MAX_BUSES;
pub const RECORDER_PORTS: usize = MAX_BUSES;
pub const MAX_PORTS: usize = INPUT_PORTS + OUTPUT_PORTS + RECORDER_PORTS;

pub const PORT_RING_FRAMES: usize = 4096;
/// Ring capacity in `f32` samples (not frames): 8 channels x 4096 frames.
pub const PORT_RING_SAMPLES: usize = MAX_CHANNELS * PORT_RING_FRAMES;

const OUTPUT_BASE: usize = INPUT_PORTS;
const RECORDER_BASE: usize = INPUT_PORTS + OUTPUT_PORTS;

// SECTION: Slot numbering
/// Global slot number of the `i`-th strip-input ring.
pub const fn input_slot(i: usize) -> PortSlot {
    PortSlot(i as u8)
}
/// Global slot number of the `i`-th bus-output ring.
pub const fn output_slot(i: usize) -> PortSlot {
    PortSlot((OUTPUT_BASE + i) as u8)
}
/// Global slot number of the recorder ring that belongs to `bus`.
pub const fn recorder_slot(bus: BusId) -> PortSlot {
    PortSlot((RECORDER_BASE + bus.0 as usize) as u8)
}

/// `Some(index into the input rings)` when `slot` is a strip-input slot.
pub fn input_index(slot: PortSlot) -> Option<usize> {
    let s = slot.0 as usize;
    (s < INPUT_PORTS).then_some(s)
}
/// `Some(index into the output rings)` when `slot` is a bus-output slot.
pub fn output_index(slot: PortSlot) -> Option<usize> {
    let s = slot.0 as usize;
    (OUTPUT_BASE..OUTPUT_BASE + OUTPUT_PORTS)
        .contains(&s)
        .then(|| s - OUTPUT_BASE)
}
// !SECTION: Slot numbering

// SECTION: Engine side (owned by MixerEngine)
/// The halves of the rings the RT thread owns.
// ? Fields are `pub(crate)`: only the engine touches them, and only on the RT thread.
pub struct EnginePorts {
    pub(crate) inputs: [Consumer<f32>; INPUT_PORTS],
    pub(crate) outputs: [Producer<f32>; OUTPUT_PORTS],
    pub(crate) recorders: [Producer<f32>; RECORDER_PORTS],
}
// !SECTION: Engine side (owned by MixerEngine)

// SECTION: Control side (device / net / recorder threads)
/// A strip-input ring: the device or network thread pushes interleaved `f32` frames here.
#[derive(Debug)]
pub struct InputPort {
    slot: PortSlot,
    pub producer: Producer<f32>,
}

/// A bus-output ring: the device or network thread pops interleaved `f32` frames from here.
#[derive(Debug)]
pub struct OutputPort {
    slot: PortSlot,
    pub consumer: Consumer<f32>,
}

/// A bus-recorder ring: the recorder thread pops interleaved `f32` frames from here.
#[derive(Debug)]
pub struct RecorderPort {
    slot: PortSlot,
    pub consumer: Consumer<f32>,
}

impl InputPort {
    /// Slot number to put in `EngineCommand::AttachStripInput`.
    pub fn slot(&self) -> PortSlot {
        self.slot
    }

    /// Push as many whole samples as fit; returns how many were accepted.
    pub fn push(&mut self, samples: &[f32]) -> usize {
        let n = samples.len().min(self.producer.slots());
        if n == 0 {
            return 0;
        }
        match self.producer.write_chunk(n) {
            Ok(mut chunk) => {
                let (a, b) = chunk.as_mut_slices();
                let (src_a, src_b) = samples[..n].split_at(a.len());
                a.copy_from_slice(src_a);
                b.copy_from_slice(src_b);
                chunk.commit_all();
                n
            }
            Err(_) => 0,
        }
    }

    /// Split into the slot number and the bare ring half.
    // ? Net threads want a bare `Producer` (`StreamRx`) or `Consumer` (`TxStream`), and hand it
    // ? back when they stop. Keeping the slot number on the side and rebuilding the port with
    // ? `from_parts` is how a LAN stream returns its ring to the pool (rings are never freed).
    pub fn into_parts(self) -> (PortSlot, Producer<f32>) {
        (self.slot, self.producer)
    }

    pub fn from_parts(slot: PortSlot, producer: Producer<f32>) -> Self {
        Self { slot, producer }
    }
}

impl OutputPort {
    /// Slot number to put in `EngineCommand::AttachBusOutput`.
    pub fn slot(&self) -> PortSlot {
        self.slot
    }

    /// See [`InputPort::into_parts`].
    pub fn into_parts(self) -> (PortSlot, Consumer<f32>) {
        (self.slot, self.consumer)
    }

    pub fn from_parts(slot: PortSlot, consumer: Consumer<f32>) -> Self {
        Self { slot, consumer }
    }

    /// Pop up to `out.len()` samples; returns how many were written into `out`.
    pub fn pop(&mut self, out: &mut [f32]) -> usize {
        pop_into(&mut self.consumer, out)
    }
}

impl RecorderPort {
    pub fn slot(&self) -> PortSlot {
        self.slot
    }

    /// Pop up to `out.len()` samples; returns how many were written into `out`.
    pub fn pop(&mut self, out: &mut [f32]) -> usize {
        pop_into(&mut self.consumer, out)
    }
}

fn pop_into(consumer: &mut Consumer<f32>, out: &mut [f32]) -> usize {
    let n = out.len().min(consumer.slots());
    if n == 0 {
        return 0;
    }
    match consumer.read_chunk(n) {
        Ok(chunk) => {
            let (a, b) = chunk.as_slices();
            let (dst_a, dst_b) = out[..n].split_at_mut(a.len());
            dst_a.copy_from_slice(a);
            dst_b.copy_from_slice(b);
            chunk.commit_all();
            n
        }
        Err(_) => 0,
    }
}

/// The halves of the rings that are *not* on the RT thread, waiting to be handed out.
// ? "Acquire" moves the half out of an `Option` (so exactly one owner exists at a time) and
// ? "release" puts it back. Nothing is ever dropped, so the ring memory is never freed.
pub struct ControlPorts {
    inputs: [Option<Producer<f32>>; INPUT_PORTS],
    outputs: [Option<Consumer<f32>>; OUTPUT_PORTS],
    recorders: [Option<Consumer<f32>>; RECORDER_PORTS],
}

impl ControlPorts {
    /// Take the lowest free strip-input ring.
    pub fn acquire_input(&mut self) -> Option<InputPort> {
        let (i, producer) = self
            .inputs
            .iter_mut()
            .enumerate()
            .find_map(|(i, p)| p.take().map(|p| (i, p)))?;
        Some(InputPort {
            slot: input_slot(i),
            producer,
        })
    }

    pub fn release_input(&mut self, port: InputPort) {
        if let Some(i) = input_index(port.slot) {
            self.inputs[i] = Some(port.producer);
        }
    }

    /// Take the lowest free bus-output ring.
    pub fn acquire_output(&mut self) -> Option<OutputPort> {
        let (i, consumer) = self
            .outputs
            .iter_mut()
            .enumerate()
            .find_map(|(i, c)| c.take().map(|c| (i, c)))?;
        Some(OutputPort {
            slot: output_slot(i),
            consumer,
        })
    }

    /// Return an output ring. Any audio still in it is discarded so the next user starts clean.
    pub fn release_output(&mut self, mut port: OutputPort) {
        discard_all(&mut port.consumer);
        if let Some(i) = output_index(port.slot) {
            self.outputs[i] = Some(port.consumer);
        }
    }

    /// Take the recorder ring of `bus` (each bus has exactly one, fixed).
    pub fn acquire_recorder(&mut self, bus: BusId) -> Option<RecorderPort> {
        let consumer = self.recorders.get_mut(bus.0 as usize)?.take()?;
        Some(RecorderPort {
            slot: recorder_slot(bus),
            consumer,
        })
    }

    pub fn release_recorder(&mut self, mut port: RecorderPort) {
        discard_all(&mut port.consumer);
        let i = port.slot.0 as usize;
        if let Some(r) = i
            .checked_sub(RECORDER_BASE)
            .and_then(|i| self.recorders.get_mut(i))
        {
            *r = Some(port.consumer);
        }
    }

    pub fn free_inputs(&self) -> usize {
        self.inputs.iter().filter(|p| p.is_some()).count()
    }
    pub fn free_outputs(&self) -> usize {
        self.outputs.iter().filter(|p| p.is_some()).count()
    }
}
// !SECTION: Control side (device / net / recorder threads)

// SECTION: Pool construction
/// Allocate all 48 rings. Control thread only, once, at startup.
pub fn create_port_pool() -> (EnginePorts, ControlPorts) {
    let mut in_tx = Vec::with_capacity(INPUT_PORTS);
    let mut in_rx = Vec::with_capacity(INPUT_PORTS);
    for _ in 0..INPUT_PORTS {
        let (p, c) = RingBuffer::new(PORT_RING_SAMPLES);
        in_tx.push(Some(p));
        in_rx.push(c);
    }
    let mut out_tx = Vec::with_capacity(OUTPUT_PORTS);
    let mut out_rx = Vec::with_capacity(OUTPUT_PORTS);
    for _ in 0..OUTPUT_PORTS {
        let (p, c) = RingBuffer::new(PORT_RING_SAMPLES);
        out_tx.push(p);
        out_rx.push(Some(c));
    }
    let mut rec_tx = Vec::with_capacity(RECORDER_PORTS);
    let mut rec_rx = Vec::with_capacity(RECORDER_PORTS);
    for _ in 0..RECORDER_PORTS {
        let (p, c) = RingBuffer::new(PORT_RING_SAMPLES);
        rec_tx.push(p);
        rec_rx.push(Some(c));
    }
    (
        EnginePorts {
            inputs: into_array(in_rx),
            outputs: into_array(out_tx),
            recorders: into_array(rec_tx),
        },
        ControlPorts {
            inputs: into_array(in_tx),
            outputs: into_array(out_rx),
            recorders: into_array(rec_rx),
        },
    )
}

// ? A `Vec` has a runtime length, an array has a compile-time one. We built each Vec with
// ? exactly N items just above, so the conversion cannot fail.
fn into_array<T, const N: usize>(v: Vec<T>) -> [T; N] {
    match <[T; N]>::try_from(v) {
        Ok(a) => a,
        Err(_) => unreachable!("pool vectors are built with exactly N elements"),
    }
}
// !SECTION: Pool construction

// SECTION: RT-side ring I/O
// * Called from process_block: no alloc, no locks.
/// Drop everything currently in the ring.
pub(crate) fn discard_all(cons: &mut Consumer<f32>) {
    let n = cons.slots();
    if n > 0 {
        if let Ok(chunk) = cons.read_chunk(n) {
            chunk.commit_all();
        }
    }
}

/// Samples beyond ±`INPUT_LIMIT` (+60 dBFS) are clamped on the way in.
// !! Network audio is untrusted (VBAN has no authentication). A single sample of 3e38 would
// !! overflow to infinity somewhere in the gain chain, and `inf - inf = NaN` then lives in a
// !! filter state forever. Clamping at the door keeps every later stage finite even with
// !! every gain at its maximum (worst case ~1e35, f32 tops out at 3.4e38).
pub const INPUT_LIMIT: f32 = 1.0e3;

/// Largest block the ring helpers handle: 8 channels x one engine block.
pub const INTERLEAVE_SAMPLES: usize = MAX_CHANNELS * ENGINE_BLOCK_FRAMES;

/// Scratch for one interleaved block (the ring side of a (de)interleave).
pub(crate) type InterleaveBuf = [f32; INTERLEAVE_SAMPLES];

// ? Both directions go through a contiguous scratch buffer: one `memcpy` to/from the ring
// ? (rtrb's `*_entire_slice` handle the wrap-around) and then a tight loop with no ring
// ? bookkeeping inside it. Much faster than walking the ring's two halves sample by sample.

#[inline(always)]
pub(crate) fn clean(s: f32) -> f32 {
    // Common case first: one compare. NaN fails `<=`, so it lands in the slow branches.
    if s.abs() <= INPUT_LIMIT {
        s
    } else if s.is_finite() {
        INPUT_LIMIT.copysign(s)
    } else {
        0.0
    }
}

// ? Generic over the row length `N` so the drift adapter's wider staging buffer
// ? (`Work`, 192 frames per channel) can share this code with the 128-frame engine blocks.
pub(crate) fn deinterleave<const N: usize>(
    src: &[f32],
    channels: usize,
    out: &mut [[f32; N]; MAX_CHANNELS],
) {
    match channels {
        1 => {
            for (o, &s) in out[0].iter_mut().zip(src) {
                *o = clean(s);
            }
        }
        2 => {
            // ? The by far most common layout gets its own loop with both outputs in hand.
            let (l, r) = out.split_at_mut(1);
            for ((l, r), f) in l[0]
                .iter_mut()
                .zip(r[0].iter_mut())
                .zip(src.chunks_exact(2))
            {
                *l = clean(f[0]);
                *r = clean(f[1]);
            }
        }
        _ => {
            for (i, frame) in src.chunks_exact(channels).enumerate() {
                for (c, &s) in frame.iter().enumerate() {
                    out[c][i] = clean(s);
                }
            }
        }
    }
}

fn interleave(src: &Planar, channels: usize, frames: usize, dst: &mut [f32]) {
    match channels {
        1 => dst[..frames].copy_from_slice(&src[0][..frames]),
        2 => {
            for ((d, &l), &r) in dst
                .chunks_exact_mut(2)
                .zip(&src[0][..frames])
                .zip(&src[1][..frames])
            {
                d[0] = l;
                d[1] = r;
            }
        }
        _ => {
            for (f, d) in dst.chunks_exact_mut(channels).enumerate() {
                for (c, v) in d.iter_mut().enumerate() {
                    *v = src[c][f];
                }
            }
        }
    }
}

/// De-interleave up to `frames` frames of `channels` channels into `out`.
/// Returns the number of frames actually read; the rest of the block is zero-filled.
/// Non-finite samples become 0 and huge ones are clamped to `INPUT_LIMIT`.
pub(crate) fn read_interleaved(
    cons: &mut Consumer<f32>,
    channels: usize,
    frames: usize,
    out: &mut Planar,
    tmp: &mut InterleaveBuf,
) -> usize {
    let channels = channels.clamp(1, MAX_CHANNELS);
    let frames = frames.min(ENGINE_BLOCK_FRAMES);
    let take = frames.min(cons.slots() / channels);
    let n = take * channels;
    if n > 0 && cons.pop_entire_slice(&mut tmp[..n]).is_ok() {
        deinterleave(&tmp[..n], channels, out);
    }
    let take = if n > 0 { take } else { 0 };
    for row in out.iter_mut().take(channels) {
        row[take..frames].fill(0.0);
    }
    take
}

/// Interleave `frames` frames of `channels` channels into the ring.
/// All-or-nothing: returns `false` (writing nothing) when the ring lacks room for the block.
pub(crate) fn write_interleaved(
    prod: &mut Producer<f32>,
    src: &Planar,
    channels: usize,
    frames: usize,
    tmp: &mut InterleaveBuf,
) -> bool {
    let channels = channels.clamp(1, MAX_CHANNELS);
    let frames = frames.min(ENGINE_BLOCK_FRAMES);
    let n = frames * channels;
    if prod.slots() < n {
        return false;
    }
    interleave(src, channels, frames, &mut tmp[..n]);
    prod.push_entire_slice(&tmp[..n]).is_ok()
}
// !SECTION: RT-side ring I/O

#[cfg(test)]
mod tests {
    use super::*;
    use audio_types::ENGINE_BLOCK_FRAMES;

    #[test]
    fn pool_has_48_rings_and_the_documented_size() {
        assert_eq!(MAX_PORTS, 48);
        assert_eq!(PORT_RING_SAMPLES * core::mem::size_of::<f32>(), 128 * 1024);
        let (_engine, mut ctl) = create_port_pool();
        assert_eq!(ctl.free_inputs(), 16);
        assert_eq!(ctl.free_outputs(), 16);
        // 16 inputs can all be taken, the 17th fails.
        let held: Vec<_> = (0..16).map(|_| ctl.acquire_input().unwrap()).collect();
        assert!(ctl.acquire_input().is_none());
        let slots: Vec<u8> = held.iter().map(|p| p.slot().0).collect();
        assert_eq!(slots, (0..16).collect::<Vec<u8>>());
    }

    #[test]
    fn slot_numbering_round_trips() {
        assert_eq!(input_index(input_slot(5)), Some(5));
        assert_eq!(input_index(output_slot(0)), None);
        assert_eq!(output_index(output_slot(15)), Some(15));
        assert_eq!(output_index(input_slot(0)), None);
        assert_eq!(output_index(recorder_slot(BusId(0))), None);
        assert_eq!(recorder_slot(BusId(15)).0, 47);
        assert_eq!(output_index(PortSlot(48)), None);
    }

    #[test]
    fn rings_are_reused_not_freed() {
        let (_engine, mut ctl) = create_port_pool();
        let first = ctl.acquire_input().unwrap();
        let slot = first.slot();
        ctl.release_input(first);
        let again = ctl.acquire_input().unwrap();
        assert_eq!(again.slot(), slot);

        let mut out = ctl.acquire_output().unwrap();
        assert_eq!(out.slot(), output_slot(0));
        ctl.release_output(out);
        out = ctl.acquire_output().unwrap();
        assert_eq!(out.slot(), output_slot(0));
    }

    #[test]
    fn interleave_roundtrip_with_ring_wraparound() {
        let (mut prod, mut cons) = RingBuffer::<f32>::new(10); // tiny ring forces a wrap
        let mut src = [[0.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS];
        let mut out = [[9.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS];
        let mut tmp = [0.0; INTERLEAVE_SAMPLES];
        for round in 0..20 {
            let base = (round * 10) as f32;
            src[0][..2].copy_from_slice(&[base, base + 1.0]);
            src[1][..2].copy_from_slice(&[-base, -(base + 1.0)]);
            assert!(write_interleaved(&mut prod, &src, 2, 2, &mut tmp));
            // 4 samples are queued, so only 6 of the 10 slots are free: 4 more frames (8
            // samples) must be refused without writing anything.
            assert!(!write_interleaved(&mut prod, &src, 2, 4, &mut tmp));
            assert_eq!(read_interleaved(&mut cons, 2, 2, &mut out, &mut tmp), 2);
            assert_eq!(out[0][..2], src[0][..2]);
            assert_eq!(out[1][..2], src[1][..2]);
        }
    }

    #[test]
    fn every_channel_count_roundtrips() {
        // 1, 2 (fast path) and 3..=8 (generic path), through a ring whose size is NOT a
        // multiple of the frame size, so the wrap lands in the middle of a frame.
        let mut tmp = [0.0; INTERLEAVE_SAMPLES];
        for channels in 1..=MAX_CHANNELS {
            let (mut prod, mut cons) = RingBuffer::<f32>::new(channels * 7 + 3);
            let mut src = [[0.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS];
            let mut out = [[0.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS];
            for round in 0..30 {
                let frames = 1 + round % 6;
                // ? Distinct value per (round, channel, frame), all well inside INPUT_LIMIT.
                for (c, row) in src.iter_mut().enumerate().take(channels) {
                    for (f, v) in row.iter_mut().enumerate().take(frames) {
                        *v = round as f32 * 10.0 + c as f32 + f as f32 * 0.1;
                    }
                }
                assert!(write_interleaved(
                    &mut prod, &src, channels, frames, &mut tmp
                ));
                assert_eq!(
                    read_interleaved(&mut cons, channels, frames, &mut out, &mut tmp),
                    frames
                );
                for c in 0..channels {
                    assert_eq!(
                        out[c][..frames],
                        src[c][..frames],
                        "{channels} ch, round {round}"
                    );
                }
            }
        }
    }

    #[test]
    fn partial_read_zero_fills_and_reports() {
        let (mut prod, mut cons) = RingBuffer::<f32>::new(64);
        let mut tmp = [0.0; INTERLEAVE_SAMPLES];
        for v in [1.0_f32, 2.0, 3.0, 4.0, 5.0] {
            prod.push(v).unwrap(); // 5 samples = 2 whole stereo frames + 1 stray sample
        }
        let mut out = [[7.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS];
        assert_eq!(read_interleaved(&mut cons, 2, 8, &mut out, &mut tmp), 2);
        assert_eq!(out[0][..4], [1.0, 3.0, 0.0, 0.0]);
        assert_eq!(out[1][..4], [2.0, 4.0, 0.0, 0.0]);
        // The stray half-frame stays in the ring for the next call.
        assert_eq!(cons.slots(), 1);
    }

    #[test]
    fn non_finite_input_becomes_silence_and_huge_input_is_clamped() {
        let (mut prod, mut cons) = RingBuffer::<f32>::new(16);
        for v in [
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            0.5,
            3.0e38,
            -3.0e38,
        ] {
            prod.push(v).unwrap();
        }
        let mut out = [[1.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS];
        let mut tmp = [0.0; INTERLEAVE_SAMPLES];
        read_interleaved(&mut cons, 2, 3, &mut out, &mut tmp);
        assert_eq!(out[0][..3], [0.0, 0.0, 3.0e38_f32.min(INPUT_LIMIT)]);
        assert_eq!(out[1][..3], [0.0, 0.5, -INPUT_LIMIT]);
    }

    #[test]
    fn input_push_and_output_pop_handle_wrap() {
        let (_e, mut ctl) = create_port_pool();
        let mut port = ctl.acquire_input().unwrap();
        let data: Vec<f32> = (0..PORT_RING_SAMPLES + 100).map(|i| i as f32).collect();
        // Only the ring's capacity is accepted.
        assert_eq!(port.push(&data), PORT_RING_SAMPLES);
        assert_eq!(port.push(&data), 0);
    }
}
