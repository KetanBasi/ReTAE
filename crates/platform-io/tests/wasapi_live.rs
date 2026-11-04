//! The WASAPI backend against the real Windows audio stack.
//!
//! Run on a Windows machine with: `cargo test -p platform-io --test wasapi_live -- --nocapture`
//!
//! These tests need a sound device (a laptop or desktop; a build server has none, and the tests
//! then print `SKIP:` and pass). One of them plays a **quiet 1 kHz tone for about 3 seconds**
//! on the default output. Nothing is left behind: every stream is closed.
//!
//! The first test only prints what the backend sees: read it, it is the quickest way to find
//! out what the machine offers (build number, cables, exclusive-mode formats, apps).
//!
//! What they prove is the stream plumbing and our handling of it. Latency from the speaker to
//! your ear is not measurable here (PLAN §12).

#![cfg(windows)]

use audio_engine::{ControlPorts, InputPort, OutputPort, create_engine};
use audio_types::{
    BusId, ClockDomain, EngineCommand, FixedStr, PortSlot, RouteSend, SinkEndpoint, SourceEndpoint,
    StripId,
};
use platform_io::{
    AudioBackend, BackendError, ClosedPort, DeviceKind, RateClock, TimerMaster, WasapiBackend,
    WasapiOptions,
};
use rtrb::Producer;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

// SECTION: Fixtures
fn backend(options: WasapiOptions) -> Option<WasapiBackend> {
    match WasapiBackend::connect(options) {
        Ok(b) => Some(b),
        Err(e) => {
            println!("SKIP: cannot reach Windows audio ({e})");
            None
        }
    }
}

fn has_default_output(b: &WasapiBackend) -> bool {
    let yes = b
        .devices()
        .iter()
        .any(|d| d.kind == DeviceKind::Output && d.is_default);
    if !yes {
        println!("SKIP: no default output device");
    }
    yes
}

