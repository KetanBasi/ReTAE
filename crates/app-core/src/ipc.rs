//! `ControlApi` over a local socket: postcard messages in length-prefixed frames.
//!
//! * Linux: an abstract-namespace Unix socket. Windows: a named pipe. Neither leaves a file
//!   behind, so a crashed daemon cannot leave a stale socket that blocks the next start.
//! * One request, one response, in order, per connection. A connection gets its own thread.

use crate::control::{ControlApi, ControlResult};
use crate::types::{
    Command, ControlError, MeterSnapshot, PROTOCOL_VERSION, ProbeReport, ProbeRequest,
    StateSnapshot,
};
use interprocess::local_socket::{
    GenericNamespaced, Listener, ListenerOptions, Stream, ToNsName, prelude::*,
};
use platform_io::{AppInfo, DeviceInfo};
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;

/// A frame bigger than this is garbage or an attack. A full snapshot is a few kilobytes.
pub const MAX_FRAME_BYTES: usize = 1 << 20;

// SECTION: Messages
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Request {
    Hello { protocol: u32 },
    Command(Command),
    Snapshot,
    Meters,
    Devices,
    Apps,
    Probe(ProbeRequest),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Response {
    Hello { protocol: u32 },
    Done,
    Snapshot(Box<StateSnapshot>),
    Meters(Box<MeterSnapshot>),
    Devices(Vec<DeviceInfo>),
    Apps(Vec<AppInfo>),
    Probe(Box<ProbeReport>),
    Error(ControlError),
}
// !SECTION: Messages

// SECTION: Framing
/// Write `value` as `u32 length (LE)` + postcard bytes.
pub fn write_frame<W: Write, T: Serialize>(out: &mut W, value: &T) -> io::Result<()> {
    let bytes = postcard::to_stdvec(value).map_err(io::Error::other)?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(io::Error::other("message too large"));
    }
    out.write_all(&(bytes.len() as u32).to_le_bytes())?;
    out.write_all(&bytes)?;
    out.flush()
}

/// Read one frame. `Ok(None)` = the other side closed the connection cleanly.
pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(input: &mut R) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match input.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes is over the {MAX_FRAME_BYTES} limit"),
        ));
    }
    // ? The length was checked first, so a hostile peer cannot make us allocate gigabytes.
    let mut buf = vec![0u8; len];
    input.read_exact(&mut buf)?;
    postcard::from_bytes(&buf)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
// !SECTION: Framing

fn socket_name(name: &str) -> io::Result<interprocess::local_socket::Name<'static>> {
    name.to_owned().to_ns_name::<GenericNamespaced>()
}

/// Does something already answer on `name`? (The single-instance check.)
pub fn is_running(name: &str) -> bool {
    socket_name(name).is_ok_and(|n| Stream::connect(n).is_ok())
}

// SECTION: Server
#[derive(Debug)]
pub enum IpcError {
    /// Another daemon already answers on this name.
    AlreadyRunning,
    Io(io::Error),
}

impl core::fmt::Display for IpcError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::AlreadyRunning => f.write_str("another routerd is already running"),
            Self::Io(e) => write!(f, "IPC error: {e}"),
        }
    }
}

impl std::error::Error for IpcError {}

