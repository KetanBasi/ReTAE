//! The one thread that owns every PipeWire object.
//!
//! PipeWire's Rust objects (core, registry, streams) are not `Send`, so they are all created and
//! used on this thread. The rest of the program talks to it with `Msg` values over a channel
//! that the PipeWire loop itself wakes up for, and reads the registry through a shared
//! `GraphState`.

use super::stream::{CaptureState, PlaybackState, Slot, format_pod};
use crate::graph::{
    CLASS_APP_OUTPUT, ClientInfo, GraphState, NodeInfo, PortDir, PortInfo, default_name_from_json,
    plan_links,
};
use pipewire as pw;
use pw::keys;
use pw::link::Link;
use pw::metadata::Metadata;
use pw::node::{Node, NodeState};
use pw::properties::properties;
use pw::proxy::{Listener, ProxyT};
use pw::spa;
use pw::stream::{StreamFlags, StreamListener, StreamRc, StreamState};
use pw::types::ObjectType;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

/// What the data callback gets: the slot it moves audio through.
pub(crate) enum Data {
    Capture(Arc<Slot<CaptureState>>),
    Playback(Arc<Slot<PlaybackState>>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Record from a device (or from a sink's monitor).
    Capture,
    /// Play to a device.
    Playback,
    /// Our own `Audio/Sink` node: other applications play into it.
    VirtualSink,
    /// Our own `Audio/Source/Virtual` node: other applications record from it.
    VirtualSource,
    /// Record one application: its ports are linked to ours by hand.
    AppCapture,
}

pub(crate) struct OpenSpec {
    pub id: u64,
    pub kind: Kind,
    /// Name of the node we create (`retae-...`).
    pub node_name: String,
    pub description: String,
    /// `node.name` of the device to connect to; `None` lets the session manager choose.
    pub target: Option<String>,
    /// Capture the monitor of the sink named by `target`.
    pub capture_sink: bool,
    pub channels: u8,
    /// Process whose streams an `AppCapture` records.
    pub pid: Option<u32>,
    pub data: Data,
}

enum Msg {
    Open(Box<OpenSpec>, SyncSender<Result<(), String>>),
    Close(u64, SyncSender<()>),
    Quit,
}

/// The latency every stream asks for: one engine block (PLAN §6.2). It is a request; the graph
/// keeps its own quantum if another client needs a larger one.
const NODE_LATENCY: &str = "128/48000";

/// How long a stream gets to reach `Paused` before the open is reported as failed.
const OPEN_TIMEOUT: Duration = Duration::from_secs(3);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

// SECTION: Handle (control-thread side)
pub(crate) struct Loop {
    tx: pw::channel::Sender<Msg>,
    pub graph: Arc<Mutex<GraphState>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Loop {
    /// Start the thread and wait until it is connected and has seen the whole registry.
    pub fn start() -> Result<Self, String> {
        let graph = Arc::new(Mutex::new(GraphState::default()));
        let (tx, rx) = pw::channel::channel::<Msg>();
        let (ready_tx, ready_rx) = sync_channel::<Result<(), String>>(1);
        let thread = {
            let graph = graph.clone();
            std::thread::Builder::new()
                .name("pipewire-loop".into())
                .spawn(move || run(rx, &graph, &ready_tx))
                .map_err(|e| format!("cannot start the PipeWire thread: {e}"))?
        };
        match ready_rx.recv_timeout(CONNECT_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                tx,
                graph,
                thread: Mutex::new(Some(thread)),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                // The thread is stuck connecting; leave it, it ends with the process.
                Err("PipeWire did not answer in time".into())
            }
        }
    }

    pub fn open(&self, spec: OpenSpec) -> Result<(), String> {
        let (reply_tx, reply_rx) = sync_channel(1);
        self.tx
            .send(Msg::Open(Box::new(spec), reply_tx))
            .map_err(|_| "the PipeWire thread has stopped".to_owned())?;
        match reply_rx.recv_timeout(OPEN_TIMEOUT) {
            Ok(result) => result,
            Err(_) => Err("the stream did not connect in time".into()),
        }
    }

    /// Destroy a stream and wait until its callbacks can no longer run.
    pub fn close(&self, id: u64) {
        let (reply_tx, reply_rx) = sync_channel(1);
        if self.tx.send(Msg::Close(id, reply_tx)).is_ok() {
            let _ = reply_rx.recv_timeout(OPEN_TIMEOUT);
        }
    }

    pub fn graph(&self) -> GraphState {
        self.graph
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for Loop {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Quit);
        let handle = self
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}
// !SECTION: Handle (control-thread side)

// SECTION: Loop-thread state
/// Per-stream callback data.
struct StreamData {
    data: Data,
    channels: usize,
    /// Answered once, when the stream first connects or fails.
    reply: Option<SyncSender<Result<(), String>>>,
}

struct Entry {
    // ? Field order is drop order: the listener goes first, so no callback can run while the
    // ? stream is being destroyed.
    _listener: StreamListener<StreamData>,
    stream: StreamRc,
    kind: Kind,
    pid: Option<u32>,
    /// Links we asked for (output port, input port), so none is requested twice.
    links: HashSet<(u32, u32)>,
    _link_proxies: Vec<Link>,
}

type Streams = Rc<RefCell<HashMap<u64, Entry>>>;
type Proxies = Rc<RefCell<HashMap<u32, (Box<dyn ProxyT>, Box<dyn Listener>)>>>;

fn lock(graph: &Mutex<GraphState>) -> std::sync::MutexGuard<'_, GraphState> {
    graph.lock().unwrap_or_else(PoisonError::into_inner)
}
// !SECTION: Loop-thread state

// SECTION: Loop body
fn run(
    rx: pw::channel::Receiver<Msg>,
    graph: &Arc<Mutex<GraphState>>,
    ready: &SyncSender<Result<(), String>>,
) {
    pw::init();
    let setup = || -> Result<_, pw::Error> {
        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None)?;
        Ok((mainloop, context, core))
    };
    let (mainloop, _context, core) = match setup() {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(format!("cannot connect to PipeWire: {e}")));
            return;
        }
    };

    // A lost connection ends the loop, which makes later requests fail with a clear error.
    let quit = mainloop.downgrade();
    let first_sync = Rc::new(RefCell::new(None::<spa::utils::result::AsyncSeq>));
    let ready_cell = Rc::new(RefCell::new(Some(ready.clone())));
    let _core_listener = {
        let (first_sync, ready_done) = (first_sync.clone(), ready_cell.clone());
        let ready_error = ready_cell.clone();
        core.add_listener_local()
            .done(move |id, seq| {
                if id == pw::core::PW_ID_CORE && *first_sync.borrow() == Some(seq) {
                    if let Some(r) = ready_done.borrow_mut().take() {
                        let _ = r.send(Ok(()));
                    }
                }
            })
            .error(move |id, _seq, res, message| {
                if id == pw::core::PW_ID_CORE {
                    if let Some(r) = ready_error.borrow_mut().take() {
                        let _ = r.send(Err(format!("PipeWire error {res}: {message}")));
                    }
                    if let Some(l) = quit.upgrade() {
                        l.quit();
                    }
                }
            })
            .register()
    };

    let registry = match core.get_registry_rc() {
        Ok(r) => r,
        Err(e) => {
            let _ = ready.send(Err(format!("cannot read the PipeWire registry: {e}")));
            return;
        }
    };
    let streams: Streams = Rc::new(RefCell::new(HashMap::new()));
    let proxies: Proxies = Rc::new(RefCell::new(HashMap::new()));

    let _registry_listener = {
        let (graph_a, graph_b) = (graph.clone(), graph.clone());
        let (streams_a, streams_b) = (streams.clone(), streams.clone());
        let (proxies_a, proxies_b) = (proxies.clone(), proxies.clone());
        let (core_a, core_b) = (core.clone(), core.clone());
        let registry_weak = registry.downgrade();
        registry
            .add_listener_local()
            .global(move |obj| {
                let Some(props) = obj.props else { return };
                let id = obj.id;
                match obj.type_ {
                    ObjectType::Node => {
                        let info = node_info(id, props);
                        let is_app = info.media_class == CLASS_APP_OUTPUT;
                        lock(&graph_a).nodes.insert(id, info);
                        if is_app {
                            if let Some(registry) = registry_weak.upgrade() {
                                watch_app_node(&registry, obj, &graph_a, &proxies_a);
                            }
                        }
                    }
                    ObjectType::Client => {
                        lock(&graph_a).clients.insert(id, client_info(props));
                    }
                    ObjectType::Port => {
                        if let Some(port) = port_info(id, props) {
                            lock(&graph_a).ports.insert(id, port);
                        }
                    }
                    ObjectType::Metadata if props.get("metadata.name") == Some("default") => {
                        if let Some(registry) = registry_weak.upgrade() {
                            watch_default_metadata(&registry, obj, &graph_a, &proxies_a);
                        }
                    }
                    _ => {}
                }
                reconcile_links(&core_a, &graph_a, &streams_a);
            })
            .global_remove(move |id| {
                {
                    let mut g = lock(&graph_b);
                    g.nodes.remove(&id);
                    g.ports.remove(&id);
                    g.clients.remove(&id);
                }
                proxies_b.borrow_mut().remove(&id);
                reconcile_links(&core_b, &graph_b, &streams_b);
            })
            .register()
    };

    // The first round trip is done when everything that existed before us has been announced.
    match core.sync(0) {
        Ok(seq) => *first_sync.borrow_mut() = Some(seq),
        Err(e) => {
            let _ = ready.send(Err(format!("PipeWire sync failed: {e}")));
            return;
        }
    }

    let _attached = {
        let (core, graph, streams) = (core.clone(), graph.clone(), streams.clone());
        let quit = mainloop.downgrade();
        rx.attach(mainloop.loop_(), move |msg| match msg {
            Msg::Open(spec, reply) => open_stream(&core, &graph, &streams, *spec, reply),
            Msg::Close(id, reply) => {
                // ? Removing the entry drops the listener and destroys the stream. After that no
                // ? callback can run, so the caller may take the port out of its slot.
                let removed = streams.borrow_mut().remove(&id);
                if let Some(entry) = removed {
                    let _ = entry.stream.disconnect();
                }
                let _ = reply.send(());
            }
            Msg::Quit => {
                if let Some(l) = quit.upgrade() {
                    l.quit();
                }
            }
        })
    };

    mainloop.run();

    // Fall out of scope in a safe order: streams first, then the listeners and the core.
    streams.borrow_mut().clear();
    proxies.borrow_mut().clear();
    lock(graph).nodes.clear();
}
// !SECTION: Loop body

