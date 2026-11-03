//! Control plane shared by every UI: `ControlApi`, TOML profiles, `UiLayout`, IPC protocol.
//!
//! * `types`: the serde data that crosses `ControlApi` (commands, state, meters, probe).
//! * `control`: the `ControlApi` trait. `local::LocalControl` runs the engine in-process;
//!   `ipc::RemoteControl` talks to a daemon. UIs hold an `Arc<dyn ControlApi>`.
//! * `profile`: TOML save/load. `ui_state`: PLAN.md §9.8. `paths`: where files and sockets go.

#![forbid(unsafe_code)]

pub mod control;
pub mod ipc;
pub mod local;
pub mod paths;
pub mod probe;
pub mod profile;
pub mod types;
pub mod ui_state;

pub use control::{ControlApi, ControlResult};
pub use local::{DaemonConfig, LocalControl};
pub use types::{Command, ControlError, MeterSnapshot, StateSnapshot};
