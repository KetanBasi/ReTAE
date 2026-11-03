//! mDNS / DNS-SD discovery of other copies of this app (PLAN.md §4.5; CLAUDE.md law 3: broadcast
//! and multicast are for discovery only, audio is always unicast).
//!
//! Service type `_lanrouter._udp.local.`, TXT keys:
//!
//! | key         | value                                   |
//! |-------------|-----------------------------------------|
//! | `name`      | human-readable device name              |
//! | `os`        | `windows`, `linux`, `android`, ...      |
//! | `ver`       | app version, `1.2.3`                    |
//! | `vban_port` | UDP port VBAN audio is received on      |
//! | `codecs`    | `pcm` or `pcm,opus`                     |
//! | `ctrl_port` | UDP port of the control channel (0 = none) |
//!
//! This is the *primary* discovery path (it also sees machines that never answer VBAN pings).
//! The VBAN-SERVICE ping in `service.rs` is the fallback and the only way to find VoiceMeeter.
//!
//! ```text
//!  control thread:  Discovery::announce(&Advert)   --> mdns-sd daemon thread --> multicast
//!                   Discovery::start_browse()
//!                   Discovery::poll()  <-- flume channel <-- daemon thread <-- multicast
//!                   Discovery::devices()
//! ```
//!
//! # Not trusted
//! Anything in an mDNS answer can be forged by any machine on the link. So:
//! - TXT text is stripped of control characters and length-capped before it reaches a UI.
//! - **Only LAN-range addresses are kept** (`service::is_lan`). Otherwise a forged answer could
//!   name a public address and the user's next click on "stream to this device" would send audio
//!   to a stranger.
//! - Devices must carry a valid, non-zero `vban_port`; the rest is optional.
//!
//! The mdns-sd daemon runs its own thread, so none of this touches the audio path.

use crate::service::is_lan;
use audio_types::VBAN_DEFAULT_PORT;
use mdns_sd::{Receiver, ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashSet;
use std::fmt;
use std::net::{Ipv4Addr, SocketAddrV4};

/// The DNS-SD service type (PLAN.md §4.5).
pub const SERVICE_TYPE: &str = "_lanrouter._udp.local.";
/// Longest device name / os / version we keep from a TXT record.
pub const MAX_TEXT: usize = 64;
/// Devices kept in the table; a link with more than this is under attack or misconfigured.
pub const MAX_DEVICES: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveryError(pub String);

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "discovery: {}", self.0)
    }
}

impl std::error::Error for DiscoveryError {}

impl From<mdns_sd::Error> for DiscoveryError {
    fn from(e: mdns_sd::Error) -> Self {
        Self(e.to_string())
    }
}

// SECTION: What we announce
/// What this machine tells the LAN about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advert {
    /// Shown to the user on other machines. Also the mDNS instance name.
    pub name: String,
    pub os: String,
    pub version: String,
    pub vban_port: u16,
    pub opus: bool,
    /// 0 = no control channel.
    pub ctrl_port: u16,
}

impl Advert {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            os: std::env::consts::OS.to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            vban_port: VBAN_DEFAULT_PORT,
            opus: cfg!(feature = "opus"),
            ctrl_port: 0,
        }
    }

    pub fn codecs(&self) -> &'static str {
        if self.opus { "pcm,opus" } else { "pcm" }
    }

    /// The TXT record, key order as in the table above.
    pub fn txt(&self) -> Vec<(&'static str, String)> {
        vec![
            ("name", cap(&self.name)),
            ("os", cap(&self.os)),
            ("ver", cap(&self.version)),
            ("vban_port", self.vban_port.to_string()),
            ("codecs", self.codecs().to_owned()),
            ("ctrl_port", self.ctrl_port.to_string()),
        ]
    }

    /// `<label>.local.` for the SRV record: letters, digits and hyphens only.
    pub fn host_name(&self) -> String {
        let mut label: String = self
            .name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        label.truncate(40);
        let label = label.trim_matches('-');
        format!("{}.local.", if label.is_empty() { "retae" } else { label })
    }
}

/// Cut `s` to [`MAX_TEXT`] bytes on a character boundary.
fn cap(s: &str) -> String {
    let mut end = s.len().min(MAX_TEXT);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}
// !SECTION: What we announce

// SECTION: What we learn
/// What a TXT record said, already made safe to display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub os: String,
    pub version: String,
    pub vban_port: u16,
    pub opus: bool,
    pub ctrl_port: Option<u16>,
}

