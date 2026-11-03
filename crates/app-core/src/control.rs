//! The one interface every UI talks to (CLAUDE.md law 5: UIs are thin views over this).
//!
//! `LocalControl` implements it in-process; `RemoteControl` implements it over IPC. A UI
//! cannot tell which one it holds.

use crate::types::{
    Command, ControlError, MeterSnapshot, ProbeReport, ProbeRequest, StateSnapshot,
};
use platform_io::{AppInfo, DeviceInfo};

pub type ControlResult<T> = Result<T, ControlError>;

pub trait ControlApi: Send + Sync {
    /// Apply one change. The state it leaves behind is visible in the next `snapshot`.
    fn command(&self, cmd: Command) -> ControlResult<()>;

    /// Everything that is not a meter. Always complete: a UI replaces its copy.
    fn snapshot(&self) -> ControlResult<StateSnapshot>;

    /// Levels and live counters. Meant to be polled at about 30 Hz.
    fn meters(&self) -> ControlResult<MeterSnapshot>;

    fn devices(&self) -> ControlResult<Vec<DeviceInfo>>;

    fn apps(&self) -> ControlResult<Vec<AppInfo>>;

    /// Measure the pipeline (takes seconds).
    fn probe(&self, request: ProbeRequest) -> ControlResult<ProbeReport>;
}