// SECTION: Registry parsing
fn node_info(id: u32, props: &spa::utils::dict::DictRef) -> NodeInfo {
    let get = |k: &str| props.get(k).unwrap_or_default().to_owned();
    let channels = props
        .get("audio.channels")
        .and_then(|c| c.parse::<u8>().ok())
        .or_else(|| {
            // `[ FL FR ]` or `[FL FR]`: count the names.
            props.get("audio.position").map(|p| {
                p.trim_matches(|c| c == '[' || c == ']')
                    .split([',', ' '])
                    .filter(|s| !s.is_empty())
                    .count()
                    .min(255) as u8
            })
        })
        .filter(|&c| c > 0);
    NodeInfo {
        id,
        name: get("node.name"),
        description: props
            .get("node.description")
            .or_else(|| props.get("node.nick"))
            .unwrap_or_default()
            .to_owned(),
        media_class: get("media.class"),
        pid: props
            .get("application.process.id")
            .and_then(|p| p.parse().ok()),
        app_name: get("application.name"),
        binary: get("application.process.binary"),
        channels,
        client_id: props.get("client.id").and_then(|c| c.parse().ok()),
        running: false,
    }
}

fn client_info(props: &spa::utils::dict::DictRef) -> ClientInfo {
    ClientInfo {
        // ? Programs may or may not say who they are (`pw-cat` does not). The server itself
        // ? stamps every connection with the peer's real pid, so use that as the fallback.
        pid: props
            .get("application.process.id")
            .or_else(|| props.get("pipewire.sec.pid"))
            .and_then(|p| p.parse().ok()),
        app_name: props.get("application.name").unwrap_or_default().to_owned(),
        binary: props
            .get("application.process.binary")
            .unwrap_or_default()
            .to_owned(),
    }
}

