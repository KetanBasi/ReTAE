//! `LocalControl` against the mock backend and a real (timer-clocked) mixer.
//!
//! Timing-sensitive checks read the host's measured noise first. The dev container is a
//! microVM where 1 ms sleeps sometimes wake 3 to 30 ms late; there the checks relax and say so.

use app_core::ipc::{IpcServer, RemoteControl};
use app_core::types::{Command, ControlError, DiscoveryState, LinkState, ProbeRequest};
use app_core::{ControlApi, DaemonConfig, LocalControl};
use audio_engine::create_engine;
use audio_types::{
    FixedStr, LanRxConfig, LanTxConfig, NetworkBufferPreset, RouteSend, SinkEndpoint,
    SourceEndpoint,
};
use net_vban::service::{TextCommand, TextSender};
use platform_io::{MockBackend, MockControl, Signal, TimerMaster};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

// SECTION: Rig
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("retae-local-{tag}-{}", std::process::id()));
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

/// One daemon's worth of parts, in-process.
struct Rig {
    control: Arc<LocalControl>,
    mock: MockControl,
    master: Option<TimerMaster>,
    _dir: TempDir,
}

impl Rig {
    fn new(tag: &str) -> Self {
        Self::build(tag, true)
    }

    fn build(tag: &str, run_mixer: bool) -> Self {
        let dir = TempDir::new(tag);
        let (engine, handles) = create_engine();
        let (backend, mock) = MockBackend::new();
        let control = Arc::new(
            LocalControl::new(
                handles,
                Box::new(backend),
                DaemonConfig {
                    name: format!("test-{tag}"),
                    rx_port: 0,
                    discovery: false,
                    device_target_ms: 8.0,
                    config_dir: dir.0.clone(),
                },
            )
            .expect("LocalControl::new"),
        );
        let master = run_mixer.then(|| TimerMaster::spawn(engine, 0.0).unwrap());
        Self {
            control,
            mock,
            master,
            _dir: dir,
        }
    }

    fn cmd(&self, c: Command) {
        self.control
            .command(c)
            .unwrap_or_else(|e| panic!("command failed: {e}"));
    }

    fn mic(&self) -> SourceEndpoint {
        SourceEndpoint::HardwareInput {
            device_id: FixedStr::from_str_trunc("mock-in-1"),
        }
    }

    fn speakers(&self) -> SinkEndpoint {
        SinkEndpoint::HardwareOutput {
            device_id: FixedStr::from_str_trunc("mock-out-1"),
        }
    }

    /// strip 0 <- mock-in-1 (sine) --> bus 0 -> mock-out-1
    fn wire_sine_to_speakers(&self) {
        self.mock.set_signal(
            "mock-in-1",
            Signal::Sine {
                freq_hz: 1000.0,
                amp: 0.5,
            },
        );
        self.cmd(Command::BusSink {
            bus: 0,
            sink: self.speakers(),
            channels: 2,
        });
        self.cmd(Command::Route {
            strip: 0,
            bus: 0,
            send: RouteSend {
                enabled: true,
                gain_db: 0.0,
                pre_fader: false,
            },
        });
        self.cmd(Command::StripSource {
            strip: 0,
            source: self.mic(),
            channels: 2,
        });
    }

    fn rejected(&self) -> u32 {
        self.control.meters().unwrap().engine.rejected_commands
    }
}

fn wait_until(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}
// !SECTION: Rig

// SECTION: Commands and state
#[test]
fn commands_change_the_snapshot_and_the_engine_accepts_all_of_them() {
    let rig = Rig::new("cmds");
    rig.cmd(Command::StripGain {
        strip: 2,
        gain_db: -7.5,
    });
    rig.cmd(Command::StripPan { strip: 2, pan: 0.5 });
    rig.cmd(Command::StripMute {
        strip: 2,
        mute: true,
    });
    rig.cmd(Command::StripSolo {
        strip: 3,
        solo: true,
    });
    rig.cmd(Command::StripLabel {
        strip: 2,
        label: "Guitar".into(),
    });
    rig.cmd(Command::BusGain {
        bus: 1,
        gain_db: -3.0,
    });
    rig.cmd(Command::BusMute { bus: 1, mute: true });
    rig.cmd(Command::BusLabel {
        bus: 1,
        label: "Phones".into(),
    });
    rig.cmd(Command::Route {
        strip: 2,
        bus: 1,
        send: RouteSend {
            enabled: true,
            gain_db: -2.0,
            pre_fader: true,
        },
    });
    rig.cmd(Command::Beacon { on: true });

    let s = rig.control.snapshot().unwrap();
    assert_eq!(s.strips.len(), 16);
    assert_eq!(s.buses.len(), 16);
    let st = &s.strips[2].config;
    assert_eq!((st.gain_db, st.pan, st.mute), (-7.5, 0.5, true));
    assert_eq!(st.label.as_str(), "Guitar");
    assert!(s.strips[3].config.solo);
    assert_eq!(s.buses[1].config.label.as_str(), "Phones");
    assert!(s.buses[1].config.mute);
    assert!(st.sends[1].enabled && st.sends[1].pre_fader);
    assert!(s.beacon);
    assert_eq!(s.backend, "mock");
    assert!(s.rx_port != 0);

    // The mixer applies commands at its next block; give it a few.
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(
        rig.rejected(),
        0,
        "the engine refused a command the control plane accepted"
    );
}

