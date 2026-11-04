//! `ControlApi` over the LAN: the IPC protocol inside a Noise channel, with device pairing.
//!
//! * Transport: TCP. Handshake `Noise_XX_25519_ChaChaPoly_BLAKE2s` (each side proves a static
//!   key), then every byte of the IPC protocol (`ipc::write_frame`/`read_frame`) is encrypted.
//! * Trust: a daemon keeps a list of the controller keys it accepts (`PeerBook`). A controller
//!   keeps the daemon key it paired with and refuses any other (no trust on later first use).
//! * Pairing: the daemon opens a short window and shows a one-time code (80 random bits). The
//!   controller connects with `Noise_XXpsk3` using that code as the pre-shared key, so only
//!   someone who read the code can finish the handshake. Both sides store the other's key.
//! * Off by default. The daemon only listens when told to (`routerd --lan-control`).
//!
//! !! A paired controller can do everything a local UI can: change routing, record, send text
//! !! commands to peers. Pairing is therefore a human decision, never automatic.

use crate::control::ControlApi;
use crate::ipc::{self, RemoteControl};
use crate::types::ControlError;
use serde::{Deserialize, Serialize};
use snow::{Builder, HandshakeState, TransportState, params::NoiseParams};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Default TCP port of the LAN control service.
pub const DEFAULT_PORT: u16 = 6990;

const PARAMS_KNOWN: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
const PARAMS_PAIR: &str = "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s";
const PROLOGUE: &[u8] = b"retae-control/1";
const MODE_KNOWN: u8 = 0;
const MODE_PAIR: u8 = 1;
const MAX_NAME: usize = 64;
const KEY_LEN: usize = 32;
/// A Noise message is at most 65535 bytes, 16 of which are the authentication tag.
const NOISE_MAX: usize = 65535;
const TAG_LEN: usize = 16;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// A paired controller that sends nothing for this long is dropped (a UI polls ~30 times/s).
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_CONNECTIONS: usize = 8;
/// Wrong pairing codes tolerated before the window closes.
const PAIRING_ATTEMPTS: u8 = 5;
/// The pairing code: 16 Crockford-base32 characters = 80 bits.
const CODE_CHARS: usize = 16;
const CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

type Key = [u8; KEY_LEN];

// SECTION: Hex and codes
fn hex(bytes: &[u8]) -> String {
    use core::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn unhex(text: &str) -> Option<Key> {
    let text = text.trim();
    if text.len() != KEY_LEN * 2 || !text.is_ascii() {
        return None;
    }
    let mut out = [0u8; KEY_LEN];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// A short, human-comparable form of a key: the first 8 bytes in groups of four hex digits.
pub fn fingerprint(key: &[u8]) -> String {
    let h = hex(&key[..key.len().min(8)]);
    h.as_bytes()
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join("-")
}

/// The one-time pairing code, as typed by a person: `XXXX-XXXX-XXXX-XXXX`.
#[derive(Clone, PartialEq, Eq)]
pub struct PairingCode([u8; 10]);

impl PairingCode {
    fn random() -> io::Result<Self> {
        let mut bytes = [0u8; 10];
        getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
        Ok(Self(bytes))
    }

    /// Read a typed code. Case, dashes and spaces do not matter; `O`, `I` and `L` are read as
    /// `0`, `1` and `1` (Crockford), so a misread letter still works.
    pub fn parse(text: &str) -> Option<Self> {
        let mut bits: u128 = 0;
        let mut n = 0;
        for c in text.chars().filter(|c| !matches!(c, '-' | ' ')) {
            let c = match c.to_ascii_uppercase() {
                'O' => '0',
                'I' | 'L' => '1',
                other => other,
            };
            let v = CODE_ALPHABET.iter().position(|&a| a as char == c)?;
            bits = (bits << 5) | v as u128;
            n += 1;
        }
        if n != CODE_CHARS {
            return None;
        }
        let mut bytes = [0u8; 10];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (bits >> (8 * (9 - i))) as u8;
        }
        Some(Self(bytes))
    }

    /// The Noise pre-shared key: the 80 code bits, zero padded to 32 bytes.
    // ? Padding adds no strength but costs none either: Noise runs the key through HKDF, and
    // ? the secret is the 80 random bits, which no one can guess in the five tries a window gives.
    fn psk(&self) -> Key {
        let mut k = [0u8; KEY_LEN];
        k[..10].copy_from_slice(&self.0);
        k
    }
}

impl core::fmt::Display for PairingCode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut bits: u128 = 0;
        for b in self.0 {
            bits = (bits << 8) | u128::from(b);
        }
        for i in 0..CODE_CHARS {
            if i > 0 && i % 4 == 0 {
                f.write_str("-")?;
            }
            let v = (bits >> (5 * (CODE_CHARS - 1 - i))) & 31;
            write!(f, "{}", CODE_ALPHABET[v as usize] as char)?;
        }
        Ok(())
    }
}

// !! Never print the code through `Debug` into a log: it is a password for a few minutes.
impl core::fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("PairingCode(..)")
    }
}
// !SECTION: Hex and codes

