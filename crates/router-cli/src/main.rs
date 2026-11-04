//! `router-cli`: the command-line view of a running `routerd`. All the work is in the library so
//! tests can drive it; this file only wires the real connections and the exit code.

use app_core::ipc::RemoteControl;
use app_core::{ControlApi, ControlResult, paths};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config_dir = paths::config_dir();
    let (lan, args) = match router_cli::split_lan(&args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("router-cli: {e}");
            std::process::exit(router_cli::EXIT_USAGE);
        }
    };
    if args.first().map(String::as_str) == Some("pair") {
        let code = router_cli::pair_command(
            &args[1..],
            &config_dir,
            &mut std::io::stdout().lock(),
            &mut std::io::stderr().lock(),
        );
        std::process::exit(code);
    }
    // ? `Box<dyn ControlApi>` lets the library treat a real socket, an encrypted LAN link and a
    // ? test double the same.
    let connect = |instance: Option<&str>| -> ControlResult<Box<dyn ControlApi>> {
        match &lan {
            Some(addr) => router_cli::connect_lan(addr, &config_dir),
            None => Ok(Box::new(RemoteControl::connect(&paths::ipc_name(
                instance,
            ))?)),
        }
    };
    let code = router_cli::run(
        &args,
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
        &connect,
    );
    std::process::exit(code);
}