#[test]
fn bad_requests_are_refused_before_they_reach_the_engine() {
    let rig = Rig::new("bad");
    let invalid = |c: Command| {
        assert!(
            matches!(
                rig.control.command(c.clone()),
                Err(ControlError::Invalid(_))
            ),
            "{c:?} should be Invalid"
        );
    };
    invalid(Command::StripGain {
        strip: 16,
        gain_db: 0.0,
    });
    invalid(Command::StripGain {
        strip: 0,
        gain_db: f32::NAN,
    });
    invalid(Command::StripPan {
        strip: 0,
        pan: f32::INFINITY,
    });
    invalid(Command::BusGain {
        bus: 99,
        gain_db: 0.0,
    });
    invalid(Command::Route {
        strip: 0,
        bus: 16,
        send: RouteSend::default(),
    });
    invalid(Command::StripSource {
        strip: 0,
        source: SourceEndpoint::Unassigned,
        channels: 0,
    });
    invalid(Command::StripSource {
        strip: 0,
        source: SourceEndpoint::Unassigned,
        channels: 9,
    });
    assert!(matches!(
        rig.control.command(Command::SaveProfile {
            name: "../escape".into()
        }),
        Err(ControlError::Profile(_))
    ));
    assert!(!std::path::Path::new("../escape.toml").exists());
    // Silly but finite values are clamped, not refused.
    rig.cmd(Command::StripGain {
        strip: 0,
        gain_db: 500.0,
    });
    assert_eq!(
        rig.control.snapshot().unwrap().strips[0].config.gain_db,
        24.0
    );
    std::thread::sleep(Duration::from_millis(40));
    assert_eq!(rig.rejected(), 0);
}
// !SECTION: Commands and state

// SECTION: Devices and ports
#[test]
fn audio_flows_from_a_mock_input_through_the_mixer_to_a_mock_output() {
    let rig = Rig::new("flow");
    rig.wire_sine_to_speakers();

    let snap = rig.control.snapshot().unwrap();
    assert_eq!(snap.strips[0].link, LinkState::Running);
    assert_eq!(snap.buses[0].link, LinkState::Running);
    assert!(rig.mock.is_open("mock-in-1") && rig.mock.is_open("mock-out-1"));

    // The fader ramps up from silence when a strip starts, so wait for the full level.
    wait_until(
        "a full-level signal at the speakers",
        Duration::from_secs(5),
        || rig.mock.capture("mock-out-1").peak > 0.45,
    );
    let cap = rig.mock.capture("mock-out-1");
    // A stereo source at centre pan passes unchanged: the 0.5 sine arrives at 0.5.
    assert!((0.45..0.55).contains(&cap.peak), "peak {}", cap.peak);

    let m = rig.control.meters().unwrap();
    assert!(
        m.strips[0].peak_db[0] > -15.0,
        "strip meter {:?}",
        m.strips[0].peak_db
    );
    assert!(m.buses[0].peak_db[0] > -15.0);
    assert!(m.any_audio_active);

    // Muting the strip silences the speakers.
    rig.cmd(Command::StripMute {
        strip: 0,
        mute: true,
    });
    std::thread::sleep(Duration::from_millis(150));
    rig.mock.reset_capture("mock-out-1");
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        rig.mock.capture("mock-out-1").peak < 0.01,
        "still audible after mute"
    );
}

