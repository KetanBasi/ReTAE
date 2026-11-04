//! Windows decisions that need no Windows to test (PLAN.md §6.1).
//!
//! The WASAPI backend (`win/`) only compiles on Windows, so everything it *decides* from plain
//! data lives here instead and is tested on every platform:
//!
//! * which Windows builds have process loopback,
//! * which application process to hand to process loopback (parent-PID resolution),
//! * which devices are virtual audio cables, and how a cable's two endpoints pair up.
//!
//! Nothing here calls the OS.

use crate::backend::DeviceKind;

// SECTION: Process loopback
/// Process loopback (`ActivateAudioInterfaceAsync` with `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`)
/// exists from Windows 10 build 20348 (Server 2022) and Windows 11. Win10 22H2 is build 19045
/// and does not have it (PLAN.md §2 row 13).
pub const PROCESS_LOOPBACK_MIN_BUILD: u32 = 20348;

pub fn process_loopback_supported(build: u32) -> bool {
    build >= PROCESS_LOOPBACK_MIN_BUILD
}

/// One row of the process list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcEntry {
    pub pid: u32,
    pub parent: u32,
    /// File name of the executable, for example `chrome.exe`.
    pub exe: String,
}

/// The process to hand to process loopback for audio played by `pid`.
///
/// Browsers, Discord and Electron apps play sound from a child process (`chrome.exe` renderer
/// or audio service) while the "application" is the root of a tree of the same executable.
/// Capturing the child alone would miss the other children, and process loopback can include a
/// whole tree, so we climb to the topmost ancestor that runs the *same executable* and stop
/// there: a launcher, `explorer.exe` or a shell above it must not be captured too.
pub fn loopback_root(pid: u32, table: &[ProcEntry]) -> u32 {
    let find = |p: u32| table.iter().find(|e| e.pid == p);
    let Some(mut current) = find(pid) else {
        return pid;
    };
    // ? The hop limit guards against a cycle in a table that was read while processes were
    // ? starting and ending (a recycled parent id can point back down the tree).
    for _ in 0..32 {
        match find(current.parent) {
            Some(parent)
                if parent.pid != current.pid && parent.exe.eq_ignore_ascii_case(&current.exe) =>
            {
                current = parent;
            }
            _ => break,
        }
    }
    current.pid
}
// !SECTION: Process loopback

// SECTION: Virtual cables
/// Which end of a virtual cable an endpoint is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CableSide {
    /// A playback device: other programs play *into* the cable here ("CABLE Input").
    PlaybackEnd,
    /// A recording device: the cable's audio comes *out* here ("CABLE Output").
    RecordingEnd,
}

/// A recognised cable endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CableEndpoint {
    /// Names the cable, shared by its two endpoints (`VB-Audio Virtual Cable`).
    pub family: String,
    pub side: CableSide,
}

/// Recognise an endpoint of a VB-Audio cable (VB-CABLE, CABLE A/B, Hi-Fi Cable).
///
/// The names look like `CABLE Input (VB-Audio Virtual Cable)`; the text in brackets is the same
/// for the playback and the recording end, which is how the two are paired. VoiceMeeter's own
/// devices (`VoiceMeeter Input (VB-Audio VoiceMeeter VAIO)`, `… Point`, `… Aux`) are **not**
/// cables: they only pass audio while VoiceMeeter's engine runs, which is the program this
/// project replaces (PLAN.md §6.1).
///
/// `kind` is the endpoint's direction: `Output` = playback device, `Input` = recording device.
pub fn parse_cable(friendly_name: &str, kind: DeviceKind) -> Option<CableEndpoint> {
    let open = friendly_name.rfind('(')?;
    let close = friendly_name.rfind(')')?;
    if close <= open {
        return None;
    }
    let family = friendly_name[open + 1..close].trim();
    let lead = friendly_name[..open].trim();
    let (fam_l, lead_l) = (family.to_ascii_lowercase(), lead.to_ascii_lowercase());

    if !fam_l.starts_with("vb-audio") || !fam_l.contains("cable") {
        return None;
    }
    if ["voicemeeter", "vaio", "point"]
        .iter()
        .any(|w| fam_l.contains(w) || lead_l.contains(w))
    {
        return None;
    }
    // The word in the lead says which end it is; it must agree with the endpoint's direction,
    // or this is something else that merely mentions a cable.
    let side = match kind {
        DeviceKind::Output if lead_l.ends_with("input") => CableSide::PlaybackEnd,
        DeviceKind::Input if lead_l.ends_with("output") => CableSide::RecordingEnd,
        _ => return None,
    };
    Some(CableEndpoint {
        family: family.to_owned(),
        side,
    })
}

