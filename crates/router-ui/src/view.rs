//! What every view draws from and writes to.
//!
//! A frame reads the newest snapshot (immutable) and *collects* what the user did into `Out`.
//! `app.rs` sends the collected commands after drawing. That keeps the borrow rules simple (the
//! views never hold the model mutably) and means one frame can never send half of a gesture.

use app_core::types::{
    Command, LanRxLevels, MeterLevels, MeterSnapshot, StateSnapshot, StripState,
};
use app_core::ui_state::{GraphicsSettings, UiLayout};
use audio_types::{
    LanRxConfig, LanTxConfig, MAX_BUSES, MAX_STRIPS, NetworkBufferPreset, SinkEndpoint,
    SourceEndpoint,
};
use platform_io::{AppInfo, DeviceInfo};

use crate::model::PeakHold;

/// Strip column width, PLAN.md §9.2.
pub const STRIP_W: f32 = 125.0;
/// Collapsed pillar width.
pub const PILLAR_W: f32 = 28.0;
/// Below this window width the narrow layout applies (§9.7).
pub const NARROW_BELOW: f32 = 600.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Strip(u8),
    Bus(u8),
}

/// Text buffers of the open inspector. Reloaded whenever the inspected strip or bus changes.
#[derive(Clone, Debug, Default)]
pub struct EditBuffers {
    pub loaded_for: Option<Target>,
    pub label: String,
    /// Receive side of a LAN strip, and the text boxes that feed it.
    pub rx: LanRxConfig,
    pub rx_name: String,
    pub rx_ip: String,
    pub rx_channels: u8,
    /// Send side of a LAN bus.
    pub tx: LanTxConfig,
    pub tx_name: String,
    pub tx_ip: String,
    pub tx_port: String,
    pub tx_channels: u8,
}

/// UI-only state that is not worth keeping in the daemon: which drawer is open, how many
/// strips are shown. (Collapsed/expanded and the layout *are* kept there, in `UiLayout`.)
#[derive(Clone, Debug)]
pub struct ViewState {
    pub strips_shown: usize,
    pub buses_shown: usize,
    /// Set once the first snapshot sized the two counts above.
    pub sized: bool,
    /// Narrow layout: a strip is a pillar unless it is in this list (§9.7).
    pub narrow_open_strip: [bool; MAX_STRIPS],
    pub narrow_open_bus: [bool; MAX_BUSES],
    /// (strip, bus) whose send gain knob is open.
    pub send_open: Option<(u8, u8)>,
    pub drawer: Option<Target>,
    pub edit: EditBuffers,
    pub simple: SimpleState,
    /// Text box of the profile menu.
    pub profile_name: String,
}

/// What is picked in the two Simple-mode cards. Indices into lists that are rebuilt every frame,
/// so a pick is clamped when the list gets shorter.
#[derive(Clone, Debug)]
pub struct SimpleState {
    pub send_source: usize,
    pub send_target: usize,
    pub send_ip: String,
    pub recv_stream: usize,
    pub recv_output: usize,
    pub recv_buffer: NetworkBufferPreset,
}

impl Default for SimpleState {
    fn default() -> Self {
        Self {
            send_source: 0,
            send_target: 0,
            send_ip: String::new(),
            recv_stream: 0,
            recv_output: 0,
            recv_buffer: NetworkBufferPreset::Fast,
        }
    }
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            strips_shown: 4,
            buses_shown: 3,
            sized: false,
            narrow_open_strip: [false; MAX_STRIPS],
            narrow_open_bus: [false; MAX_BUSES],
            send_open: None,
            drawer: None,
            edit: EditBuffers::default(),
            simple: SimpleState::default(),
            profile_name: String::new(),
        }
    }
}

impl ViewState {
    /// Show every strip and bus that is doing something, plus a few spare.
    pub fn size_to(&mut self, snap: &StateSnapshot) {
        let last_strip = snap
            .strips
            .iter()
            .rposition(|s| s.config.source != SourceEndpoint::Unassigned);
        let last_bus = snap
            .buses
            .iter()
            .rposition(|b| b.config.sink != SinkEndpoint::Unassigned);
        self.strips_shown = last_strip.map_or(4, |i| (i + 1).max(4)).min(MAX_STRIPS);
        self.buses_shown = last_bus.map_or(3, |i| (i + 1).max(3)).min(MAX_BUSES);
        self.sized = true;
    }
}

/// What the frame wants to happen.
#[derive(Debug, Default)]
pub struct Out {
    pub commands: Vec<Command>,
    pub refresh_lists: bool,
    /// Short messages for the status line ("not an IPv4 address").
    pub notices: Vec<String>,
    /// A working copy of the layout, made the first time a view changes it.
    layout: Option<UiLayout>,
}

/// Everything `Out` collected, ready to be acted on.
#[derive(Debug, Default)]
pub struct Finished {
    pub commands: Vec<Command>,
    pub refresh_lists: bool,
    pub notices: Vec<String>,
}