#[test]
fn closing_an_endpoint_frees_the_device_and_the_ring() {
    let rig = Rig::new("close");
    rig.wire_sine_to_speakers();
    rig.cmd(Command::StripSource {
        strip: 0,
        source: SourceEndpoint::Unassigned,
        channels: 2,
    });
    rig.cmd(Command::BusSink {
        bus: 0,
        sink: SinkEndpoint::Unassigned,
        channels: 2,
    });
    assert!(!rig.mock.is_open("mock-in-1") && !rig.mock.is_open("mock-out-1"));
    let s = rig.control.snapshot().unwrap();
    assert_eq!(s.strips[0].link, LinkState::Idle);
    assert!(s.strips[0].stream.is_none());
}

#[test]
fn failed_opens_report_why_and_never_leak_a_ring() {
    let rig = Rig::new("leak");
    let missing = SourceEndpoint::HardwareInput {
        device_id: FixedStr::from_str_trunc("no-such-device"),
    };
    // The pool has 16 input rings: far more failures than that must all fail the same way.
    for _ in 0..40 {
        match rig.control.command(Command::StripSource {
            strip: 1,
            source: missing,
            channels: 2,
        }) {
            Err(ControlError::Backend(m)) => assert!(m.contains("no-such-device"), "{m}"),
            other => panic!("expected a backend error, got {other:?}"),
        }
    }
    let s = rig.control.snapshot().unwrap();
    assert!(matches!(&s.strips[1].link, LinkState::Failed(m) if m.contains("no-such-device")));
    // The failed request is kept, so a UI can show what the user asked for.
    assert_eq!(s.strips[1].config.source, missing);

    // Open and close one device far more often than there are rings.
    for _ in 0..40 {
        rig.cmd(Command::StripSource {
            strip: 1,
            source: rig.mic(),
            channels: 2,
        });
        rig.cmd(Command::StripSource {
            strip: 1,
            source: SourceEndpoint::Unassigned,
            channels: 2,
        });
    }
    // The pool is whole: a real device still opens, and a second strip cannot take the same one.
    rig.cmd(Command::StripSource {
        strip: 0,
        source: rig.mic(),
        channels: 2,
    });
    assert!(matches!(
        rig.control.command(Command::StripSource { strip: 5, source: rig.mic(), channels: 2 }),
        Err(ControlError::Backend(m)) if m.contains("busy")
    ));
    assert!(rig.mock.is_open("mock-in-1"));
}

#[test]
fn dropping_the_control_closes_every_device() {
    let rig = Rig::new("drop");
    rig.wire_sine_to_speakers();
    let mock = rig.mock.clone();
    let Rig {
        control, master, ..
    } = rig;
    drop(master); // stop the mixer first, as the daemon does
    drop(control);
    assert!(!mock.is_open("mock-in-1") && !mock.is_open("mock-out-1"));
}
// !SECTION: Devices and ports

// SECTION: Profiles
#[test]
fn a_saved_profile_restores_the_whole_setup() {
    let rig = Rig::new("profile");
    rig.wire_sine_to_speakers();
    rig.cmd(Command::StripGain {
        strip: 0,
        gain_db: -4.0,
    });
    rig.cmd(Command::StripLabel {
        strip: 0,
        label: "Mic".into(),
    });
    rig.cmd(Command::RxPolicy {
        accept_any: false,
        allowed: vec![[127, 0, 0, 1]],
        trust_discovered: false,
    });
    rig.cmd(Command::SaveProfile {
        name: "stage".into(),
    });
    let saved = rig.control.snapshot().unwrap();
    assert_eq!(saved.profile.as_deref(), Some("stage"));
    assert_eq!(saved.profiles, ["stage"]);

    // Tear everything down, then load.
    rig.cmd(Command::StripSource {
        strip: 0,
        source: SourceEndpoint::Unassigned,
        channels: 2,
    });
    rig.cmd(Command::BusSink {
        bus: 0,
        sink: SinkEndpoint::Unassigned,
        channels: 2,
    });
    rig.cmd(Command::StripGain {
        strip: 0,
        gain_db: 0.0,
    });
    rig.cmd(Command::RxPolicy {
        accept_any: true,
        allowed: vec![],
        trust_discovered: false,
    });
    assert!(!rig.mock.is_open("mock-in-1"));

    rig.cmd(Command::LoadProfile {
        name: "stage".into(),
    });
    let loaded = rig.control.snapshot().unwrap();
    assert_eq!(loaded.strips[0].config, saved.strips[0].config);
    assert_eq!(loaded.buses[0].config, saved.buses[0].config);
    assert_eq!(loaded.policy, saved.policy);
    assert_eq!(loaded.strips[0].link, LinkState::Running);
    assert!(rig.mock.is_open("mock-in-1") && rig.mock.is_open("mock-out-1"));
    wait_until("audio after the load", Duration::from_secs(5), || {
        rig.mock.capture("mock-out-1").peak > 0.1
    });
    std::thread::sleep(Duration::from_millis(40));
    assert_eq!(rig.rejected(), 0);

    assert!(matches!(
        rig.control.command(Command::LoadProfile {
            name: "nothing".into()
        }),
        Err(ControlError::Profile(_))
    ));
    rig.cmd(Command::DeleteProfile {
        name: "stage".into(),
    });
    assert!(rig.control.snapshot().unwrap().profiles.is_empty());
}