fn hw_out(id: &str) -> SinkEndpoint {
    SinkEndpoint::HardwareOutput {
        device_id: FixedStr::from_str_trunc(id),
    }
}
fn loopback(id: &str) -> SourceEndpoint {
    SourceEndpoint::DeviceLoopback {
        device_id: FixedStr::from_str_trunc(id),
    }
}

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
                buf.clear();
                for _ in 0..clock.due() {
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

/// Pop everything a tap port gathers for `ms` milliseconds, as left-channel samples.
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
// !SECTION: Fixtures

/// Not a pass/fail check: prints what this machine offers. Always read this first.
#[test]
fn report_what_the_machine_offers() {
    let Some(b) = backend(WasapiOptions::default()) else {
        return;
    };
    let caps = b.capabilities();
    println!("Windows build {}", b.os_build());
    println!("capabilities: {caps:?}");
    for d in b.devices() {
        println!(
            "  {:?} {}{} ({} ch)  id {}",
            d.kind,
            d.name,
            if d.is_default { " [default]" } else { "" },
            d.channels,
            d.id
        );
    }
    println!(
        "virtual inputs {}, virtual mics {} (cables found)",
        caps.virtual_inputs, caps.virtual_mics
    );
    for a in b.apps() {
        println!(
            "  app {} (pid {}){}",
            a.name,
            a.pid,
            if a.playing { " playing" } else { "" }
        );
    }
}

#[test]
fn failed_opens_hand_the_port_back_and_say_why() {
    let Some(mut b) = backend(WasapiOptions::default()) else {
        return;
    };
    let mut rig = Rig::new();
    let free_in = rig.ports.free_inputs();
    let free_out = rig.ports.free_outputs();

    let port = rig.ports.acquire_output().unwrap();
    let (err, port) = b
        .open_output(&hw_out("{0.0.0.00000000}.{no-such-endpoint}"), 2, port)
        .err()
        .expect("an unknown id must fail");
    assert!(
        matches!(err, BackendError::NoSuchDevice(_) | BackendError::Io(_)),
        "{err:?}"
    );
    let (err, port) = b
        .open_output(&hw_out("default"), 9, port)
        .err()
        .expect("9 channels is refused");
    assert_eq!(err, BackendError::BadChannels(9));
    rig.ports.release_output(port);

    let port = rig.ports.acquire_input().unwrap();
    let (err, port) = b
        .open_input(
            &SourceEndpoint::AndroidMic {
                preset: audio_types::AndroidInputPreset::Unprocessed,
            },
            2,
            port,
        )
        .err()
        .expect("not a Windows source");
    assert!(matches!(err, BackendError::NotSupported(_)), "{err:?}");
    let (err, port) = b
        .open_input(&SourceEndpoint::VirtualInput { index: 200 }, 2, port)
        .err()
        .expect("no such cable");
    assert!(matches!(err, BackendError::NoSuchDevice(_)), "{err:?}");
    rig.ports.release_input(port);

    assert_eq!(rig.ports.free_inputs(), free_in, "no port may be lost");
    assert_eq!(rig.ports.free_outputs(), free_out, "no port may be lost");
}

/// The headline test: a quiet tone goes through the engine, out to the default speakers, back in
/// through device loopback, through the engine again, and arrives intact.
#[test]
fn a_tone_survives_playback_and_device_loopback() {
    let Some(mut b) = backend(WasapiOptions::default()) else {
        return;
    };
    if !has_default_output(&b) {
        return;
    }
    let mut rig = Rig::new();

    // tone -> strip 0 -> bus 0 -> default output
    let tone_port = rig.ports.acquire_input().unwrap();
    let tone_slot = tone_port.slot();
    let play_port = rig.ports.acquire_output().unwrap();
    let play_slot = play_port.slot();
    rig.strip_to_bus(0, tone_slot, 2, 0);
    rig.attach_output(0, play_slot, 2);
    let tone = ToneSource::start(tone_port, 2, 1000.0, 0.15);
    let playback = b
        .open_output(&hw_out("default"), 2, play_port)
        .map_err(|(e, _)| e)
        .expect("open the default output");
    println!("playback: {}", playback.detail());

    // default output's loopback -> strip 1 -> bus 1 -> a tap we read here
    let cap_port = rig.ports.acquire_input().unwrap();
    let cap_slot = cap_port.slot();
    let tap_port = rig.ports.acquire_output().unwrap();
    let tap_slot = tap_port.slot();
    rig.strip_to_bus(1, cap_slot, 2, 1);
    rig.attach_output(1, tap_slot, 2);
    let capture = b
        .open_input(&loopback("default"), 2, cap_port)
        .map_err(|(e, _)| e)
        .expect("open loopback");
    println!("loopback: {}", capture.detail());
    let mut tap = tap_port;

    std::thread::sleep(Duration::from_millis(1500));
    let _ = record_left(&mut tap, 2, 50);
    let heard = record_left(&mut tap, 2, 1000);
    let (level, freq) = (rms(&heard), freq_hz(&heard));
    println!(
        "heard {} frames, rms {level:.3}, {freq:.0} Hz; playback {:?}, capture {:?}",
        heard.len(),
        playback.stats(),
        capture.stats()
    );
    println!("playback after: {}", playback.detail());
    println!("loopback after: {}", capture.detail());

    assert!(heard.len() > 40_000, "only {} frames in 1 s", heard.len());
    // A 0.15 sine has an RMS of 0.106. Allow a lot: system volume enhancements may change it.
    assert!((0.03..0.30).contains(&level), "rms {level}");
    assert!((freq - 1000.0).abs() < 40.0, "{freq} Hz");
    assert!(playback.stats().frames > 40_000);
    assert!(capture.stats().frames > 40_000);

    give_back_output(&mut rig, playback.close());
    give_back_input(&mut rig, capture.close());
    rig.ports.release_input(tone.stop());
    rig.ports.release_output(tap);
}

/// Same, but in exclusive mode (the device must accept 48 kHz). Skips when it does not.
#[test]
fn exclusive_mode_plays_when_the_device_allows_it() {
    let opts = WasapiOptions {
        exclusive: true,
        ..WasapiOptions::default()
    };
    let Some(mut b) = backend(opts) else { return };
    if !has_default_output(&b) {
        return;
    }
    let mut rig = Rig::new();
    let tone_port = rig.ports.acquire_input().unwrap();
    let tone_slot = tone_port.slot();
    let play_port = rig.ports.acquire_output().unwrap();
    let play_slot = play_port.slot();
    rig.strip_to_bus(0, tone_slot, 2, 0);
    rig.attach_output(0, play_slot, 2);
    let tone = ToneSource::start(tone_port, 2, 600.0, 0.1);

    match b.open_output(&hw_out("default"), 2, play_port) {
        Ok(s) => {
            println!("exclusive: {}", s.detail());
            std::thread::sleep(Duration::from_millis(1500));
            let st = s.stats();
            println!("exclusive stats: {st:?}; {}", s.detail());
            assert!(st.frames > 30_000, "{st:?}");
            give_back_output(&mut rig, s.close());
        }
        Err((e, port)) => {
            println!("SKIP: exclusive mode refused ({e})");
            rig.ports.release_output(port);
        }
    }
    rig.ports.release_input(tone.stop());
}

#[test]
fn a_stream_can_be_closed_and_reopened_many_times() {
    let Some(mut b) = backend(WasapiOptions::default()) else {
        return;
    };
    if !has_default_output(&b) {
        return;
    }
    let mut rig = Rig::new();
    let free = rig.ports.free_outputs();
    let mut port = rig.ports.acquire_output().unwrap();
    for round in 0..5 {
        let s = b
            .open_output(&hw_out("default"), 2, port)
            .map_err(|(e, _)| e)
            .unwrap_or_else(|e| panic!("round {round}: {e}"));
        std::thread::sleep(Duration::from_millis(100));
        port = match s.close() {
            Some(ClosedPort::Output(p)) => p,
            other => panic!("round {round}: {other:?}"),
        };
    }
    rig.ports.release_output(port);
    assert_eq!(rig.ports.free_outputs(), free);
}
