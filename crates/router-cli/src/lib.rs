//! Command-line client: `devices`, `apps`, `status`, `route`, `probe <peer>`.
//!
//! A thin view over `app_core::ControlApi` (CLAUDE.md law 5): it formats what the daemon says
//! and sends commands; it never touches the engine. `run` takes its connection as a function so
//! tests can point it at an in-process daemon.

#![forbid(unsafe_code)]

mod format;

use app_core::lan::{self, Identity, PairingCode, PeerBook};
use app_core::types::{Command, ControlError, ProbeRequest};
use app_core::{ControlApi, ControlResult, paths};
use audio_types::{RouteSend, VBAN_DEFAULT_PORT};
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::Path;
use std::str::FromStr;

pub const HELP: &str = "\
router-cli: talk to a running routerd

USAGE:
    router-cli [--instance <NAME>] <COMMAND>

COMMANDS:
    devices                          list audio devices
    apps                             list applications that can be captured
    status                           show the engine, inputs, outputs and peers
    route                            show which strip feeds which bus
    route <strip> <bus> on|off [<gain_db>] [pre|post]
                                     change one send (strips and buses count from 0)
    probe [<peer>] [--pings <N>] [--clicks <N>] [--no-click-loop]
                                     measure the round trip to <peer> (ip or ip:port, default
                                     port 6980) and the delay of this machine's mixer path
";

pub const LAN_HELP: &str = "\
CONTROLLING ANOTHER MACHINE (its routerd must run with --lan-control):
    router-cli pair <host[:port]> <code>
                                     pair this device with that daemon using the one-time
                                     code it printed (routerd --lan-control --pair)
    router-cli --lan <host[:port]> <COMMAND>
                                     run any command above on a paired daemon, encrypted
";

/// Exit codes: 0 done, 1 the daemon said no or is unreachable, 2 wrong usage.
pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_USAGE: i32 = 2;

type Connect<'a> = &'a dyn Fn(Option<&str>) -> ControlResult<Box<dyn ControlApi>>;

enum Parsed {
    Help,
    Devices,
    Apps,
    Status,
    RouteShow,
    RouteSet {
        strip: u8,
        bus: u8,
        enabled: bool,
        gain_db: Option<f32>,
        pre_fader: Option<bool>,
    },
    Probe(ProbeRequest),
}

/// Run one command line (without the program name). Returns the exit code.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write, connect: Connect<'_>) -> i32 {
    let (instance, rest) = match split_instance(args) {
        Ok(v) => v,
        Err(e) => return usage(err, &e),
    };
    let parsed = match parse(&rest) {
        Ok(p) => p,
        Err(e) => return usage(err, &e),
    };
    if matches!(parsed, Parsed::Help) {
        let _ = out.write_all(HELP.as_bytes());
        return EXIT_OK;
    }
    let control = match connect(instance.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(err, "router-cli: {e}");
            return EXIT_FAILED;
        }
    };
    match execute(&*control, parsed, out) {
        Ok(()) => EXIT_OK,
        Err(e) => {
            let _ = writeln!(err, "router-cli: {e}");
            EXIT_FAILED
        }
    }
}

fn usage(err: &mut dyn Write, message: &str) -> i32 {
    let _ = writeln!(err, "router-cli: {message}\n\n{HELP}\n{LAN_HELP}");
    EXIT_USAGE
}

// SECTION: Another machine
/// Take `--lan <addr>` out of the arguments. Returns the address and what is left.
pub fn split_lan(args: &[String]) -> Result<(Option<String>, Vec<String>), String> {
    let mut lan = None;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--lan" {
            lan = Some(it.next().ok_or("--lan needs a host or host:port")?.clone());
        } else {
            rest.push(a.clone());
        }
    }
    Ok((lan, rest))
}

/// `host` or `host:port`; the port defaults to the LAN control port.
fn lan_target(s: &str) -> String {
    if s.contains(':') {
        s.to_owned()
    } else {
        format!("{s}:{}", app_core::lan::DEFAULT_PORT)
    }
}

/// A name for this device, shown on the daemon after pairing.
const CLIENT_NAME: &str = "router-cli";

/// Connect to a paired daemon over the LAN; keys live in `config_dir`.
pub fn connect_lan(addr: &str, config_dir: &Path) -> ControlResult<Box<dyn ControlApi>> {
    let files = |e: std::io::Error| ControlError::Invalid(format!("cannot read the keys: {e}"));
    let identity =
        Identity::load_or_create(&paths::lan_identity_path(config_dir)).map_err(files)?;
    let book = PeerBook::load(&paths::lan_peers_path(config_dir)).map_err(files)?;
    Ok(Box::new(lan::connect_paired(
        lan_target(addr),
        &identity,
        CLIENT_NAME,
        &book,
    )?))
}