impl Out {
    pub fn send(&mut self, cmd: Command) {
        self.commands.push(cmd);
    }

    pub fn say(&mut self, text: impl Into<String>) {
        self.notices.push(text.into());
    }

    /// Edit the layout: the closure sees a copy that becomes one `Command::Ui` at the end.
    pub fn edit_layout(&mut self, snap: &StateSnapshot, f: impl FnOnce(&mut UiLayout)) {
        f(self.layout.get_or_insert_with(|| snap.ui.clone()));
    }

    /// The commands to send, with the layout change (if any) last.
    pub fn finish(mut self) -> Finished {
        if let Some(l) = self.layout.take() {
            self.commands.push(Command::Ui(l));
        }
        Finished {
            commands: self.commands,
            refresh_lists: self.refresh_lists,
            notices: self.notices,
        }
    }
}

pub struct Ctx<'a> {
    pub snap: &'a StateSnapshot,
    pub meters: Option<&'a MeterSnapshot>,
    pub devices: &'a [DeviceInfo],
    pub apps: &'a [AppInfo],
    pub strip_hold: &'a PeakHold,
    pub bus_hold: &'a PeakHold,
    pub view: &'a mut ViewState,
    pub out: &'a mut Out,
    pub narrow: bool,
    pub gfx: GraphicsSettings,
}

impl Ctx<'_> {
    pub fn strip_levels(&self, i: usize) -> Option<&MeterLevels> {
        self.meters.and_then(|m| m.strips.get(i))
    }

    pub fn bus_levels(&self, i: usize) -> Option<&MeterLevels> {
        self.meters.and_then(|m| m.buses.get(i))
    }

    pub fn lan_rx(&self, strip: usize) -> Option<&LanRxLevels> {
        self.meters.and_then(|m| m.lan_rx.get(strip))
    }

    pub fn strip(&self, i: u8) -> Option<&StripState> {
        self.snap.strips.get(usize::from(i))
    }

    /// Touch-sized controls on narrow windows.
    pub fn grab(&self) -> f32 {
        if self.narrow { 44.0 } else { 22.0 }
    }

    pub fn peak_hold_on(&self) -> bool {
        self.gfx.meter_peak_hold
    }

    pub fn strip_collapsed(&self, i: usize) -> bool {
        if self.narrow {
            !self.view.narrow_open_strip[i]
        } else {
            self.snap.ui.strips[i].collapsed_in_strips
        }
    }

    pub fn bus_collapsed(&self, i: usize) -> bool {
        if self.narrow {
            !self.view.narrow_open_bus[i]
        } else {
            self.snap.ui.buses[i].collapsed_in_strips
        }
    }

    pub fn toggle_strip_collapsed(&mut self, i: usize) {
        if self.narrow {
            self.view.narrow_open_strip[i] = !self.view.narrow_open_strip[i];
        } else {
            let on = !self.snap.ui.strips[i].collapsed_in_strips;
            self.out
                .edit_layout(self.snap, |l| l.strips[i].collapsed_in_strips = on);
        }
    }

    pub fn toggle_bus_collapsed(&mut self, i: usize) {
        if self.narrow {
            self.view.narrow_open_bus[i] = !self.view.narrow_open_bus[i];
        } else {
            let on = !self.snap.ui.buses[i].collapsed_in_strips;
            self.out
                .edit_layout(self.snap, |l| l.buses[i].collapsed_in_strips = on);
        }
    }
}

// SECTION: Names
/// "A1", "B2"...: A = a bus that plays to a hardware device, B = every other bus (virtual mic,
/// LAN, not yet assigned). The number counts buses of the same letter, in order.
// ? The letter follows what the bus is wired to, so assigning a sound card to a "B" bus turns it
// ? into an "A" bus. VoiceMeeter fixes them by position; here a bus has no fixed role.
pub fn bus_name(snap: &StateSnapshot, bus: usize) -> String {
    let letter = |i: usize| match snap.buses.get(i).map(|b| b.config.sink) {
        Some(SinkEndpoint::HardwareOutput { .. }) => 'A',
        _ => 'B',
    };
    let l = letter(bus);
    let n = (0..=bus).filter(|&i| letter(i) == l).count();
    format!("{l}{n}")
}