/// A peer found by mDNS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    /// DNS-SD full name, the table key (`Living room._lanrouter._udp.local.`).
    pub fullname: String,
    pub host: String,
    /// LAN IPv4 addresses only, sorted.
    pub addrs: Vec<Ipv4Addr>,
    pub info: DeviceInfo,
}

impl Device {
    /// Where to send VBAN audio to this device.
    pub fn vban_addr(&self) -> Option<SocketAddrV4> {
        self.addrs
            .first()
            .map(|&ip| SocketAddrV4::new(ip, self.info.vban_port))
    }

    pub fn ctrl_addr(&self) -> Option<SocketAddrV4> {
        let port = self.info.ctrl_port?;
        self.addrs.first().map(|&ip| SocketAddrV4::new(ip, port))
    }
}

/// Printable text from the network: no control characters, at most [`MAX_TEXT`] bytes, trimmed.
fn clean(s: &str) -> String {
    let filtered: String = s.chars().filter(|c| !c.is_control()).collect();
    cap(filtered.trim())
}

fn parse_port(s: &str) -> Option<u16> {
    s.trim().parse::<u16>().ok().filter(|&p| p != 0)
}

/// Interpret a TXT record. `get` looks a key up (case-insensitively, as DNS-SD does).
/// `None` when it is not one of ours (no usable `vban_port`).
pub fn parse_txt<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Option<DeviceInfo> {
    let vban_port = parse_port(get("vban_port")?)?;
    let codecs = get("codecs").unwrap_or("pcm");
    Some(DeviceInfo {
        name: get("name").map(clean).unwrap_or_default(),
        os: get("os").map(clean).unwrap_or_default(),
        version: get("ver").map(clean).unwrap_or_default(),
        vban_port,
        opus: codecs
            .split(',')
            .any(|c| c.trim().eq_ignore_ascii_case("opus")),
        ctrl_port: get("ctrl_port").and_then(parse_port),
    })
}

/// Build a [`Device`] from the parts of a resolved service. Pure, so it is testable without a
/// network (`ResolvedService` itself cannot be constructed outside mdns-sd).
pub fn device_from_parts<'a>(
    fullname: &str,
    host: &str,
    addrs: impl IntoIterator<Item = Ipv4Addr>,
    txt: impl Fn(&str) -> Option<&'a str>,
) -> Option<Device> {
    let info = parse_txt(txt)?;
    let mut addrs: Vec<Ipv4Addr> = addrs
        .into_iter()
        .filter(|&ip| is_lan(ip))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    // !! No LAN address, no device: see the module docs.
    if addrs.is_empty() {
        return None;
    }
    addrs.sort();
    let name = if info.name.is_empty() {
        // The instance name is the fallback label.
        clean(fullname.split("._lanrouter.").next().unwrap_or(fullname))
    } else {
        info.name.clone()
    };
    Some(Device {
        fullname: fullname.to_lowercase(),
        host: clean(host),
        addrs,
        info: DeviceInfo { name, ..info },
    })
}

fn device_from_resolved(r: &ResolvedService) -> Option<Device> {
    device_from_parts(
        r.get_fullname(),
        r.get_hostname(),
        r.get_addresses_v4(),
        |k| r.get_property_val_str(k),
    )
}
// !SECTION: What we learn

// SECTION: Device table
/// What a call to [`Discovery::poll`] changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Added(Device),
    Updated(Device),
    Removed(String),
}

/// The set of devices seen so far. Separate from the daemon so it is testable.
#[derive(Debug, Default)]
pub struct DeviceTable {
    devices: Vec<Device>,
    /// Our own announcement, ignored when it comes back.
    own: Option<String>,
}

impl DeviceTable {
    pub fn set_own(&mut self, fullname: Option<&str>) {
        self.own = fullname.map(str::to_lowercase);
        if let Some(own) = &self.own {
            self.devices.retain(|d| &d.fullname != own);
        }
    }

    pub fn devices(&self) -> &[Device] {
        &self.devices
    }

    pub fn clear(&mut self) {
        self.devices.clear();
    }

