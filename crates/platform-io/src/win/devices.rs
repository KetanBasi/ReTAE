//! Listing and opening Windows audio endpoints through `wasapi`.
//!
//! Device ids are the endpoint ids Windows itself uses (`{0.0.0.00000000}.{guid}`), which stay the
//! same across reboots. `"default"` (or an empty id) means the default device for the console
//! role. A render endpoint is an `Output` device; device loopback records an `Output` device.

use crate::backend::{BackendError, DeviceKind};
use crate::win_rules::Endpoint;
use audio_types::MAX_CHANNELS;
use wasapi::{Device, DeviceEnumerator, Direction, SessionState, WasapiError};

// SECTION: COM
// ? WASAPI is COM, and a thread must join the COM "multithreaded apartment" before it makes any
// ? call. The control plane calls the backend from whichever thread it happens to be on, so
// ? each thread joins once, the first time it needs to, and leaves when it ends. The result is
// ? ignored on purpose: a thread that is already in another apartment (a UI thread) can still
// ? use the in-process objects, which is all we need.
struct ComGuard {
    /// Only a successful `CoInitializeEx` (including "already initialised") may be paired with
    /// a `CoUninitialize`; a refused one (thread already in another apartment) must not be.
    joined: bool,
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.joined {
            wasapi::deinitialize();
        }
    }
}

thread_local! {
    static COM: ComGuard = ComGuard {
        joined: wasapi::initialize_mta().is_ok(),
    };
}

/// Join COM on the calling thread (idempotent).
pub fn ensure_com() {
    COM.with(|_| {});
}
// !SECTION: COM

pub fn direction_of(kind: DeviceKind) -> Direction {
    match kind {
        DeviceKind::Input => Direction::Capture,
        DeviceKind::Output => Direction::Render,
    }
}

/// An endpoint as the UI lists it.
#[derive(Clone, Debug)]
pub struct Listed {
    pub endpoint: Endpoint,
    pub channels: u8,
    pub is_default: bool,
}

/// Every active endpoint, inputs first, defaults first within each direction.
pub fn list_endpoints() -> Result<Vec<Listed>, BackendError> {
    ensure_com();
    let enumerator = DeviceEnumerator::new().map_err(|e| map_error(&e, "the device list"))?;
    let mut out = Vec::new();
    for kind in [DeviceKind::Input, DeviceKind::Output] {
        let dir = direction_of(kind);
        let default_id = enumerator
            .get_default_device(&dir)
            .and_then(|d| d.get_id())
            .ok();
        let collection = enumerator
            .get_device_collection(&dir)
            .map_err(|e| map_error(&e, "the device list"))?;
        let count = collection.get_nbr_devices().unwrap_or(0);
        for index in 0..count {
            // ? One broken endpoint (a driver that fails to answer) must not hide the others.
            let Ok(device) = collection.get_device_at_index(index) else {
                continue;
            };
            let (Ok(id), Ok(name)) = (device.get_id(), device.get_friendlyname()) else {
                continue;
            };
            let channels = device
                .get_device_format()
                .map_or(2, |f| f.get_nchannels() as usize)
                .clamp(1, MAX_CHANNELS) as u8;
            out.push(Listed {
                is_default: default_id.as_deref() == Some(id.as_str()),
                endpoint: Endpoint { id, name, kind },
                channels,
            });
        }
    }
    out.sort_by(|a, b| {
        (a.endpoint.kind as u8, !a.is_default, &a.endpoint.name).cmp(&(
            b.endpoint.kind as u8,
            !b.is_default,
            &b.endpoint.name,
        ))
    });
    Ok(out)
}

/// Open an endpoint by id (or the default one) and check it has the direction asked for.
pub fn open_device(id: &str, kind: DeviceKind) -> Result<Device, BackendError> {
    ensure_com();
    let enumerator = DeviceEnumerator::new().map_err(|e| map_error(&e, id))?;
    let dir = direction_of(kind);
    let device = if id.is_empty() || id == "default" {
        enumerator.get_default_device(&dir)
    } else {
        enumerator.get_device(id)
    }
    .map_err(|e| map_error(&e, id))?;
    // ? `get_device` finds any endpoint, so a recording id could be passed where a playback one
    // ? is meant. Refusing here gives a clear message instead of a confusing WASAPI failure.
    if device.get_direction() == dir {
        Ok(device)
    } else {
        Err(BackendError::NoSuchDevice(id.to_owned()))
    }
}

/// Programs that have an audio session on any playback device: `(pid, playing now)`.
/// Pid 0 (the system sounds session) is left out. A program on two devices appears once.
pub fn app_sessions() -> Vec<(u32, bool)> {
    ensure_com();
    let mut found: Vec<(u32, bool)> = Vec::new();
    let Ok(enumerator) = DeviceEnumerator::new() else {
        return found;
    };
    let Ok(devices) = enumerator.get_device_collection(&Direction::Render) else {
        return found;
    };
    let count = devices.get_nbr_devices().unwrap_or(0);
    for index in 0..count {
        let sessions = devices
            .get_device_at_index(index)
            .and_then(|d| d.get_iaudiosessionmanager())
            .and_then(|m| m.get_audiosessionenumerator());
        let Ok(sessions) = sessions else { continue };
        for i in 0..sessions.get_count().unwrap_or(0) {
            let Ok(session) = sessions.get_session(i) else {
                continue;
            };
            let Ok(pid) = session.get_process_id() else {
                continue;
            };
            if pid == 0 {
                continue;
            }
            let playing = session.get_state().is_ok_and(|s| s == SessionState::Active);
            match found.iter_mut().find(|(p, _)| *p == pid) {
                Some(entry) => entry.1 |= playing,
                None => found.push((pid, playing)),
            }
        }
    }
    found
}

// SECTION: Errors
// HRESULTs from audioclient.h that mean something to the person using the program.
const AUDCLNT_E_DEVICE_IN_USE: u32 = 0x8889_000A;
const AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED: u32 = 0x8889_000E;
const AUDCLNT_E_DEVICE_INVALIDATED: u32 = 0x8889_0004;
const AUDCLNT_E_ENDPOINT_CREATE_FAILED: u32 = 0x8889_0008;
const E_NOTFOUND: u32 = 0x8007_0490;
const E_ACCESSDENIED: u32 = 0x8007_0005;

/// The HRESULT inside a WASAPI error, if it carries one.
pub fn hresult_of(e: &WasapiError) -> Option<u32> {
    match e {
        WasapiError::Windows(w) => Some(w.code().0 as u32),
        _ => None,
    }
}

pub fn map_error(e: &WasapiError, what: &str) -> BackendError {
    match (e, hresult_of(e)) {
        (WasapiError::DeviceNotFound(_), _) => BackendError::NoSuchDevice(what.to_owned()),
        (_, Some(AUDCLNT_E_DEVICE_IN_USE | AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED)) => {
            BackendError::Busy(format!("{what} (another program holds it exclusively)"))
        }
        (_, Some(AUDCLNT_E_DEVICE_INVALIDATED | AUDCLNT_E_ENDPOINT_CREATE_FAILED | E_NOTFOUND)) => {
            BackendError::NoSuchDevice(what.to_owned())
        }
        (_, Some(E_ACCESSDENIED)) => BackendError::Io(format!(
            "{what}: access denied (Settings > Privacy > Microphone may be off)"
        )),
        _ => BackendError::Io(format!("{what}: {e}")),
    }
}
// !SECTION: Errors
