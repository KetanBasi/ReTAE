//! The PipeWire backend against a real PipeWire server.
//!
//! Run with: `cargo test -p platform-io --features pipewire --test pipewire_live`
//!
//! These tests need a running server (a desktop session, or a headless `pipewire` with
//! `XDG_RUNTIME_DIR` set) and the `pw-cli` / `pw-cat` tools. Without a server they print
//! `SKIP:` and pass, so a plain `cargo test --features pipewire` still works on a build machine.
//! They never leave anything behind: the fake devices are owned by a child process that dies
//! with the test, and every stream is closed.
//!
//! What they prove is the *graph* behaviour (nodes, links, formats, our drift handling). They
//! say nothing about a real sound card's latency (PLAN §12).

#![cfg(all(target_os = "linux", feature = "pipewire"))]

use audio_engine::{ControlPorts, InputPort, OutputPort, create_engine};
use audio_types::{
    BusId, ClockDomain, EngineCommand, FixedStr, PortSlot, ProcessLoopbackMode, RouteSend,
    SinkEndpoint, SourceEndpoint, StripId,
};
use platform_io::{
    AudioBackend, BackendError, ClosedPort, DeviceKind, PipeWireBackend, PipeWireOptions,
    RateClock, TimerMaster,
};
use rtrb::Producer;
use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

// SECTION: Fixtures
/// Connect, or say why this machine cannot run the tests.
fn backend() -> Option<PipeWireBackend> {
    match PipeWireBackend::connect(PipeWireOptions::default()) {
        Ok(b) => Some(b),
        Err(e) => {
            println!("SKIP: no PipeWire server ({e})");
            None
        }
    }
}