fn port_info(id: u32, props: &spa::utils::dict::DictRef) -> Option<PortInfo> {
    Some(PortInfo {
        id,
        node_id: props.get("node.id")?.parse().ok()?,
        dir: match props.get("port.direction")? {
            "in" => PortDir::In,
            "out" => PortDir::Out,
            _ => return None,
        },
        channel: props.get("audio.channel").unwrap_or_default().to_owned(),
    })
}

/// Follow the state of an application's stream so `apps()` can say who is playing.
fn watch_app_node(
    registry: &pw::registry::RegistryRc,
    obj: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
    graph: &Arc<Mutex<GraphState>>,
    proxies: &Proxies,
) {
    let Ok(node) = registry.bind::<Node, _>(obj) else {
        return;
    };
    let id = obj.id;
    let graph = graph.clone();
    let listener = node
        .add_listener_local()
        .info(move |info| {
            let running = matches!(info.state(), NodeState::Running);
            if let Some(n) = lock(&graph).nodes.get_mut(&id) {
                n.running = running;
            }
        })
        .register();
    proxies
        .borrow_mut()
        .insert(id, (Box::new(node), Box::new(listener)));
}

/// The `default` metadata tells which sink and source are the system defaults.
fn watch_default_metadata(
    registry: &pw::registry::RegistryRc,
    obj: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
    graph: &Arc<Mutex<GraphState>>,
    proxies: &Proxies,
) {
    let Ok(meta) = registry.bind::<Metadata, _>(obj) else {
        return;
    };
    let graph = graph.clone();
    let listener = meta
        .add_listener_local()
        .property(move |_subject, key, _type, value| {
            let name = value.and_then(default_name_from_json);
            let mut g = lock(&graph);
            match key {
                Some("default.audio.sink") => g.default_sink = name,
                Some("default.audio.source") => g.default_source = name,
                _ => {}
            }
            0
        })
        .register();
    proxies
        .borrow_mut()
        .insert(obj.id, (Box::new(meta), Box::new(listener)));
}
// !SECTION: Registry parsing

