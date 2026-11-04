//! A plain-data picture of a Linux audio graph (PipeWire nodes and ports) and the decisions
//! made from it: which nodes are devices, which are applications, and which ports to link.
//!
//! This module has no PipeWire dependency. The PipeWire thread fills a [`GraphState`] from
//! registry events and the backend reads it, so all the logic here is tested without a server.

use crate::backend::{AppInfo, DeviceInfo, DeviceKind};
use std::collections::BTreeMap;

/// Nodes we create ourselves start with this, so they never show up in our own device list.
pub const OWN_NODE_PREFIX: &str = "retae-";

pub const CLASS_SINK: &str = "Audio/Sink";
pub const CLASS_SOURCE: &str = "Audio/Source";
pub const CLASS_VIRTUAL_SOURCE: &str = "Audio/Source/Virtual";
/// An application that is playing (or could play) audio.
pub const CLASS_APP_OUTPUT: &str = "Stream/Output/Audio";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeInfo {
    pub id: u32,
    /// `node.name`: stable across restarts, what a profile stores as the device id.
    pub name: String,
    pub description: String,
    pub media_class: String,
    pub pid: Option<u32>,
    pub app_name: String,
    pub binary: String,
    pub channels: Option<u8>,
    /// `client.id`: the connection that owns this node. Native PipeWire programs (`pw-cat`,
    /// most games) put their process id on the *client*, not on the node, so it is looked up
    /// through [`GraphState::clients`].
    pub client_id: Option<u32>,
    /// Known only for application nodes (the registry does not carry it; the node proxy does).
    pub running: bool,
}

