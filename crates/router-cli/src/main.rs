//! `router-cli`: the command-line view of a running `routerd`. All the work is in the library so
//! tests can drive it; this file only wires the real IPC connection and the exit code.

use app_core::ipc::RemoteControl;
use app_core::{ControlApi, ControlResult, paths};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // ? `Box<dyn ControlApi>` lets the library treat a real socket and a test double the same.
    let connect = |instance: Option<&str>| -> ControlResult<Box<dyn ControlApi>> {
        Ok(Box::new(RemoteControl::connect(&paths::ipc_name(
            instance,
        ))?))
    };
    let code = router_cli::run(
        &args,
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
        &connect,
    );
    std::process::exit(code);
}