/// Serves a `ControlApi` on a local socket. Dropping it stops accepting and refuses further
/// requests on open connections.
pub struct IpcServer {
    name: String,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl IpcServer {
    /// Bind `name` and start serving. This is also the single-instance lock: it fails with
    /// `AlreadyRunning` if something already listens there.
    // ? The server holds only a `Weak` reference to the control. Open connections therefore
    // ? cannot keep the engine alive after the daemon has dropped its own `Arc`: the next
    // ? request on such a connection gets an error and the connection ends.
    pub fn start(name: &str, control: Weak<dyn ControlApi>) -> Result<Self, IpcError> {
        // Something answers already? Then it is a running daemon (a connect would also have
        // woken a stuck one, which is fine: it sends nothing until asked).
        if Stream::connect(socket_name(name).map_err(IpcError::Io)?).is_ok() {
            return Err(IpcError::AlreadyRunning);
        }
        let listener: Listener = ListenerOptions::new()
            .name(socket_name(name).map_err(IpcError::Io)?)
            .create_sync()
            .map_err(|e| {
                if e.kind() == io::ErrorKind::AddrInUse {
                    IpcError::AlreadyRunning
                } else {
                    IpcError::Io(e)
                }
            })?;
        let stop = Arc::new(AtomicBool::new(false));
        let accept = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("ipc-accept".into())
                .spawn(move || accept_loop(&listener, &control, &stop))
                .map_err(IpcError::Io)?
        };
        Ok(Self {
            name: name.to_owned(),
            stop,
            accept: Some(accept),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        // `accept` blocks, so wake it with a connection of our own.
        if let Ok(n) = socket_name(&self.name) {
            let _ = Stream::connect(n);
        }
        if let Some(t) = self.accept.take() {
            let _ = t.join();
        }
    }
}

fn accept_loop(listener: &Listener, control: &Weak<dyn ControlApi>, stop: &AtomicBool) {
    loop {
        let conn = listener.accept();
        if stop.load(Relaxed) {
            return;
        }
        let Ok(conn) = conn else { continue };
        let control = control.clone();
        let _ = std::thread::Builder::new()
            .name("ipc-conn".into())
            .spawn(move || serve(conn, &control));
    }
}

fn serve(mut conn: Stream, control: &Weak<dyn ControlApi>) {
    serve_stream(&mut conn, control);
}

/// Answer requests on any byte stream until it closes. The local socket and the encrypted LAN
/// connection (`lan.rs`) share this loop, so both speak exactly the same protocol.
pub(crate) fn serve_stream<S: Read + Write>(conn: &mut S, control: &Weak<dyn ControlApi>) {
    loop {
        let request: Request = match read_frame(conn) {
            Ok(Some(r)) => r,
            Ok(None) | Err(_) => return,
        };
        let Some(control) = control.upgrade() else {
            let _ = write_frame(
                conn,
                &Response::Error(ControlError::Disconnected(
                    "the daemon is shutting down".into(),
                )),
            );
            return;
        };
        let response = handle(&*control, request);
        drop(control); // do not hold the engine alive while waiting for the next request
        if write_frame(conn, &response).is_err() {
            return;
        }
    }
}

fn handle(control: &dyn ControlApi, request: Request) -> Response {
    fn wrap<T>(r: ControlResult<T>, ok: impl FnOnce(T) -> Response) -> Response {
        r.map_or_else(Response::Error, ok)
    }
    match request {
        Request::Hello { protocol } if protocol == PROTOCOL_VERSION => Response::Hello {
            protocol: PROTOCOL_VERSION,
        },
        Request::Hello { protocol } => Response::Error(ControlError::Protocol(format!(
            "the client speaks protocol {protocol}, the daemon {PROTOCOL_VERSION}"
        ))),
        Request::Command(c) => wrap(control.command(c), |()| Response::Done),
        Request::Snapshot => wrap(control.snapshot(), |s| Response::Snapshot(Box::new(s))),
        Request::Meters => wrap(control.meters(), |m| Response::Meters(Box::new(m))),
        Request::Devices => wrap(control.devices(), Response::Devices),
        Request::Apps => wrap(control.apps(), Response::Apps),
        Request::Probe(p) => wrap(control.probe(p), |r| Response::Probe(Box::new(r))),
    }
}
// !SECTION: Server

// SECTION: Client
/// Any byte stream a `RemoteControl` can run over (a local socket or the encrypted LAN link).
pub(crate) trait Duplex: Read + Write + Send {}
impl<T: Read + Write + Send> Duplex for T {}

/// A `ControlApi` that forwards to a daemon over IPC (or, through `lan.rs`, over the LAN).
pub struct RemoteControl {
    // ? A boxed trait object, because the two transports are different types but the client
    // ? code above them is identical.
    conn: Mutex<Box<dyn Duplex>>,
}

impl RemoteControl {
    pub fn connect(name: &str) -> ControlResult<Self> {
        let n = socket_name(name).map_err(|e| ControlError::Disconnected(e.to_string()))?;
        let conn = Stream::connect(n).map_err(|e| {
            ControlError::Disconnected(format!("cannot reach routerd ({name}): {e}"))
        })?;
        Self::over(Box::new(conn))
    }

    /// Run the protocol over an already-open stream: the version handshake, then requests.
    pub(crate) fn over(conn: Box<dyn Duplex>) -> ControlResult<Self> {
        let remote = Self {
            conn: Mutex::new(conn),
        };
        match remote.call(&Request::Hello {
            protocol: PROTOCOL_VERSION,
        })? {
            Response::Hello { .. } => Ok(remote),
            other => Err(unexpected(&other)),
        }
    }

    fn call(&self, request: &Request) -> ControlResult<Response> {
        // ? A panic while holding the lock cannot leave the stream half-written in a way that
        // ? matters: the next call would just fail and report Disconnected.
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let down = |e: io::Error| ControlError::Disconnected(e.to_string());
        write_frame(&mut *conn, request).map_err(down)?;
        match read_frame(&mut *conn).map_err(down)? {
            Some(Response::Error(e)) => Err(e),
            Some(r) => Ok(r),
            None => Err(ControlError::Disconnected(
                "the daemon closed the connection".into(),
            )),
        }
    }
}

fn unexpected(r: &Response) -> ControlError {
    ControlError::Protocol(format!("unexpected reply {r:?}"))
}

impl ControlApi for RemoteControl {
    fn command(&self, cmd: Command) -> ControlResult<()> {
        match self.call(&Request::Command(cmd))? {
            Response::Done => Ok(()),
            other => Err(unexpected(&other)),
        }
    }

    fn snapshot(&self) -> ControlResult<StateSnapshot> {
        match self.call(&Request::Snapshot)? {
            Response::Snapshot(s) => Ok(*s),
            other => Err(unexpected(&other)),
        }
    }

    fn meters(&self) -> ControlResult<MeterSnapshot> {
        match self.call(&Request::Meters)? {
            Response::Meters(m) => Ok(*m),
            other => Err(unexpected(&other)),
        }
    }

    fn devices(&self) -> ControlResult<Vec<DeviceInfo>> {
        match self.call(&Request::Devices)? {
            Response::Devices(d) => Ok(d),
            other => Err(unexpected(&other)),
        }
    }

    fn apps(&self) -> ControlResult<Vec<AppInfo>> {
        match self.call(&Request::Apps)? {
            Response::Apps(a) => Ok(a),
            other => Err(unexpected(&other)),
        }
    }

    fn probe(&self, request: ProbeRequest) -> ControlResult<ProbeReport> {
        match self.call(&Request::Probe(request))? {
            Response::Probe(p) => Ok(*p),
            other => Err(unexpected(&other)),
        }
    }
}
// !SECTION: Client

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EngineStatus, LinkState};
    use std::sync::atomic::AtomicU32;

