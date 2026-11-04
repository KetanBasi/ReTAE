//! `router-tray`: the Windows tray helper.

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    router_tray::win::run()
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!("router-tray: the tray helper is only available on Windows");
    std::process::ExitCode::from(2)
}