// SECTION: Streams
fn open_stream(
    core: &pw::core::CoreRc,
    graph: &Arc<Mutex<GraphState>>,
    streams: &Streams,
    spec: OpenSpec,
    reply: SyncSender<Result<(), String>>,
) {
    let id = spec.id;
    match create_stream(core, spec, reply.clone()) {
        Ok(entry) => {
            streams.borrow_mut().insert(id, entry);
            reconcile_links(core, graph, streams);
        }
        Err(e) => {
            let _ = reply.send(Err(e));
        }
    }
}

fn create_stream(
    core: &pw::core::CoreRc,
    spec: OpenSpec,
    reply: SyncSender<Result<(), String>>,
) -> Result<Entry, String> {
    let capture_like = matches!(
        spec.kind,
        Kind::Capture | Kind::VirtualSink | Kind::AppCapture
    );
    let mut props = properties! {
        *keys::MEDIA_TYPE => "Audio",
        *keys::MEDIA_CATEGORY => if capture_like { "Capture" } else { "Playback" },
        *keys::MEDIA_ROLE => "Production",
        *keys::NODE_NAME => spec.node_name.as_str(),
        *keys::NODE_DESCRIPTION => spec.description.as_str(),
        *keys::NODE_LATENCY => NODE_LATENCY,
    };
    match spec.kind {
        Kind::Capture | Kind::Playback => {
            if let Some(target) = &spec.target {
                props.insert(*keys::TARGET_OBJECT, target.as_str());
                // ? The user chose this device. Do not let the session manager move the stream
                // ? to another one if it disappears and reappears.
                props.insert(*keys::NODE_DONT_RECONNECT, "true");
            }
            if spec.capture_sink {
                props.insert(*keys::STREAM_CAPTURE_SINK, "true");
            }
        }
        Kind::VirtualSink | Kind::VirtualSource => {
            // ? Plan said `Audio/Source/Virtual`. On PipeWire 1.0.5 that class never gets its
            // ? capture ports (even `pw-loopback` ends up with one placeholder input port) and
            // ? WirePlumber logs "Failed to create links because of wrong ports", so nothing can
            // ? record from it. Plain `Audio/Source` + `node.virtual` works there and shows up
            // ? as an ordinary microphone to every program, which is the goal.
            let class = if spec.kind == Kind::VirtualSink {
                "Audio/Sink"
            } else {
                "Audio/Source"
            };
            props.insert(*keys::MEDIA_CLASS, class);
            props.insert(*keys::NODE_VIRTUAL, "true");
            // ? Without these a virtual node has one placeholder port until somebody links
            // ? to it, and the session manager has nothing sensible to link. Declaring the
            // ? layout makes `FL`/`FR` exist as soon as the node does.
            props.insert(*keys::AUDIO_CHANNELS, spec.channels.to_string());
            props.insert("audio.position", position_names(spec.channels));
        }
        Kind::AppCapture => {
            props.insert(*keys::NODE_DONT_RECONNECT, "true");
        }
    }

    let stream = StreamRc::new(core.clone(), &spec.node_name, props)
        .map_err(|e| format!("cannot create the stream: {e}"))?;

    let user = StreamData {
        data: match &spec.data {
            Data::Capture(s) => Data::Capture(s.clone()),
            Data::Playback(s) => Data::Playback(s.clone()),
        },
        channels: usize::from(spec.channels),
        reply: Some(reply),
    };
    let listener = stream
        .add_local_listener_with_user_data(user)
        .state_changed(|_stream, user, _old, new| match new {
            StreamState::Paused | StreamState::Streaming => {
                if let Some(r) = user.reply.take() {
                    let _ = r.send(Ok(()));
                }
            }
            StreamState::Error(msg) => {
                if let Some(r) = user.reply.take() {
                    let _ = r.send(Err(msg));
                }
            }
            _ => {}
        })
        .process(process)
        .register()
        .map_err(|e| format!("cannot listen to the stream: {e}"))?;

    let pod = format_pod(spec.channels);
    let mut params = [spa::pod::Pod::from_bytes(&pod).ok_or("cannot build the stream format")?];
    let mut flags = StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS;
    // ? Virtual nodes and application capture are wired by other clients (or by us, link by
    // ? link), not by the session manager.
    if matches!(spec.kind, Kind::Capture | Kind::Playback) {
        flags |= StreamFlags::AUTOCONNECT;
    }
    let direction = if capture_like {
        spa::utils::Direction::Input
    } else {
        spa::utils::Direction::Output
    };
    stream
        .connect(direction, None, flags, &mut params)
        .map_err(|e| format!("cannot connect the stream: {e}"))?;

    Ok(Entry {
        _listener: listener,
        stream,
        kind: spec.kind,
        pid: spec.pid,
        links: HashSet::new(),
        _link_proxies: Vec::new(),
    })
}

