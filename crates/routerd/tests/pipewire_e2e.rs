//! The whole daemon on a real PipeWire server: commands in, audio out through our virtual
//! microphone, recorded by an independent program (`pw-cat`).
//!
//! Run with: `cargo test -p routerd --features pipewire --test pipewire_e2e`
//!
//! Needs a running PipeWire server *with a session manager* (WirePlumber), which links the
//! test programs to our nodes, and the `pw-cli` / `pw-cat` tools. Without them each test prints
//! `SKIP:` and passes. This proves the graph and the control plane; it says nothing about a
//! real sound card (PLAN §12).

#![cfg(all(target_os = "linux", feature = "pipewire"))]

use app_core::ControlApi;
use app_core::types::{Command, LinkState};
use audio_types::{FixedStr, ProcessLoopbackMode, RouteSend, SinkEndpoint, SourceEndpoint};
use routerd::{BackendKind, Daemon, Options};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command as Proc, Stdio};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::time::{Duration, Instant};

// SECTION: Helpers
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("retae-pw-e2e-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tool_exists(name: &str) -> bool {
    Proc::new(name)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// Start a daemon on PipeWire, or say why this machine cannot.
fn daemon(tag: &str, dir: &TempDir) -> Option<Daemon> {
    if !tool_exists("pw-cat") || !tool_exists("pw-cli") {
        println!("SKIP: pw-cat / pw-cli are not installed");
        return None;
    }
    let options = Options {
        name: format!("e2e-{tag}"),
        backend: BackendKind::PipeWire,
        rx_port: 0,
        instance: Some(format!("e2e-{tag}-{}", std::process::id())),
        config_dir: dir.0.clone(),
        ..Options::default()
    };
    match Daemon::start(options) {
        Ok(d) => Some(d),
        Err(e) => {
            println!("SKIP: no PipeWire server ({e})");
            None
        }
    }
}

fn wait_until(what: &str, secs: u64, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(
            start.elapsed() < Duration::from_secs(secs),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn route(strip: u8, bus: u8) -> Command {
    Command::Route {
        strip,
        bus,
        send: RouteSend {
            enabled: true,
            gain_db: 0.0,
            pre_fader: false,
        },
    }
}

/// A fake sound card that disappears when the test ends.
struct FakeSink {
    child: Child,
    _stdin: ChildStdin,
    name: String,
}

impl FakeSink {
    fn new(tag: &str) -> Self {
        let name = format!("e2e-speakers-{tag}");
        let mut child = Proc::new("pw-cli")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("pw-cli");
        let mut stdin = child.stdin.take().expect("stdin");
        writeln!(
            stdin,
            "create-node adapter {{ factory.name=support.null-audio-sink node.name={name} \
             media.class=Audio/Sink audio.position=[FL FR] }}"
        )
        .unwrap();
        stdin.flush().unwrap();
        Self {
            child,
            _stdin: stdin,
            name,
        }
    }
}

impl Drop for FakeSink {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `pw-cat` playing or recording a WAV file, as an independent program.
// ? This `pw-cat` talks to libsndfile, so it plays and records WAV files, not raw pipes.
struct PwCat {
    child: Child,
    file: PathBuf,
}

impl PwCat {
    fn temp_wav(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        std::env::temp_dir().join(format!(
            "retae-e2e-{}-{tag}-{}.wav",
            std::process::id(),
            N.fetch_add(1, Relaxed)
        ))
    }

    fn play(target: &str, freq: f32, secs: u32) -> Self {
        let file = Self::temp_wav("play");
        let frames = 48_000 * secs as usize;
        let data_len = (frames * 8) as u32;
        let mut wav = Vec::with_capacity(44 + frames * 8);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        for v in [16u32, 3 | (2 << 16), 48_000, 48_000 * 8, 8 | (32 << 16)] {
            wav.extend_from_slice(&v.to_le_bytes());
        }
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..frames {
            let t = (i as f64 * f64::from(freq) / 48_000.0).fract();
            let v = (t * std::f64::consts::TAU).sin() as f32 * 0.5;
            wav.extend_from_slice(&v.to_le_bytes());
            wav.extend_from_slice(&v.to_le_bytes());
        }
        std::fs::write(&file, wav).unwrap();
        let child = Proc::new("pw-cat")
            .args(["-p", "--target", target, "--latency", "128"])
            .arg(&file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("pw-cat");
        Self { child, file }
    }

    fn record(target: &str) -> Self {
        let file = Self::temp_wav("rec");
        let child = Proc::new("pw-cat")
            .args([
                "-r",
                "--format",
                "f32",
                "--rate",
                "48000",
                "--channels",
                "2",
            ])
            .args(["--target", target, "--latency", "128"])
            .arg(&file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("pw-cat");
        Self { child, file }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Stop like Ctrl-C so the file is closed properly; return the left channel.
    fn finish_recording(&mut self) -> Vec<f32> {
        let _ = Proc::new("kill")
            .args(["-INT", &self.child.id().to_string()])
            .status();
        let _ = self.child.wait();
        let bytes = std::fs::read(&self.file).unwrap_or_default();
        let Some(at) = bytes.windows(4).position(|w| w == b"data") else {
            return Vec::new();
        };
        bytes[(at + 8).min(bytes.len())..]
            .chunks_exact(8)
            .map(|f| f32::from_le_bytes([f[0], f[1], f[2], f[3]]))
            .collect()
    }
}

impl Drop for PwCat {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.file);
    }
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / x.len().max(1) as f64).sqrt() as f32
}

fn freq_hz(x: &[f32]) -> f32 {
    let crossings = x.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
    crossings as f32 * 48_000.0 / x.len().max(1) as f32
}
// !SECTION: Helpers

// SECTION: Tests
#[test]
fn commands_wire_a_virtual_input_through_the_mixer_to_a_virtual_mic() {
    let dir = TempDir::new("virt");
    let Some(d) = daemon("virt", &dir) else {
        return;
    };
    let c = d.control();
    assert_eq!(d.backend_name(), "pipewire");

    for cmd in [
        route(0, 0),
        Command::StripSource {
            strip: 0,
            source: SourceEndpoint::VirtualInput { index: 0 },
            channels: 2,
        },
        Command::BusSink {
            bus: 0,
            sink: SinkEndpoint::VirtualMic { index: 0 },
            channels: 2,
        },
    ] {
        c.command(cmd).unwrap_or_else(|e| panic!("setup: {e}"));
    }
    let snap = c.snapshot().unwrap();
    assert_eq!(
        snap.strips[0].link,
        LinkState::Running,
        "{:?}",
        snap.strips[0].link
    );
    assert_eq!(
        snap.buses[0].link,
        LinkState::Running,
        "{:?}",
        snap.buses[0].link
    );

    // A program plays into our virtual input; another records from our virtual mic.
    let _player = PwCat::play("retae-virtual-in-0", 1000.0, 20);
    std::thread::sleep(Duration::from_millis(1500));
    let mut recorder = PwCat::record("retae-virtual-mic-0");
    std::thread::sleep(Duration::from_millis(2500));
    let heard = recorder.finish_recording();
    let steady = heard.get(24_000..).unwrap_or_default();
    let (level, freq) = (rms(steady), freq_hz(steady));
    println!(
        "recorded {} frames, rms {level:.3}, {freq:.0} Hz",
        heard.len()
    );
    assert!(heard.len() >= 48_000, "only {} frames", heard.len());
    assert!((0.28..0.42).contains(&level), "rms {level}");
    assert!((freq - 1000.0).abs() < 40.0, "{freq} Hz");

    // Removing the endpoint closes the node.
    c.command(Command::StripSource {
        strip: 0,
        source: SourceEndpoint::Unassigned,
        channels: 2,
    })
    .unwrap();
    wait_until("the virtual input to go away", 5, || {
        !Proc::new("pw-link")
            .arg("-i")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("retae-virtual-in-0"))
            .unwrap_or(false)
    });
}

#[test]
fn one_application_is_captured_and_sent_to_the_virtual_mic() {
    let dir = TempDir::new("app");
    let Some(d) = daemon("app", &dir) else { return };
    let c = d.control();
    let card = FakeSink::new("app");
    wait_until("the fake card", 5, || {
        c.devices().unwrap().iter().any(|dev| dev.id == card.name)
    });

    // Two programs play different tones to the same sink; only one is captured.
    let wanted = PwCat::play(&card.name, 440.0, 30);
    let other = PwCat::play(&card.name, 2000.0, 30);
    let (wanted_pid, other_pid) = (wanted.pid(), other.pid());
    wait_until("both programs in apps()", 8, || {
        let apps = c.apps().unwrap();
        [wanted_pid, other_pid]
            .iter()
            .all(|p| apps.iter().any(|a| a.pid == *p))
    });

    for cmd in [
        route(0, 0),
        Command::StripSource {
            strip: 0,
            source: SourceEndpoint::ProcessLoopback {
                pid: wanted_pid,
                exe_name: FixedStr::from_str_trunc("pw-cat"),
                mode: ProcessLoopbackMode::IncludeTree,
            },
            channels: 2,
        },
        Command::BusSink {
            bus: 0,
            sink: SinkEndpoint::VirtualMic { index: 0 },
            channels: 2,
        },
    ] {
        c.command(cmd).unwrap_or_else(|e| panic!("setup: {e}"));
    }

    wait_until("the virtual mic", 5, || {
        Proc::new("pw-link")
            .arg("-o")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("retae-virtual-mic-0"))
            .unwrap_or(false)
    });
    std::thread::sleep(Duration::from_millis(1000));
    let mut recorder = PwCat::record("retae-virtual-mic-0");
    std::thread::sleep(Duration::from_millis(2500));
    let heard = recorder.finish_recording();
    let steady = heard.get(24_000..).unwrap_or_default();
    let (level, freq) = (rms(steady), freq_hz(steady));
    println!(
        "recorded {} frames, rms {level:.3}, {freq:.0} Hz",
        heard.len()
    );
    assert!(heard.len() >= 48_000, "only {} frames", heard.len());
    assert!((0.28..0.42).contains(&level), "rms {level}");
    assert!(
        (freq - 440.0).abs() < 25.0,
        "heard {freq} Hz: the other program leaked in, or the wrong one was captured"
    );
}
// !SECTION: Tests
