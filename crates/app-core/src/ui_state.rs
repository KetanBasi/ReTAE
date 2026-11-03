//! UI-side settings (PLAN.md §9.8). They live here, not in `audio-types`, so the engine
//! contract stays UI-agnostic. They are saved in the profile and carried over IPC so every UI
//! attached to the same daemon shows the same layout.

use audio_types::{MAX_BUSES, MAX_STRIPS};
use serde::{Deserialize, Serialize};

// SECTION: UI settings
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum UiMode {
    #[default]
    Simple,
    Expert,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutMode {
    #[default]
    ConsoleStrips,
    Matrix,
    PatchbayCards,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GraphicsProfile {
    #[default]
    RichMotion,
    EcoReducedMotion,
    Custom,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphicsSettings {
    pub profile: GraphicsProfile,
    /// Rich 60 / Eco 30.
    pub active_fps: u8,
    /// Rich 15 / Eco 5.
    pub silent_fps: u8,
    pub card_spring_physics: bool,
    pub wire_particles: bool,
    pub smooth_transitions: bool,
    pub meter_peak_hold: bool,
}

impl GraphicsSettings {
    pub const RICH: Self = Self {
        profile: GraphicsProfile::RichMotion,
        active_fps: 60,
        silent_fps: 15,
        card_spring_physics: true,
        wire_particles: true,
        smooth_transitions: true,
        meter_peak_hold: true,
    };
    pub const ECO: Self = Self {
        profile: GraphicsProfile::EcoReducedMotion,
        active_fps: 30,
        silent_fps: 5,
        card_spring_physics: false,
        wire_particles: false,
        smooth_transitions: false,
        meter_peak_hold: false,
    };

    /// Frame rates a UI may use: at least 1 fps, never above 240, silent never above active.
    pub fn sanitized(self) -> Self {
        let active_fps = self.active_fps.clamp(1, 240);
        Self {
            active_fps,
            silent_fps: self.silent_fps.clamp(1, active_fps),
            ..self
        }
    }
}

/// Per-channel view state, kept out of `StripConfig` so the engine config stays UI-agnostic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelViewState {
    pub collapsed_in_strips: bool,
    pub expanded_in_cards: bool,
    pub card_order: u8,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UiLayout {
    pub mode: UiMode,
    pub layout: LayoutMode,
    /// `None` = RICH on desktop, ECO on Android.
    pub graphics: Option<GraphicsSettings>,
    pub strips: [ChannelViewState; MAX_STRIPS],
    pub buses: [ChannelViewState; MAX_BUSES],
}

impl UiLayout {
    /// The graphics settings to use on this platform.
    pub fn effective_graphics(&self, android: bool) -> GraphicsSettings {
        self.graphics
            .unwrap_or(if android {
                GraphicsSettings::ECO
            } else {
                GraphicsSettings::RICH
            })
            .sanitized()
    }

    /// Fix values a hand-edited profile or a misbehaving client could put in.
    pub fn sanitized(mut self) -> Self {
        self.graphics = self.graphics.map(GraphicsSettings::sanitized);
        self
    }
}
// !SECTION: UI settings

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphics_default_depends_on_the_platform() {
        let ui = UiLayout::default();
        assert_eq!(ui.effective_graphics(false), GraphicsSettings::RICH);
        assert_eq!(ui.effective_graphics(true), GraphicsSettings::ECO);
        let custom = UiLayout {
            graphics: Some(GraphicsSettings {
                profile: GraphicsProfile::Custom,
                active_fps: 45,
                ..GraphicsSettings::ECO
            }),
            ..UiLayout::default()
        };
        assert_eq!(custom.effective_graphics(true).active_fps, 45);
    }

    #[test]
    fn silly_frame_rates_are_fixed() {
        let s = GraphicsSettings {
            active_fps: 0,
            silent_fps: 200,
            ..GraphicsSettings::RICH
        }
        .sanitized();
        assert_eq!((s.active_fps, s.silent_fps), (1, 1));
        let s = GraphicsSettings {
            active_fps: 255,
            silent_fps: 255,
            ..GraphicsSettings::RICH
        }
        .sanitized();
        assert_eq!((s.active_fps, s.silent_fps), (240, 240));
    }
}
