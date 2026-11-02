//! Per-block constants and the pre-allocated scratch memory the RT thread works in.

use crate::dsp::biquad::{QUAD_LANES, QuadLanes};
use crate::dsp::smooth::PARAM_SMOOTH_MS;
use crate::dsp::util::pole_coeff;
use crate::dsp::{Planar, SILENT};
use crate::meter::MeterConsts;
use crate::ports::{INTERLEAVE_SAMPLES, InterleaveBuf};
use audio_types::{ENGINE_BLOCK_FRAMES, MAX_BUSES};

/// Constants that only depend on the block size. Recomputed when the size changes
/// (normally never: the engine always runs 128-frame blocks).
#[derive(Clone, Copy, Debug)]
pub(crate) struct BlockConsts {
    pub frames: usize,
    /// One-pole coefficient for gain / pan / mute smoothing, stepped once per block.
    pub smooth: f32,
    pub meter: MeterConsts,
}

impl BlockConsts {
    pub fn new(frames: usize) -> Self {
        Self {
            frames,
            smooth: pole_coeff(PARAM_SMOOTH_MS, frames as f32),
            meter: MeterConsts::new(frames),
        }
    }

    pub fn ensure(&mut self, frames: usize) {
        if self.frames != frames {
            *self = Self::new(frames);
        }
    }
}

impl Default for BlockConsts {
    fn default() -> Self {
        Self::new(ENGINE_BLOCK_FRAMES)
    }
}

/// What strips need to know about each bus while mixing into it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BusInfo {
    pub channels: [usize; MAX_BUSES],
    /// A bus is *active* when it has an output, a recorder, or at least one send into it.
    pub active: [bool; MAX_BUSES],
}

/// All the audio memory `process_block` touches. Allocated once, up front.
// ? ~70 KB. It lives in a `Box` so it sits on the heap (allocated here, on the control
// ? thread) and `MixerEngine` itself stays small enough to move around cheaply.
pub(crate) struct Scratch {
    /// The four strips currently being processed (the EQ stage runs on all four at once).
    pub strip_buf: [Planar; 4],
    /// One accumulator per bus; strips add their sends into these.
    pub bus_acc: [Planar; MAX_BUSES],
    pub bus: BusInfo,
    /// Interleaved staging buffer for ring reads and writes.
    pub interleave: InterleaveBuf,
    /// Lane-major scratch for the 4-chain EQ kernel.
    pub eq_lanes: QuadLanes,
}

impl Scratch {
    pub fn new() -> Box<Self> {
        Box::new(Self {
            strip_buf: [SILENT; 4],
            bus_acc: [SILENT; MAX_BUSES],
            bus: BusInfo {
                channels: [2; MAX_BUSES],
                active: [false; MAX_BUSES],
            },
            interleave: [0.0; INTERLEAVE_SAMPLES],
            eq_lanes: [[0.0; QUAD_LANES]; ENGINE_BLOCK_FRAMES],
        })
    }
}