    /// A control that just remembers what it was told.
    struct Fake {
        gain_commands: AtomicU32,
    }

    impl ControlApi for Fake {
        fn command(&self, cmd: Command) -> ControlResult<()> {
            match cmd {
                Command::StripGain { .. } => {
                    self.gain_commands.fetch_add(1, Relaxed);
                    Ok(())
                }
                _ => Err(ControlError::Invalid("only gains in this fake".into())),
            }
        }
        fn snapshot(&self) -> ControlResult<StateSnapshot> {
            Err(ControlError::Invalid("no snapshot".into()))
        }
        fn meters(&self) -> ControlResult<MeterSnapshot> {
            Ok(MeterSnapshot {
                strips: vec![],
                buses: vec![],
                lan_rx: vec![],
                any_audio_active: true,
                engine: EngineStatus {
                    rt_load_permille: 7,
                    ..EngineStatus::default()
                },
            })
        }
        fn devices(&self) -> ControlResult<Vec<DeviceInfo>> {
            Ok(vec![])
        }
        fn apps(&self) -> ControlResult<Vec<AppInfo>> {
            Ok(vec![])
        }
        fn probe(&self, _: ProbeRequest) -> ControlResult<ProbeReport> {
            Ok(ProbeReport::default())
        }
    }

    fn unique(tag: &str) -> String {
        format!("retae-test-{tag}-{}", std::process::id())
    }