// SECTION: Identity and trusted peers
/// This machine's long-term key pair. Created on first use and kept in the config directory.
pub struct Identity {
    private: Key,
    public: Key,
}

#[derive(Serialize, Deserialize)]
struct IdentityFile {
    private: String,
    public: String,
}

impl Identity {
    pub fn generate() -> io::Result<Self> {
        let params: NoiseParams = PARAMS_KNOWN.parse().map_err(io::Error::other)?;
        let pair = Builder::new(params)
            .generate_keypair()
            .map_err(io::Error::other)?;
        let to_key = |v: &[u8]| -> io::Result<Key> {
            v.try_into()
                .map_err(|_| io::Error::other("unexpected key length"))
        };
        Ok(Self {
            private: to_key(&pair.private)?,
            public: to_key(&pair.public)?,
        })
    }

    /// Load `path`, or create it. The file holds the private key, so it is private to the user.
    pub fn load_or_create(path: &Path) -> io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let file: IdentityFile = toml::from_str(&text).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{}: {e}", path.display()),
                    )
                })?;
                match (unhex(&file.private), unhex(&file.public)) {
                    (Some(private), Some(public)) => Ok(Self { private, public }),
                    _ => Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{}: not a valid key", path.display()),
                    )),
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let id = Self::generate()?;
                let text = toml::to_string(&IdentityFile {
                    private: hex(&id.private),
                    public: hex(&id.public),
                })
                .map_err(io::Error::other)?;
                write_private(path, &text)?;
                Ok(id)
            }
            Err(e) => Err(e),
        }
    }

    pub fn public(&self) -> Key {
        self.public
    }
}

// !! The private key never goes through `Debug`.
impl core::fmt::Debug for Identity {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Identity({})", fingerprint(&self.public))
    }
}

/// Write a file only its owner can read (Unix 0600; on Windows the user profile ACL applies).
fn write_private(path: &Path, text: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(text.as_bytes())
}

/// One machine we trust: a name to show and its public key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedPeer {
    pub name: String,
    pub key: [u8; KEY_LEN],
}

#[derive(Serialize, Deserialize, Default)]
struct PeerFile {
    #[serde(default, rename = "peer")]
    peers: Vec<PeerRecord>,
}

#[derive(Serialize, Deserialize)]
struct PeerRecord {
    name: String,
    key: String,
}

/// The keys a machine trusts: controllers on a daemon, daemons on a controller.
#[derive(Debug, Default)]
pub struct PeerBook {
    path: Option<PathBuf>,
    peers: Vec<TrustedPeer>,
}

impl PeerBook {
    /// A book that is never written to disk (tests, one-off sessions).
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Load `path`; a missing file is an empty book, and changes are saved back to it.
    pub fn load(path: &Path) -> io::Result<Self> {
        let peers = match std::fs::read_to_string(path) {
            Ok(text) => {
                let file: PeerFile = toml::from_str(&text).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{}: {e}", path.display()),
                    )
                })?;
                file.peers
                    .into_iter()
                    .filter_map(|p| {
                        Some(TrustedPeer {
                            name: p.name,
                            key: unhex(&p.key)?,
                        })
                    })
                    .collect()
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            path: Some(path.to_owned()),
            peers,
        })
    }

    pub fn peers(&self) -> &[TrustedPeer] {
        &self.peers
    }

    pub fn contains(&self, key: &Key) -> bool {
        self.peers.iter().any(|p| &p.key == key)
    }

    /// Trust `peer` (a known key only has its name updated) and save.
    pub fn add(&mut self, peer: TrustedPeer) -> io::Result<()> {
        match self.peers.iter_mut().find(|p| p.key == peer.key) {
            Some(existing) => existing.name = peer.name,
            None => self.peers.push(peer),
        }
        self.save()
    }

    /// Stop trusting `key` and save. Returns whether it was trusted.
    pub fn remove(&mut self, key: &Key) -> io::Result<bool> {
        let before = self.peers.len();
        self.peers.retain(|p| &p.key != key);
        let removed = self.peers.len() != before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let file = PeerFile {
            peers: self
                .peers
                .iter()
                .map(|p| PeerRecord {
                    name: p.name.clone(),
                    key: hex(&p.key),
                })
                .collect(),
        };
        let text = toml::to_string(&file).map_err(io::Error::other)?;
        write_private(path, &text)
    }
}
// !SECTION: Identity and trusted peers

// SECTION: Noise plumbing
fn noise_err(e: snow::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("noise: {e}"))
}

fn build_handshake(
    mode: u8,
    psk: Option<&Key>,
    identity: &Identity,
    initiator: bool,
) -> io::Result<HandshakeState> {
    let params: NoiseParams = if mode == MODE_PAIR {
        PARAMS_PAIR
    } else {
        PARAMS_KNOWN
    }
    .parse()
    .map_err(noise_err)?;
    // ? The mode is in the prologue, so an attacker who flips the mode byte on the wire makes
    // ? both sides derive different keys and the handshake fails.
    let mut prologue = PROLOGUE.to_vec();
    prologue.push(mode);
    let mut builder = Builder::new(params)
        .local_private_key(&identity.private)
        .map_err(noise_err)?
        .prologue(&prologue)
        .map_err(noise_err)?;
    if let Some(psk) = psk {
        builder = builder.psk(3, psk).map_err(noise_err)?;
    }
    if initiator {
        builder.build_initiator()
    } else {
        builder.build_responder()
    }
    .map_err(noise_err)
}