/// A cable that has both ends present.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cable {
    pub family: String,
    /// Endpoint id of the playback end (we play into it to make a virtual microphone).
    pub playback_id: String,
    /// Endpoint id of the recording end (we record from it to make a virtual input).
    pub recording_id: String,
    /// Name of the playback end, for display ("CABLE Input (VB-Audio Virtual Cable)").
    pub playback_name: String,
    pub recording_name: String,
}

/// An endpoint as the backend lists it.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
}

/// Pair up the cables among `endpoints`. The result is sorted by family name, so index `n`
/// keeps meaning the same cable while the set of installed cables does not change.
pub fn find_cables(endpoints: &[Endpoint]) -> Vec<Cable> {
    let mut cables: Vec<Cable> = Vec::new();
    for ep in endpoints {
        let Some(c) = parse_cable(&ep.name, ep.kind) else {
            continue;
        };
        if c.side != CableSide::PlaybackEnd {
            continue;
        }
        let other = endpoints.iter().find(|o| {
            o.kind == DeviceKind::Input
                && parse_cable(&o.name, o.kind)
                    .is_some_and(|x| x.side == CableSide::RecordingEnd && x.family == c.family)
        });
        if let Some(other) = other {
            cables.push(Cable {
                family: c.family,
                playback_id: ep.id.clone(),
                recording_id: other.id.clone(),
                playback_name: ep.name.clone(),
                recording_name: other.name.clone(),
            });
        }
    }
    cables.sort_by(|a, b| a.family.cmp(&b.family));
    cables.dedup_by(|a, b| a.family == b.family);
    cables
}

/// The "Virtual Audio Driver" speaker (VirtualDrivers/Virtual-Audio-Driver). It is a playback
/// device only; we capture what is played to it with device loopback, which gives a virtual
/// input without needing the driver to pipe audio to its microphone.
// ? Unproven: the endpoint name comes from the project's README ("Virtual Audio Driver" for
// ? the speaker, "Virtual Mic Driver" for the mic), not from an installed copy.
pub fn is_virtual_audio_driver_speaker(friendly_name: &str, kind: DeviceKind) -> bool {
    kind == DeviceKind::Output
        && friendly_name
            .to_ascii_lowercase()
            .contains("virtual audio driver")
}

/// A virtual input the backend can offer: something other programs play into, that we record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualIn {
    /// What stops the same cable being opened twice (also as a virtual mic).
    pub claim: String,
    /// The endpoint to open: a recording end (`via_loopback == false`) or a playback device
    /// recorded with loopback (`via_loopback == true`).
    pub endpoint_id: String,
    pub via_loopback: bool,
    pub name: String,
}

/// A virtual microphone: we play into a cable's playback end, other programs record its output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualMic {
    pub claim: String,
    pub endpoint_id: String,
    pub name: String,
}

