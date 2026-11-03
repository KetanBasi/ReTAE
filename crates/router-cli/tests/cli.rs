//! The CLI library against real in-process daemons over the real IPC socket.

use app_core::ControlApi;
use app_core::ipc::RemoteControl;
use app_core::types::Command;
use app_core::{ControlResult, paths};
use audio_types::{FixedStr, SourceEndpoint};
use routerd::{Daemon, Options};
use std::path::PathBuf;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("retae-cli-{tag}-{}", std::process::id()));
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

fn start(tag: &str, dir: &TempDir) -> (Daemon, String) {
    let instance = format!("cli-{tag}-{}", std::process::id());
    let daemon = Daemon::start(Options {
        name: format!("daemon-{tag}"),
        rx_port: 0,
        instance: Some(instance.clone()),
        config_dir: dir.0.clone(),
        ..Options::default()
    })
    .unwrap();
    (daemon, instance)
}

/// Run the CLI like `main` does. Returns (exit code, stdout, stderr).
fn cli(instance: &str, line: &str) -> (i32, String, String) {
    let mut args = vec!["--instance".to_owned(), instance.to_owned()];
    args.extend(line.split_whitespace().map(str::to_owned));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let connect = |instance: Option<&str>| -> ControlResult<Box<dyn ControlApi>> {
        Ok(Box::new(RemoteControl::connect(&paths::ipc_name(
            instance,
        ))?))
    };
    let code = router_cli::run(&args, &mut out, &mut err, &connect);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[test]
fn devices_and_apps_list_what_the_backend_offers() {
    let dir = TempDir::new("lists");
    let (_daemon, inst) = start("lists", &dir);

    let (code, out, err) = cli(&inst, "devices");
    assert_eq!(code, 0, "{err}");
    for id in ["mock-in-1", "mock-in-2", "mock-out-1", "mock-out-2"] {
        assert!(out.contains(id), "devices output lacks {id}:\n{out}");
    }
    assert!(out.contains("(default)"));

    let (code, out, err) = cli(&inst, "apps");
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("1001") && out.contains("PID"), "{out}");
}

#[test]
fn status_shows_what_is_assigned_and_route_changes_a_send() {
    let dir = TempDir::new("status");
    let (daemon, inst) = start("status", &dir);

    let (code, out, _) = cli(&inst, "status");
    assert_eq!(code, 0);
    assert!(
        out.contains("daemon-status") && out.contains("(none assigned)"),
        "{out}"
    );

    let remote = RemoteControl::connect(daemon.ipc_name()).unwrap();
    remote
        .command(Command::StripSource {
            strip: 2,
            source: SourceEndpoint::HardwareInput {
                device_id: FixedStr::from_str_trunc("mock-in-1"),
            },
            channels: 2,
        })
        .unwrap();
    let (_, out, _) = cli(&inst, "status");
    assert!(
        out.contains("input mock-in-1") && out.contains("running"),
        "{out}"
    );

    // No route is on at first; turn one on and read it back through both views.
    let (_, out, _) = cli(&inst, "route");
    assert!(out.contains("no routes are on"), "{out}");
    let (code, out, err) = cli(&inst, "route 2 1 on -6 pre");
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("strip 2 -> bus 1: on (-6.0 dB, pre-fader)"),
        "{out}"
    );
    let (_, out, _) = cli(&inst, "route");
    assert!(
        out.contains("strip 2") && out.contains("bus 1") && out.contains("-6.0 dB"),
        "{out}"
    );
    let send = remote.snapshot().unwrap().strips[2].config.sends[1];
    assert!(send.enabled && send.pre_fader && send.gain_db == -6.0);

    // Omitted fields keep their value, and `off` turns the send off.
    let (code, _, err) = cli(&inst, "route 2 1 off");
    assert_eq!(code, 0, "{err}");
    let send = remote.snapshot().unwrap().strips[2].config.sends[1];
    assert!(!send.enabled && send.pre_fader && send.gain_db == -6.0);
}

#[test]
fn route_to_a_strip_that_does_not_exist_is_a_failure_not_a_crash() {
    let dir = TempDir::new("badroute");
    let (_daemon, inst) = start("badroute", &dir);
    let (code, out, err) = cli(&inst, "route 99 0 on");
    assert_eq!(code, router_cli::EXIT_FAILED);
    assert!(out.is_empty());
    assert!(err.contains("no strip 99"), "{err}");
}

#[test]
fn probe_prints_the_round_trip_histogram_and_the_click_loop() {
    let (dir_a, dir_b) = (TempDir::new("probe-a"), TempDir::new("probe-b"));
    let (_a, inst_a) = start("probe-a", &dir_a);
    let (b, _) = start("probe-b", &dir_b);

    let line = format!(
        "probe 127.0.0.1:{} --pings 4 --clicks 8",
        b.control().rx_port()
    );
    let (code, out, err) = cli(&inst_a, &line);
    println!("{out}");
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("pinging 127.0.0.1:"),
        "progress line missing:\n{out}"
    );
    assert!(out.contains("VBAN-SERVICE round trip:"), "{out}");
    assert!(
        out.contains("p50") && out.contains(" | "),
        "histogram missing:\n{out}"
    );
    assert!(
        out.contains("click loop through this machine's mixer"),
        "{out}"
    );
    assert!(
        out.contains("covers:"),
        "the scope of the number must be stated:\n{out}"
    );
}

#[test]
fn a_probe_without_a_peer_measures_only_the_local_pipeline() {
    let dir = TempDir::new("local-probe");
    let (_daemon, inst) = start("local-probe", &dir);
    let (code, out, err) = cli(&inst, "probe --clicks 6");
    assert_eq!(code, 0, "{err}");
    assert!(!out.contains("VBAN-SERVICE"), "{out}");
    assert!(out.contains("click loop"), "{out}");
}

#[test]
fn no_daemon_gives_a_clear_message_and_exit_code_one() {
    let (code, out, err) = cli("cli-nobody-home", "status");
    assert_eq!(code, router_cli::EXIT_FAILED);
    assert!(out.is_empty());
    assert!(err.starts_with("router-cli: "), "{err}");
}
