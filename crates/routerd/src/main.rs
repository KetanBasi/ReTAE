//! `routerd`: the router daemon.

use app_core::recorder::RecordFormat;
use routerd::{BackendKind, Daemon, DaemonError, Options};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

// !! In debug builds an allocation inside `process_block` aborts the process (CLAUDE.md law 1).
// ! This is a no-op for release builds, which do not link the check.
#[cfg(debug_assertions)]
#[global_allocator]
static ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

const HELP: &str = "\
routerd: low-latency LAN audio router

USAGE:
    routerd [OPTIONS]

OPTIONS:
    --name <NAME>            name other machines see            (default: ReTAE)
    --backend <KIND>         audio backend: mock, pipewire, alsa, wasapi, auto
                             (default: auto if this build has a real backend, else mock)
    --port <PORT>            UDP port for VBAN audio            (default: 6980)
    --discovery              announce and browse over mDNS
    --instance <NAME>        run next to another routerd (separate IPC name)
    --config-dir <DIR>       where profiles are kept
    --profile <NAME>         load this profile at start
    --device-target-ms <MS>  buffer for non-master devices      (default: 8)
    --recordings-dir <DIR>   where bus recordings go            (default: ~/Music/ReTAE)
    --record-format <FMT>    wav16, wav24, wav32f or flac       (default: wav24)
    --lan-control [PORT]     let paired devices control this mixer over the LAN,
                             encrypted (default port: 6990; off unless given)
    --pair                   with --lan-control: open a 5-minute pairing window and
                             print its one-time code at start
    -h, --help               show this help
";

fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Options>, String> {
    let mut o = Options {
        backend: BackendKind::preferred(),
        ..Options::default()
    };
    let mut args = args.into_iter().peekable();
    while let Some(arg) = args.next() {
        // ? `--lan-control` takes an optional port: look at the next word before consuming it.
        if arg == "--lan-control" {
            let port = match args.peek() {
                Some(next) if !next.starts_with('-') => {
                    let port = next
                        .parse::<u16>()
                        .ok()
                        .filter(|p| *p != 0)
                        .ok_or_else(|| "--lan-control needs a port from 1 to 65535".to_owned())?;
                    args.next();
                    port
                }
                _ => app_core::lan::DEFAULT_PORT,
            };
            o.lan_control = Some(port);
            continue;
        }
        let mut value = |what: &str| args.next().ok_or_else(|| format!("{arg} needs {what}"));
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--pair" => o.pair = true,
            "--name" => o.name = value("a name")?,
            "--backend" => o.backend = value("a backend")?.parse::<BackendKind>()?,
            "--port" => {
                o.rx_port = value("a port")?
                    .parse()
                    .map_err(|_| "--port needs a number from 0 to 65535".to_owned())?;
            }
            "--discovery" => o.discovery = true,
            "--instance" => o.instance = Some(value("a name")?),
            "--config-dir" => o.config_dir = PathBuf::from(value("a directory")?),
            "--profile" => o.profile = Some(value("a profile name")?),
            "--recordings-dir" => o.recordings_dir = PathBuf::from(value("a directory")?),
            "--record-format" => {
                o.record_format = RecordFormat::parse(&value("a format")?).ok_or_else(|| {
                    "--record-format is one of wav16, wav24, wav32f, flac".to_owned()
                })?;
            }
            "--device-target-ms" => {
                o.device_target_ms = value("a number")?
                    .parse()
                    .ok()
                    .filter(|v: &f32| v.is_finite() && (1.0..=200.0).contains(v))
                    .ok_or_else(|| "--device-target-ms needs a number from 1 to 200".to_owned())?;
            }
            other => return Err(format!("unknown option {other:?} (try --help)")),
        }
    }
    Ok(Some(o))
}