/// A handshake message on the wire: `u16 length (BE)` + bytes.
fn send_handshake(tcp: &mut TcpStream, hs: &mut HandshakeState, payload: &[u8]) -> io::Result<()> {
    let mut buf = vec![0u8; NOISE_MAX];
    let n = hs.write_message(payload, &mut buf).map_err(noise_err)?;
    tcp.write_all(&(n as u16).to_be_bytes())?;
    tcp.write_all(&buf[..n])?;
    tcp.flush()
}

/// Read one handshake message; returns the payload it carried.
fn recv_handshake(tcp: &mut TcpStream, hs: &mut HandshakeState) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 2];
    tcp.read_exact(&mut len)?;
    let mut msg = vec![0u8; usize::from(u16::from_be_bytes(len))];
    tcp.read_exact(&mut msg)?;
    let mut payload = vec![0u8; NOISE_MAX];
    let n = hs.read_message(&msg, &mut payload).map_err(noise_err)?;
    payload.truncate(n);
    Ok(payload)
}

/// A name from the network is only ever shown, but keep it short and printable.
fn clean_name(payload: &[u8]) -> String {
    String::from_utf8_lossy(payload)
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME)
        .collect()
}

fn key_of(tcp_key: Option<&[u8]>) -> io::Result<Key> {
    tcp_key
        .and_then(|k| Key::try_from(k).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "the peer sent no key"))
}

/// The encrypted byte stream: what `ipc::write_frame`/`read_frame` run on top of.
///
/// Plaintext is collected by `write` and encrypted into chunks of at most 65519 bytes on
/// `flush`; each chunk goes out as `u16 length (BE)` + ciphertext. `read` decrypts one chunk at
/// a time. A chunk that fails authentication is an error and the connection ends.
struct NoiseIo {
    tcp: TcpStream,
    noise: TransportState,
    plain_in: Vec<u8>,
    pos: usize,
    plain_out: Vec<u8>,
}

impl NoiseIo {
    fn new(tcp: TcpStream, noise: TransportState) -> Self {
        Self {
            tcp,
            noise,
            plain_in: Vec::new(),
            pos: 0,
            plain_out: Vec::new(),
        }
    }
}

impl Read for NoiseIo {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.pos >= self.plain_in.len() {
            let mut len = [0u8; 2];
            match self.tcp.read_exact(&mut len) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(0),
                Err(e) => return Err(e),
            }
            let mut cipher = vec![0u8; usize::from(u16::from_be_bytes(len))];
            self.tcp.read_exact(&mut cipher)?;
            let mut plain = vec![0u8; cipher.len()];
            let n = self
                .noise
                .read_message(&cipher, &mut plain)
                .map_err(noise_err)?;
            // ? We never send an empty chunk, and an empty read would look like end-of-stream.
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "empty encrypted chunk",
                ));
            }
            plain.truncate(n);
            self.plain_in = plain;
            self.pos = 0;
        }
        let n = out.len().min(self.plain_in.len() - self.pos);
        out[..n].copy_from_slice(&self.plain_in[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl Write for NoiseIo {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        // ? `write_frame` has already limited a message to `MAX_FRAME_BYTES`, so this buffer
        // ? stays small.
        self.plain_out.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut wire = Vec::with_capacity(self.plain_out.len() + 64);
        let mut buf = vec![0u8; NOISE_MAX];
        for chunk in self.plain_out.chunks(NOISE_MAX - TAG_LEN) {
            let n = self
                .noise
                .write_message(chunk, &mut buf)
                .map_err(noise_err)?;
            wire.extend_from_slice(&(n as u16).to_be_bytes());
            wire.extend_from_slice(&buf[..n]);
        }
        self.plain_out.clear();
        self.tcp.write_all(&wire)?;
        self.tcp.flush()
    }
}

/// The daemon's first encrypted message: let the controller in, or say why not.
#[derive(Serialize, Deserialize)]
enum Greeting {
    Welcome,
    Refused(String),
}
// !SECTION: Noise plumbing

// SECTION: Server
struct Pairing {
    code: PairingCode,
    expires: Instant,
    failures: u8,
}

struct Shared {
    name: String,
    identity: Identity,
    book: Arc<Mutex<PeerBook>>,
    pairing: Mutex<Option<Pairing>>,
    connections: AtomicUsize,
}

impl Shared {
    /// The pairing key while a window is open.
    fn pairing_psk(&self) -> Option<Key> {
        let mut guard = self.pairing.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(p) if Instant::now() < p.expires => Some(p.code.psk()),
            Some(_) => {
                *guard = None;
                None
            }
            None => None,
        }
    }

    fn pairing_failed(&self) {
        let mut guard = self.pairing.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = guard.as_mut() {
            p.failures += 1;
            if p.failures >= PAIRING_ATTEMPTS {
                *guard = None;
            }
        }
    }

    fn close_pairing(&self) {
        *self.pairing.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Frees a connection slot when the connection thread ends, however it ends.
struct Slot(Arc<Shared>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Relaxed);
    }
}

