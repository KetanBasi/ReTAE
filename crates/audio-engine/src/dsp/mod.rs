//! Allocation-free DSP building blocks used by the RT mixer.
//!
//! * Everything here works on planar blocks (`Planar`) and fixed-size state, so it is
//!   safe to call from `process_block` (CLAUDE.md law 1).

use audio_types::{ENGINE_BLOCK_FRAMES, MAX_CHANNELS};

pub mod biquad;
pub mod dynamics;
pub mod smooth;
pub mod util;

/// One channel of one engine block.
pub type Block = [f32; ENGINE_BLOCK_FRAMES];
/// `MAX_CHANNELS` planar channels. Only the first `channels` rows carry signal.
// ? Planar (one array per channel) makes every per-channel DSP loop a plain slice loop
// ? that the compiler can auto-vectorize. The port rings are interleaved because a FIFO
// ? has to carry frames in order; we (de)interleave exactly once at the edges.
pub type Planar = [Block; MAX_CHANNELS];

/// A zeroed planar block (a `static`, so every use shares one address and no copy is made).
pub static SILENT: Planar = [[0.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS];
