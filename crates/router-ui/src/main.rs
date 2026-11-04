//! `router-ui`: the desktop window. It talks to a running `routerd`; it is not the audio engine.

use router_ui::app::RouterApp;
use std::process::ExitCode;

const HELP: &str = "\
router-ui: desktop window for the ReTAE audio router

USAGE:
    router-ui [OPTIONS]

OPTIONS:
    --instance <NAME>   talk to the routerd started with the same --instance
    -h, --help          show this help
";

fn main() -> ExitCode {
    let mut instance = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return ExitCode::SUCCESS;
            }
            "--instance" => match args.next() {
                Some(v) => instance = Some(v),
                None => {
                    eprintln!("router-ui: --instance needs a name\n\n{HELP}");
                    return ExitCode::from(2);
                }
            },
            other => {
                eprintln!("router-ui: unknown option {other:?}\n\n{HELP}");
                return ExitCode::from(2);
            }
        }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ReTAE")
            .with_inner_size([1100.0, 680.0])
            .with_min_inner_size([320.0, 420.0]),
        ..Default::default()
    };
    let result = eframe::run_native(
        "ReTAE",
        options,
        Box::new(move |cc| Ok(Box::new(RouterApp::new(&cc.egui_ctx, instance)))),
    );
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("router-ui: {e}");
            ExitCode::FAILURE
        }
    }
}