/// The virtual inputs and microphones among `endpoints`, in a stable order. Index `n` of
/// `SourceEndpoint::VirtualInput` / `SinkEndpoint::VirtualMic` selects entry `n`.
///
/// A cable is one pipe: what is played into its playback end comes out of its recording end. So
/// the same cable can be a virtual input **or** a virtual mic, never both at once, and both
/// lists carry the same `claim`. (Using one cable in both directions would feed the mixer its
/// own output.) Install a second cable to have both.
pub fn plan_virtuals(endpoints: &[Endpoint]) -> (Vec<VirtualIn>, Vec<VirtualMic>) {
    let cables = find_cables(endpoints);
    let mut inputs: Vec<VirtualIn> = cables
        .iter()
        .map(|c| VirtualIn {
            claim: c.family.clone(),
            endpoint_id: c.recording_id.clone(),
            via_loopback: false,
            name: c.recording_name.clone(),
        })
        .collect();
    let mics = cables
        .iter()
        .map(|c| VirtualMic {
            claim: c.family.clone(),
            endpoint_id: c.playback_id.clone(),
            name: c.playback_name.clone(),
        })
        .collect();
    for ep in endpoints {
        if is_virtual_audio_driver_speaker(&ep.name, ep.kind) {
            inputs.push(VirtualIn {
                claim: ep.id.clone(),
                endpoint_id: ep.id.clone(),
                via_loopback: true,
                name: ep.name.clone(),
            });
        }
    }
    (inputs, mics)
}
// !SECTION: Virtual cables

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(id: &str, name: &str, kind: DeviceKind) -> Endpoint {
        Endpoint {
            id: id.into(),
            name: name.into(),
            kind,
        }
    }

    #[test]
    fn process_loopback_starts_at_build_20348() {
        assert!(!process_loopback_supported(19045)); // Windows 10 22H2
        assert!(!process_loopback_supported(20347));
        assert!(process_loopback_supported(20348)); // Server 2022
        assert!(process_loopback_supported(22631)); // Windows 11 23H2
    }

    fn proc(pid: u32, parent: u32, exe: &str) -> ProcEntry {
        ProcEntry {
            pid,
            parent,
            exe: exe.into(),
        }
    }

    #[test]
    fn a_browser_helper_resolves_to_the_browser_root() {
        let table = [
            proc(4, 0, "System"),
            proc(100, 4, "explorer.exe"),
            proc(200, 100, "chrome.exe"),
            proc(210, 200, "chrome.exe"), // audio service
            proc(220, 210, "Chrome.EXE"), // case differs, same program
            proc(300, 100, "notepad.exe"),
        ];
        assert_eq!(loopback_root(220, &table), 200);
        assert_eq!(loopback_root(210, &table), 200);
        assert_eq!(loopback_root(200, &table), 200, "the root stays the root");
        // The shell above the browser is a different program: never climbed into.
        assert_eq!(loopback_root(300, &table), 300);
        // Unknown pid: handed back unchanged, the OS will say it does not exist.
        assert_eq!(loopback_root(999, &table), 999);
    }

    #[test]
    fn a_cyclic_table_terminates() {
        let table = [proc(1, 2, "a.exe"), proc(2, 1, "a.exe")];
        let root = loopback_root(1, &table);
        assert!(root == 1 || root == 2);
        let selfref = [proc(5, 5, "a.exe")];
        assert_eq!(loopback_root(5, &selfref), 5);
    }

    #[test]
    fn vb_audio_cables_are_recognised_by_direction_and_name() {
        let p = parse_cable("CABLE Input (VB-Audio Virtual Cable)", DeviceKind::Output).unwrap();
        assert_eq!(p.family, "VB-Audio Virtual Cable");
        assert_eq!(p.side, CableSide::PlaybackEnd);
        let r = parse_cable("CABLE Output (VB-Audio Virtual Cable)", DeviceKind::Input).unwrap();
        assert_eq!(r.side, CableSide::RecordingEnd);
        assert!(parse_cable("CABLE-A Input (VB-Audio Cable A)", DeviceKind::Output).is_some());
        assert!(
            parse_cable(
                "Hi-Fi Cable Input (VB-Audio Hi-Fi Cable)",
                DeviceKind::Output
            )
            .is_some()
        );
        // Wrong direction for the word: not a cable end.
        assert!(parse_cable("CABLE Input (VB-Audio Virtual Cable)", DeviceKind::Input).is_none());
    }

    #[test]
    fn voicemeeter_devices_and_ordinary_hardware_are_not_cables() {
        for (name, kind) in [
            (
                "VoiceMeeter Input (VB-Audio VoiceMeeter VAIO)",
                DeviceKind::Output,
            ),
            (
                "VoiceMeeter Aux Input (VB-Audio VoiceMeeter AUX VAIO)",
                DeviceKind::Output,
            ),
            (
                "VoiceMeeter Output (VB-Audio VoiceMeeter VAIO)",
                DeviceKind::Input,
            ),
            ("Speakers (Realtek(R) Audio)", DeviceKind::Output),
            ("Headset Microphone (USB Audio)", DeviceKind::Input),
            ("no brackets", DeviceKind::Output),
            (") backwards (", DeviceKind::Output),
        ] {
            assert!(parse_cable(name, kind).is_none(), "{name}");
        }
    }

    #[test]
    fn cables_pair_up_and_come_back_in_a_stable_order() {
        let list = [
            ep("o1", "Speakers (Realtek(R) Audio)", DeviceKind::Output),
            ep("o2", "CABLE-B Input (VB-Audio Cable B)", DeviceKind::Output),
            ep("i2", "CABLE-B Output (VB-Audio Cable B)", DeviceKind::Input),
            ep("o3", "CABLE-A Input (VB-Audio Cable A)", DeviceKind::Output),
            ep("i3", "CABLE-A Output (VB-Audio Cable A)", DeviceKind::Input),
            // An orphan: the recording end is not installed or is disabled.
            ep(
                "o4",
                "CABLE Input (VB-Audio Virtual Cable)",
                DeviceKind::Output,
            ),
        ];
        let cables = find_cables(&list);
        assert_eq!(cables.len(), 2, "the orphan has no pair");
        assert_eq!(cables[0].family, "VB-Audio Cable A");
        assert_eq!(
            (
                cables[0].playback_id.as_str(),
                cables[0].recording_id.as_str()
            ),
            ("o3", "i3")
        );
        assert_eq!(cables[1].family, "VB-Audio Cable B");
    }

    #[test]
    fn one_cable_is_one_input_and_one_mic_sharing_a_claim() {
        let list = [
            ep(
                "o1",
                "CABLE Input (VB-Audio Virtual Cable)",
                DeviceKind::Output,
            ),
            ep(
                "i1",
                "CABLE Output (VB-Audio Virtual Cable)",
                DeviceKind::Input,
            ),
            ep("o2", "Virtual Audio Driver", DeviceKind::Output),
        ];
        let (inputs, mics) = plan_virtuals(&list);
        assert_eq!(inputs.len(), 2);
        assert_eq!(mics.len(), 1, "the speaker-only driver cannot be a mic");
        // We record the cable's *output* end and play into its *input* end.
        assert_eq!(
            (inputs[0].endpoint_id.as_str(), inputs[0].via_loopback),
            ("i1", false)
        );
        assert_eq!(mics[0].endpoint_id, "o1");
        assert_eq!(inputs[0].claim, mics[0].claim);
        assert_eq!(
            (inputs[1].endpoint_id.as_str(), inputs[1].via_loopback),
            ("o2", true)
        );
        assert_eq!(plan_virtuals(&[]), (Vec::new(), Vec::new()));
    }

    #[test]
    fn the_virtual_audio_driver_speaker_is_a_playback_device_only() {
        assert!(is_virtual_audio_driver_speaker(
            "Virtual Audio Driver",
            DeviceKind::Output
        ));
        assert!(!is_virtual_audio_driver_speaker(
            "Virtual Audio Driver",
            DeviceKind::Input
        ));
        assert!(!is_virtual_audio_driver_speaker(
            "Speakers",
            DeviceKind::Output
        ));
    }
}