    #[test]
    fn frames_round_trip_and_reject_oversize_lengths() {
        let mut wire = Vec::new();
        write_frame(&mut wire, &Request::Snapshot).unwrap();
        let back: Option<Request> = read_frame(&mut wire.as_slice()).unwrap();
        assert_eq!(back, Some(Request::Snapshot));

        let empty: Option<Request> = read_frame(&mut [].as_slice()).unwrap();
        assert_eq!(empty, None, "a clean close is not an error");

        let hostile = (u32::MAX).to_le_bytes();
        let r: io::Result<Option<Request>> = read_frame(&mut hostile.as_slice());
        assert_eq!(r.unwrap_err().kind(), io::ErrorKind::InvalidData);

        let mut truncated = 100u32.to_le_bytes().to_vec();
        truncated.extend([1, 2, 3]);
        let r: io::Result<Option<Request>> = read_frame(&mut truncated.as_slice());
        assert!(r.is_err());

        let mut junk = 3u32.to_le_bytes().to_vec();
        junk.extend([0xFF, 0xFF, 0xFF]);
        let r: io::Result<Option<Request>> = read_frame(&mut junk.as_slice());
        assert_eq!(r.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_client_talks_to_a_server_and_errors_cross_the_wire() {
        let name = unique("rpc");
        let fake = Arc::new(Fake {
            gain_commands: AtomicU32::new(0),
        });
        let as_dyn: Arc<dyn ControlApi> = fake.clone();
        let server = IpcServer::start(&name, Arc::downgrade(&as_dyn)).unwrap();

        let client = RemoteControl::connect(&name).unwrap();
        client
            .command(Command::StripGain {
                strip: 1,
                gain_db: -3.0,
            })
            .unwrap();
        assert_eq!(fake.gain_commands.load(Relaxed), 1);
        assert_eq!(
            client.command(Command::Beacon { on: true }),
            Err(ControlError::Invalid("only gains in this fake".into()))
        );
        assert_eq!(client.meters().unwrap().engine.rt_load_permille, 7);
        assert!(client.snapshot().is_err());
        assert_eq!(
            client.probe(ProbeRequest::default()).unwrap(),
            ProbeReport::default()
        );

        // A second client at the same time.
        let second = RemoteControl::connect(&name).unwrap();
        assert!(second.devices().unwrap().is_empty());
        drop(server);
        // Not a LinkState use, but keeps the type in the public surface honest.
        let _ = LinkState::Idle;
    }

    #[test]
    fn only_one_server_per_name() {
        let name = unique("single");
        let fake: Arc<dyn ControlApi> = Arc::new(Fake {
            gain_commands: AtomicU32::new(0),
        });
        let first = IpcServer::start(&name, Arc::downgrade(&fake)).unwrap();
        match IpcServer::start(&name, Arc::downgrade(&fake)) {
            Err(IpcError::AlreadyRunning) => {}
            other => panic!("expected AlreadyRunning, got {:?}", other.err()),
        }
        // After the first one is gone the name is free again.
        drop(first);
        let again = IpcServer::start(&name, Arc::downgrade(&fake)).unwrap();
        drop(again);
    }

    #[test]
    fn connecting_to_nothing_is_a_clear_error() {
        match RemoteControl::connect(&unique("nobody")) {
            Err(ControlError::Disconnected(_)) => {}
            other => panic!("expected Disconnected, got {:?}", other.err()),
        }
    }

    #[test]
    fn a_dropped_control_ends_the_conversation() {
        let name = unique("weak");
        let fake: Arc<dyn ControlApi> = Arc::new(Fake {
            gain_commands: AtomicU32::new(0),
        });
        let server = IpcServer::start(&name, Arc::downgrade(&fake)).unwrap();
        let client = RemoteControl::connect(&name).unwrap();
        assert!(client.devices().is_ok());
        drop(fake);
        assert!(matches!(
            client.devices(),
            Err(ControlError::Disconnected(_))
        ));
        drop(server);
    }
}