    fn upsert(&mut self, device: Device) -> Option<Change> {
        if self.own.as_deref() == Some(device.fullname.as_str()) {
            return None;
        }
        if let Some(slot) = self
            .devices
            .iter_mut()
            .find(|d| d.fullname == device.fullname)
        {
            if *slot == device {
                return None;
            }
            *slot = device.clone();
            return Some(Change::Updated(device));
        }
        if self.devices.len() >= MAX_DEVICES {
            return None;
        }
        self.devices.push(device.clone());
        Some(Change::Added(device))
    }

    fn remove(&mut self, fullname: &str) -> Option<Change> {
        let key = fullname.to_lowercase();
        let before = self.devices.len();
        self.devices.retain(|d| d.fullname != key);
        (self.devices.len() != before).then_some(Change::Removed(key))
    }

    /// Fold one daemon event into the table.
    pub fn apply(&mut self, event: &ServiceEvent) -> Option<Change> {
        match event {
            ServiceEvent::ServiceResolved(r) => match device_from_resolved(r) {
                Some(d) => self.upsert(d),
                // ? Resolved again with data we no longer accept (all addresses gone, bad port):
                // ? forget what we had rather than keep a stale entry.
                None => self.remove(r.get_fullname()),
            },
            ServiceEvent::ServiceRemoved(_, fullname) => self.remove(fullname),
            _ => None,
        }
    }
}
// !SECTION: Device table

// SECTION: Daemon handle
/// Announce this machine and/or browse for others. One mdns-sd daemon thread serves both.
pub struct Discovery {
    daemon: ServiceDaemon,
    announced: Option<String>,
    browse: Option<Receiver<ServiceEvent>>,
    table: DeviceTable,
}

impl Discovery {
    pub fn new() -> Result<Self, DiscoveryError> {
        Ok(Self::from_daemon(ServiceDaemon::new()?))
    }

    /// For tests: a private mDNS port so a run does not collide with the system's responder.
    pub fn with_port(port: u16) -> Result<Self, DiscoveryError> {
        Ok(Self::from_daemon(ServiceDaemon::new_with_port(port)?))
    }

    fn from_daemon(daemon: ServiceDaemon) -> Self {
        Self {
            daemon,
            announced: None,
            browse: None,
            table: DeviceTable::default(),
        }
    }