fn main() -> ExitCode {
    let options = match parse(std::env::args().skip(1)) {
        Ok(Some(o)) => o,
        Ok(None) => {
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("routerd: {e}");
            return ExitCode::from(2);
        }
    };
    // ? Must be set before the daemon starts: a network thread asks for real-time priority when
    // ? it begins, and the hook is read then. (Rx/Tx threads wake about once a millisecond, so
    // ? 48 frames is the period we tell the scheduler.)
    net_vban::hook::set_thread_start_hook(|_role| {
        let _ = platform_io::rt::promote_current_thread(48, 48_000);
    });
    let daemon = match Daemon::start(options) {
        Ok(d) => d,
        Err(DaemonError::AlreadyRunning(name)) => {
            eprintln!("routerd: another instance is already running ({name})");
            return ExitCode::from(3);
        }
        Err(e) => {
            eprintln!("routerd: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "routerd: running; backend {}, VBAN on UDP {}, IPC {:?}",
        daemon.backend_name(),
        daemon.control().rx_port(),
        daemon.ipc_name()
    );
    if let Some(addr) = daemon.lan_addr() {
        println!("routerd: LAN control on TCP {addr} (encrypted; paired devices only)");
    }
    if let Some(code) = daemon.pairing_code() {
        println!(
            "routerd: pairing window open for 5 minutes. One-time code: {code}\n\
             routerd: enter it on the controller; it works once and is forgotten."
        );
    }
    // The mixer thread reports within a moment whether it got real-time priority. Saying so
    // matters: without it audio still works, but a busy machine can cause dropouts.
    let report = daemon.master().stats().clone();
    for _ in 0..50 {
        if report.rt.get().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    match report.rt.get() {
        Some(platform_io::rt::RtOutcome::Promoted) => {
            println!("routerd: mixer thread has real-time priority");
        }
        Some(platform_io::rt::RtOutcome::Unavailable(why)) => {
            println!("routerd: mixer thread runs at normal priority ({why})");
        }
        None => println!("routerd: mixer thread priority not reported yet"),
    }
    // ? There is no signal handling yet: Ctrl-C ends the process and the OS reclaims the
    // ? sockets and threads. A clean shutdown (devices closed in order) arrives with the tray
    // ? helper, which owns the lifetime of the daemon.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn options_are_parsed() {
        let o = parse(args(&[
            "--name",
            "Desk",
            "--port",
            "7000",
            "--instance",
            "b",
            "--discovery",
            "--device-target-ms",
            "12.5",
            "--profile",
            "stage",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!((o.name.as_str(), o.rx_port), ("Desk", 7000));
        assert_eq!(o.instance.as_deref(), Some("b"));
        assert!(o.discovery);
        assert_eq!(o.device_target_ms, 12.5);
        assert_eq!(o.profile.as_deref(), Some("stage"));
    }

    #[test]
    fn lan_control_is_off_by_default_and_takes_an_optional_port() {
        let off = parse(args(&[])).unwrap().unwrap();
        assert_eq!((off.lan_control, off.pair), (None, false));
        let default = parse(args(&["--lan-control"])).unwrap().unwrap();
        assert_eq!(default.lan_control, Some(app_core::lan::DEFAULT_PORT));
        // A flag after it is not mistaken for its port.
        let o = parse(args(&["--lan-control", "--pair", "--name", "x"]))
            .unwrap()
            .unwrap();
        assert_eq!(o.lan_control, Some(app_core::lan::DEFAULT_PORT));
        assert!(o.pair);
        assert_eq!(o.name, "x");
        let o = parse(args(&["--lan-control", "7777"])).unwrap().unwrap();
        assert_eq!(o.lan_control, Some(7777));
        assert!(parse(args(&["--lan-control", "0"])).is_err());
        assert!(parse(args(&["--lan-control", "http"])).is_err());
    }

    #[test]
    fn every_backend_name_parses_in_every_build() {
        for (name, kind) in [
            ("mock", BackendKind::Mock),
            ("pipewire", BackendKind::PipeWire),
            ("alsa", BackendKind::Alsa),
            ("wasapi", BackendKind::Wasapi),
            ("auto", BackendKind::Auto),
        ] {
            let o = parse(args(&["--backend", name])).unwrap().unwrap();
            assert_eq!(o.backend, kind);
            assert_eq!(kind.as_str(), name);
        }
        // No flag: the build's preferred backend.
        let o = parse(args(&[])).unwrap().unwrap();
        assert_eq!(o.backend, BackendKind::preferred());
    }

    #[test]
    fn bad_options_say_what_is_wrong() {
        for bad in [
            &["--port"][..],
            &["--port", "99999"],
            &["--backend", "coreaudio"],
            &["--device-target-ms", "0"],
            &["--device-target-ms", "nan"],
            &["--frobnicate"],
        ] {
            assert!(parse(args(bad)).is_err(), "{bad:?} must be refused");
        }
        assert!(parse(args(&["--help"])).unwrap().is_none());
    }
}
