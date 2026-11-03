//! Platform audio I/O: the `AudioBackend` trait, a mock backend for tests and headless runs,
//! and the timer-thread master clock. Real backends arrive in S5 (PipeWire), S6 (WASAPI/cpal)
//! and S8 (AAudio).
//!
//! * The mixer never sees a device. A backend moves audio between a device and one of the
//!   engine's pre-allocated port rings, and `app-core` owns the wiring (CLAUDE.md law 1).

#![forbid(unsafe_code)]

pub mod backend;
pub mod clock;
pub mod master;
pub mod mock;

pub use backend::{
    AppInfo, AudioBackend, AudioStream, BackendError, Capabilities, ClosedPort, DeviceInfo,
    DeviceKind, StreamStats,
};
pub use clock::RateClock;
pub use master::{MasterStats, TimerMaster};
pub use mock::{Capture, MockBackend, MockControl, Signal};
