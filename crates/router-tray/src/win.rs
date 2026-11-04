//! The tray icon, its menu, and the Windows message loop (Windows only).
//!
//! * Starts `routerd` (hidden, no console window) when no engine answers.
//! * Menu: Open ReTAE, Close ReTAE window, Quit tray, Quit tray and engine.
//! * Left click on the icon opens the UI.
//!
//! The tray icon is a window, so the thread that made it must run a message loop. All menu
//! callbacks arrive on that thread from inside the loop, which is why they may touch the
//! `Mutex`-guarded child slots without any other synchronisation.

// ? `windows-sys` is only used for the message loop and PostQuitMessage: the one place this
// ? crate needs `unsafe`. The crate denies it everywhere else (see `lib.rs`).

use crate::{ChildSlot, engine_running, sibling_exe};
use std::os::windows::process::CommandExt;
use std::process::{Command, ExitCode};
use std::sync::Mutex;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, PostQuitMessage, TranslateMessage,
};

/// `CREATE_NO_WINDOW`: a child console program gets no console window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

static ENGINE: Mutex<ChildSlot> = Mutex::new(ChildSlot::new());
static UI: Mutex<ChildSlot> = Mutex::new(ChildSlot::new());

fn lock(slot: &'static Mutex<ChildSlot>) -> std::sync::MutexGuard<'static, ChildSlot> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn program(name: &str) -> Option<Command> {
    let exe = std::env::current_exe().ok()?;
    let mut cmd = Command::new(sibling_exe(&exe, name));
    cmd.creation_flags(CREATE_NO_WINDOW);
    Some(cmd)
}

fn start_engine() {
    if engine_running(None) {
        return;
    }
    match program("routerd").map(|c| lock(&ENGINE).start(c)) {
        Some(Ok(_)) => {}
        Some(Err(e)) => eprintln!("router-tray: cannot start routerd: {e}"),
        None => eprintln!("router-tray: cannot find its own folder"),
    }
}

fn open_ui() {
    start_engine();
    // ? The UI is a normal window program, so it keeps its own window; only a console program
    // ? needs CREATE_NO_WINDOW, and it does no harm here.
    match program("router-ui").map(|c| lock(&UI).start(c)) {
        Some(Ok(_)) => {}
        Some(Err(e)) => eprintln!("router-tray: cannot start router-ui: {e}"),
        None => eprintln!("router-tray: cannot find its own folder"),
    }
}

fn close_ui() {
    lock(&UI).stop();
}

fn quit(stop_engine: bool) {
    if stop_engine {
        lock(&ENGINE).stop();
    }
    // SAFETY: no arguments but the exit code; it only posts WM_QUIT to this thread's queue.
    unsafe { PostQuitMessage(0) };
}

/// A 32x32 round badge, drawn in code so the binary needs no image file.
fn icon() -> Option<Icon> {
    const N: u32 = 32;
    let mut rgba = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
            let d = (dx * dx + dy * dy).sqrt();
            let px: [u8; 4] = if d < 7.0 {
                [255, 255, 255, 255] // white centre
            } else if d < 14.5 {
                [45, 156, 219, 255] // blue disc
            } else {
                [0, 0, 0, 0] // transparent corners
            };
            rgba.extend_from_slice(&px);
        }
    }
    Icon::from_rgba(rgba, N, N).ok()
}

pub fn run() -> ExitCode {
    start_engine();

    let open = MenuItem::new("Open ReTAE", true, None);
    let close = MenuItem::new("Close ReTAE window", true, None);
    let quit_tray = MenuItem::new("Quit tray", true, None);
    // ? Only an engine this tray started can be stopped from here.
    let quit_all = MenuItem::new(
        "Quit tray and engine",
        lock(&ENGINE).was_started_here(),
        None,
    );
    let menu = Menu::new();
    if menu
        .append_items(&[
            &open,
            &close,
            &PredefinedMenuItem::separator(),
            &quit_tray,
            &quit_all,
        ])
        .is_err()
    {
        eprintln!("router-tray: cannot build the menu");
        return ExitCode::FAILURE;
    }

    let (open_id, close_id) = (open.id().clone(), close.id().clone());
    let (quit_id, quit_all_id) = (quit_tray.id().clone(), quit_all.id().clone());
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == open_id {
            open_ui();
        } else if event.id == close_id {
            close_ui();
        } else if event.id == quit_id {
            quit(false);
        } else if event.id == quit_all_id {
            quit(true);
        }
    }));
    TrayIconEvent::set_event_handler(Some(|event: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        {
            open_ui();
        }
    }));

    let mut builder = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("ReTAE audio router");
    if let Some(icon) = icon() {
        builder = builder.with_icon(icon);
    }
    // ? The icon disappears when this value is dropped, so it lives until the loop ends.
    let _tray = match builder.build() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("router-tray: cannot create the tray icon: {e}");
            return ExitCode::FAILURE;
        }
    };

    // SAFETY: `msg` is a live local the calls fill and read; a null window handle means "any
    // window of this thread". GetMessageW returns 0 on WM_QUIT and -1 on error, both end the loop.
    unsafe {
        let mut msg: MSG = core::mem::zeroed();
        while GetMessageW(&mut msg, core::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    ExitCode::SUCCESS
}
