//! The eframe application: one frame = poll, govern, draw, send.

use crate::governor::{Rate, Repaint, RepaintGovernor, WindowState};
use crate::link::{self, Link, Session, Wake};
use crate::model::{Conn, Model};
use crate::view::{Ctx, NARROW_BELOW, Out, ViewState};
use crate::{daemon, inspector, matrix, patchbay, simple, strips, toolbar};
use app_core::ui_state::{GraphicsSettings, LayoutMode, UiMode};
use egui::{Color32, RichText};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often the worker tries to reach a daemon that is not there.
const RECONNECT_EVERY: Duration = Duration::from_secs(1);

pub struct RouterApp {
    pub model: Model,
    pub view: ViewState,
    gov: RepaintGovernor,
    started: Instant,
    instance: Option<String>,
    /// What the governor decided in the newest frame. Public for the tests and the status bar.
    pub last_decision: (Rate, Repaint),
    daemon_error: Option<String>,
    /// Tests set this to step time by hand instead of waiting for it.
    pub clock_override_s: Option<f64>,
}

impl RouterApp {
    /// A real app: IPC to `routerd` on a worker thread.
    pub fn new(ctx: &egui::Context, instance: Option<String>) -> Self {
        let wake: Wake = {
            let ctx = ctx.clone();
            // ? A command's result is drawn at once; a short delay, not an immediate repaint, lets
            // ? several quick commands (a fader drag) share one frame.
            Arc::new(move || ctx.request_repaint_after(Duration::from_millis(8)))
        };
        let session = Session::new(link::ipc_connector(instance.clone()), RECONNECT_EVERY);
        Self::with_link(Link::spawn(session, Some(wake)), instance)
    }

    pub fn with_link(link: Link, instance: Option<String>) -> Self {
        Self {
            model: Model::new(link),
            view: ViewState::default(),
            gov: RepaintGovernor::new(),
            started: Instant::now(),
            instance,
            last_decision: (Rate::Active, Repaint::None),
            daemon_error: None,
            clock_override_s: None,
        }
    }

    pub fn now_s(&self) -> f64 {
        self.clock_override_s
            .unwrap_or_else(|| self.started.elapsed().as_secs_f64())
    }

    /// Draw one frame. `eframe::App::update` calls this; the tests call it directly.
    pub fn run_frame(&mut self, ctx: &egui::Context) {
        let now = self.now_s();
        self.model.pump(now);

        let vp = ctx.input(|i| i.viewport().clone());
        let window = WindowState {
            minimized: vp.minimized,
            focused: vp.focused,
        };
        let gfx = self
            .model
            .snapshot
            .as_ref()
            .map_or(GraphicsSettings::RICH, |s| s.ui.effective_graphics(false));
        self.last_decision = self
            .gov
            .decide(now, window, self.model.any_audio_active(), &gfx);
        // ? The ONLY place a timed repaint is requested. `Repaint::None` (a minimized window)
        // ? requests nothing, so the UI process goes quiet (CLAUDE.md law 5).
        if let Repaint::After(d) = self.last_decision.1 {
            ctx.request_repaint_after(d);
        }

        let narrow = ctx.screen_rect().width() < NARROW_BELOW;
        let mut out = Out::default();
        let Self {
            model,
            view,
            instance,
            daemon_error,
            last_decision,
            ..
        } = self;

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let (text, color) = match &model.conn {
                    Conn::Up => ("connected".to_owned(), Color32::from_rgb(70, 200, 100)),
                    Conn::Connecting => ("connecting…".to_owned(), Color32::GRAY),
                    Conn::Down(_) => (
                        "daemon not running".to_owned(),
                        Color32::from_rgb(235, 110, 90),
                    ),
                };
                ui.label(RichText::new(format!("● {text}")).color(color).small());
                if let Some(n) = &model.notice {
                    ui.label(RichText::new(&n.text).small());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let fps = match last_decision.1 {
                        Repaint::After(d) => Some((1.0 / d.as_secs_f64()).round() as u32),
                        Repaint::None => None,
                    };
                    ui.label(
                        RichText::new(toolbar::rate_text(last_decision.0, fps))
                            .small()
                            .weak(),
                    );
                });
            });
        });

        // The banner, when there is no daemon.
        if let Conn::Down(why) = &model.conn {
            egui::TopBottomPanel::top("banner").show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("The audio daemon (routerd) is not running.").strong());
                    let start = ui.button("Start daemon");
                    crate::widgets::name(&start, egui::WidgetType::Button, "Start daemon");
                    if start.clicked() {
                        *daemon_error = daemon::start(instance.as_deref())
                            .err()
                            .map(|e| format!("cannot start routerd: {e}"));
                    }
                    ui.label(RichText::new("Trying again every second.").weak());
                });
                if !why.is_empty() {
                    ui.label(RichText::new(why).small().weak());
                }
                if let Some(e) = daemon_error {
                    ui.label(
                        RichText::new(&*e)
                            .small()
                            .color(Color32::from_rgb(235, 110, 90)),
                    );
                }
            });
        }

        if let Some(snap) = &model.snapshot {
            if !view.sized {
                view.size_to(snap);
            }
            let mut cx = Ctx {
                snap,
                meters: model.meters.as_ref(),
                devices: &model.devices,
                apps: &model.apps,
                strip_hold: &model.strip_hold,
                bus_hold: &model.bus_hold,
                view,
                out: &mut out,
                narrow,
                gfx,
            };
            egui::TopBottomPanel::top("toolbar").show(ctx, |ui| toolbar::toolbar(ui, &mut cx));
            egui::CentralPanel::default().show(ctx, |ui| match snap.ui.mode {
                UiMode::Simple => simple::simple(ui, &mut cx),
                UiMode::Expert => expert(ui, &mut cx),
            });
        } else {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.label(match &model.conn {
                        Conn::Down(_) => "Waiting for the audio daemon…",
                        _ => "Connecting…",
                    });
                });
            });
        }

        let done = out.finish();
        for n in done.notices {
            self.model.say(n, now);
        }
        let acted = !done.commands.is_empty();
        for c in done.commands {
            self.model.command(c, now);
        }
        if done.refresh_lists {
            self.model.refresh_lists();
        }
        if acted {
            // ? A gesture in a frame that was drawn: show its result next frame instead of waiting
            // ? for the governor's timer (which can be 200 ms away when the meters are quiet).
            // ? This is bounded by user input; it is not a loop (CLAUDE.md law 5).
            ctx.request_repaint();
        }
    }
}

fn expert(ui: &mut egui::Ui, cx: &mut Ctx<'_>) {
    // Narrow window with an inspector open: the inspector takes the whole page (§9.7).
    if cx.narrow {
        if let Some(t) = cx.view.drawer {
            let h = ui.available_height();
            inspector::drawer(ui, cx, t, h);
            return;
        }
    }
    match cx.snap.ui.layout {
        LayoutMode::Matrix => matrix::matrix(ui, cx),
        LayoutMode::ConsoleStrips => strips::console(ui, cx),
        LayoutMode::PatchbayCards => patchbay::patchbay(ui, cx),
    }
}

impl eframe::App for RouterApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.run_frame(ctx);
    }
}
