//! `audio-engine`: the real-time mixer (PLAN.md §7).
//!
//! ```text
//!  device/net thread --> InputPort ring --\                        /--> OutputPort ring --> device/net thread
//!  device/net thread --> InputPort ring ---+--> MixerEngine -------+--> OutputPort ring --> device/net thread
//!  control thread --> EngineCommand queue -/   (process_block)     \--> RecorderPort ring --> recorder thread
//! ```
//!
//! * `create_engine()` builds the engine plus the control-plane handles. The engine goes to
//!   the audio thread; the handles stay on the control thread.
//! * `MixerEngine::process_block(frames)` is the **only** call the RT thread makes. It obeys
//!   CLAUDE.md law 1: no alloc, no locks, no syscalls, no logging.
//!
//! # `assert_no_alloc`
//! `process_block` wraps itself in `assert_no_alloc`. That only *bites* if the program
//! installs the checking allocator, which a library must not do for its users:
//! ```ignore
//! #[cfg(debug_assertions)]
//! #[global_allocator]
//! static ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
//! ```
//! `routerd` and this crate's tests do that in debug builds; release builds compile it out.

// ? The RT engine has no use for `unsafe`; forbidding it makes that a compile-time guarantee.
#![forbid(unsafe_code)]

mod block;
mod bus;
pub mod drift;
pub mod dsp;
mod engine;
mod meter;
mod mixmap;
mod params;
pub mod ports;
mod stats;
mod strip;

pub use drift::{ClockDomainAdapter, DriftController, DriftResampler, HermiteResampler};
pub use engine::{
    DEFAULT_COMMAND_CAPACITY, EngineHandles, MAX_COMMANDS_PER_BLOCK, MixerEngine, create_engine,
};
pub use meter::{PEAK_FALL_DB_PER_S, RMS_TAU_MS};
pub use mixmap::pan_gains;
pub use params::{sanitize_band, sanitize_bands, sanitize_comp, sanitize_dsp, sanitize_gate};
pub use ports::{
    ControlPorts, EnginePorts, InputPort, MAX_PORTS, OutputPort, PORT_RING_SAMPLES, RecorderPort,
    create_port_pool, input_slot, output_slot, recorder_slot,
};
pub use stats::EngineStats;

// Unit tests in this crate allocate freely *outside* `process_block`; inside it the checking
// allocator aborts the process on any alloc/free.
#[cfg(all(test, debug_assertions))]
#[global_allocator]
static ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