/// `pair <host[:port]> <code>`: trust that daemon from now on. Returns the exit code.
pub fn pair_command(
    args: &[String],
    config_dir: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let [addr, code] = args else {
        return usage(err, "pair needs <host[:port]> and the one-time <code>");
    };
    let Some(code) = PairingCode::parse(code) else {
        return usage(
            err,
            "the code is 16 letters and digits, like 7H2K-9QXM-04TD-VB3N",
        );
    };
    let result = (|| -> ControlResult<_> {
        let files = |e: std::io::Error| ControlError::Invalid(format!("keys: {e}"));
        let identity =
            Identity::load_or_create(&paths::lan_identity_path(config_dir)).map_err(files)?;
        let mut book = PeerBook::load(&paths::lan_peers_path(config_dir)).map_err(files)?;
        let daemon = lan::pair(lan_target(addr), &identity, CLIENT_NAME, &code)?;
        book.add(daemon.clone()).map_err(files)?;
        Ok(daemon)
    })();
    match result {
        Ok(daemon) => {
            let _ = writeln!(
                out,
                "paired with {:?} (key {}); use: router-cli --lan {addr} status",
                daemon.name,
                lan::fingerprint(&daemon.key)
            );
            EXIT_OK
        }
        Err(e) => {
            let _ = writeln!(err, "router-cli: {e}");
            EXIT_FAILED
        }
    }
}
// !SECTION: Another machine

fn split_instance(args: &[String]) -> Result<(Option<String>, Vec<String>), String> {
    let mut instance = None;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--instance" {
            instance = Some(it.next().ok_or("--instance needs a name")?.clone());
        } else {
            rest.push(a.clone());
        }
    }
    Ok((instance, rest))
}

/// `192.168.1.5`, `192.168.1.5:7000` or `localhost`.
pub fn parse_peer(s: &str) -> Result<SocketAddrV4, String> {
    if let Ok(addr) = SocketAddrV4::from_str(s) {
        return Ok(addr);
    }
    let ip = if s == "localhost" || s == "local" {
        Ipv4Addr::LOCALHOST
    } else {
        Ipv4Addr::from_str(s).map_err(|_| {
            format!("{s:?} is not an IPv4 address (use 192.168.1.5 or 192.168.1.5:7000)")
        })?
    };
    Ok(SocketAddrV4::new(ip, VBAN_DEFAULT_PORT))
}

fn number<T: FromStr>(s: &str, what: &str) -> Result<T, String> {
    s.parse()
        .map_err(|_| format!("{what} must be a number, not {s:?}"))
}

fn parse(args: &[String]) -> Result<Parsed, String> {
    let Some((cmd, rest)) = args.split_first() else {
        return Ok(Parsed::Help);
    };
    match (cmd.as_str(), rest) {
        ("-h" | "--help" | "help", _) => Ok(Parsed::Help),
        ("devices", []) => Ok(Parsed::Devices),
        ("apps", []) => Ok(Parsed::Apps),
        ("status", []) => Ok(Parsed::Status),
        ("route", []) => Ok(Parsed::RouteShow),
        ("route", [strip, bus, state, extra @ ..]) => {
            let enabled = match state.as_str() {
                "on" => true,
                "off" => false,
                other => return Err(format!("expected on or off, not {other:?}")),
            };
            let (mut gain_db, mut pre_fader) = (None, None);
            for e in extra {
                match e.as_str() {
                    "pre" => pre_fader = Some(true),
                    "post" => pre_fader = Some(false),
                    n => gain_db = Some(number::<f32>(n, "the gain")?),
                }
            }
            Ok(Parsed::RouteSet {
                strip: number(strip, "the strip")?,
                bus: number(bus, "the bus")?,
                enabled,
                gain_db,
                pre_fader,
            })
        }
        ("probe", rest) => {
            let mut req = ProbeRequest {
                peer: None,
                ..ProbeRequest::default()
            };
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--pings" => {
                        req.pings = number(it.next().ok_or("--pings needs a number")?, "--pings")?
                    }
                    "--clicks" => {
                        req.clicks =
                            number(it.next().ok_or("--clicks needs a number")?, "--clicks")?
                    }
                    "--no-click-loop" => req.click_loop = false,
                    flag if flag.starts_with('-') => {
                        return Err(format!("unknown option {flag:?}"));
                    }
                    peer if req.peer.is_none() => req.peer = Some(parse_peer(peer)?),
                    _ => return Err("probe takes one peer".into()),
                }
            }
            if req.peer.is_none() && !req.click_loop {
                return Err("nothing to measure: give a peer, or drop --no-click-loop".into());
            }
            Ok(Parsed::Probe(req))
        }
        (other, _) => Err(format!("unknown or incomplete command {other:?}")),
    }
}