pub fn describe_source(src: &SourceEndpoint, devices: &[DeviceInfo]) -> String {
    let dev = |id: &str| {
        devices
            .iter()
            .find(|d| d.id == id)
            .map_or_else(|| id.to_owned(), |d| d.name.clone())
    };
    match src {
        SourceEndpoint::Unassigned => "— no source —".into(),
        SourceEndpoint::HardwareInput { device_id } => dev(device_id.as_str()),
        SourceEndpoint::DeviceLoopback { device_id } => {
            format!("Loopback: {}", dev(device_id.as_str()))
        }
        SourceEndpoint::ProcessLoopback { exe_name, mode, .. } => {
            use audio_types::ProcessLoopbackMode::{ExcludeTree, IncludeTree};
            match mode {
                IncludeTree => format!("App: {}", exe_name.as_str()),
                ExcludeTree => format!("All but {}", exe_name.as_str()),
            }
        }
        SourceEndpoint::VirtualInput { index } => {
            format!("Virtual input {}", u32::from(*index) + 1)
        }
        SourceEndpoint::AndroidMic { .. } => "Phone microphone".into(),
        SourceEndpoint::AndroidPlaybackCapture { .. } => "Phone app audio".into(),
        SourceEndpoint::LanIn(c) => {
            let ip = c.source_ipv4;
            if ip == [0, 0, 0, 0] {
                format!("LAN: {}", c.stream_name.as_str())
            } else {
                format!(
                    "LAN: {} @ {}.{}.{}.{}",
                    c.stream_name.as_str(),
                    ip[0],
                    ip[1],
                    ip[2],
                    ip[3]
                )
            }
        }
    }
}

pub fn describe_sink(sink: &SinkEndpoint, devices: &[DeviceInfo]) -> String {
    match sink {
        SinkEndpoint::Unassigned => "— no output —".into(),
        SinkEndpoint::HardwareOutput { device_id } => devices
            .iter()
            .find(|d| d.id == device_id.as_str())
            .map_or_else(|| device_id.as_str().to_owned(), |d| d.name.clone()),
        SinkEndpoint::VirtualMic { index } => format!("Virtual mic {}", u32::from(*index) + 1),
        SinkEndpoint::LanOut(c) => {
            let ip = c.peer_ipv4;
            format!("LAN → {}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
        }
    }
}

pub fn preset_name(p: NetworkBufferPreset) -> String {
    match p {
        NetworkBufferPreset::Optimal => "Optimal (6 ms)".into(),
        NetworkBufferPreset::Fast => "Fast (12 ms)".into(),
        NetworkBufferPreset::Medium => "Medium (24 ms)".into(),
        NetworkBufferPreset::Slow => "Slow (48 ms)".into(),
        NetworkBufferPreset::VerySlow => "Very slow (96 ms)".into(),
        NetworkBufferPreset::Custom { target_ms } => format!("Custom ({target_ms} ms)"),
    }
}

pub const PRESETS: [NetworkBufferPreset; 5] = [
    NetworkBufferPreset::Optimal,
    NetworkBufferPreset::Fast,
    NetworkBufferPreset::Medium,
    NetworkBufferPreset::Slow,
    NetworkBufferPreset::VerySlow,
];

/// Parse "192.168.1.20" into four bytes.
pub fn parse_ipv4(text: &str) -> Option<[u8; 4]> {
    text.trim()
        .parse::<std::net::Ipv4Addr>()
        .ok()
        .map(|a| a.octets())
}
// !SECTION: Names

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::sample_snapshot;
    use audio_types::FixedStr;

    #[test]
    fn bus_letters_follow_the_sink() {
        let mut s = sample_snapshot();
        assert_eq!(bus_name(&s, 0), "B1");
        assert_eq!(bus_name(&s, 2), "B3");
        s.buses[1].config.sink = SinkEndpoint::HardwareOutput {
            device_id: FixedStr::from_str_trunc("x"),
        };
        assert_eq!(bus_name(&s, 1), "A1");
        assert_eq!(bus_name(&s, 2), "B2", "B counts only the B buses");
    }

    #[test]
    fn the_view_shows_used_slots_plus_spares() {
        let mut v = ViewState::default();
        let mut s = sample_snapshot();
        v.size_to(&s);
        assert_eq!((v.strips_shown, v.buses_shown), (4, 3));
        s.strips[6].config.source = SourceEndpoint::VirtualInput { index: 0 };
        s.buses[4].config.sink = SinkEndpoint::VirtualMic { index: 0 };
        v.size_to(&s);
        assert_eq!((v.strips_shown, v.buses_shown), (7, 5));
    }

    #[test]
    fn layout_edits_become_one_command() {
        let s = sample_snapshot();
        let mut out = Out::default();
        out.edit_layout(&s, |l| l.strips[0].collapsed_in_strips = true);
        out.edit_layout(&s, |l| l.buses[1].collapsed_in_strips = true);
        let done = out.finish();
        assert_eq!(done.commands.len(), 1);
        match &done.commands[0] {
            Command::Ui(l) => {
                assert!(l.strips[0].collapsed_in_strips && l.buses[1].collapsed_in_strips)
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn ipv4_parsing_is_strict() {
        assert_eq!(parse_ipv4(" 192.168.1.20 "), Some([192, 168, 1, 20]));
        assert_eq!(parse_ipv4("192.168.1"), None);
        assert_eq!(parse_ipv4("300.1.1.1"), None);
        assert_eq!(parse_ipv4(""), None);
    }
}
