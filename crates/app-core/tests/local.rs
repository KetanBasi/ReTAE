//! `LocalControl` against the mock backend and a real (timer-clocked) mixer.
//!
//! Timing-sensitive checks read the host's measured noise first. The dev container is a
//! microVM where 1 ms sleeps sometimes wake 3 to 30 ms late; there the checks relax and say so.

use app_core::ipc::{IpcServer, RemoteControl};
use app_core::recorder::RecordFormat;
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
                    recordings_dir: dir.0.join("recordings"),
                    record_format: RecordFormat::default(),
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

// SECTION: Recording
/// The one file in `<rig dir>/recordings` with this extension.
fn recorded_files(rig: &Rig, ext: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(rig._dir.0.join("recordings"))
        .map(|d| {
            d.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == ext))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn recorded_frames(rig: &Rig, bus: usize) -> u64 {
    rig.control.snapshot().unwrap().buses[bus]
        .recording
        .as_ref()
        .map_or(0, |r| r.frames)
}

#[test]
fn recording_a_bus_writes_the_mixer_output_to_a_wav_file() {
    let rig = Rig::new("rec-wav");
    rig.wire_sine_to_speakers();
    rig.cmd(Command::BusRecord {
        bus: 0,
        enabled: true,
    });

    let snap = rig.control.snapshot().unwrap();
    assert!(snap.buses[0].config.record);
    assert!(snap.buses[0].record_error.is_none());
    wait_until("one second recorded", Duration::from_secs(10), || {
        recorded_frames(&rig, 0) >= 48_000
    });
    rig.cmd(Command::BusRecord {
        bus: 0,
        enabled: false,
    });
    let snap = rig.control.snapshot().unwrap();
    assert!(!snap.buses[0].config.record);
    assert!(snap.buses[0].recording.is_none(), "stopping ends the take");

    let files = recorded_files(&rig, "wav");
    assert_eq!(files.len(), 1, "{files:?}");
    let name = files[0].file_name().unwrap().to_string_lossy().to_string();
    assert!(
        name.contains("bus1") && name.starts_with("test-rec-wav"),
        "{name}"
    );

    let mut reader = hound::WavReader::open(&files[0]).unwrap();
    let spec = reader.spec();
    assert_eq!((spec.channels, spec.sample_rate), (2, 48_000));
    assert_eq!(spec.bits_per_sample, 24);
    assert!(reader.duration() >= 48_000, "{} frames", reader.duration());
    let samples: Vec<f32> = reader
        .samples::<i32>()
        .map(|s| s.unwrap() as f32 / 8_388_607.0)
        .collect();
    // Skip the fader ramp at the start; the rest is a 0.5 sine at 1 kHz on both channels.
    let tail = &samples[samples.len() / 2..];
    let peak = tail.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!((0.45..0.55).contains(&peak), "peak {peak}");
    let left: Vec<f32> = tail.iter().step_by(2).copied().collect();
    let crossings = left
        .windows(2)
        .filter(|w| w[0] <= 0.0 && w[1] > 0.0)
        .count();
    let seconds = left.len() as f32 / 48_000.0;
    let hz = crossings as f32 / seconds;
    assert!((980.0..1020.0).contains(&hz), "{hz} Hz");

    // The ring went back to the pool: a second take works and is a second file.
    rig.cmd(Command::BusRecord {
        bus: 0,
        enabled: true,
    });
    wait_until("a second take", Duration::from_secs(10), || {
        recorded_frames(&rig, 0) >= 4_800
    });
    rig.cmd(Command::BusRecord {
        bus: 0,
        enabled: false,
    });
    assert_eq!(recorded_files(&rig, "wav").len(), 2);
    assert_eq!(rig.rejected(), 0);
}

#[test]
fn a_flac_recording_decodes() {
    let dir = TempDir::new("rec-flac");
    let (engine, handles) = create_engine();
    let (backend, mock) = MockBackend::new();
    let control = Arc::new(
        LocalControl::new(
            handles,
            Box::new(backend),
            DaemonConfig {
                name: "flac".into(),
                rx_port: 0,
                config_dir: dir.0.clone(),
                recordings_dir: dir.0.join("rec"),
                record_format: RecordFormat::Flac,
                ..DaemonConfig::default()
            },
        )
        .unwrap(),
    );
    let _master = TimerMaster::spawn(engine, 0.0).unwrap();
    mock.set_signal(
        "mock-in-1",
        Signal::Sine {
            freq_hz: 440.0,
            amp: 0.5,
        },
    );
    let speakers = SinkEndpoint::HardwareOutput {
        device_id: FixedStr::from_str_trunc("mock-out-1"),
    };
    let mic = SourceEndpoint::HardwareInput {
        device_id: FixedStr::from_str_trunc("mock-in-1"),
    };
    control
        .command(Command::BusSink {
            bus: 0,
            sink: speakers,
            channels: 2,
        })
        .unwrap();
    control
        .command(Command::Route {
            strip: 0,
            bus: 0,
            send: RouteSend {
                enabled: true,
                gain_db: 0.0,
                pre_fader: false,
            },
        })
        .unwrap();
    control
        .command(Command::StripSource {
            strip: 0,
            source: mic,
            channels: 2,
        })
        .unwrap();
    control
        .command(Command::BusRecord {
            bus: 0,
            enabled: true,
        })
        .unwrap();
    wait_until("1.5 s recorded", Duration::from_secs(10), || {
        control.snapshot().unwrap().buses[0]
            .recording
            .as_ref()
            .is_some_and(|r| r.frames >= 72_000)
    });
    // Dropping the control closes the file, like a daemon shutting down mid-take.
    drop(control);

    let files: Vec<PathBuf> = std::fs::read_dir(dir.0.join("rec"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    assert_eq!(files[0].extension().unwrap(), "flac");
    let bytes = std::fs::read(&files[0]).unwrap();
    assert_eq!(&bytes[..4], b"fLaC");

    // A real decoder, when the machine has one. `-count_frames` decodes every frame and `-v error`
    // prints a line for each damaged one.
    if let Ok(out) = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-count_frames", "-select_streams", "a:0"])
        .args([
            "-show_entries",
            "stream=codec_name,channels,sample_rate,nb_read_frames",
        ])
        .args(["-of", "default=nw=1"])
        .arg(&files[0])
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success() && err.trim().is_empty(), "{text}{err}");
        assert!(text.contains("codec_name=flac"), "{text}");
        assert!(
            text.contains("channels=2") && text.contains("sample_rate=48000"),
            "{text}"
        );
    }
}

#[test]
fn a_recording_that_cannot_start_says_why_and_leaves_the_bus_alone() {
    let rig = Rig::new("rec-fail");
    // A file where the recordings folder should be.
    std::fs::write(rig._dir.0.join("recordings"), b"not a folder").unwrap();
    let err = rig
        .control
        .command(Command::BusRecord {
            bus: 0,
            enabled: true,
        })
        .unwrap_err();
    assert!(matches!(err, ControlError::Backend(_)), "{err:?}");
    let snap = rig.control.snapshot().unwrap();
    assert!(!snap.buses[0].config.record);
    assert!(snap.buses[0].record_error.is_some());
    assert!(snap.buses[0].recording.is_none());
    // The ring is back in the pool: after the folder is fixed, recording works.
    std::fs::remove_file(rig._dir.0.join("recordings")).unwrap();
    rig.cmd(Command::BusRecord {
        bus: 0,
        enabled: true,
    });
    assert!(
        rig.control.snapshot().unwrap().buses[0]
            .record_error
            .is_none()
    );
    assert_eq!(rig.rejected(), 0);
}

#[test]
fn a_recording_follows_the_buses_channel_count() {
    let rig = Rig::new("rec-ch");
    rig.wire_sine_to_speakers();
    rig.cmd(Command::BusRecord {
        bus: 0,
        enabled: true,
    });
    // Re-open the bus with 1 channel: the stereo file is closed (empty, so removed) and a mono one
    // is started.
    rig.cmd(Command::BusSink {
        bus: 0,
        sink: rig.speakers(),
        channels: 1,
    });
    wait_until("mono audio recorded", Duration::from_secs(10), || {
        recorded_frames(&rig, 0) >= 4_800
    });
    rig.cmd(Command::BusRecord {
        bus: 0,
        enabled: false,
    });
    let files = recorded_files(&rig, "wav");
    let channels: Vec<u16> = files
        .iter()
        .map(|p| hound::WavReader::open(p).unwrap().spec().channels)
        .collect();
    assert!(channels.contains(&1), "{channels:?}");
}
// !SECTION: Recording

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

/// A second rig that trusts loopback, so it obeys the first one's remote commands.
fn trusting_peer(tag: &str) -> Rig {
    let peer = Rig::new(tag);
    peer.cmd(Command::RxPolicy {
        accept_any: false,
        allowed: vec![[127, 0, 0, 1]],
        trust_discovered: false,
    });
    // ? The socket thread picks the policy up between receives (<= 5 ms).
    std::thread::sleep(Duration::from_millis(30));
    peer
}

#[test]
fn one_control_can_command_another_over_vban_text() {
    let me = Rig::new("peer-a");
    let peer = trusting_peer("peer-b");
    let dest = loopback(peer.control.rx_port());

    me.cmd(Command::PeerText {
        peer: dest,
        text: "Strip[0].Gain=-4.5; Bus[1].Mute=1; Strip[1].Solo=1; Strip[1].Pan_x=0.25;".into(),
    });
    wait_until("the remote commands", Duration::from_secs(3), || {
        let s = peer.control.snapshot().unwrap();
        s.strips[0].config.gain_db == -4.5
            && s.buses[1].config.mute
            && s.strips[1].config.solo
            && (s.strips[1].config.pan - 0.5).abs() < 1e-6
    });
    // The sender's own state is untouched.
    let mine = me.control.snapshot().unwrap();
    assert_eq!(mine.strips[0].config.gain_db, 0.0);
    assert!(!mine.buses[1].config.mute);
}

#[test]
fn a_remote_route_switch_keeps_the_sends_gain() {
    let me = Rig::new("route-a");
    let peer = trusting_peer("route-b");
    peer.cmd(Command::Route {
        strip: 0,
        bus: 1,
        send: RouteSend {
            enabled: false,
            gain_db: -7.0,
            pre_fader: true,
        },
    });
    let dest = loopback(peer.control.rx_port());
    // ? VoiceMeeter names the buses A1.. then B1..; B1 is our bus 5 only on a Potato layout,
    // ? so ask for the first (A2 = Bus[1]).
    me.cmd(Command::PeerText {
        peer: dest,
        text: "Strip[0].A2=1;".into(),
    });
    wait_until("the route", Duration::from_secs(3), || {
        peer.control.snapshot().unwrap().strips[0].config.sends[1].enabled
    });
    let send = peer.control.snapshot().unwrap().strips[0].config.sends[1];
    assert_eq!(send.gain_db, -7.0);
    assert!(send.pre_fader);
}

#[test]
fn peer_commands_must_be_unicast_short_plain_text() {
    let me = Rig::new("peer-bad");
    let send = |peer: SocketAddrV4, text: &str| {
        me.control.command(Command::PeerText {
            peer,
            text: text.into(),
        })
    };
    let good = loopback(6980);
    assert!(
        send(
            SocketAddrV4::new(Ipv4Addr::BROADCAST, 6980),
            "Bus[0].Mute=1;"
        )
        .is_err()
    );
    assert!(
        send(
            SocketAddrV4::new(Ipv4Addr::new(224, 0, 0, 1), 6980),
            "Bus[0].Mute=1;"
        )
        .is_err()
    );
    assert!(
        send(
            SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 6980),
            "Bus[0].Mute=1;"
        )
        .is_err()
    );
    assert!(send(loopback(0), "Bus[0].Mute=1;").is_err());
    assert!(send(good, "   ").is_err());
    assert!(send(good, &"a".repeat(513)).is_err());
    assert!(send(good, "Bus[0].Mute=1;\u{0}").is_err());
    // A well-formed one to a unicast address is accepted (UDP: nobody has to be listening).
    assert!(send(good, "Bus[0].Mute=1;").is_ok());
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
            adaptive: false,
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
#[test]
fn the_mixer_follows_the_receive_streams_target_when_the_packet_size_is_learned() {
    // Optimal is a 6 ms floor, but 256-frame packets last 5.33 ms and the stream wants two of
    // them (10.67 ms). The strip is attached before any packet arrives, so only the housekeeping
    // tuner can bring the mixer's target to the same number.
    let rx = Rig::new("tune-rx");
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
            stream_name: FixedStr::from_str_trunc("Tune"),
            buffer: NetworkBufferPreset::Optimal,
            adaptive: true,
        }),
        channels: 2,
    });
    wait_until("the attach-time target", Duration::from_secs(3), || {
        (rx.control.meters().unwrap().lan_rx[0].target_ms - 6.0).abs() < 0.1
    });

    let tx = Rig::new("tune-tx");
    tx.mock.set_signal(
        "mock-in-1",
        Signal::Sine {
            freq_hz: 500.0,
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
            stream_name: FixedStr::from_str_trunc("Tune"),
            frames_per_packet: 256,
            ..LanTxConfig::default()
        }),
        channels: 2,
    });
    wait_until(
        "the mixer's target to reach two packets",
        Duration::from_secs(8),
        || (rx.control.meters().unwrap().lan_rx[0].target_ms - 10.667).abs() < 0.2,
    );
    assert_eq!(rx.rejected(), 0);
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
