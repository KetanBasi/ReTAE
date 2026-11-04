//! Whole daemons, in-process, talking to each other over loopback and to us over IPC.
//!
//! Installs the allocation checker like the real binary: any heap use inside `process_block`
//! aborts the test run.

use app_core::ControlApi;
use app_core::ipc::RemoteControl;
use app_core::types::{Command, LinkState, ProbeRequest};
use audio_types::{
    FixedStr, LanRxConfig, LanTxConfig, NetworkBufferPreset, RouteSend, SinkEndpoint,
    SourceEndpoint,
};
use platform_io::Signal;
use routerd::{Daemon, DaemonError, Options};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[cfg(debug_assertions)]
#[global_allocator]
static ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

// SECTION: Helpers
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("retae-routerd-{tag}-{}", std::process::id()));
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

fn options(tag: &str, dir: &TempDir) -> Options {
    Options {
        name: format!("daemon-{tag}"),
        rx_port: 0,
        instance: Some(format!("{tag}-{}", std::process::id())),
        config_dir: dir.0.clone(),
        ..Options::default()
    }
}

fn wait_until(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn send_all(strip: u8, bus: u8) -> Command {
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

fn mic(id: &str) -> SourceEndpoint {
    SourceEndpoint::HardwareInput {
        device_id: FixedStr::from_str_trunc(id),
    }
}

fn speakers(id: &str) -> SinkEndpoint {
    SinkEndpoint::HardwareOutput {
        device_id: FixedStr::from_str_trunc(id),
    }
}

/// `this` sends its microphone (strip `s`, bus `b`) to `peer` as stream `name`.
fn send_mic_to(this: &RemoteControl, s: u8, b: u8, peer_port: u16, name: &str) {
    for c in [
        send_all(s, b),
        Command::StripSource {
            strip: s,
            source: mic("mock-in-1"),
            channels: 2,
        },
        Command::BusSink {
            bus: b,
            sink: SinkEndpoint::LanOut(LanTxConfig {
                peer_ipv4: [127, 0, 0, 1],
                port: peer_port,
                stream_name: FixedStr::from_str_trunc(name),
                ..LanTxConfig::default()
            }),
            channels: 2,
        },
    ] {
        this.command(c)
            .unwrap_or_else(|e| panic!("send setup: {e}"));
    }
}

/// `this` plays stream `name` from 127.0.0.1 on its speakers (strip `s`, bus `b`).
fn play_from_lan(this: &RemoteControl, s: u8, b: u8, name: &str) {
    for c in [
        send_all(s, b),
        Command::BusSink {
            bus: b,
            sink: speakers("mock-out-1"),
            channels: 2,
        },
        Command::StripSource {
            strip: s,
            source: SourceEndpoint::LanIn(LanRxConfig {
                source_ipv4: [127, 0, 0, 1],
                stream_name: FixedStr::from_str_trunc(name),
                buffer: NetworkBufferPreset::Fast,
                adaptive: false,
            }),
            channels: 2,
        },
    ] {
        this.command(c)
            .unwrap_or_else(|e| panic!("play setup: {e}"));
    }
}
// !SECTION: Helpers

// SECTION: Single instance
#[test]
fn a_second_daemon_with_the_same_ipc_name_is_refused() {
    let dir = TempDir::new("single");
    let first = Daemon::start(options("single", &dir)).unwrap();
    match Daemon::start(options("single", &dir)) {
        Err(DaemonError::AlreadyRunning(name)) => assert_eq!(name, first.ipc_name()),
        other => panic!("expected AlreadyRunning, got {:?}", other.err()),
    }
    // The first one is unharmed.
    let remote = RemoteControl::connect(first.ipc_name()).unwrap();
    assert_eq!(remote.snapshot().unwrap().backend, "mock");

    // Once it is gone the name is free again.
    let name = first.ipc_name().to_owned();
    drop(remote);
    drop(first);
    let again = Daemon::start(options("single", &dir)).unwrap();
    assert_eq!(again.ipc_name(), name);
}

#[test]
fn a_restart_with_a_profile_restores_the_setup() {
    let dir = TempDir::new("restart");
    {
        let d = Daemon::start(options("restart", &dir)).unwrap();
        let r = RemoteControl::connect(d.ipc_name()).unwrap();
        r.command(Command::StripGain {
            strip: 5,
            gain_db: -11.0,
        })
        .unwrap();
        r.command(Command::StripLabel {
            strip: 5,
            label: "Synth".into(),
        })
        .unwrap();
        r.command(Command::BusSink {
            bus: 2,
            sink: speakers("mock-out-2"),
            channels: 2,
        })
        .unwrap();
        r.command(Command::SaveProfile {
            name: "evening".into(),
        })
        .unwrap();
    }
    let d = Daemon::start(Options {
        profile: Some("evening".into()),
        ..options("restart", &dir)
    })
    .unwrap();
    let s = RemoteControl::connect(d.ipc_name())
        .unwrap()
        .snapshot()
        .unwrap();
    assert_eq!(s.strips[5].config.gain_db, -11.0);
    assert_eq!(s.strips[5].config.label.as_str(), "Synth");
    assert_eq!(s.buses[2].link, LinkState::Running);
    assert!(d.mock().unwrap().is_open("mock-out-2"));

    // A profile that does not exist stops the start instead of running with defaults.
    let missing = Daemon::start(Options {
        profile: Some("nope".into()),
        instance: Some(format!("restart-b-{}", std::process::id())),
        ..options("restart", &dir)
    });
    assert!(matches!(missing, Err(DaemonError::Start(_))));
}
// !SECTION: Single instance

// SECTION: LAN control
#[test]
fn a_paired_device_drives_a_daemon_over_the_encrypted_lan_link() {
    use app_core::lan::{self, Identity};
    let dir = TempDir::new("lan");
    // Off by default: nothing listens, and --pair without it is a mistake, not a silent no-op.
    let off = Daemon::start(options("lan-off", &dir)).unwrap();
    assert!(off.lan_addr().is_none() && off.pairing_code().is_none());
    assert!(off.open_pairing().is_none());
    drop(off);
    let bad = Daemon::start(Options {
        pair: true,
        ..options("lan-bad", &dir)
    });
    assert!(matches!(bad, Err(DaemonError::Start(_))));

    let daemon = Daemon::start(Options {
        lan_control: Some(0),
        pair: true,
        ..options("lan", &dir)
    })
    .unwrap();
    let port = daemon.lan_addr().unwrap().port();
    let addr = (Ipv4Addr::LOCALHOST, port);
    let code = daemon
        .pairing_code()
        .expect("--pair opens a window")
        .clone();

    let me_dir = TempDir::new("lan-me");
    let me = Identity::load_or_create(&me_dir.0.join("id.toml")).unwrap();
    let found = lan::pair(addr, &me, "phone", &code).unwrap();
    assert_eq!(found.name, "daemon-lan");

    let remote = lan::connect(addr, &me, "phone", &found.key).unwrap();
    remote
        .command(Command::StripGain {
            strip: 2,
            gain_db: -9.0,
        })
        .unwrap();
    let snap = remote.snapshot().unwrap();
    assert_eq!(snap.strips[2].config.gain_db, -9.0);
    assert_eq!(snap.backend, "mock");
    // The very same mixer answers on the local socket.
    let local = RemoteControl::connect(daemon.ipc_name()).unwrap();
    assert_eq!(local.snapshot().unwrap().strips[2].config.gain_db, -9.0);

    // The pairing survives a restart: the daemon's peer book and key are on disk.
    drop(remote);
    drop(daemon);
    let again = Daemon::start(Options {
        lan_control: Some(0),
        ..options("lan", &dir)
    })
    .unwrap();
    assert!(again.pairing_code().is_none(), "no window unless asked for");
    let addr = (Ipv4Addr::LOCALHOST, again.lan_addr().unwrap().port());
    let remote = lan::connect(addr, &me, "phone", &found.key).unwrap();
    assert_eq!(remote.snapshot().unwrap().strips[2].config.gain_db, 0.0);
    // ...and a fresh window can pair a second device.
    let second = Identity::generate().unwrap();
    let code = again.open_pairing().unwrap().unwrap();
    lan::pair(addr, &second, "tablet", &code).unwrap();
}
// !SECTION: LAN control

// SECTION: Two daemons
/// Strict on a quiet host; on a noisy one (1 ms sleeps waking 3 to 30 ms late) a few lost or
/// late packets and underruns are expected and are only bounded.
const QUIET_HOST_US: u32 = 6_000;

#[test]
fn two_daemons_stream_to_each_other_in_both_directions() {
    let (dir_a, dir_b) = (TempDir::new("pair-a"), TempDir::new("pair-b"));
    let a = Daemon::start(options("pair-a", &dir_a)).unwrap();
    let b = Daemon::start(options("pair-b", &dir_b)).unwrap();
    let (ra, rb) = (
        RemoteControl::connect(a.ipc_name()).unwrap(),
        RemoteControl::connect(b.ipc_name()).unwrap(),
    );
    let (port_a, port_b) = (a.control().rx_port(), b.control().rx_port());
    assert_ne!(port_a, port_b);

    let tone = Signal::Sine {
        freq_hz: 1000.0,
        amp: 0.5,
    };
    a.mock().unwrap().set_signal("mock-in-1", tone);
    b.mock().unwrap().set_signal("mock-in-1", tone);

    // A -> B on stream "AtoB" (strip 0 / bus 0), B -> A on "BtoA" (strip 1 / bus 1).
    play_from_lan(&rb, 0, 0, "AtoB");
    play_from_lan(&ra, 1, 1, "BtoA");
    send_mic_to(&ra, 0, 0, port_b, "AtoB");
    send_mic_to(&rb, 1, 1, port_a, "BtoA");

    for (daemon, who) in [(&a, "A"), (&b, "B")] {
        wait_until(
            &format!("{who} to hear the other daemon"),
            Duration::from_secs(10),
            || daemon.mock().unwrap().capture("mock-out-1").peak > 0.45,
        );
    }

    // Settle, then measure a clean second.
    std::thread::sleep(Duration::from_millis(500));
    a.mock().unwrap().reset_capture("mock-out-1");
    b.mock().unwrap().reset_capture("mock-out-1");
    std::thread::sleep(Duration::from_millis(1500));

    let noise = a
        .master()
        .stats()
        .max_gap_us
        .load(std::sync::atomic::Ordering::Relaxed)
        .max(
            b.master()
                .stats()
                .max_gap_us
                .load(std::sync::atomic::Ordering::Relaxed),
        );
    let quiet = noise < QUIET_HOST_US;
    if !quiet {
        println!(
            "NOTE: noisy host (a mixer clock woke {:.1} ms late): tolerances widened",
            noise as f32 / 1000.0
        );
    }

    for (daemon, who, strip, out) in [(&a, "A", 1usize, "mock-out-1"), (&b, "B", 0, "mock-out-1")] {
        let cap = daemon.mock().unwrap().capture(out);
        // A 0.5 sine has an RMS of 0.354; allow for the odd concealed block on a noisy host.
        let (lo, hi) = if quiet { (0.33, 0.37) } else { (0.25, 0.37) };
        assert!(
            (lo..hi).contains(&cap.rms()),
            "{who} heard RMS {} (peak {}, {} frames)",
            cap.rms(),
            cap.peak,
            cap.frames
        );
        assert!(
            cap.frames > 48_000,
            "{who} got only {} frames in 1.5 s",
            cap.frames
        );

        let ctl = RemoteControl::connect(daemon.ipc_name()).unwrap();
        let m = ctl.meters().unwrap();
        let lan = m.lan_rx[strip];
        let bound = if quiet { 0 } else { 20 };
        assert!(
            lan.packets_lost <= bound,
            "{who} lost {} packets",
            lan.packets_lost
        );
        assert!(
            lan.packets_late <= bound,
            "{who} had {} late packets",
            lan.packets_late
        );
        assert!(
            lan.underruns <= bound,
            "{who} underran {} times",
            lan.underruns
        );
        assert!(
            lan.buffer_ms > 0.0 && lan.target_ms >= 12.0,
            "{who}: {lan:?}"
        );
        assert_eq!(m.engine.rejected_commands, 0, "{who}");
        assert!(m.any_audio_active);
        // The mock output drains what the mixer writes, so no block may be dropped.
        assert!(m.engine.output_overruns <= bound, "{who}: {:?}", m.engine);

        let s = ctl.snapshot().unwrap();
        let sent = s.buses[usize::from(strip == 0)].tx.expect("sender stats");
        assert!(sent.packets > 300, "{who} sent {} packets", sent.packets);
        assert_eq!(sent.send_errors, 0);
    }
}

#[test]
fn the_probe_reaches_the_other_daemon_and_measures_the_local_pipeline() {
    let (dir_a, dir_b) = (TempDir::new("probe-a"), TempDir::new("probe-b"));
    let a = Daemon::start(options("probe-a", &dir_a)).unwrap();
    let b = Daemon::start(options("probe-b", &dir_b)).unwrap();
    let ra = RemoteControl::connect(a.ipc_name()).unwrap();

    let report = ra
        .probe(ProbeRequest {
            peer: Some(SocketAddrV4::new(
                Ipv4Addr::LOCALHOST,
                b.control().rx_port(),
            )),
            pings: 4,
            click_loop: true,
            clicks: 10,
        })
        .unwrap();
    println!("{report:#?}");
    let rtt = report.rtt.expect("rtt");
    assert!(
        rtt.received >= 3,
        "{} of {} pings answered",
        rtt.received,
        rtt.sent
    );
    assert!(rtt.p50_ms < 50.0);
    let c = report.click_loop.expect("click loop");
    assert!(
        c.clicks_found >= 8,
        "{} of {} clicks",
        c.clicks_found,
        c.clicks_sent
    );
    if c.host_noise_ms < 4.0 {
        assert!(
            c.within_one_block,
            "p50 {:.2} ms vs {:.2} ms expected",
            c.p50_ms, c.expected_ms
        );
    } else {
        println!(
            "NOTE: noisy host ({:.1} ms): click-loop only sanity-checked",
            c.host_noise_ms
        );
        assert!(c.p50_ms > 2.0 && c.p50_ms < c.expected_ms + 40.0);
    }
}
// !SECTION: Two daemons