/// The `audio.position` property for `channels` channels (same layouts as the stream format).
fn position_names(channels: u8) -> &'static str {
    match channels {
        1 => "MONO",
        2 => "FL,FR",
        3 => "FL,FR,FC",
        4 => "FL,FR,RL,RR",
        6 => "FL,FR,FC,LFE,RL,RR",
        8 => "FL,FR,FC,LFE,RL,RR,SL,SR",
        _ => "FL,FR",
    }
}

/// The data-thread callback. Real-time rules apply (see `stream.rs`).
fn process(stream: &pw::stream::Stream, user: &mut StreamData) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let requested = buffer.requested() as usize;
    let datas = buffer.datas_mut();
    let Some(data) = datas.first_mut() else {
        return;
    };
    match &user.data {
        Data::Capture(slot) => {
            let (offset, size) = {
                let chunk = data.chunk();
                (chunk.offset() as usize, chunk.size() as usize)
            };
            let Some(bytes) = data.data() else { return };
            let end = offset.saturating_add(size).min(bytes.len());
            let start = offset.min(end);
            if let Ok(mut guard) = slot.state.try_lock() {
                if let Some(state) = guard.as_mut() {
                    state.feed(&bytes[start..end], &slot.counters);
                }
            }
        }
        Data::Playback(slot) => {
            let stride = user.channels.max(1) * core::mem::size_of::<f32>();
            let frames = {
                let Some(bytes) = data.data() else { return };
                let capacity = bytes.len() / stride;
                let frames = if requested > 0 {
                    requested.min(capacity)
                } else {
                    capacity
                };
                let mut played = false;
                if let Ok(mut guard) = slot.state.try_lock() {
                    if let Some(state) = guard.as_mut() {
                        state.render(&mut bytes[..frames * stride], frames, &slot.counters);
                        played = true;
                    }
                }
                if !played {
                    bytes[..frames * stride].fill(0);
                }
                frames
            };
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as i32;
            *chunk.size_mut() = (frames * stride) as u32;
        }
    }
}
// !SECTION: Streams