    /// Start (or replace) our announcement. `ip` is optional: left out, the daemon fills in and
    /// keeps current the addresses of every interface.
    pub fn announce(&mut self, advert: &Advert) -> Result<(), DiscoveryError> {
        self.unannounce();
        let txt: Vec<(&str, String)> = advert.txt();
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            &cap(&advert.name),
            &advert.host_name(),
            "",
            advert.vban_port,
            &txt[..],
        )?
        .enable_addr_auto();
        let fullname = info.get_fullname().to_owned();
        self.daemon.register(info)?;
        self.table.set_own(Some(&fullname));
        self.announced = Some(fullname);
        Ok(())
    }

    /// Withdraw our announcement (peers drop us within moments instead of after the TTL).
    pub fn unannounce(&mut self) {
        if let Some(fullname) = self.announced.take() {
            // ? Best effort: the daemon may already be gone while the app shuts down.
            let _ = self.daemon.unregister(&fullname);
        }
    }

    pub fn is_announcing(&self) -> bool {
        self.announced.is_some()
    }

    pub fn start_browse(&mut self) -> Result<(), DiscoveryError> {
        if self.browse.is_none() {
            self.browse = Some(self.daemon.browse(SERVICE_TYPE)?);
        }
        Ok(())
    }

    pub fn stop_browse(&mut self) {
        if self.browse.take().is_some() {
            let _ = self.daemon.stop_browse(SERVICE_TYPE);
        }
        self.table.clear();
    }

    pub fn is_browsing(&self) -> bool {
        self.browse.is_some()
    }

    /// Take everything the daemon has reported since the last call. Never blocks; call it from
    /// the UI's control tick.
    pub fn poll(&mut self) -> Vec<Change> {
        let mut changes = Vec::new();
        let Some(rx) = &self.browse else {
            return changes;
        };
        while let Ok(event) = rx.try_recv() {
            if let Some(change) = self.table.apply(&event) {
                changes.push(change);
            }
        }
        changes
    }

    pub fn devices(&self) -> &[Device] {
        self.table.devices()
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.unannounce();
        // ? The daemon thread otherwise outlives us; shutdown asks it to leave and says goodbye
        // ? for any registered service.
        let _ = self.daemon.shutdown();
    }
}
// !SECTION: Daemon handle

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup<'a>(
        map: &'a HashMap<&'static str, &'static str>,
    ) -> impl Fn(&str) -> Option<&'static str> + 'a {
        move |k| map.get(k).copied()
    }

    fn full_txt() -> HashMap<&'static str, &'static str> {
        HashMap::from([
            ("name", "Living room"),
            ("os", "linux"),
            ("ver", "0.1.0"),
            ("vban_port", "6980"),
            ("codecs", "pcm,opus"),
            ("ctrl_port", "6981"),
        ])
    }

    const FULLNAME: &str = "Living room._lanrouter._udp.local.";

    // SECTION: Announcement
    #[test]
    fn the_advert_carries_exactly_the_documented_keys() {
        let mut a = Advert::new("Desk PC");
        a.vban_port = 7000;
        a.ctrl_port = 7001;
        a.opus = true;
        let txt = a.txt();
        let keys: Vec<_> = txt.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            keys,
            ["name", "os", "ver", "vban_port", "codecs", "ctrl_port"]
        );
        let get = |k: &str| {
            txt.iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("name"), Some("Desk PC"));
        assert_eq!(get("os"), Some(std::env::consts::OS));
        assert_eq!(get("vban_port"), Some("7000"));
        assert_eq!(get("codecs"), Some("pcm,opus"));
        assert_eq!(get("ctrl_port"), Some("7001"));
        assert!(get("ver").unwrap().split('.').count() >= 3);
    }

    #[test]
    fn codecs_follow_the_opus_flag_and_the_build_default_follows_the_feature() {
        let mut a = Advert::new("x");
        a.opus = false;
        assert_eq!(a.codecs(), "pcm");
        a.opus = true;
        assert_eq!(a.codecs(), "pcm,opus");
        assert_eq!(Advert::new("x").opus, cfg!(feature = "opus"));
    }

    #[test]
    fn our_own_advert_parses_back_to_what_we_put_in() {
        let mut a = Advert::new("Desk PC");
        a.ctrl_port = 7001;
        a.opus = true;
        let txt = a.txt();
        let info = parse_txt(|k| {
            txt.iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.as_str())
        })
        .unwrap();
        assert_eq!(info.name, "Desk PC");
        assert_eq!(info.vban_port, a.vban_port);
        assert!(info.opus);
        assert_eq!(info.ctrl_port, Some(7001));
    }

    #[test]
    fn host_names_are_valid_dns_labels() {
        assert_eq!(
            Advert::new("Living room PC").host_name(),
            "living-room-pc.local."
        );
        assert_eq!(Advert::new("Ünïcode !!").host_name(), "n-code.local.");
        assert_eq!(Advert::new("!!!").host_name(), "retae.local.");
        assert_eq!(Advert::new("").host_name(), "retae.local.");
        let long = Advert::new(&"a".repeat(200)).host_name();
        assert!(long.len() <= 40 + ".local.".len());
    }

    #[test]
    fn long_names_are_cut_on_a_character_boundary() {
        let a = Advert::new(&"é".repeat(100));
        let name = &a.txt()[0].1;
        assert!(name.len() <= MAX_TEXT && name.chars().all(|c| c == 'é'));
    }
    // !SECTION: Announcement

    // SECTION: TXT parsing
    #[test]
    fn a_full_record_parses() {
        let map = full_txt();
        let info = parse_txt(lookup(&map)).unwrap();
        assert_eq!(
            info,
            DeviceInfo {
                name: "Living room".into(),
                os: "linux".into(),
                version: "0.1.0".into(),
                vban_port: 6980,
                opus: true,
                ctrl_port: Some(6981),
            }
        );
    }

    #[test]
    fn only_the_vban_port_is_required() {
        let map = HashMap::from([("vban_port", "6980")]);
        let info = parse_txt(lookup(&map)).unwrap();
        assert_eq!(info.vban_port, 6980);
        assert!(!info.opus, "no codecs key means PCM only");
        assert_eq!(info.ctrl_port, None);
        assert!(info.name.is_empty());
    }

    #[test]
    fn a_record_without_a_usable_port_is_not_one_of_ours() {
        for bad in ["", "0", "70000", "-1", "abc", "6980x"] {
            let map = HashMap::from([("vban_port", bad), ("name", "x")]);
            assert!(parse_txt(lookup(&map)).is_none(), "{bad:?}");
        }
        assert!(parse_txt(lookup(&HashMap::from([("name", "x")]))).is_none());
        assert!(parse_txt(lookup(&HashMap::new())).is_none());
    }

    #[test]
    fn a_bad_control_port_means_none_not_failure() {
        for bad in ["0", "abc", "99999", ""] {
            let map = HashMap::from([("vban_port", "6980"), ("ctrl_port", bad)]);
            assert_eq!(parse_txt(lookup(&map)).unwrap().ctrl_port, None, "{bad:?}");
        }
    }

    #[test]
    fn codec_lists_are_read_loosely() {
        for (codecs, opus) in [
            ("pcm", false),
            ("pcm,opus", true),
            ("PCM, Opus", true),
            ("opus", true),
            ("opusx", false),
            ("", false),
        ] {
            let map = HashMap::from([("vban_port", "6980"), ("codecs", codecs)]);
            assert_eq!(parse_txt(lookup(&map)).unwrap().opus, opus, "{codecs:?}");
        }
    }

    #[test]
    fn text_from_the_network_is_made_safe_to_show() {
        let long = "z".repeat(500);
        let map = HashMap::from([
            ("vban_port", "6980"),
            ("name", "  evil\x1b[2J\r\nname\u{7}  "),
            ("os", "li\0nux"),
        ]);
        let info = parse_txt(lookup(&map)).unwrap();
        assert_eq!(info.name, "evil[2Jname");
        assert_eq!(info.os, "linux");
        assert!(clean(&long).len() <= MAX_TEXT);
    }
    // !SECTION: TXT parsing

    // SECTION: Devices
    fn lan(a: [u8; 4]) -> Ipv4Addr {
        Ipv4Addr::from(a)
    }

    #[test]
    fn a_device_is_built_from_a_resolved_service() {
        let map = full_txt();
        let d = device_from_parts(
            FULLNAME,
            "living-room.local.",
            [lan([192, 168, 1, 20])],
            lookup(&map),
        )
        .unwrap();
        assert_eq!(d.fullname, "living room._lanrouter._udp.local.");
        assert_eq!(d.info.name, "Living room");
        assert_eq!(d.vban_addr(), Some("192.168.1.20:6980".parse().unwrap()));
        assert_eq!(d.ctrl_addr(), Some("192.168.1.20:6981".parse().unwrap()));
    }

    #[test]
    fn addresses_outside_the_lan_are_dropped() {
        let map = full_txt();
        let addrs = [
            lan([8, 8, 8, 8]),
            lan([192, 168, 1, 20]),
            lan([10, 0, 0, 2]),
            lan([192, 168, 1, 20]),
        ];
        let d = device_from_parts(FULLNAME, "h.local.", addrs, lookup(&map)).unwrap();
        assert_eq!(
            d.addrs,
            [lan([10, 0, 0, 2]), lan([192, 168, 1, 20])],
            "sorted, de-duplicated, LAN only"
        );

        // A forged answer naming only a public address yields no device at all.
        assert!(
            device_from_parts(FULLNAME, "h.local.", [lan([8, 8, 8, 8])], lookup(&map)).is_none()
        );
        assert!(device_from_parts(FULLNAME, "h.local.", [], lookup(&map)).is_none());
    }

    #[test]
    fn a_missing_name_falls_back_to_the_instance_name() {
        let map = HashMap::from([("vban_port", "6980")]);
        let d =
            device_from_parts(FULLNAME, "h.local.", [lan([10, 0, 0, 2])], lookup(&map)).unwrap();
        assert_eq!(d.info.name, "Living room");
    }

    #[test]
    fn a_device_without_a_control_port_has_no_control_address() {
        let map = HashMap::from([("vban_port", "6980")]);
        let d =
            device_from_parts(FULLNAME, "h.local.", [lan([10, 0, 0, 2])], lookup(&map)).unwrap();
        assert_eq!(d.ctrl_addr(), None);
    }
    // !SECTION: Devices

    // SECTION: Table
    fn device(fullname: &str, ip: [u8; 4]) -> Device {
        let map = full_txt();
        device_from_parts(fullname, "h.local.", [lan(ip)], lookup(&map)).unwrap()
    }

    #[test]
    fn the_table_adds_updates_and_removes() {
        let mut t = DeviceTable::default();
        let a = device("A._lanrouter._udp.local.", [10, 0, 0, 1]);
        assert!(matches!(t.upsert(a.clone()), Some(Change::Added(_))));
        assert_eq!(t.upsert(a.clone()), None, "no change, no event");
        let moved = device("A._lanrouter._udp.local.", [10, 0, 0, 9]);
        assert!(
            matches!(t.upsert(moved), Some(Change::Updated(d)) if d.addrs == [lan([10, 0, 0, 9])])
        );
        assert_eq!(t.devices().len(), 1);
        assert_eq!(
            t.remove("a._LANROUTER._udp.local."),
            Some(Change::Removed("a._lanrouter._udp.local.".into()))
        );
        assert_eq!(t.remove("a._lanrouter._udp.local."), None);
        assert!(t.devices().is_empty());
    }

    #[test]
    fn our_own_announcement_never_appears_in_the_list() {
        let mut t = DeviceTable::default();
        t.upsert(device("Me._lanrouter._udp.local.", [10, 0, 0, 1]));
        t.set_own(Some("Me._lanrouter._udp.local."));
        assert!(t.devices().is_empty(), "removed once we know it is us");
        assert_eq!(
            t.upsert(device("me._lanrouter._udp.local.", [10, 0, 0, 1])),
            None
        );
        assert!(matches!(
            t.upsert(device("You._lanrouter._udp.local.", [10, 0, 0, 2])),
            Some(Change::Added(_))
        ));
    }

    #[test]
    fn the_table_is_bounded() {
        let mut t = DeviceTable::default();
        for i in 0..MAX_DEVICES + 50 {
            t.upsert(device(
                &format!("d{i}._lanrouter._udp.local."),
                [10, 0, (i / 250) as u8, (i % 250) as u8 + 1],
            ));
        }
        assert_eq!(t.devices().len(), MAX_DEVICES);
    }
    // !SECTION: Table

    // SECTION: Live mDNS on the loopback
    use std::time::{Duration, Instant};

    fn wait_for(d: &mut Discovery, mut cond: impl FnMut(&Discovery) -> bool, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            d.poll();
            if cond(d) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        d.poll();
        cond(d)
    }

    // ? Needs working IP multicast on the host, so it is `#[ignore]`d by default: a CI runner or
    // ? sandbox without multicast would fail `cargo test --workspace` for no fault of the code.
    // ? Run it with `cargo test -p net-vban --lib -- --ignored mdns`. In the dev container it
    // ? passed 6/6 runs (~2 s: announce, find, goodbye). Two daemons in one process on one host
    // ? still don't prove behaviour across a real LAN, Wi-Fi AP multicast filtering or Android
    // ? (PLAN.md §12).
    #[test]
    #[ignore = "needs IP multicast; run on a real machine"]
    fn mdns_an_announced_device_is_found_and_then_disappears() {
        let port = 15353; // not 5353: don't fight the system's own responder
        let mut announcer = Discovery::with_port(port).unwrap();
        let mut browser = Discovery::with_port(port).unwrap();
        let mut advert = Advert::new("Test device");
        advert.vban_port = 17000;
        advert.ctrl_port = 17001;
        announcer.announce(&advert).unwrap();
        assert!(announcer.is_announcing());
        browser.start_browse().unwrap();

        assert!(
            wait_for(&mut browser, |b| !b.devices().is_empty(), 10),
            "never saw the announcement"
        );
        let found = &browser.devices()[0];
        assert_eq!(found.info.name, "Test device");
        assert_eq!(found.info.vban_port, 17000);
        assert_eq!(found.info.ctrl_port, Some(17001));
        assert!(found.vban_addr().is_some());

        announcer.unannounce();
        assert!(
            wait_for(&mut browser, |b| b.devices().is_empty(), 10),
            "goodbye not seen"
        );
    }

    #[test]
    fn a_discovery_object_starts_stops_and_drops_without_a_network_peer() {
        // Nothing here needs a second machine or even working multicast: the daemon thread is
        // created and told to exit, announce/browse calls succeed, nothing hangs.
        let mut d = Discovery::with_port(15354).unwrap();
        d.announce(&Advert::new("Lonely")).unwrap();
        d.start_browse().unwrap();
        assert!(d.is_announcing() && d.is_browsing());
        let _ = d.poll();
        d.stop_browse();
        d.unannounce();
        assert!(!d.is_announcing() && !d.is_browsing());
        assert!(d.devices().is_empty());
        drop(d);
    }
    // !SECTION: Live mDNS on the loopback
}