fn execute(control: &dyn ControlApi, parsed: Parsed, out: &mut dyn Write) -> ControlResult<()> {
    let io = |e: std::io::Error| ControlError::Invalid(format!("cannot write output: {e}"));
    match parsed {
        Parsed::Help => Ok(()),
        Parsed::Devices => out
            .write_all(format::devices(&control.devices()?).as_bytes())
            .map_err(io),
        Parsed::Apps => out
            .write_all(format::apps(&control.apps()?).as_bytes())
            .map_err(io),
        Parsed::Status => {
            let (state, meters) = (control.snapshot()?, control.meters()?);
            out.write_all(format::status(&state, &meters).as_bytes())
                .map_err(io)
        }
        Parsed::RouteShow => out
            .write_all(format::routes(&control.snapshot()?).as_bytes())
            .map_err(io),
        Parsed::RouteSet {
            strip,
            bus,
            enabled,
            gain_db,
            pre_fader,
        } => {
            let state = control.snapshot()?;
            let current = state
                .strips
                .get(usize::from(strip))
                .and_then(|s| s.config.sends.get(usize::from(bus)))
                .copied()
                .ok_or_else(|| ControlError::Invalid(format!("no strip {strip} / bus {bus}")))?;
            let send = RouteSend {
                enabled,
                gain_db: gain_db.unwrap_or(current.gain_db),
                pre_fader: pre_fader.unwrap_or(current.pre_fader),
            };
            control.command(Command::Route { strip, bus, send })?;
            writeln!(
                out,
                "strip {strip} -> bus {bus}: {} ({:+.1} dB, {}-fader)",
                if send.enabled { "on" } else { "off" },
                send.gain_db,
                if send.pre_fader { "pre" } else { "post" }
            )
            .map_err(io)
        }
        Parsed::Probe(request) => {
            let wait = format::probe_wait(&request);
            if !wait.is_empty() {
                writeln!(out, "{wait}").map_err(io)?;
                let _ = out.flush();
            }
            let report = control.probe(request)?;
            out.write_all(format::probe(&report).as_bytes()).map_err(io)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn peers_take_an_optional_port() {
        assert_eq!(
            parse_peer("192.168.1.5").unwrap(),
            SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 5), 6980)
        );
        assert_eq!(parse_peer("10.0.0.2:7000").unwrap().port(), 7000);
        assert_eq!(parse_peer("localhost").unwrap().ip(), &Ipv4Addr::LOCALHOST);
        assert!(parse_peer("example.com").is_err());
        assert!(parse_peer("300.1.1.1").is_err());
    }

    #[test]
    fn commands_parse() {
        assert!(matches!(parse(&args("devices")), Ok(Parsed::Devices)));
        assert!(matches!(parse(&args("")), Ok(Parsed::Help)));
        assert!(matches!(parse(&args("route")), Ok(Parsed::RouteShow)));
        match parse(&args("route 1 2 on -6.5 pre")).unwrap() {
            Parsed::RouteSet {
                strip: 1,
                bus: 2,
                enabled: true,
                gain_db: Some(g),
                pre_fader: Some(true),
            } => {
                assert_eq!(g, -6.5);
            }
            _ => panic!("route did not parse"),
        }
        match parse(&args("probe 10.0.0.2 --pings 3 --no-click-loop")).unwrap() {
            Parsed::Probe(p) => {
                assert_eq!(p.peer.unwrap().port(), 6980);
                assert_eq!(p.pings, 3);
                assert!(!p.click_loop);
            }
            _ => panic!("probe did not parse"),
        }
        match parse(&args("probe")).unwrap() {
            Parsed::Probe(p) => assert!(p.peer.is_none() && p.click_loop),
            _ => panic!(),
        }
    }

    #[test]
    fn bad_command_lines_are_usage_errors() {
        for bad in [
            "frobnicate",
            "devices extra",
            "route 1",
            "route a 2 on",
            "route 1 2 maybe",
            "route 1 2 on loud",
            "probe a b",
            "probe --bogus",
            "probe --no-click-loop",
            "probe --pings",
            "probe not-an-ip",
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad:?} should not parse");
        }
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(&args("frobnicate"), &mut out, &mut err, &|_| {
            unreachable!("no connection for bad usage")
        });
        assert_eq!(code, EXIT_USAGE);
        assert!(String::from_utf8(err).unwrap().contains("USAGE"));
    }

    #[test]
    fn an_unreachable_daemon_is_a_plain_failure() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(&args("status"), &mut out, &mut err, &|_| {
            Err(ControlError::Disconnected("cannot reach routerd".into()))
        });
        assert_eq!(code, EXIT_FAILED);
        assert!(
            String::from_utf8(err)
                .unwrap()
                .contains("cannot reach routerd")
        );
    }

    #[test]
    fn help_needs_no_daemon() {
        let mut out = Vec::new();
        let code = run(
            &args("--help"),
            &mut out,
            &mut Vec::new(),
            &|_| unreachable!(),
        );
        assert_eq!(code, EXIT_OK);
        assert!(String::from_utf8(out).unwrap().contains("probe"));
    }
}