#[test]
fn a_profile_naming_a_missing_device_still_loads_the_rest() {
    let rig = Rig::new("partial");
    rig.wire_sine_to_speakers();
    let gone = SourceEndpoint::HardwareInput {
        device_id: FixedStr::from_str_trunc("usb-gone"),
    };
    // Fails, but the request stays in the config, so the profile records it.
    assert!(
        rig.control
            .command(Command::StripSource {
                strip: 1,
                source: gone,
                channels: 2
            })
            .is_err()
    );
    rig.cmd(Command::SaveProfile {
        name: "partial".into(),
    });
    rig.cmd(Command::StripSource {
        strip: 0,
        source: SourceEndpoint::Unassigned,
        channels: 2,
    });

    rig.cmd(Command::LoadProfile {
        name: "partial".into(),
    }); // Ok even though one device is gone
    let s = rig.control.snapshot().unwrap();
    assert_eq!(s.strips[0].link, LinkState::Running);
    assert!(matches!(&s.strips[1].link, LinkState::Failed(m) if m.contains("usb-gone")));
}
// !SECTION: Profiles

// SECTION: Network
fn loopback(port: u16) -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)
}

#[test]
fn remote_text_commands_obey_the_receive_policy() {
    let rig = Rig::new("text");
    let dest = loopback(rig.control.rx_port());
    let mut sender = TextSender::new("Command1").unwrap();

    // Nobody is allowed yet: the command is dropped.
    sender
        .send_command(dest, &TextCommand::StripGain { index: 0, db: -6.0 })
        .unwrap();
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        rig.control.snapshot().unwrap().strips[0].config.gain_db,
        0.0
    );

    rig.cmd(Command::RxPolicy {
        accept_any: false,
        allowed: vec![[127, 0, 0, 1]],
        trust_discovered: false,
    });
    assert_eq!(
        rig.control.snapshot().unwrap().policy.effective,
        vec![[127, 0, 0, 1]]
    );
    // ? The socket thread picks the new policy up between receives (at most one 5 ms timeout
    // ? later), so a packet sent in that instant could still meet the old policy.
    std::thread::sleep(Duration::from_millis(30));
    sender
        .send_command(dest, &TextCommand::StripGain { index: 0, db: -6.0 })
        .unwrap();
    sender
        .send_command(
            dest,
            &TextCommand::BusMute {
                index: 1,
                muted: true,
            },
        )
        .unwrap();
    wait_until("the remote gain", Duration::from_secs(3), || {
        rig.control.snapshot().unwrap().strips[0].config.gain_db == -6.0
    });
    wait_until("the remote mute", Duration::from_secs(3), || {
        rig.control.snapshot().unwrap().buses[1].config.mute
    });
}

