//! Platform audio I/O: the `AudioBackend` trait, a mock backend for tests and headless runs,
//! the timer-thread master clock, and the Linux backends (PipeWire, S5). WASAPI is S6 and
//! AAudio S8.
//!
//! * The mixer never sees a device. A backend moves audio between a device and one of the
//!   engine's pre-allocated port rings, and `app-core` owns the wiring (CLAUDE.md law 1).
//! * Backends that bind a C library are cargo features and are off by default, so a plain
//!   `cargo build` needs no system packages: `pipewire`, `alsa`, `rt` (see `Cargo.toml`).

#![forbid(unsafe_code)]

pub mod backend;
pub mod clock;
pub mod graph;
pub mod master;
pub mod mock;
pub mod rt;

// ? Device callbacks shared by the Linux backends. It is compiled (and tested) in every build,
// ? but PipeWire hands over bytes and ALSA hands over `f32` slices, so each backend leaves part
// ? of it unused. With both features on, the dead-code check sees every function used.
#[cfg_attr(
    not(all(feature = "pipewire", feature = "alsa")),
    allow(dead_code, unused_imports)
)]
mod ring_io;

#[cfg(all(target_os = "linux", feature = "pipewire"))]
pub mod pw;

#[cfg(all(target_os = "linux", feature = "alsa"))]
pub mod alsa;

pub use backend::{
    AppInfo, AudioBackend, AudioStream, BackendError, Capabilities, ClosedPort, DeviceInfo,
    DeviceKind, StreamStats,
};
pub use clock::RateClock;
pub use master::{MasterStats, TimerMaster};
pub use mock::{Capture, MockBackend, MockControl, Signal};

#[cfg(all(target_os = "linux", feature = "alsa"))]
pub use alsa::{AlsaBackend, AlsaOptions};
#[cfg(all(target_os = "linux", feature = "pipewire"))]
pub use pw::{PipeWireBackend, PipeWireOptions};
