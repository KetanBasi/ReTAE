//! The repaint governor (PLAN.md §9.9, CLAUDE.md law 5). Pure: it gets the facts of this frame
//! and says when the next frame is wanted. `app.rs` turns the answer into one
//! `request_repaint_after` call, or into none at all.
//!
//! * UI window minimized or hidden: no repaint (and, because the UI polls the daemon from its own
//!   frames, no polling either).
//! * Not focused (this is also the answer on Wayland, which never says "minimized"), or no audio
//!   for more than 500 ms: `silent_fps`.
//! * Otherwise: `active_fps`.
//!
//! There is deliberately no way to ask for "as soon as possible": a repaint that the user's own
//! input causes is egui's business, not the governor's.

use app_core::ui_state::GraphicsSettings;
use std::time::Duration;

/// How long the meters may stay at zero before the UI drops to `silent_fps`.
pub const SILENCE_BEFORE_SLOWDOWN_S: f64 = 0.5;

/// What the window system says about the window, as far as it says anything.
/// `None` = the platform does not report it (Wayland never reports "minimized").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WindowState {
    pub minimized: Option<bool>,
    pub focused: Option<bool>,
}

impl WindowState {
    pub const VISIBLE_AND_FOCUSED: Self = Self {
        minimized: Some(false),
        focused: Some(true),
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repaint {
    /// Do not ask for a repaint. The next one comes from input or from the OS.
    None,
    After(Duration),
}

/// Why the governor chose what it did. Shown in the status line and used by the tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rate {
    Paused,
    Silent,
    Active,
}

#[derive(Debug, Default)]
pub struct RepaintGovernor {
    /// Seconds (any monotonic clock) at which audio was last seen, or at which we started
    /// watching if none was.
    last_audio_s: Option<f64>,
}

impl RepaintGovernor {
    pub fn new() -> Self {
        Self::default()
    }

    /// `now_s`: a monotonic clock in seconds. `audio_active`: the daemon's `any_audio_active`
    /// from the newest meters.
    pub fn decide(
        &mut self,
        now_s: f64,
        window: WindowState,
        audio_active: bool,
        graphics: &GraphicsSettings,
    ) -> (Rate, Repaint) {
        if audio_active {
            self.last_audio_s = Some(now_s);
        }
        // ? Before the first frame has seen sound, silence is counted from the first call, so a
        // ? freshly opened UI shows active meters for half a second, then slows down.
        let since = *self.last_audio_s.get_or_insert(now_s);

        if window.minimized == Some(true) {
            return (Rate::Paused, Repaint::None);
        }
        let g = graphics.sanitized();
        let unfocused = window.focused == Some(false);
        let silent = now_s - since > SILENCE_BEFORE_SLOWDOWN_S;
        if unfocused || silent {
            (Rate::Silent, after(g.silent_fps))
        } else {
            (Rate::Active, after(g.active_fps))
        }
    }
}

fn after(fps: u8) -> Repaint {
    Repaint::After(Duration::from_secs_f64(1.0 / f64::from(fps.max(1))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fps(r: Repaint) -> Option<u32> {
        match r {
            Repaint::None => None,
            Repaint::After(d) => Some((1.0 / d.as_secs_f64()).round() as u32),
        }
    }

    const BOTH: [GraphicsSettings; 2] = [GraphicsSettings::RICH, GraphicsSettings::ECO];

    #[test]
    fn minimized_means_no_repaint_in_rich_and_eco() {
        for g in BOTH {
            let mut gov = RepaintGovernor::new();
            let min = WindowState {
                minimized: Some(true),
                focused: Some(false),
            };
            // Loud audio does not matter: nobody can see the meters.
            assert_eq!(
                gov.decide(0.0, min, true, &g),
                (Rate::Paused, Repaint::None)
            );
            assert_eq!(
                gov.decide(10.0, min, true, &g),
                (Rate::Paused, Repaint::None)
            );
        }
    }

    #[test]
    fn audio_in_a_visible_window_runs_at_active_fps() {
        let mut gov = RepaintGovernor::new();
        let (rate, r) = gov.decide(
            0.0,
            WindowState::VISIBLE_AND_FOCUSED,
            true,
            &GraphicsSettings::RICH,
        );
        assert_eq!((rate, fps(r)), (Rate::Active, Some(60)));
        let mut gov = RepaintGovernor::new();
        let (_, r) = gov.decide(
            0.0,
            WindowState::VISIBLE_AND_FOCUSED,
            true,
            &GraphicsSettings::ECO,
        );
        assert_eq!(fps(r), Some(30));
    }

    #[test]
    fn silence_for_more_than_half_a_second_drops_to_silent_fps() {
        for (g, active, silent) in [
            (GraphicsSettings::RICH, 60, 15),
            (GraphicsSettings::ECO, 30, 5),
        ] {
            let mut gov = RepaintGovernor::new();
            let w = WindowState::VISIBLE_AND_FOCUSED;
            assert_eq!(fps(gov.decide(0.0, w, true, &g).1), Some(active));
            // Exactly 500 ms of silence is still "active"; the rule is "more than".
            assert_eq!(fps(gov.decide(0.5, w, false, &g).1), Some(active));
            assert_eq!(fps(gov.decide(0.51, w, false, &g).1), Some(silent));
            // Sound again: straight back.
            assert_eq!(fps(gov.decide(1.0, w, true, &g).1), Some(active));
        }
    }

    #[test]
    fn a_fresh_ui_counts_silence_from_its_first_frame() {
        let mut gov = RepaintGovernor::new();
        let w = WindowState::VISIBLE_AND_FOCUSED;
        let g = GraphicsSettings::RICH;
        assert_eq!(fps(gov.decide(100.0, w, false, &g).1), Some(60));
        assert_eq!(fps(gov.decide(100.6, w, false, &g).1), Some(15));
    }

    #[test]
    fn unfocused_runs_at_silent_fps_even_with_audio() {
        // This is also what Wayland gets: `minimized` is None there, `focused` is known.
        let mut gov = RepaintGovernor::new();
        let w = WindowState {
            minimized: None,
            focused: Some(false),
        };
        let (rate, r) = gov.decide(0.0, w, true, &GraphicsSettings::RICH);
        assert_eq!((rate, fps(r)), (Rate::Silent, Some(15)));
    }

    #[test]
    fn a_platform_that_reports_nothing_counts_as_visible() {
        let mut gov = RepaintGovernor::new();
        let (rate, _) = gov.decide(0.0, WindowState::default(), true, &GraphicsSettings::RICH);
        assert_eq!(rate, Rate::Active);
    }

    #[test]
    fn silly_settings_cannot_make_a_busy_loop() {
        let mut gov = RepaintGovernor::new();
        let g = GraphicsSettings {
            active_fps: 0,
            silent_fps: 0,
            ..GraphicsSettings::RICH
        };
        let (_, r) = gov.decide(0.0, WindowState::VISIBLE_AND_FOCUSED, true, &g);
        assert_eq!(fps(r), Some(1));
    }
}