#[test]
fn a_lan_stream_from_a_named_source_arrives_and_unnamed_senders_are_listed() {
    let rx = Rig::new("lan-rx");
    // Receive "Link" from 127.0.0.1 on strip 0, play it on bus 0.
    rx.cmd(Command::BusSink {
        bus: 0,
        sink: rx.speakers(),
        channels: 2,
    });
    rx.cmd(Command::Route {
        strip: 0,
        bus: 0,
        send: RouteSend {
            enabled: true,
            gain_db: 0.0,
            pre_fader: false,
        },
    });
    rx.cmd(Command::StripSource {
        strip: 0,
        source: SourceEndpoint::LanIn(LanRxConfig {
            source_ipv4: [127, 0, 0, 1],
            stream_name: FixedStr::from_str_trunc("Link"),
            buffer: NetworkBufferPreset::Fast,
        }),
        channels: 2,
    });

    // The sender is another control in the same process: mic -> bus 0 -> LAN.
    let tx = Rig::new("lan-tx");
    tx.mock.set_signal(
        "mock-in-1",
        Signal::Sine {
            freq_hz: 1000.0,
            amp: 0.5,
        },
    );
    tx.cmd(Command::Route {
        strip: 0,
        bus: 0,
        send: RouteSend {
            enabled: true,
            gain_db: 0.0,
            pre_fader: false,
        },
    });
    tx.cmd(Command::StripSource {
        strip: 0,
        source: tx.mic(),
        channels: 2,
    });
    tx.cmd(Command::BusSink {
        bus: 0,
        sink: SinkEndpoint::LanOut(LanTxConfig {
            peer_ipv4: [127, 0, 0, 1],
            port: rx.control.rx_port(),
            stream_name: FixedStr::from_str_trunc("Link"),
            ..LanTxConfig::default()
        }),
        channels: 2,
    });

    wait_until(
        "LAN audio at the receiver's speakers",
        Duration::from_secs(8),
        || rx.mock.capture("mock-out-1").peak > 0.2,
    );
    wait_until("50 packets sent", Duration::from_secs(5), || {
        tx.control.snapshot().unwrap().buses[0]
            .tx
            .is_some_and(|t| t.packets > 50)
    });
    let t = tx.control.snapshot().unwrap().buses[0]
        .tx
        .expect("sender stats");
    assert_eq!(t.send_errors, 0, "{t:?}");
    let lan = rx.control.meters().unwrap().lan_rx[0];
    assert!(
        lan.target_ms > 0.0 && lan.last_packet_ms_ago < 1000,
        "{lan:?}"
    );

    // Another sender with a different name is not routed anywhere: it is offered to the UI.
    tx.cmd(Command::BusSink {
        bus: 0,
        sink: SinkEndpoint::Unassigned,
        channels: 2,
    });
    tx.cmd(Command::BusSink {
        bus: 1,
        sink: SinkEndpoint::LanOut(LanTxConfig {
            peer_ipv4: [127, 0, 0, 1],
            port: rx.control.rx_port(),
            stream_name: FixedStr::from_str_trunc("Stranger"),
            ..LanTxConfig::default()
        }),
        channels: 2,
    });
    tx.cmd(Command::Route {
        strip: 0,
        bus: 1,
        send: RouteSend {
            enabled: true,
            gain_db: 0.0,
            pre_fader: false,
        },
    });
    wait_until("the stranger to be listed", Duration::from_secs(5), || {
        rx.control
            .snapshot()
            .unwrap()
            .seen_senders
            .iter()
            .any(|s| s.stream_name == "Stranger" && s.ip == [127, 0, 0, 1])
    });
    assert_eq!(
        rx.control.snapshot().unwrap().discovery,
        DiscoveryState::Off
    );
}
// !SECTION: Network

// SECTION: Probe
/// Strict on a quiet host; on a noisy one the numbers are printed and only sanity is checked.
const QUIET_HOST_MS: f32 = 4.0;

#[test]
fn the_click_loop_measures_the_configured_pipeline_within_one_block() {
    let rig = Rig::new("probe");
    let report = rig
        .control
        .probe(ProbeRequest {
            peer: None,
            pings: 0,
            click_loop: true,
            clicks: 12,
        })
        .unwrap();
    let c = report.click_loop.expect("click-loop report");
    println!("click-loop: {c:#?}");
    assert!(report.notes.is_empty(), "{:?}", report.notes);
    assert_eq!(c.clicks_sent, 12);
    assert!(
        c.clicks_found >= 10,
        "{} of 12 clicks came back",
        c.clicks_found
    );
    assert_eq!(c.expected_ms, 8.0, "the configured device_target_ms");
    if c.host_noise_ms < QUIET_HOST_MS {
        assert!(
            c.within_one_block,
            "p50 {:.2} ms against {:.2} ms expected (one block = {:.2} ms)",
            c.p50_ms, c.expected_ms, c.block_ms
        );
    } else {
        println!(
            "NOTE: noisy host ({:.1} ms late wakes): only sanity-checking the probe",
            c.host_noise_ms
        );
        assert!(
            c.p50_ms > 2.0 && c.p50_ms < c.expected_ms + 40.0,
            "p50 {:.2} ms",
            c.p50_ms
        );
    }

    // The borrowed strip and bus were put back: the setup is as it was.
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(rig.rejected(), 0);
    let s = rig.control.snapshot().unwrap();
    assert!(s.strips.iter().all(|s| s.link == LinkState::Idle));
    assert!(
        s.strips
            .iter()
            .all(|s| s.config.sends.iter().all(|x| !x.enabled))
    );
    let m = rig.control.meters().unwrap();
    assert_eq!(m.engine.output_overruns, 0);
    // And a second probe works (the rings really came back).
    let again = rig
        .control
        .probe(ProbeRequest {
            peer: None,
            pings: 0,
            click_loop: true,
            clicks: 3,
        })
        .unwrap();
    assert!(again.click_loop.unwrap().clicks_found >= 2);
}

