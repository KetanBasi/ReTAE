//! Packet priority marking (CLAUDE.md law 3, "QoS"; PLAN.md §4.2).
//!
//! * Linux / Android / macOS: set `IP_TOS` on the socket (`DSCP << 2`).
//! * Windows: `IP_TOS` is **ignored** by the stack, so the socket is added to a qWAVE flow
//!   instead (`QOSAddSocketToFlow`), which makes Windows mark the packets itself.
//!
//! Marking is best effort: a refusal never stops audio, it is only reported in `QosOutcome`
//! (many Wi-Fi/consumer routers ignore or bleach DSCP anyway; the mark helps on managed LANs
//! and on APs that map EF/CS6 to WMM AC_VO).
//!
//! The only `unsafe` in this crate lives in the `qwave` module at the bottom (Windows only).

use std::io;
use std::net::{SocketAddrV4, UdpSocket};

// SECTION: DSCP helpers
/// The IPv4 TOS byte for a DSCP code point. The two low bits (ECN) stay 0.
pub const fn tos_byte(dscp: u8) -> u8 {
    (dscp & 0x3F) << 2
}

/// Windows qWAVE traffic types (the numeric values are `QOS_TRAFFIC_TYPE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrafficClass {
    BestEffort = 0,
    Background = 1,
    ExcellentEffort = 2,
    AudioVideo = 3,
    Voice = 4,
}

/// Which qWAVE class stands in for a DSCP value.
/// ? qWAVE has no call that says "use DSCP n" without administrator rights (`QOSSetFlow`), only
/// ? these classes, which Windows maps to fixed code points (Voice -> 46 / EF). So on Windows
/// ? the configured `dscp` selects the nearest class instead of being copied literally.
pub const fn traffic_class_for(dscp: u8) -> TrafficClass {
    match dscp & 0x3F {
        40..=63 => TrafficClass::Voice,           // EF (46), CS5..CS7
        24..=39 => TrafficClass::AudioVideo,      // CS3, AF3x, CS4, AF4x
        16..=23 => TrafficClass::ExcellentEffort, // CS2, AF2x
        8..=15 => TrafficClass::Background,       // CS1, AF1x
        _ => TrafficClass::BestEffort,
    }
}
// !SECTION: DSCP helpers

/// What `mark` managed to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QosOutcome {
    /// `dscp == 0`: nothing to mark.
    NotRequested,
    Applied,
    /// The OS refused (or this platform can't). Audio still flows, unprioritised.
    Failed(io::ErrorKind),
}

/// Keeps a platform QoS registration alive; drop it to remove it. Empty off Windows.
#[derive(Debug, Default)]
pub struct QosGuard {
    #[cfg(windows)]
    _flow: Option<qwave::Flow>,
}

/// Mark `socket`'s packets towards `dest` with `dscp` (0 = best effort, 46 = EF).
/// ? The guard must outlive the socket's use; hold it next to the socket.
pub fn mark(socket: &UdpSocket, dest: SocketAddrV4, dscp: u8) -> (QosGuard, QosOutcome) {
    if dscp & 0x3F == 0 {
        return (QosGuard::default(), QosOutcome::NotRequested);
    }
    mark_impl(socket, dest, dscp)
}

#[cfg(unix)]
fn mark_impl(socket: &UdpSocket, _dest: SocketAddrV4, dscp: u8) -> (QosGuard, QosOutcome) {
    // ? `SockRef` borrows the std socket as a socket2 one for the duration of the call; it
    // ? never closes it. That is how we reach `IP_TOS`, which std does not expose.
    let outcome = match socket2::SockRef::from(socket).set_tos_v4(u32::from(tos_byte(dscp))) {
        Ok(()) => QosOutcome::Applied,
        Err(e) => QosOutcome::Failed(e.kind()),
    };
    (QosGuard::default(), outcome)
}

#[cfg(windows)]
fn mark_impl(socket: &UdpSocket, dest: SocketAddrV4, dscp: u8) -> (QosGuard, QosOutcome) {
    match qwave::Flow::new(socket, dest, traffic_class_for(dscp)) {
        Ok(flow) => (QosGuard { _flow: Some(flow) }, QosOutcome::Applied),
        Err(e) => (QosGuard::default(), QosOutcome::Failed(e.kind())),
    }
}

#[cfg(not(any(unix, windows)))]
fn mark_impl(_socket: &UdpSocket, _dest: SocketAddrV4, _dscp: u8) -> (QosGuard, QosOutcome) {
    (
        QosGuard::default(),
        QosOutcome::Failed(io::ErrorKind::Unsupported),
    )
}

// SECTION: qWAVE (Windows)
// !! The only unsafe code in net-vban. Everything crossing the FFI boundary is a plain integer
// !! or a pointer to a local that outlives the call; see each SAFETY note.
#[cfg(windows)]
#[allow(unsafe_code)]
mod qwave {
    use super::TrafficClass;
    use std::net::{SocketAddr, SocketAddrV4, UdpSocket};
    use std::os::windows::io::AsRawSocket;
    use std::{io, ptr};
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::NetworkManagement::QoS::{
        QOS_NON_ADAPTIVE_FLOW, QOS_VERSION, QOSAddSocketToFlow, QOSCloseHandle, QOSCreateHandle,
    };
    use windows_sys::Win32::Networking::WinSock::{SOCKADDR, SOCKET};