/// Serves a `ControlApi` over the LAN. Dropping it stops listening; open connections end at
/// their next request.
pub struct LanServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
    accept: Option<JoinHandle<()>>,
}

impl LanServer {
    /// Listen on `bind`. `name` is what controllers see after pairing.
    // ? Like the IPC server, this holds only a `Weak` control, so a connection cannot keep the
    // ? engine alive after the daemon has dropped it.
    pub fn start(
        bind: SocketAddr,
        name: &str,
        identity: Identity,
        book: Arc<Mutex<PeerBook>>,
        control: Weak<dyn ControlApi>,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(bind)?;
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            name: name.chars().take(MAX_NAME).collect(),
            identity,
            book,
            pairing: Mutex::new(None),
            connections: AtomicUsize::new(0),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let accept = {
            let (stop, shared) = (stop.clone(), shared.clone());
            std::thread::Builder::new()
                .name("lan-accept".into())
                .spawn(move || accept_loop(&listener, &shared, &control, &stop))?
        };
        Ok(Self {
            addr,
            stop,
            shared,
            accept: Some(accept),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn public_key(&self) -> Key {
        self.shared.identity.public
    }

    /// Open a pairing window and return the code to show. A new call replaces the old code.
    pub fn open_pairing(&self, lifetime: Duration) -> io::Result<PairingCode> {
        let code = PairingCode::random()?;
        *self
            .shared
            .pairing
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Pairing {
            code: code.clone(),
            expires: Instant::now() + lifetime,
            failures: 0,
        });
        Ok(code)
    }

    pub fn close_pairing(&self) {
        self.shared.close_pairing();
    }
}

impl Drop for LanServer {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        // `accept` blocks, so wake it with a connection of our own.
        let wake = if self.addr.ip().is_unspecified() {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.addr.port())
        } else {
            self.addr
        };
        let _ = TcpStream::connect_timeout(&wake, Duration::from_secs(1));
        if let Some(t) = self.accept.take() {
            let _ = t.join();
        }
    }
}

fn accept_loop(
    listener: &TcpListener,
    shared: &Arc<Shared>,
    control: &Weak<dyn ControlApi>,
    stop: &AtomicBool,
) {
    loop {
        let conn = listener.accept();
        if stop.load(Relaxed) {
            return;
        }
        let Ok((tcp, _)) = conn else { continue };
        if shared.connections.fetch_add(1, Relaxed) >= MAX_CONNECTIONS {
            shared.connections.fetch_sub(1, Relaxed);
            continue; // dropping `tcp` closes it
        }
        let slot = Slot(shared.clone());
        let control = control.clone();
        let spawned = std::thread::Builder::new()
            .name("lan-conn".into())
            .spawn(move || {
                let _slot = slot;
                let shared = _slot.0.clone();
                let _ = serve_connection(tcp, &shared, &control);
            });
        // A failed spawn drops the closure and with it the slot, so the count stays right.
        drop(spawned);
    }
}

fn serve_connection(
    mut tcp: TcpStream,
    shared: &Shared,
    control: &Weak<dyn ControlApi>,
) -> io::Result<()> {
    let _ = tcp.set_nodelay(true);
    tcp.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    tcp.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;

    let mut mode = [0u8; 1];
    tcp.read_exact(&mut mode)?;
    let mode = mode[0];
    let psk = match mode {
        MODE_KNOWN => None,
        // No open window: say nothing and close, so a scanner learns nothing.
        MODE_PAIR => Some(shared.pairing_psk().ok_or_else(|| {
            io::Error::new(io::ErrorKind::PermissionDenied, "no pairing window is open")
        })?),
        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown mode")),
    };

    // -> e ; <- e, ee, s, es (our name) ; -> s, se (their name)
    let mut hs = build_handshake(mode, psk.as_ref(), &shared.identity, false)?;
    let handshake = (|| {
        recv_handshake(&mut tcp, &mut hs)?;
        send_handshake(&mut tcp, &mut hs, shared.name.as_bytes())?;
        recv_handshake(&mut tcp, &mut hs)
    })();
    let their_name = match handshake {
        Ok(payload) => clean_name(&payload),
        Err(e) => {
            if mode == MODE_PAIR {
                shared.pairing_failed();
            }
            return Err(e);
        }
    };
    let noise = hs.into_transport_mode().map_err(noise_err)?;
    let their_key = key_of(noise.get_remote_static())?;

    let mut io = NoiseIo::new(tcp, noise);
    let decision = if mode == MODE_PAIR {
        // The handshake only completes for someone who knew the code. One code, one device.
        shared.close_pairing();
        let name = if their_name.is_empty() {
            "controller".to_owned()
        } else {
            their_name
        };
        let mut book = shared.book.lock().unwrap_or_else(|e| e.into_inner());
        book.add(TrustedPeer {
            name,
            key: their_key,
        })
        .map_err(|e| format!("cannot store the pairing: {e}"))
    } else if shared
        .book
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&their_key)
    {
        Ok(())
    } else {
        Err("this device is not paired: open a pairing window on the daemon and pair it".to_owned())
    };
    match decision {
        Ok(()) => ipc::write_frame(&mut io, &Greeting::Welcome)?,
        Err(why) => {
            ipc::write_frame(&mut io, &Greeting::Refused(why))?;
            return Ok(());
        }
    }
    io.tcp.set_read_timeout(Some(IDLE_TIMEOUT))?;
    io.tcp.set_write_timeout(Some(IDLE_TIMEOUT))?;
    ipc::serve_stream(&mut io, control);
    Ok(())
}
// !SECTION: Server