#[test]
fn the_click_loop_says_so_when_no_mixer_is_running() {
    let rig = Rig::build("probe-dead", false);
    let report = rig
        .control
        .probe(ProbeRequest {
            peer: None,
            pings: 0,
            click_loop: true,
            clicks: 3,
        })
        .unwrap();
    assert!(report.click_loop.is_none());
    assert!(
        report.notes.iter().any(|n| n.contains("mixer")),
        "notes: {:?}",
        report.notes
    );
    assert!(rig.master.is_none());
}

#[test]
fn a_probe_cannot_start_while_another_runs_and_limits_are_enforced() {
    let rig = Rig::new("probe-busy");
    let control = rig.control.clone();
    let first = std::thread::spawn(move || {
        control.probe(ProbeRequest {
            peer: None,
            pings: 0,
            click_loop: true,
            clicks: 6,
        })
    });
    wait_until("the first probe to start", Duration::from_secs(2), || {
        matches!(
            rig.control.probe(ProbeRequest {
                peer: None,
                pings: 0,
                click_loop: false,
                clicks: 1
            }),
            Err(ControlError::Busy(_))
        )
    });
    assert!(first.join().unwrap().is_ok());
    assert!(matches!(
        rig.control.probe(ProbeRequest {
            peer: None,
            pings: 200,
            click_loop: false,
            clicks: 1
        }),
        Err(ControlError::Invalid(_))
    ));
}

#[test]
fn the_rtt_probe_pings_another_router() {
    let a = Rig::new("rtt-a");
    let b = Rig::new("rtt-b");
    let report = a
        .control
        .probe(ProbeRequest {
            peer: Some(loopback(b.control.rx_port())),
            pings: 4,
            click_loop: false,
            clicks: 1,
        })
        .unwrap();
    let rtt = report.rtt.expect("rtt report");
    println!("rtt: {rtt:#?}");
    assert_eq!(rtt.sent, 4);
    assert!(rtt.received >= 3, "{} of 4 pings answered", rtt.received);
    assert!(
        rtt.p50_ms > 0.0 && rtt.p50_ms < 50.0,
        "p50 {} ms",
        rtt.p50_ms
    );
    // B shows up as a peer in A's state, with the name it announced.
    let peers = a.control.snapshot().unwrap().peers;
    assert!(
        peers
            .iter()
            .any(|p| p.device_name == "test-rtt-b" && p.rtt_ms.is_some()),
        "{peers:?}"
    );

    let nobody = a
        .control
        .probe(ProbeRequest {
            peer: Some(loopback(1)),
            pings: 1,
            click_loop: false,
            clicks: 1,
        })
        .unwrap();
    assert_eq!(nobody.rtt.unwrap().received, 0);
    assert!(nobody.notes.iter().any(|n| n.contains("did not answer")));
}
// !SECTION: Probe

// SECTION: Over IPC
#[test]
fn a_remote_control_behaves_like_the_local_one() {
    let rig = Rig::new("ipc");
    let name = format!("retae-test-local-ipc-{}", std::process::id());
    let as_dyn: Arc<dyn ControlApi> = rig.control.clone();
    let server = IpcServer::start(&name, Arc::downgrade(&as_dyn)).unwrap();
    let remote = RemoteControl::connect(&name).unwrap();

    remote
        .command(Command::StripGain {
            strip: 4,
            gain_db: -9.0,
        })
        .unwrap();
    assert_eq!(remote.snapshot().unwrap(), rig.control.snapshot().unwrap());
    assert_eq!(remote.devices().unwrap().len(), 4);
    assert_eq!(remote.apps().unwrap().len(), 2);
    assert!(matches!(
        remote.command(Command::StripGain {
            strip: 99,
            gain_db: 0.0
        }),
        Err(ControlError::Invalid(_))
    ));
    assert_eq!(remote.meters().unwrap().strips.len(), 16);

    // A fully populated snapshot (devices open) also survives the trip.
    rig.wire_sine_to_speakers();
    assert_eq!(
        remote.snapshot().unwrap().strips[0].link,
        LinkState::Running
    );
    drop(server);
}
// !SECTION: Over IPC
