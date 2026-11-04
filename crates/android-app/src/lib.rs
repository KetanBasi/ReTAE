//! Android host for the router (PLAN.md §6.3, §11 S8).
//!
//! | Module | What | Tested on |
//! |---|---|---|
//! | `lifecycle` | foreground-service and screen-capture state machine (steps 1-6) | host |
//! | `natives` | the Java `native` methods Rust registers, checked against the Java files | host |
//! | `view_model` | the compact Simple-mode screen as data, and taps as `ControlApi` commands | host |
//! | `glue` | JNI: registration, notification, WifiLock/WakeLock, MediaProjection, engine host | phone only |
//!
//! * Java lives only in `resources/android/java/` and every method there forwards to a Rust
//!   `native` function. Nothing in here draws a pixel: the Makepad screen is a view over
//!   `view_model::screen` and `ControlApi` and is not part of this crate yet (see NOTES.md).

// ? `deny`, not `forbid`: the glue passes raw function pointers to `RegisterNatives` and opts in
// ? with `#[allow(unsafe_code)]` where it does.
#![deny(unsafe_code)]

pub mod lifecycle;
pub mod natives;
pub mod view_model;

#[cfg(target_os = "android")]
pub mod glue;