/// What a PipeWire client (one connection to the server) says about its program.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientInfo {
    pub pid: Option<u32>,
    pub app_name: String,
    pub binary: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortDir {
    In,
    Out,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortInfo {
    pub id: u32,
    pub node_id: u32,
    pub dir: PortDir,
    /// `FL`, `FR`, `MONO`, ... from `audio.channel` (empty if the port has none).
    pub channel: String,
}

#[derive(Clone, Debug, Default)]
pub struct GraphState {
    pub nodes: BTreeMap<u32, NodeInfo>,
    pub ports: BTreeMap<u32, PortInfo>,
    pub clients: BTreeMap<u32, ClientInfo>,
    /// `node.name` of the system default sink / source, from the `default` metadata.
    pub default_sink: Option<String>,
    pub default_source: Option<String>,
}

impl GraphState {
    fn is_own(node: &NodeInfo) -> bool {
        node.name.starts_with(OWN_NODE_PREFIX)
    }

    /// Hardware-like devices: every sink as an output, every source as an input. Our own
    /// virtual nodes are left out.
    pub fn devices(&self) -> Vec<DeviceInfo> {
        let mut out: Vec<DeviceInfo> = self
            .nodes
            .values()
            .filter(|n| !Self::is_own(n))
            .filter_map(|n| {
                let (kind, default) = match n.media_class.as_str() {
                    CLASS_SINK => (DeviceKind::Output, &self.default_sink),
                    CLASS_SOURCE | CLASS_VIRTUAL_SOURCE => {
                        (DeviceKind::Input, &self.default_source)
                    }
                    _ => return None,
                };
                Some(DeviceInfo {
                    id: n.name.clone(),
                    name: if n.description.is_empty() {
                        n.name.clone()
                    } else {
                        n.description.clone()
                    },
                    kind,
                    channels: n.channels.unwrap_or(2).clamp(1, 8),
                    is_default: default.as_deref() == Some(n.name.as_str()),
                })
            })
            .collect();
        // Inputs first, then outputs, defaults on top: a stable order for the UI and the CLI.
        out.sort_by(|a, b| {
            (a.kind as u8, !a.is_default, &a.name).cmp(&(b.kind as u8, !b.is_default, &b.name))
        });
        out
    }

    /// The node a device id names, if it exists and has the right direction.
    pub fn find_device(&self, id: &str, kind: DeviceKind) -> Option<&NodeInfo> {
        self.nodes.values().find(|n| {
            !Self::is_own(n)
                && n.name == id
                && match kind {
                    DeviceKind::Input => {
                        n.media_class == CLASS_SOURCE || n.media_class == CLASS_VIRTUAL_SOURCE
                    }
                    DeviceKind::Output => n.media_class == CLASS_SINK,
                }
        })
    }

    /// The process a node belongs to: its own property if it has one (PulseAudio-style
    /// clients), otherwise its client's.
    pub fn pid_of(&self, node: &NodeInfo) -> Option<u32> {
        node.pid
            .or_else(|| self.client_of(node).and_then(|c| c.pid))
    }

    fn client_of(&self, node: &NodeInfo) -> Option<&ClientInfo> {
        node.client_id.and_then(|id| self.clients.get(&id))
    }

    /// One entry per process that has an audio stream. `playing` if any of its streams runs.
    pub fn apps(&self) -> Vec<AppInfo> {
        let mut by_pid: BTreeMap<u32, AppInfo> = BTreeMap::new();
        for n in self.app_streams() {
            let Some(pid) = self.pid_of(n) else { continue };
            let client = self.client_of(n);
            let name = [
                &n.app_name,
                &n.binary,
                client.map_or(&n.name, |c| &c.app_name),
                client.map_or(&n.name, |c| &c.binary),
                &n.name,
            ]
            .into_iter()
            .find(|s| !s.is_empty())
            .cloned()
            .unwrap_or_default();
            let entry = by_pid.entry(pid).or_insert(AppInfo {
                pid,
                name,
                playing: false,
            });
            entry.playing |= n.running;
        }
        by_pid.into_values().collect()
    }

    fn app_streams(&self) -> impl Iterator<Item = &NodeInfo> {
        self.nodes
            .values()
            .filter(|n| n.media_class == CLASS_APP_OUTPUT && !Self::is_own(n))
    }

    /// Every playback stream of process `pid`.
    pub fn app_nodes(&self, pid: u32) -> Vec<&NodeInfo> {
        self.app_streams()
            .filter(|n| self.pid_of(n) == Some(pid))
            .collect()
    }

    pub fn ports_of(&self, node: u32, dir: PortDir) -> Vec<&PortInfo> {
        self.ports
            .values()
            .filter(|p| p.node_id == node && p.dir == dir)
            .collect()
    }
}

/// Pairs `(output port, input port)` that link an application's output ports to the input ports
/// of our capture node.
///
/// Matching is by channel name (`FL` to `FL`). A mono application feeds every one of our ports;
/// a port with no counterpart stays unlinked.
pub fn plan_links(app_outputs: &[&PortInfo], our_inputs: &[&PortInfo]) -> Vec<(u32, u32)> {
    let mut links = Vec::new();
    for ours in our_inputs {
        let same_channel = app_outputs
            .iter()
            .filter(|o| !o.channel.is_empty() && o.channel == ours.channel);
        let mut any = false;
        for o in same_channel {
            links.push((o.id, ours.id));
            any = true;
        }
        if !any {
            // A single-port (mono) source is heard on every channel.
            let mono: Vec<_> = app_outputs
                .iter()
                .filter(|o| o.channel.is_empty() || o.channel == "MONO")
                .collect();
            if mono.len() == 1 && app_outputs.len() == 1 {
                links.push((mono[0].id, ours.id));
            }
        }
    }
    links.sort_unstable();
    links.dedup();
    links
}

/// Pulls the `name` out of the metadata JSON `{ "name": "alsa_output..." }` PipeWire uses for
/// `default.audio.sink` and `default.audio.source`.
// ? A full JSON parser for one field would be a new dependency. The value is machine-written by
// ? the session manager, so a plain search is enough and anything odd just means "no default".
pub fn default_name_from_json(json: &str) -> Option<String> {
    let after = json.split("\"name\"").nth(1)?;
    let after = after.trim_start().strip_prefix(':')?.trim_start();
    let body = after.strip_prefix('"')?;
    let end = body.find('"')?;
    let name = &body[..end];
    (!name.is_empty()).then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u32, name: &str, class: &str) -> NodeInfo {
        NodeInfo {
            id,
            name: name.into(),
            media_class: class.into(),
            ..NodeInfo::default()
        }
    }

    fn port(id: u32, node_id: u32, dir: PortDir, channel: &str) -> PortInfo {
        PortInfo {
            id,
            node_id,
            dir,
            channel: channel.into(),
        }
    }

    fn graph() -> GraphState {
        let mut g = GraphState::default();
        let mut speakers = node(10, "alsa_output.pci.analog-stereo", CLASS_SINK);
        speakers.description = "Built-in Audio Analog Stereo".into();
        let hdmi = node(11, "alsa_output.hdmi", CLASS_SINK);
        let mic = node(12, "alsa_input.usb-mic", CLASS_SOURCE);
        let ours = node(13, "retae-virtual-in-0", CLASS_SINK);
        let mut player = node(20, "mpv", CLASS_APP_OUTPUT);
        player.pid = Some(4242);
        player.app_name = "mpv".into();
        player.running = true;
        let mut tab = node(21, "Firefox", CLASS_APP_OUTPUT);
        tab.pid = Some(777);
        tab.app_name = "Firefox".into();
        let mut tab2 = tab.clone();
        tab2.id = 22;
        tab2.running = true;
        let usb = node(30, "alsa_output.usb", CLASS_SINK);
        for n in [speakers, hdmi, mic, ours, player, tab, tab2, usb] {
            g.nodes.insert(n.id, n);
        }
        g.default_sink = Some("alsa_output.pci.analog-stereo".into());
        g.default_source = Some("alsa_input.usb-mic".into());
        g
    }

    #[test]
    fn devices_list_sinks_and_sources_but_not_our_own_nodes_or_apps() {
        let devices = graph().devices();
        let ids: Vec<_> = devices.iter().map(|d| d.id.as_str()).collect();
        assert!(!ids.contains(&"retae-virtual-in-0"), "{ids:?}");
        assert!(!ids.contains(&"mpv"));
        // Inputs come first, then outputs with the default one on top.
        assert_eq!(
            ids,
            [
                "alsa_input.usb-mic",
                "alsa_output.pci.analog-stereo",
                "alsa_output.hdmi",
                "alsa_output.usb"
            ]
        );
        assert!(devices[0].is_default && devices[1].is_default);
        assert!(!devices[2].is_default && !devices[3].is_default);
        assert_eq!(devices[1].name, "Built-in Audio Analog Stereo");
        assert_eq!(
            devices[2].name, "alsa_output.hdmi",
            "no description: show the id"
        );
    }

    #[test]
    fn a_device_id_must_have_the_right_direction() {
        let g = graph();
        assert!(
            g.find_device("alsa_input.usb-mic", DeviceKind::Input)
                .is_some()
        );
        assert!(
            g.find_device("alsa_input.usb-mic", DeviceKind::Output)
                .is_none()
        );
        assert!(
            g.find_device("alsa_output.hdmi", DeviceKind::Output)
                .is_some()
        );
        assert!(
            g.find_device("retae-virtual-in-0", DeviceKind::Output)
                .is_none()
        );
        assert!(g.find_device("nope", DeviceKind::Input).is_none());
    }

    #[test]
    fn apps_are_one_row_per_process_and_playing_if_any_stream_runs() {
        let apps = graph().apps();
        assert_eq!(apps.len(), 2);
        let mpv = apps.iter().find(|a| a.pid == 4242).unwrap();
        assert!(mpv.playing && mpv.name == "mpv");
        let ff = apps.iter().find(|a| a.pid == 777).unwrap();
        assert!(ff.playing, "one of the two Firefox streams is running");
        assert_eq!(graph().app_nodes(777).len(), 2);
        assert!(graph().app_nodes(1).is_empty());
    }

    #[test]
    fn a_native_pipewire_program_is_identified_through_its_client() {
        // ? pw-cat style: the node carries only `client.id`; the pid is on the client.
        let mut g = GraphState::default();
        let mut n = node(40, "pw-cat", CLASS_APP_OUTPUT);
        n.client_id = Some(9);
        g.nodes.insert(40, n);
        assert!(g.apps().is_empty(), "client not announced yet: pid unknown");
        g.clients.insert(
            9,
            ClientInfo {
                pid: Some(31337),
                app_name: "pw-cat".into(),
                binary: "pw-cat".into(),
            },
        );
        let apps = g.apps();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].pid, 31337);
        assert_eq!(g.app_nodes(31337).len(), 1);
    }

    #[test]
    fn links_follow_channel_names() {
        let (fl, fr) = (
            port(1, 20, PortDir::Out, "FL"),
            port(2, 20, PortDir::Out, "FR"),
        );
        let (ifl, ifr) = (
            port(3, 50, PortDir::In, "FL"),
            port(4, 50, PortDir::In, "FR"),
        );
        assert_eq!(plan_links(&[&fl, &fr], &[&ifl, &ifr]), [(1, 3), (2, 4)]);
        // Our side is mono: it gets the matching front-left only.
        assert_eq!(plan_links(&[&fl, &fr], &[&ifl]), [(1, 3)]);
        // Several streams of one app: every matching pair, no duplicates.
        let (fl2, fr2) = (
            port(5, 21, PortDir::Out, "FL"),
            port(6, 21, PortDir::Out, "FR"),
        );
        let links = plan_links(&[&fl, &fr, &fl2, &fr2], &[&ifl, &ifr]);
        assert_eq!(links, [(1, 3), (2, 4), (5, 3), (6, 4)]);
    }

    #[test]
    fn a_mono_application_is_heard_on_both_channels() {
        let mono = port(7, 20, PortDir::Out, "MONO");
        let (ifl, ifr) = (
            port(3, 50, PortDir::In, "FL"),
            port(4, 50, PortDir::In, "FR"),
        );
        assert_eq!(plan_links(&[&mono], &[&ifl, &ifr]), [(7, 3), (7, 4)]);
        let unnamed = port(8, 20, PortDir::Out, "");
        assert_eq!(plan_links(&[&unnamed], &[&ifl, &ifr]), [(8, 3), (8, 4)]);
    }

    #[test]
    fn unmatched_ports_stay_unlinked() {
        let (fl, fr) = (
            port(1, 20, PortDir::Out, "FL"),
            port(2, 20, PortDir::Out, "FR"),
        );
        let lfe = port(9, 50, PortDir::In, "LFE");
        assert!(
            plan_links(&[&fl, &fr], &[&lfe]).is_empty(),
            "stereo is not mono"
        );
        assert!(plan_links(&[], &[&lfe]).is_empty());
        assert!(plan_links(&[&fl], &[]).is_empty());
    }

    #[test]
    fn default_names_come_out_of_the_metadata_json() {
        assert_eq!(
            default_name_from_json(r#"{ "name": "alsa_output.pci-0000_00_1f.3.analog-stereo" }"#),
            Some("alsa_output.pci-0000_00_1f.3.analog-stereo".into())
        );
        assert_eq!(default_name_from_json(r#"{"name":"x"}"#), Some("x".into()));
        assert_eq!(default_name_from_json(r#"{"name":""}"#), None);
        assert_eq!(default_name_from_json("not json"), None);
        assert_eq!(default_name_from_json(r#"{"name": 5}"#), None);
        assert_eq!(default_name_from_json(""), None);
    }
}
