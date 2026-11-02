//! Engine counters that are not part of the `audio-types` telemetry contract.
//!
//! * Shared as `Arc<EngineStats>`; the RT thread only ever does relaxed `fetch_add`.
//! * ? Kept out of `audio_types::Telemetry` because S1 uses that file unchanged (PLAN.md §11).
//!   Fold these in there if the UI needs them per port.

use core::sync::atomic::AtomicU32;

#[derive(Debug, Default)]
pub struct EngineStats {
    /// Blocks where a strip's ring had fewer frames than the block needed (zero-filled).
    pub input_underruns: AtomicU32,
    /// Blocks dropped because a bus-output ring was full (device not draining).
    pub output_overruns: AtomicU32,
    /// Blocks dropped because a recorder ring was full (writer thread too slow).
    pub recorder_overruns: AtomicU32,
    /// Commands ignored: bad id, bad slot, non-finite or out-of-range value.
    pub rejected_commands: AtomicU32,
}