    /// One qWAVE session holding one flow. Closing the handle removes the flow too.
    #[derive(Debug)]
    pub struct Flow {
        handle: HANDLE,
    }

    // SAFETY: a qWAVE handle is an opaque kernel handle; the API documents no thread
    // affinity, and `Flow` only ever closes it (from whichever thread drops it).
    unsafe impl Send for Flow {}

    impl Flow {
        pub fn new(
            socket: &UdpSocket,
            dest: SocketAddrV4,
            class: TrafficClass,
        ) -> io::Result<Self> {
            let version = QOS_VERSION {
                MajorVersion: 1,
                MinorVersion: 0,
            };
            let mut handle: HANDLE = ptr::null_mut();
            // SAFETY: `version` and `handle` are live locals for the whole call; qwave.dll
            // writes the new handle through the second pointer.
            if unsafe { QOSCreateHandle(&version, &mut handle) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // From here on `flow` owns the handle, so an early return closes it.
            let flow = Self { handle };

            // ? A SOCKADDR_IN built by socket2; the cast goes through a raw pointer because
            // ? socket2 and this crate may link different windows-sys versions.
            let addr = socket2::SockAddr::from(SocketAddr::V4(dest));
            let mut flow_id: u32 = 0; // 0 = "create a new flow"
            // SAFETY: the socket is open (borrowed for this call), `addr` is a valid
            // SOCKADDR_IN that outlives the call, `flow_id` is a live local.
            let ok = unsafe {
                QOSAddSocketToFlow(
                    flow.handle,
                    socket.as_raw_socket() as SOCKET,
                    addr.as_ptr().cast::<SOCKADDR>(),
                    class as i32,
                    QOS_NON_ADAPTIVE_FLOW,
                    &mut flow_id,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(flow)
        }
    }

    impl Drop for Flow {
        fn drop(&mut self) {
            // SAFETY: `handle` came from a successful `QOSCreateHandle` and is closed once.
            unsafe {
                QOSCloseHandle(self.handle);
            }
        }
    }
}
// !SECTION: qWAVE (Windows)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tos_byte_is_dscp_shifted_past_the_ecn_bits() {
        assert_eq!(tos_byte(46), 184); // EF
        assert_eq!(tos_byte(48), 192); // CS6
        assert_eq!(tos_byte(0), 0);
        assert_eq!(tos_byte(63), 252);
        // Out-of-range input can't spill into the ECN bits or wrap.
        assert_eq!(tos_byte(0xFF), 252);
    }

    #[test]
    fn every_dscp_maps_to_the_class_a_router_would_pick() {
        assert_eq!(traffic_class_for(46), TrafficClass::Voice);
        assert_eq!(traffic_class_for(48), TrafficClass::Voice);
        assert_eq!(traffic_class_for(40), TrafficClass::Voice);
        assert_eq!(traffic_class_for(34), TrafficClass::AudioVideo);
        assert_eq!(traffic_class_for(26), TrafficClass::AudioVideo);
        assert_eq!(traffic_class_for(18), TrafficClass::ExcellentEffort);
        assert_eq!(traffic_class_for(8), TrafficClass::Background);
        assert_eq!(traffic_class_for(0), TrafficClass::BestEffort);
        assert_eq!(traffic_class_for(7), TrafficClass::BestEffort);
    }

    #[test]
    fn traffic_classes_carry_the_windows_enum_values() {
        // QOS_TRAFFIC_TYPE: BestEffort 0, Background 1, ExcellentEffort 2, AudioVideo 3, Voice 4.
        assert_eq!(TrafficClass::BestEffort as i32, 0);
        assert_eq!(TrafficClass::Background as i32, 1);
        assert_eq!(TrafficClass::ExcellentEffort as i32, 2);
        assert_eq!(TrafficClass::AudioVideo as i32, 3);
        assert_eq!(TrafficClass::Voice as i32, 4);
    }

    fn loopback() -> (UdpSocket, SocketAddrV4) {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        let dest = SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, 9);
        (s, dest)
    }

    #[test]
    fn dscp_zero_changes_nothing() {
        let (s, dest) = loopback();
        let (_guard, outcome) = mark(&s, dest, 0);
        assert_eq!(outcome, QosOutcome::NotRequested);
    }

    #[cfg(unix)]
    #[test]
    fn ip_tos_is_really_set_on_the_socket() {
        let (s, dest) = loopback();
        let before = socket2::SockRef::from(&s).tos_v4().unwrap();
        assert_eq!(before, 0);
        let (_guard, outcome) = mark(&s, dest, 46);
        assert_eq!(outcome, QosOutcome::Applied);
        // Read it back from the kernel rather than trusting our own return value.
        assert_eq!(socket2::SockRef::from(&s).tos_v4().unwrap(), 184);
        let (_guard, outcome) = mark(&s, dest, 48);
        assert_eq!(outcome, QosOutcome::Applied);
        assert_eq!(socket2::SockRef::from(&s).tos_v4().unwrap(), 192);
    }
}