fn tool_exists(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// A fake sound card: a null sink and a null source that live as long as this value.
// ? `pw-cli` keeps the objects it creates only while it is connected (no `object.linger`), so
// ? killing the child removes them. Nothing is left on a developer's desktop.
struct FakeCard {
    child: Child,
    _stdin: ChildStdin,
    sink: String,
    source: String,
}

impl FakeCard {
    fn new(tag: &str) -> Option<Self> {
        if !tool_exists("pw-cli") {
            println!("SKIP: pw-cli is not installed");
            return None;
        }
        let (sink, source) = (format!("test-speakers-{tag}"), format!("test-mic-{tag}"));
        let mut child = Command::new("pw-cli")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut stdin = child.stdin.take()?;
        for (name, class) in [(&sink, "Audio/Sink"), (&source, "Audio/Source")] {
            writeln!(
                stdin,
                "create-node adapter {{ factory.name=support.null-audio-sink node.name={name} \
                 node.description=\"Test {name}\" media.class={class} audio.position=[FL FR] }}"
            )
            .ok()?;
        }
        stdin.flush().ok()?;
        Some(Self {
            child,
            _stdin: stdin,
            sink,
            source,
        })
    }
}

impl Drop for FakeCard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_for(what: &str, secs: u64, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(
            start.elapsed() < Duration::from_secs(secs),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn hw_in(id: &str) -> SourceEndpoint {
    SourceEndpoint::HardwareInput {
        device_id: FixedStr::from_str_trunc(id),
    }
}
fn loopback(id: &str) -> SourceEndpoint {
    SourceEndpoint::DeviceLoopback {
        device_id: FixedStr::from_str_trunc(id),
    }
}
fn hw_out(id: &str) -> SinkEndpoint {
    SinkEndpoint::HardwareOutput {
        device_id: FixedStr::from_str_trunc(id),
    }
}
// !SECTION: Fixtures

// SECTION: Engine rig
/// A real mixer on a timer clock with a few helpers, standing in for `app-core`.
struct Rig {
    master: Option<TimerMaster>,
    commands: Producer<EngineCommand>,
    ports: ControlPorts,
}

impl Rig {
    fn new() -> Self {
        let (engine, handles) = create_engine();
        Self {
            master: Some(TimerMaster::spawn(engine, 0.0).expect("mixer thread")),
            commands: handles.commands,
            ports: handles.ports,
        }
    }

    fn send(&mut self, c: EngineCommand) {
        self.commands.push(c).expect("command queue full");
    }

    fn input(&mut self) -> InputPort {
        self.ports.acquire_input().expect("free input port")
    }
    fn output(&mut self) -> OutputPort {
        self.ports.acquire_output().expect("free output port")
    }

    /// Feed `strip` from `slot` through the drift resampler and send it to `bus` at 0 dB.
    fn strip_to_bus(&mut self, strip: u8, slot: PortSlot, channels: u8, bus: u8) {
        let (s, b) = (StripId(strip), BusId(bus));
        self.send(EngineCommand::SetStripClock {
            strip: s,
            clock: ClockDomain::External { target_ms: 8.0 },
        });
        self.send(EngineCommand::AttachStripInput {
            strip: s,
            slot,
            channels,
        });
        self.send(EngineCommand::SetRouteSend {
            strip: s,
            bus: b,
            send: RouteSend {
                enabled: true,
                gain_db: 0.0,
                pre_fader: false,
            },
        });
    }

    fn attach_output(&mut self, bus: u8, slot: PortSlot, channels: u8) {
        self.send(EngineCommand::AttachBusOutput {
            bus: BusId(bus),
            slot,
            channels,
        });
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        drop(self.master.take());
    }
}

/// A thread that pushes a sine into a port at the pace of a sound card.
struct ToneSource {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<InputPort>>,
}

impl ToneSource {
    fn start(mut port: InputPort, channels: usize, freq: f32, amp: f32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut clock = RateClock::new(48_000.0);
            let mut phase = 0.0_f64;
            let mut buf = Vec::new();
            while !flag.load(Relaxed) {
                std::thread::sleep(Duration::from_millis(1));
                let due = clock.due();
                buf.clear();
                for _ in 0..due {
                    let v = (phase * std::f64::consts::TAU).sin() as f32 * amp;
                    phase = (phase + f64::from(freq) / 48_000.0).fract();
                    buf.extend(std::iter::repeat_n(v, channels));
                }
                port.push_frames(&buf, channels);
            }
            port
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }

    fn stop(mut self) -> InputPort {
        self.stop.store(true, Relaxed);
        self.thread.take().unwrap().join().unwrap()
    }
}

/// Pop everything a tap port has gathered for `ms` milliseconds, as left-channel samples.
fn record_left(tap: &mut OutputPort, channels: usize, ms: u64) -> Vec<f32> {
    let mut left = Vec::new();
    let mut buf = vec![0.0; 960 * channels];
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        let n = tap.pop(&mut buf) / channels;
        left.extend((0..n).map(|i| buf[i * channels]));
        std::thread::sleep(Duration::from_millis(2));
    }
    left
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / x.len().max(1) as f64).sqrt() as f32
}

/// Frequency from rising zero crossings.
fn freq_hz(x: &[f32]) -> f32 {
    let crossings = x.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
    crossings as f32 * 48_000.0 / x.len().max(1) as f32
}

fn give_back_input(rig: &mut Rig, closed: Option<ClosedPort>) {
    match closed {
        Some(ClosedPort::Input(p)) => rig.ports.release_input(p),
        other => panic!("expected the input port back, got {other:?}"),
    }
}

fn give_back_output(rig: &mut Rig, closed: Option<ClosedPort>) {
    match closed {
        Some(ClosedPort::Output(p)) => rig.ports.release_output(p),
        other => panic!("expected the output port back, got {other:?}"),
    }
}
// !SECTION: Engine rig

// SECTION: Tests
#[test]
fn devices_are_listed_with_direction_and_a_default() {
    let Some(backend) = backend() else { return };
    let Some(card) = FakeCard::new("enum") else {
        return;
    };
    wait_for("the fake card to appear", 5, || {
        backend.devices().iter().any(|d| d.id == card.sink)
    });
    let devices = backend.devices();
    let sink = devices.iter().find(|d| d.id == card.sink).unwrap();
    let source = devices.iter().find(|d| d.id == card.source).unwrap();
    assert_eq!(sink.kind, DeviceKind::Output);
    assert_eq!(source.kind, DeviceKind::Input);
    assert_eq!(sink.channels, 2);
    assert!(
        sink.name.contains("Test"),
        "description is shown: {}",
        sink.name
    );
    assert_eq!(backend.name(), "pipewire");
    assert!(backend.capabilities().process_loopback);

    // Gone with its owner.
    drop(card);
    wait_for("the fake card to disappear", 5, || {
        !backend
            .devices()
            .iter()
            .any(|d| d.id.starts_with("test-speakers-enum"))
    });
}

#[test]
fn failed_opens_hand_the_port_back_and_say_why() {
    let Some(mut backend) = backend() else { return };
    let mut rig = Rig::new();

    let port = rig.input();
    let (err, port) = backend
        .open_input(&hw_in("no-such-device"), 2, port)
        .err()
        .expect("unknown device");
    assert!(matches!(err, BackendError::NoSuchDevice(_)), "{err:?}");

    let (err, port) = backend
        .open_input(&hw_in("x"), 9, port)
        .err()
        .expect("too many channels");
    assert_eq!(err, BackendError::BadChannels(9));

    let (err, port) = backend
        .open_input(&SourceEndpoint::Unassigned, 2, port)
        .err()
        .expect("not a source");
    assert!(matches!(err, BackendError::NotSupported(_)));

    let exclude = SourceEndpoint::ProcessLoopback {
        pid: std::process::id(),
        exe_name: FixedStr::from_str_trunc("x"),
        mode: ProcessLoopbackMode::ExcludeTree,
    };
    let (err, port) = backend
        .open_input(&exclude, 2, port)
        .err()
        .expect("exclude is Windows-only");
    assert!(matches!(err, BackendError::NotSupported(_)));

    let nobody = SourceEndpoint::ProcessLoopback {
        pid: 4_000_000,
        exe_name: FixedStr::default(),
        mode: ProcessLoopbackMode::IncludeTree,
    };
    let (err, port) = backend
        .open_input(&nobody, 2, port)
        .err()
        .expect("no such app");
    assert!(matches!(err, BackendError::NoSuchDevice(_)));

    let (err, port) = backend
        .open_input(&SourceEndpoint::VirtualInput { index: 99 }, 2, port)
        .err()
        .expect("only a few virtual inputs");
    assert!(matches!(err, BackendError::NoSuchDevice(_)));
    rig.ports.release_input(port);
    assert_eq!(rig.ports.free_inputs(), 16, "no port may be lost");

    let out = rig.output();
    let (err, out) = backend
        .open_output(&hw_out("no-such-device"), 2, out)
        .err()
        .expect("unknown sink");
    assert!(matches!(err, BackendError::NoSuchDevice(_)));
    rig.ports.release_output(out);
    assert_eq!(rig.ports.free_outputs(), 16);
}

/// The headline test: a tone goes through the engine, out to a PipeWire sink, back in through
/// that sink's monitor, through the engine again, and arrives intact.
#[test]
fn a_tone_survives_playback_to_a_sink_and_capture_from_its_monitor() {
    let Some(mut backend) = backend() else { return };
    let Some(card) = FakeCard::new("loop") else {
        return;
    };
    wait_for("the fake card", 5, || {
        backend.devices().iter().any(|d| d.id == card.sink)
    });
    let mut rig = Rig::new();

    // tone -> strip 0 -> bus 0 -> PipeWire playback on the fake sink
    let tone_port = rig.input();
    let tone_slot = tone_port.slot();
    let play_port = rig.output();
    let play_slot = play_port.slot();
    rig.strip_to_bus(0, tone_slot, 2, 0);
    rig.attach_output(0, play_slot, 2);
    let tone = ToneSource::start(tone_port, 2, 1000.0, 0.5);
    let playback = backend
        .open_output(&hw_out(&card.sink), 2, play_port)
        .map_err(|(e, _)| e)
        .expect("open the sink");

    // The sink's monitor -> strip 1 -> bus 1 -> a tap we read here.
    let cap_port = rig.input();
    let cap_slot = cap_port.slot();
    let tap_port = rig.output();
    let tap_slot = tap_port.slot();
    rig.strip_to_bus(1, cap_slot, 2, 1);
    rig.attach_output(1, tap_slot, 2);
    let capture = backend
        .open_input(&loopback(&card.sink), 2, cap_port)
        .map_err(|(e, _)| e)
        .expect("open the monitor");
    let mut tap = tap_port;

    // Let the chain fill and settle, then listen for a second.
    std::thread::sleep(Duration::from_millis(1200));
    let _ = record_left(&mut tap, 2, 50);
    let heard = record_left(&mut tap, 2, 1000);
    let (level, freq) = (rms(&heard), freq_hz(&heard));
    println!(
        "heard {} frames, rms {level:.3}, {freq:.0} Hz; playback {:?}, capture {:?}",
        heard.len(),
        playback.stats(),
        capture.stats()
    );
    assert!(heard.len() > 40_000, "only {} frames in 1 s", heard.len());
    // A 0.5 sine has an RMS of 0.354.
    assert!((0.30..0.40).contains(&level), "rms {level}");
    assert!((freq - 1000.0).abs() < 30.0, "{freq} Hz");
    assert!(playback.stats().frames > 40_000);
    assert!(capture.stats().frames > 40_000);

    // The ports come back and can be reused.
    give_back_output(&mut rig, playback.close());
    give_back_input(&mut rig, capture.close());
    rig.ports.release_input(tone.stop());
    rig.ports.release_output(tap);
}

#[test]
fn virtual_nodes_exist_while_open_and_cannot_be_opened_twice() {
    let Some(mut backend) = backend() else { return };
    let mut rig = Rig::new();

    let mic_out = rig.output();
    let mic = backend
        .open_output(&SinkEndpoint::VirtualMic { index: 0 }, 2, mic_out)
        .map_err(|(e, _)| e)
        .expect("virtual mic");
    let sink_in = rig.input();
    let sink = backend
        .open_input(&SourceEndpoint::VirtualInput { index: 1 }, 2, sink_in)
        .map_err(|(e, _)| e)
        .expect("virtual input");

    wait_for("our nodes in the registry", 5, || {
        let g = backend.graph();
        g.nodes
            .values()
            .any(|n| n.name == "retae-virtual-mic-0" && n.media_class == "Audio/Source")
            && g.nodes
                .values()
                .any(|n| n.name == "retae-virtual-in-1" && n.media_class == "Audio/Sink")
    });
    // Our own nodes are never offered as devices to open again.
    assert!(
        backend
            .devices()
            .iter()
            .all(|d| !d.id.starts_with("retae-")),
        "{:?}",
        backend.devices()
    );

    // The same virtual input twice is Busy, and the refused port comes back.
    let again = rig.input();
    let (err, again) = backend
        .open_input(&SourceEndpoint::VirtualInput { index: 1 }, 2, again)
        .err()
        .expect("busy");
    assert!(matches!(err, BackendError::Busy(_)), "{err:?}");
    rig.ports.release_input(again);

    give_back_output(&mut rig, mic.close());
    give_back_input(&mut rig, sink.close());
    wait_for("our nodes to go away", 5, || {
        !backend
            .graph()
            .nodes
            .values()
            .any(|n| n.name.starts_with("retae-virtual"))
    });

    // After closing, the name is free again.
    let port = rig.input();
    let reopened = backend
        .open_input(&SourceEndpoint::VirtualInput { index: 1 }, 2, port)
        .map_err(|(e, _)| e)
        .expect("reopen");
    give_back_input(&mut rig, reopened.close());
}

/// An independent client (`pw-cat`) plays a tone into our virtual input, and records from our
/// virtual mic: the two ends other programs would use.
#[test]
fn other_programs_can_play_into_the_virtual_input_and_record_from_the_virtual_mic() {
    let Some(mut backend) = backend() else { return };
    if !tool_exists("pw-cat") {
        println!("SKIP: pw-cat is not installed");
        return;
    }
    let mut rig = Rig::new();

    // virtual input 0 -> strip 0 -> bus 0 -> virtual mic 0
    let in_port = rig.input();
    let in_slot = in_port.slot();
    let out_port = rig.output();
    let out_slot = out_port.slot();
    rig.strip_to_bus(0, in_slot, 2, 0);
    rig.attach_output(0, out_slot, 2);
    let vin = backend
        .open_input(&SourceEndpoint::VirtualInput { index: 0 }, 2, in_port)
        .map_err(|(e, _)| e)
        .expect("virtual input");
    let vmic = backend
        .open_output(&SinkEndpoint::VirtualMic { index: 0 }, 2, out_port)
        .map_err(|(e, _)| e)
        .expect("virtual mic");
    wait_for("both virtual nodes", 5, || {
        backend
            .graph()
            .nodes
            .values()
            .filter(|n| n.name.starts_with("retae-virtual"))
            .count()
            == 2
    });

    let mut player = PwCat::play("retae-virtual-in-0", 1000.0, 0.5, 20);
    std::thread::sleep(Duration::from_millis(1500));
    let mut recorder = PwCat::record("retae-virtual-mic-0");
    std::thread::sleep(Duration::from_millis(2500));
    let heard = recorder.finish_recording();
    player.kill();
    // Skip the start-up of the recorder, then judge a steady second.
    let steady = heard.get(24_000..).unwrap_or_default();
    let (level, freq) = (rms(steady), freq_hz(steady));
    println!(
        "recorded {} frames, rms {level:.3}, {freq:.0} Hz",
        heard.len()
    );
    assert!(heard.len() >= 48_000, "only {} frames", heard.len());
    assert!((0.28..0.42).contains(&level), "rms {level}");
    assert!((freq - 1000.0).abs() < 40.0, "{freq} Hz");

    give_back_input(&mut rig, vin.close());
    give_back_output(&mut rig, vmic.close());
}

/// Per-application capture: another process plays, we find it in `apps()` and record only it.
#[test]
fn one_application_can_be_captured_on_its_own() {
    let Some(mut backend) = backend() else { return };
    if !tool_exists("pw-cat") {
        println!("SKIP: pw-cat is not installed");
        return;
    }
    let Some(card) = FakeCard::new("app") else {
        return;
    };
    wait_for("the fake card", 5, || {
        backend.devices().iter().any(|d| d.id == card.sink)
    });
    let mut rig = Rig::new();

    // The "application" plays 440 Hz to the fake sink; a second one plays 2 kHz. Only the
    // first must be heard.
    let mut wanted = PwCat::play(&card.sink, 440.0, 0.5, 30);
    let mut other = PwCat::play(&card.sink, 2000.0, 0.5, 30);
    let wanted_pid = wanted.pid();
    let other_pid = other.pid();
    wait_for("both applications to appear", 8, || {
        let apps = backend.apps();
        [wanted_pid, other_pid]
            .iter()
            .all(|p| apps.iter().any(|a| a.pid == *p))
    });

    let cap_port = rig.input();
    let cap_slot = cap_port.slot();
    let tap_port = rig.output();
    let tap_slot = tap_port.slot();
    rig.strip_to_bus(0, cap_slot, 2, 0);
    rig.attach_output(0, tap_slot, 2);
    let source = SourceEndpoint::ProcessLoopback {
        pid: wanted_pid,
        exe_name: FixedStr::from_str_trunc("pw-cat"),
        mode: ProcessLoopbackMode::IncludeTree,
    };
    let capture = backend
        .open_input(&source, 2, cap_port)
        .map_err(|(e, _)| e)
        .expect("open the application");
    let mut tap = tap_port;

    std::thread::sleep(Duration::from_millis(1500));
    let _ = record_left(&mut tap, 2, 50);
    let heard = record_left(&mut tap, 2, 1000);
    let (level, freq) = (rms(&heard), freq_hz(&heard));
    println!(
        "app capture: {} frames, rms {level:.3}, {freq:.0} Hz",
        heard.len()
    );
    assert!(heard.len() > 40_000, "{} frames", heard.len());
    assert!((0.28..0.42).contains(&level), "rms {level}");
    assert!(
        (freq - 440.0).abs() < 25.0,
        "heard {freq} Hz: the other application leaked in or the wrong one was captured"
    );

    wanted.kill();
    other.kill();
    give_back_input(&mut rig, capture.close());
    rig.ports.release_output(tap);
}

/// A device that is closed and reopened works again (nothing is left half-open).
#[test]
fn a_stream_can_be_closed_and_reopened_many_times() {
    let Some(mut backend) = backend() else { return };
    let Some(card) = FakeCard::new("reopen") else {
        return;
    };
    wait_for("the fake card", 5, || {
        backend.devices().iter().any(|d| d.id == card.sink)
    });
    let mut rig = Rig::new();
    let mut port = rig.input();
    for round in 0..5 {
        // ? The sink's monitor stands in for a microphone: the fake source node is not a real
        // ? device and never leaves its idle state.
        let stream = backend
            .open_input(&loopback(&card.sink), 2, port)
            .map_err(|(e, _)| e)
            .unwrap_or_else(|e| panic!("round {round}: {e}"));
        wait_for(&format!("round {round} to deliver audio"), 5, || {
            stream.stats().frames > 0
        });
        port = match stream.close() {
            Some(ClosedPort::Input(p)) => p,
            other => panic!("round {round}: {other:?}"),
        };
    }
    rig.ports.release_input(port);
    assert_eq!(rig.ports.free_inputs(), 16);
}
// !SECTION: Tests

// SECTION: pw-cat helper
/// `pw-cat` as a stand-in for an application (play) or a recording program (record).
struct PwCat {
    child: Child,
    /// The WAV file this process plays or records; removed when the helper is dropped.
    file: std::path::PathBuf,
}

impl PwCat {
    // ? This `pw-cat` (PipeWire 1.0) reads and writes through libsndfile, so it speaks WAV
    // ? files, not a raw pipe. Playing a generated WAV and recording to a WAV is the portable
    // ? way to use it as an independent "other program".
    fn temp_wav(tag: &str) -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, Relaxed);
        std::env::temp_dir().join(format!("retae-live-{}-{tag}-{n}.wav", std::process::id()))
    }

    /// Play `secs` seconds of a stereo sine to `target`.
    fn play(target: &str, freq: f32, amp: f32, secs: u32) -> Self {
        let file = Self::temp_wav("play");
        let frames = 48_000 * secs as usize;
        let data_len = (frames * 8) as u32;
        let mut wav = Vec::with_capacity(44 + frames * 8);
        // Canonical 44-byte header, IEEE float (format 3), 2 channels, 48 kHz.
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&3u16.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&48_000u32.to_le_bytes());
        wav.extend_from_slice(&(48_000u32 * 8).to_le_bytes());
        wav.extend_from_slice(&8u16.to_le_bytes());
        wav.extend_from_slice(&32u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..frames {
            let t = (i as f64 * f64::from(freq) / 48_000.0).fract();
            let v = (t * std::f64::consts::TAU).sin() as f32 * amp;
            wav.extend_from_slice(&v.to_le_bytes());
            wav.extend_from_slice(&v.to_le_bytes());
        }
        std::fs::write(&file, wav).expect("write the tone file");
        let child = Command::new("pw-cat")
            .args(["-p", "--target", target, "--latency", "128"])
            .arg(&file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("pw-cat");
        Self { child, file }
    }

    /// Record from `target` until [`Self::finish_recording`].
    fn record(target: &str) -> Self {
        let file = Self::temp_wav("rec");
        let child = Command::new("pw-cat")
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

    /// Stop recording the way a user would (Ctrl-C) so the file is closed properly, then return
    /// the left channel.
    fn finish_recording(&mut self) -> Vec<f32> {
        let _ = Command::new("kill")
            .args(["-INT", &self.child.id().to_string()])
            .status();
        let _ = self.child.wait();
        let bytes = std::fs::read(&self.file).unwrap_or_default();
        // Find the "data" chunk instead of assuming a header size.
        let Some(at) = bytes.windows(4).position(|w| w == b"data") else {
            return Vec::new();
        };
        bytes[(at + 8).min(bytes.len())..]
            .chunks_exact(8)
            .map(|f| f32::from_le_bytes([f[0], f[1], f[2], f[3]]))
            .collect()
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for PwCat {
    fn drop(&mut self) {
        self.kill();
        let _ = std::fs::remove_file(&self.file);
    }
}
// !SECTION: pw-cat helper
