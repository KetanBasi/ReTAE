//! The raw Windows calls the backend needs besides WASAPI itself: MMCSS, the OS build number,
//! and the process list.
//!
//! !! This is the only module of `platform-io` with `unsafe` code. Each block has a SAFETY note:
//! !! every pointer passed is to a local that outlives the call, and every handle is closed.

use crate::rt::RtOutcome;
use crate::win_rules::ProcEntry;
use windows_sys::Wdk::System::SystemServices::RtlGetVersion;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows_sys::Win32::System::Threading::{
    AVRT_PRIORITY_HIGH, AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority,
};

/// Ask the Multimedia Class Scheduler Service to treat the calling thread as pro audio
/// (PLAN.md §4.3). MMCSS lifts the thread's priority and keeps other work from starving it.
// ? The task name "Pro Audio" is a key under
// ? HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Multimedia\SystemProfile\Tasks. The
// ? thread keeps the characteristics until it ends, so the returned handle is not reverted.
pub fn promote_current_thread_mmcss() -> RtOutcome {
    let task: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
    let mut index = 0u32;
    // SAFETY: `task` is NUL-terminated UTF-16 and `index` is a live local; both outlive the call.
    let handle = unsafe { AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut index) };
    if handle.is_null() {
        // SAFETY: plain thread-local error read, no arguments.
        let code = unsafe { GetLastError() };
        return RtOutcome::Unavailable(format!("MMCSS refused the thread (Windows error {code})"));
    }
    // SAFETY: `handle` was just returned by the call above and is valid for this thread.
    // The result is ignored: the default priority within the class is already acceptable.
    let _ = unsafe { AvSetMmThreadPriority(handle, AVRT_PRIORITY_HIGH) };
    RtOutcome::Promoted
}

/// The real Windows build number (for example 22631), or 0 if it cannot be read.
// ? `GetVersionEx` lies to programs without a compatibility manifest (it reports Windows 8).
// ? `RtlGetVersion` in ntdll does not, and it is what the process-loopback check needs.
pub fn os_build() -> u32 {
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        dwMajorVersion: 0,
        dwMinorVersion: 0,
        dwBuildNumber: 0,
        dwPlatformId: 0,
        szCSDVersion: [0; 128],
    };
    // SAFETY: `info` is a live, correctly sized OSVERSIONINFOW whose size field is set.
    let status = unsafe { RtlGetVersion(&mut info) };
    if status >= 0 { info.dwBuildNumber } else { 0 }
}

/// A snapshot of all processes: id, parent id and executable name.
pub fn process_table() -> Vec<ProcEntry> {
    let mut out = Vec::new();
    // SAFETY: no pointer arguments; the returned handle is closed below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return out;
    }
    // SAFETY: an all-zero PROCESSENTRY32W is a valid value (plain integers and an array).
    let mut entry: PROCESSENTRY32W = unsafe { core::mem::zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    // SAFETY: `snapshot` is valid and `entry` is a live struct with `dwSize` set.
    let mut more = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while more {
        let len = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        out.push(ProcEntry {
            pid: entry.th32ProcessID,
            parent: entry.th32ParentProcessID,
            exe: String::from_utf16_lossy(&entry.szExeFile[..len]),
        });
        // SAFETY: same as `Process32FirstW` above.
        more = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    // SAFETY: `snapshot` came from CreateToolhelp32Snapshot and is closed exactly once.
    let _ = unsafe { CloseHandle(snapshot) };
    out
}