// SECTION: Client
fn resolve(addr: impl ToSocketAddrs) -> Result<SocketAddr, ControlError> {
    addr.to_socket_addrs()
        .map_err(|e| ControlError::Disconnected(format!("bad address: {e}")))?
        .find(SocketAddr::is_ipv4)
        .ok_or_else(|| ControlError::Disconnected("no IPv4 address".into()))
}

struct Handshaken {
    io: NoiseIo,
    server_key: Key,
    server_name: String,
}

fn client_handshake(
    addr: SocketAddr,
    mode: u8,
    psk: Option<&Key>,
    identity: &Identity,
    my_name: &str,
) -> Result<Handshaken, ControlError> {
    let down = |e: io::Error| ControlError::Disconnected(format!("{addr}: {e}"));
    let mut tcp = TcpStream::connect_timeout(&addr, HANDSHAKE_TIMEOUT).map_err(down)?;
    let _ = tcp.set_nodelay(true);
    tcp.set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(down)?;
    tcp.set_write_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(down)?;
    tcp.write_all(&[mode]).map_err(down)?;

    let mut hs = build_handshake(mode, psk, identity, true).map_err(down)?;
    let name: String = my_name.chars().take(MAX_NAME).collect();
    let server_name = (|| {
        send_handshake(&mut tcp, &mut hs, &[])?;
        let payload = recv_handshake(&mut tcp, &mut hs)?;
        send_handshake(&mut tcp, &mut hs, name.as_bytes())?;
        Ok::<_, io::Error>(clean_name(&payload))
    })()
    .map_err(|e| {
        if mode == MODE_PAIR {
            ControlError::Protocol(format!(
                "pairing failed (wrong or expired code, or no pairing window open): {e}"
            ))
        } else {
            down(e)
        }
    })?;
    let noise = hs.into_transport_mode().map_err(|e| down(noise_err(e)))?;
    let server_key = key_of(noise.get_remote_static()).map_err(down)?;
    Ok(Handshaken {
        io: NoiseIo::new(tcp, noise),
        server_key,
        server_name,
    })
}

/// Pair with the daemon at `addr` using the code it shows. Returns the daemon, whose key the
/// caller must keep (a `PeerBook` on the controller) and pass to `connect` from now on.
pub fn pair(
    addr: impl ToSocketAddrs,
    identity: &Identity,
    my_name: &str,
    code: &PairingCode,
) -> Result<TrustedPeer, ControlError> {
    let addr = resolve(addr)?;
    let mut h = client_handshake(addr, MODE_PAIR, Some(&code.psk()), identity, my_name)?;
    match ipc::read_frame::<_, Greeting>(&mut h.io) {
        Ok(Some(Greeting::Welcome)) => Ok(TrustedPeer {
            name: h.server_name,
            key: h.server_key,
        }),
        Ok(Some(Greeting::Refused(why))) => Err(ControlError::Protocol(why)),
        // ? The initiator writes the last handshake message without knowing whether the
        // ? daemon accepted the code. A wrong code only shows up here, as a closed connection.
        Ok(None) | Err(_) => Err(ControlError::Protocol(
            "pairing failed (wrong or expired code, or no pairing window open)".into(),
        )),
    }
}

/// Connect to a daemon paired earlier. `server_key` is the key `pair` returned: a daemon that
/// answers with any other key is refused.
pub fn connect(
    addr: impl ToSocketAddrs,
    identity: &Identity,
    my_name: &str,
    server_key: &Key,
) -> Result<RemoteControl, ControlError> {
    connect_checked(addr, identity, my_name, |key| {
        if key == server_key {
            Ok(())
        } else {
            Err(format!(
                "it is {}, not the one you paired with ({})",
                fingerprint(key),
                fingerprint(server_key)
            ))
        }
    })
}

/// Like `connect`, for a controller that only knows addresses: accept any daemon whose key is in
/// `book` (the keys `pair` stored). A daemon that is not in the book is refused.
pub fn connect_paired(
    addr: impl ToSocketAddrs,
    identity: &Identity,
    my_name: &str,
    book: &PeerBook,
) -> Result<RemoteControl, ControlError> {
    connect_checked(addr, identity, my_name, |key| {
        if book.contains(key) {
            Ok(())
        } else {
            Err(format!(
                "it is {}, which you have not paired with: run `pair` first",
                fingerprint(key)
            ))
        }
    })
}

