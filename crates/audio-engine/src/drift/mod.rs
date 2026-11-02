//! Clock-domain handling (PLAN.md §5, CLAUDE.md law 2).
//!
//! One device clocks the mixer. **Every** other endpoint (extra HW device, LAN stream,
//! Android playback capture) passes through
//! `FIFO ring -> DriftResampler -> consumer`, with a `DriftController` steering the
//! resampling ratio so the FIFO's fill level stays at a target:
//!
//! ```text
//!  producer clock                                             consumer clock
//!  device/net thread --> [ rtrb ring: fill = jitter depth ] --> resampler --> 128-frame block
//!                                      ^                           ^
//!                                      '-- DriftController: EMA of fill vs target -> PI -> ratio
//! ```
//!
//! * [`ClockDomainAdapter`] bundles the three. It borrows the ring's `Consumer` on every
//!   call, so the *same* type serves the mixer (strip inputs) and device threads (bus
//!   outputs: the ring then carries mixer-rate audio and the device callback pulls from it).
//! * Ratio convention everywhere: `ratio = input frames consumed per output frame`
//!   (`fs_in / fs_out`). Producer faster than consumer -> the ring fills -> ratio > 1.

mod adapter;
mod controller;
mod hermite;

pub use adapter::{AdapterReport, ClockDomainAdapter};
pub use controller::{DriftController, DriftGains};
pub use hermite::HermiteResampler;

use crate::dsp::Planar;
use audio_types::MAX_CHANNELS;

/// The resampling ratio is clamped to `1 ± MAX_RATIO_OFFSET` (1 %) by every resampler.
/// ? The controller itself never asks for more than ±1000 ppm (0.1 %); this is a hard
/// ? safety net so a bug upstream can't make a resampler read past its buffers.
pub const MAX_RATIO_OFFSET: f64 = 0.01;

/// Frames of staging per channel. One 128-frame pull at the largest allowed ratio needs
/// `128 * 1.01 + 4 = 134`, so 192 leaves plenty of room.
pub const WORK_FRAMES: usize = 192;

/// Staging area for the *new* input frames handed to a resampler (planar, one row per channel).
pub type Work = [[f32; WORK_FRAMES]; MAX_CHANNELS];

/// A variable-ratio resampler driven by "give me exactly N output frames".
///
/// Implementations keep their own history, so `input` only ever holds *new* frames. The
/// protocol per pull is:
/// 1. `n = input_frames_needed(ratio, out_frames)`;
/// 2. the caller stages exactly `n` new frames into a [`Work`] buffer;
/// 3. `process(ratio, &work, n, &mut out, out_frames)`.
///
/// # RT contract
/// `process` and `input_frames_needed` must not allocate, lock, or make syscalls.
pub trait DriftResampler: Send {
    /// Forget all history and (re)configure for `channels` channels (1..=8).
    fn reset(&mut self, channels: usize);

    /// Algorithmic delay in output frames at ratio 1.
    fn latency_frames(&self) -> f32;

    /// New input frames required to produce `out_frames` output frames at `ratio`.
    // ? `&mut self` because some resamplers (rubato) can only be told the ratio by mutating
    // ? their state before they can say how much input the next call wants.
    fn input_frames_needed(&mut self, ratio: f64, out_frames: usize) -> usize;

    /// Produce exactly `out_frames` (<= 128) frames into `output`, consuming the `in_frames`
    /// frames staged in `input` (`in_frames` must be `input_frames_needed(ratio, out_frames)`).
    fn process(
        &mut self,
        ratio: f64,
        input: &Work,
        in_frames: usize,
        output: &mut Planar,
        out_frames: usize,
    );
}