// SECTION: Application links
/// Link every playing application stream of each `AppCapture` entry to its input ports.
///
/// Called after every registry change, so an application that starts later is picked up and
/// ports that vanish are forgotten (the server removes their links itself).
fn reconcile_links(core: &pw::core::CoreRc, graph: &Arc<Mutex<GraphState>>, streams: &Streams) {
    // The registry callback can fire while a stream is being added; skip rather than panic and
    // let the next event retry.
    let Ok(mut streams) = streams.try_borrow_mut() else {
        return;
    };
    let g = lock(graph);
    for entry in streams.values_mut() {
        let (Kind::AppCapture, Some(pid)) = (entry.kind, entry.pid) else {
            continue;
        };
        let our_node = entry.stream.node_id();
        if our_node == pw::constants::ID_ANY {
            continue;
        }
        entry
            .links
            .retain(|(o, i)| g.ports.contains_key(o) && g.ports.contains_key(i));
        let ours = g.ports_of(our_node, PortDir::In);
        let theirs: Vec<&PortInfo> = g
            .app_nodes(pid)
            .into_iter()
            .flat_map(|n| g.ports_of(n.id, PortDir::Out))
            .collect();
        for (out_port, in_port) in plan_links(&theirs, &ours) {
            if !entry.links.insert((out_port, in_port)) {
                continue;
            }
            let link = core.create_object::<Link>(
                "link-factory",
                &properties! {
                    *keys::LINK_OUTPUT_PORT => out_port.to_string(),
                    *keys::LINK_INPUT_PORT => in_port.to_string(),
                    *keys::OBJECT_LINGER => "false",
                },
            );
            match link {
                Ok(link) => entry._link_proxies.push(link),
                Err(_) => {
                    entry.links.remove(&(out_port, in_port));
                }
            }
        }
    }
}
// !SECTION: Application links