fn connect_checked(
    addr: impl ToSocketAddrs,
    identity: &Identity,
    my_name: &str,
    accept: impl FnOnce(&Key) -> Result<(), String>,
) -> Result<RemoteControl, ControlError> {
    let addr = resolve(addr)?;
    let mut h = client_handshake(addr, MODE_KNOWN, None, identity, my_name)?;
    // ? The daemon proved its key during the handshake, so this check cannot be faked by a
    // ? machine that merely sits on the same address.
    if let Err(why) = accept(&h.server_key) {
        return Err(ControlError::Protocol(format!(
            "the daemon at {addr}: {why}: refusing"
        )));
    }
    match ipc::read_frame::<_, Greeting>(&mut h.io) {
        Ok(Some(Greeting::Welcome)) => {}
        Ok(Some(Greeting::Refused(why))) => return Err(ControlError::Protocol(why)),
        Ok(None) => {
            return Err(ControlError::Disconnected(
                "the daemon closed the connection".into(),
            ));
        }
        Err(e) => return Err(ControlError::Disconnected(e.to_string())),
    }
    // The session can idle between UI polls; only the daemon applies an idle limit.
    h.io.tcp
        .set_read_timeout(None)
        .map_err(|e| ControlError::Disconnected(e.to_string()))?;
    h.io.tcp
        .set_write_timeout(Some(IDLE_TIMEOUT))
        .map_err(|e| ControlError::Disconnected(e.to_string()))?;
    RemoteControl::over(Box::new(h.io))
}
// !SECTION: Client

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::ControlResult;
    use crate::types::{Command, MeterSnapshot, StateSnapshot};
    use platform_io::{AppInfo, DeviceInfo};
    use std::sync::atomic::AtomicU32;

    /// A control that counts gain commands and rejects the rest.
    struct Fake {
        gains: AtomicU32,
        last_profile: Mutex<String>,
    }

    impl ControlApi for Fake {
        fn command(&self, cmd: Command) -> ControlResult<()> {
            match cmd {
                Command::StripGain { .. } => {
                    self.gains.fetch_add(1, Relaxed);
                    Ok(())
                }
                Command::SaveProfile { name } => {
                    *self.last_profile.lock().unwrap() = name;
                    Ok(())
                }
                _ => Err(ControlError::Invalid("only gains in this fake".into())),
            }
        }
        fn snapshot(&self) -> ControlResult<StateSnapshot> {
            Err(ControlError::Invalid("no snapshot".into()))
        }
        fn meters(&self) -> ControlResult<MeterSnapshot> {
            Err(ControlError::Invalid("no meters".into()))
        }
        fn devices(&self) -> ControlResult<Vec<DeviceInfo>> {
            Ok(vec![])
        }
        fn apps(&self) -> ControlResult<Vec<AppInfo>> {
            Ok(vec![])
        }
        fn probe(&self, _: crate::types::ProbeRequest) -> ControlResult<crate::types::ProbeReport> {
            Ok(crate::types::ProbeReport::default())
        }
    }

    struct Rig {
        fake: Arc<Fake>,
        _dyn: Arc<dyn ControlApi>,
        server: LanServer,
        book: Arc<Mutex<PeerBook>>,
    }

    fn rig() -> Rig {
        let fake = Arc::new(Fake {
            gains: AtomicU32::new(0),
            last_profile: Mutex::new(String::new()),
        });
        let as_dyn: Arc<dyn ControlApi> = fake.clone();
        let book = Arc::new(Mutex::new(PeerBook::in_memory()));
        let server = LanServer::start(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            "daemon-under-test",
            Identity::generate().unwrap(),
            book.clone(),
            Arc::downgrade(&as_dyn),
        )
        .unwrap();
        Rig {
            fake,
            _dyn: as_dyn,
            server,
            book,
        }
    }

    fn temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("retae-lan-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn pairing_codes_round_trip_and_forgive_typing() {
        let code = PairingCode::random().unwrap();
        let text = code.to_string();
        assert_eq!(text.len(), 19, "16 characters in four groups: {text}");
        assert_eq!(PairingCode::parse(&text), Some(code.clone()));
        assert_eq!(PairingCode::parse(&text.to_lowercase()), Some(code.clone()));
        assert_eq!(PairingCode::parse(&text.replace('-', " ")), Some(code));
        // Crockford: O is 0, I and L are 1.
        assert_eq!(
            PairingCode::parse("OIL0-0000-0000-0000"),
            PairingCode::parse("0110-0000-0000-0000")
        );
        assert_eq!(PairingCode::parse("0000-0000-0000-000"), None, "too short");
        assert_eq!(PairingCode::parse("0000-0000-0000-00000"), None, "too long");
        assert_eq!(
            PairingCode::parse("UUUU-0000-0000-0000"),
            None,
            "U is not in the alphabet"
        );
        assert_eq!(
            format!("{:?}", PairingCode::random().unwrap()),
            "PairingCode(..)"
        );
    }

    #[test]
    fn two_codes_differ() {
        assert_ne!(
            PairingCode::random().unwrap(),
            PairingCode::random().unwrap()
        );
    }

    #[test]
    fn identity_and_peer_book_persist() {
        let dir = temp_path("persist");
        let id_path = dir.join("identity.toml");
        let first = Identity::load_or_create(&id_path).unwrap();
        let again = Identity::load_or_create(&id_path).unwrap();
        assert_eq!(first.public(), again.public());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&id_path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o077,
                0,
                "the private key is readable by others: {mode:o}"
            );
        }

        let book_path = dir.join("peers.toml");
        let mut book = PeerBook::load(&book_path).unwrap();
        assert!(book.peers().is_empty());
        let peer = TrustedPeer {
            name: "phone".into(),
            key: [7; 32],
        };
        book.add(peer.clone()).unwrap();
        book.add(TrustedPeer {
            name: "renamed".into(),
            key: [7; 32],
        })
        .unwrap();
        let reloaded = PeerBook::load(&book_path).unwrap();
        assert_eq!(reloaded.peers().len(), 1, "same key, one entry");
        assert_eq!(reloaded.peers()[0].name, "renamed");
        assert!(reloaded.contains(&[7; 32]));
        let mut reloaded = reloaded;
        assert!(reloaded.remove(&[7; 32]).unwrap());
        assert!(!reloaded.remove(&[7; 32]).unwrap());
        assert!(PeerBook::load(&book_path).unwrap().peers().is_empty());

        std::fs::write(&id_path, "private = \"zz\"\npublic = \"zz\"\n").unwrap();
        assert!(
            Identity::load_or_create(&id_path).is_err(),
            "a corrupt key is an error, not a new key"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_paired_controller_drives_the_daemon_and_an_unpaired_one_cannot() {
        let rig = rig();
        let addr = rig.server.local_addr();
        let me = Identity::generate().unwrap();

        // Before pairing: refused, with a reason a person can act on.
        let early = connect(addr, &me, "laptop", &rig.server.public_key());
        let why = early.err().expect("an unpaired controller must be refused");
        assert!(format!("{why}").contains("not paired"), "{why}");
        assert!(rig.book.lock().unwrap().peers().is_empty());

        let code = rig.server.open_pairing(Duration::from_secs(30)).unwrap();
        let daemon = pair(addr, &me, "laptop", &code).unwrap();
        assert_eq!(daemon.key, rig.server.public_key());
        assert_eq!(daemon.name, "daemon-under-test");
        {
            let book = rig.book.lock().unwrap();
            assert_eq!(book.peers().len(), 1);
            assert_eq!(book.peers()[0].name, "laptop");
            assert_eq!(book.peers()[0].key, me.public());
        }

        let remote = connect(addr, &me, "laptop", &daemon.key).unwrap();
        remote
            .command(Command::StripGain {
                strip: 0,
                gain_db: -3.0,
            })
            .unwrap();
        assert_eq!(rig.fake.gains.load(Relaxed), 1);
        // Errors cross the encrypted link too.
        assert_eq!(
            remote.command(Command::Beacon { on: true }),
            Err(ControlError::Invalid("only gains in this fake".into()))
        );
        assert!(remote.devices().unwrap().is_empty());

        // A second, never-paired controller is still refused.
        let stranger = Identity::generate().unwrap();
        assert!(connect(addr, &stranger, "stranger", &daemon.key).is_err());

        // Unpairing takes effect for the next connection.
        rig.book.lock().unwrap().remove(&me.public()).unwrap();
        assert!(connect(addr, &me, "laptop", &daemon.key).is_err());
    }

    #[test]
    fn a_code_works_once_and_a_wrong_one_does_not_work_at_all() {
        let rig = rig();
        let addr = rig.server.local_addr();
        let me = Identity::generate().unwrap();

        // No window: no pairing, whatever the code.
        let guess = PairingCode::random().unwrap();
        assert!(pair(addr, &me, "me", &guess).is_err());

        let code = rig.server.open_pairing(Duration::from_secs(30)).unwrap();
        let wrong = pair(addr, &me, "me", &guess);
        assert!(matches!(wrong, Err(ControlError::Protocol(_))), "{wrong:?}");
        assert!(rig.book.lock().unwrap().peers().is_empty());

        // The right code still works after one wrong try, then is used up.
        pair(addr, &me, "me", &code).unwrap();
        let other = Identity::generate().unwrap();
        assert!(
            pair(addr, &other, "other", &code).is_err(),
            "a code is single use"
        );
        assert_eq!(rig.book.lock().unwrap().peers().len(), 1);
    }

    #[test]
    fn too_many_wrong_codes_close_the_window() {
        let rig = rig();
        let addr = rig.server.local_addr();
        let me = Identity::generate().unwrap();
        let code = rig.server.open_pairing(Duration::from_secs(30)).unwrap();
        for _ in 0..PAIRING_ATTEMPTS {
            assert!(pair(addr, &me, "me", &PairingCode::random().unwrap()).is_err());
        }
        // Even the right code is refused now: the window shut after five failures.
        assert!(pair(addr, &me, "me", &code).is_err());
        assert!(rig.book.lock().unwrap().peers().is_empty());
    }

    #[test]
    fn an_expired_window_does_not_pair() {
        let rig = rig();
        let me = Identity::generate().unwrap();
        let code = rig.server.open_pairing(Duration::from_millis(1)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert!(pair(rig.server.local_addr(), &me, "me", &code).is_err());
        let code = rig.server.open_pairing(Duration::from_secs(30)).unwrap();
        rig.server.close_pairing();
        assert!(pair(rig.server.local_addr(), &me, "me", &code).is_err());
    }

    #[test]
    fn a_controller_refuses_a_daemon_with_another_key() {
        let rig = rig();
        let addr = rig.server.local_addr();
        let me = Identity::generate().unwrap();
        let code = rig.server.open_pairing(Duration::from_secs(30)).unwrap();
        let daemon = pair(addr, &me, "me", &code).unwrap();
        let impostor = Identity::generate().unwrap().public();
        let r = connect(addr, &me, "me", &impostor);
        let msg = format!("{}", r.err().expect("must refuse"));
        assert!(msg.contains("not the one you paired with"), "{msg}");
        assert!(connect(addr, &me, "me", &daemon.key).is_ok());
    }

    #[test]
    fn junk_and_slow_peers_do_not_hurt_the_server() {
        let rig = rig();
        let addr = rig.server.local_addr();
        // Garbage, a wrong mode byte, a cut-off handshake and a silent connection.
        for bytes in [
            &b"GET / HTTP/1.1\r\n\r\n"[..],
            &[9u8][..],
            &[0u8, 0, 40, 1, 2][..],
            &[][..],
        ] {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(bytes).unwrap();
        }
        let _silent = TcpStream::connect(addr).unwrap();
        // The server is still answering real controllers.
        let me = Identity::generate().unwrap();
        let code = rig.server.open_pairing(Duration::from_secs(30)).unwrap();
        let daemon = pair(addr, &me, "me", &code).unwrap();
        assert!(connect(addr, &me, "me", &daemon.key).is_ok());
    }

    #[test]
    fn nothing_readable_crosses_the_wire() {
        // A recording proxy between the controller and the daemon.
        let rig = rig();
        let daemon_addr = rig.server.local_addr();
        let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::<u8>::new()));
        {
            let seen = seen.clone();
            std::thread::spawn(move || {
                // Pairing connection, then the working connection.
                for _ in 0..2 {
                    let Ok((client, _)) = proxy.accept() else {
                        return;
                    };
                    let server = TcpStream::connect(daemon_addr).unwrap();
                    for (mut from, mut to) in [
                        (client.try_clone().unwrap(), server.try_clone().unwrap()),
                        (server, client),
                    ] {
                        let seen = seen.clone();
                        std::thread::spawn(move || {
                            let mut buf = [0u8; 4096];
                            while let Ok(n) = from.read(&mut buf) {
                                if n == 0 || to.write_all(&buf[..n]).is_err() {
                                    break;
                                }
                                seen.lock().unwrap().extend_from_slice(&buf[..n]);
                            }
                            let _ = to.shutdown(std::net::Shutdown::Both);
                        });
                    }
                }
            });
        }
        let me = Identity::generate().unwrap();
        let code = rig.server.open_pairing(Duration::from_secs(30)).unwrap();
        let daemon = pair(proxy_addr, &me, "me", &code).unwrap();
        let remote = connect(proxy_addr, &me, "me", &daemon.key).unwrap();
        remote
            .command(Command::SaveProfile {
                name: "visible-profile-name".into(),
            })
            .unwrap();
        assert_eq!(
            &*rig.fake.last_profile.lock().unwrap(),
            "visible-profile-name"
        );
        let wire = seen.lock().unwrap().clone();
        assert!(wire.len() > 100, "the proxy saw the traffic");
        let needle = b"visible-profile-name";
        assert!(
            !wire.windows(needle.len()).any(|w| w == needle),
            "the profile name crossed the wire in clear text"
        );
        // The names given in the handshake are encrypted too.
        assert!(!wire.windows(6).any(|w| w == b"daemon"));
    }

    #[test]
    fn large_messages_cross_in_several_chunks() {
        // A message bigger than one Noise message (65519 bytes of plaintext) must survive.
        let rig = rig();
        let me = Identity::generate().unwrap();
        let code = rig.server.open_pairing(Duration::from_secs(30)).unwrap();
        let daemon = pair(rig.server.local_addr(), &me, "me", &code).unwrap();
        let remote = connect(rig.server.local_addr(), &me, "me", &daemon.key).unwrap();
        let long = "p".repeat(200_000);
        remote
            .command(Command::SaveProfile { name: long.clone() })
            .unwrap();
        assert_eq!(*rig.fake.last_profile.lock().unwrap(), long);
    }

    #[test]
    fn dropping_the_server_ends_sessions_and_frees_the_port() {
        let rig = rig();
        let addr = rig.server.local_addr();
        let me = Identity::generate().unwrap();
        let code = rig.server.open_pairing(Duration::from_secs(30)).unwrap();
        let daemon = pair(addr, &me, "me", &code).unwrap();
        let remote = connect(addr, &me, "me", &daemon.key).unwrap();
        let Rig { server, .. } = rig;
        drop(server);
        assert!(TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_err());
        // The open session's connection thread is still answering until its next request; a
        // command after the daemon's control is gone must not hang.
        let _ = remote.devices();
    }
}
