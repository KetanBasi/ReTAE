//! Desktop UI (egui / eframe, glow renderer). A thin view over `app-core::ControlApi`.
//!
//! * `governor`: when to repaint (PLAN.md §9.9). `link`: the worker that talks to the daemon.
//! * `model`: the newest snapshot/meters plus optimistic commands. `view`: what views draw from.
//! * `strips`, `matrix`, `simple`, `inspector`, `toolbar`: the screens. `widgets`: fader, meter, knob.
//! * `app`: the eframe application that ties them together.

#![forbid(unsafe_code)]

pub mod app;
pub mod daemon;
pub mod governor;
pub mod inspector;
pub mod link;
pub mod matrix;
pub mod model;
pub mod motion;
pub mod patchbay;
pub mod simple;
pub mod strips;
pub mod toolbar;
pub mod view;
pub mod widgets;

#[cfg(test)]
pub mod testing;
#[cfg(test)]
mod ui_tests;
