//! The Android foreground-service lifecycle as a pure state machine (PLAN.md §6.3 steps 1-6).
//!
//! * No JNI in here. `Lifecycle::handle(event)` returns the `Action`s the Android glue must
//!   perform, in order. That keeps the rules that are easy to get wrong (what may start while the
//!   UI is hidden, the order consent → foreground → `getMediaProjection`, the single-use token,
//!   "app audio paused") testable on the host.
//! * The glue (`android_glue`) turns each `Action` into JNI calls and feeds the results back as
//!   `Event`s.

/// Which foreground-service types are active right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fgs {
    None,
    Mic,
    MicAndProjection,
}

/// `ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE`.
pub const FGS_TYPE_MICROPHONE: i32 = 128;
/// `ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION`.
pub const FGS_TYPE_MEDIA_PROJECTION: i32 = 32;

impl Fgs {
    /// The `foregroundServiceType` bits to hand to `startForeground`.
    pub const fn type_mask(self) -> i32 {
        match self {
            Self::None => 0,
            Self::Mic => FGS_TYPE_MICROPHONE,
            Self::MicAndProjection => FGS_TYPE_MICROPHONE | FGS_TYPE_MEDIA_PROJECTION,
        }
    }
}

/// Where the app-audio (screen-capture) grant is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Projection {
    /// No grant, nobody asked.
    Off,
    /// The consent dialog is open.
    AwaitingConsent,
    /// Consent is in hand; the service is switching to the projection type.
    StartingForeground,
    /// The service has the type; `getMediaProjection` is being called.
    GettingProjection,
    /// Capturing.
    Active,
    /// The system ended the projection (screen lock on Android 15 QPR1+, or the user).
    Paused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The UI came to the foreground or went away.
    UiVisible(bool),
    /// Something now needs the engine to run (a stream is on).
    StreamingWanted(bool),
    /// The user wants app audio (first time, or "tap to resume").
    AppAudioRequested,
    /// The consent dialog answered.
    ConsentResult { granted: bool },
    /// `startForeground` returned.
    ForegroundStarted { with_projection: bool },
    /// `startForeground` was refused (permission, background start).
    ForegroundRefused,
    /// `getMediaProjection` returned a token.
    ProjectionReady,
    /// `getMediaProjection` failed.
    ProjectionFailed,
    /// `MediaProjection.Callback.onStop`.
    ProjectionStopped,
    /// The user turned app audio off.
    AppAudioDismissed,
    /// The service was destroyed (by us or by the system).
    ServiceDestroyed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Start the foreground service with the microphone type (UI must be visible).
    StartMicService,
    /// Show the screen-capture consent dialog.
    LaunchConsent,
    /// `startForeground(MICROPHONE | MEDIA_PROJECTION)`.
    StartForegroundWithProjection,
    /// `startForeground(MICROPHONE)` again, which drops the projection type.
    StartForegroundMicOnly,
    /// `getMediaProjection(resultCode, data)` and register the stop callback.
    GetProjection,
    /// Stop and drop the token, and tell the capture threads it is gone.
    ReleaseProjection,
    /// Take the Wi-Fi low-latency lock and the partial wake lock.
    AcquireLocks,
    /// Release both.
    ReleaseLocks,
    /// Show "App audio paused - tap to resume".
    NotifyAppAudioPaused,
    /// Back to the normal "running" notification text.
    NotifyRunning,
    /// Tell the UI why something did not start.
    Warn(Warning),
    /// `stopSelf`.
    StopService,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Warning {
    /// A microphone service cannot start from the background (Android 14).
    NeedsUiVisible,
    ConsentDenied,
    ProjectionFailed,
    ForegroundRefused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lifecycle {
    ui_visible: bool,
    streaming: bool,
    fgs: Fgs,
    projection: Projection,
    locks: bool,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl Lifecycle {
    pub const fn new() -> Self {
        Self {
            ui_visible: false,
            streaming: false,
            fgs: Fgs::None,
            projection: Projection::Off,
            locks: false,
        }
    }

    pub fn fgs(&self) -> Fgs {
        self.fgs
    }

    pub fn projection(&self) -> Projection {
        self.projection
    }

    pub fn locks_held(&self) -> bool {
        self.locks
    }

    /// The "app audio paused - tap to resume" banner is up.
    pub fn app_audio_paused(&self) -> bool {
        self.projection == Projection::Paused
    }

    /// Is a screen-capture consent result expected right now? Anything else is stale.
    pub fn awaiting_consent(&self) -> bool {
        self.projection == Projection::AwaitingConsent
    }

    pub fn handle(&mut self, event: Event) -> Vec<Action> {
        let mut out = Vec::new();
        match event {
            Event::UiVisible(visible) => self.ui_visible = visible,
            Event::StreamingWanted(on) => self.on_streaming(on, &mut out),
            Event::AppAudioRequested => self.on_app_audio_requested(&mut out),
            Event::ConsentResult { granted } => self.on_consent(granted, &mut out),
            Event::ForegroundStarted { with_projection } => {
                self.on_foreground_started(with_projection, &mut out);
            }
            Event::ForegroundRefused => self.on_foreground_refused(&mut out),
            Event::ProjectionReady => {
                if self.projection == Projection::GettingProjection {
                    self.projection = Projection::Active;
                    out.push(Action::NotifyRunning);
                }
            }
            Event::ProjectionFailed => {
                if self.projection == Projection::GettingProjection {
                    self.drop_projection_type(&mut out);
                    self.projection = Projection::Off;
                    out.push(Action::Warn(Warning::ProjectionFailed));
                }
            }
            Event::ProjectionStopped => self.on_projection_stopped(&mut out),
            Event::AppAudioDismissed => {
                if self.projection != Projection::Off {
                    let had_type = self.fgs == Fgs::MicAndProjection;
                    out.push(Action::ReleaseProjection);
                    self.projection = Projection::Off;
                    if had_type {
                        self.fgs = Fgs::Mic;
                        out.push(Action::StartForegroundMicOnly);
                    }
                    out.push(Action::NotifyRunning);
                }
            }
            Event::ServiceDestroyed => self.on_service_destroyed(&mut out),
        }
        out
    }

    fn on_streaming(&mut self, on: bool, out: &mut Vec<Action>) {
        if on == self.streaming {
            return;
        }
        if on {
            // Step 2: the microphone service may only start while the UI is visible.
            if self.fgs == Fgs::None {
                if !self.ui_visible {
                    out.push(Action::Warn(Warning::NeedsUiVisible));
                    return;
                }
                out.push(Action::StartMicService);
            }
            self.streaming = true;
            // ? Locks only while streaming (CLAUDE.md law 6).
            if !self.locks {
                self.locks = true;
                out.push(Action::AcquireLocks);
            }
        } else {
            self.streaming = false;
            if self.locks {
                self.locks = false;
                out.push(Action::ReleaseLocks);
            }
            if self.projection != Projection::Off {
                out.push(Action::ReleaseProjection);
                self.projection = Projection::Off;
            }
            if self.fgs != Fgs::None {
                self.fgs = Fgs::None;
                out.push(Action::StopService);
            }
        }
    }

    fn on_app_audio_requested(&mut self, out: &mut Vec<Action>) {
        match self.projection {
            // Already on its way or running.
            Projection::AwaitingConsent
            | Projection::StartingForeground
            | Projection::GettingProjection
            | Projection::Active => {}
            Projection::Off | Projection::Paused => {
                // Step 3 needs the mic service up first, and a visible UI to start it.
                if self.fgs == Fgs::None {
                    if !self.ui_visible {
                        out.push(Action::Warn(Warning::NeedsUiVisible));
                        return;
                    }
                    out.push(Action::StartMicService);
                }
                // Step 4: the token is single-use, so every request is a fresh dialog.
                self.projection = Projection::AwaitingConsent;
                out.push(Action::LaunchConsent);
            }
        }
    }

    fn on_consent(&mut self, granted: bool, out: &mut Vec<Action>) {
        if self.projection != Projection::AwaitingConsent {
            // A late or duplicate result: the grant it carries is not usable.
            return;
        }
        if granted {
            self.projection = Projection::StartingForeground;
            out.push(Action::StartForegroundWithProjection);
        } else {
            self.projection = Projection::Off;
            out.push(Action::Warn(Warning::ConsentDenied));
        }
    }

    fn on_foreground_started(&mut self, with_projection: bool, out: &mut Vec<Action>) {
        if with_projection {
            if self.projection == Projection::StartingForeground {
                self.fgs = Fgs::MicAndProjection;
                self.projection = Projection::GettingProjection;
                // Step 3: `getMediaProjection` only now, after `startForeground`.
                out.push(Action::GetProjection);
            }
        } else if self.fgs == Fgs::None {
            self.fgs = Fgs::Mic;
        }
    }

    fn on_foreground_refused(&mut self, out: &mut Vec<Action>) {
        out.push(Action::Warn(Warning::ForegroundRefused));
        if self.projection == Projection::StartingForeground {
            self.projection = Projection::Off;
        }
    }

    fn on_projection_stopped(&mut self, out: &mut Vec<Action>) {
        match self.projection {
            Projection::Off | Projection::Paused => {}
            _ => {
                // Step 5: the mic keeps going; only app audio ends.
                out.push(Action::ReleaseProjection);
                self.projection = Projection::Paused;
                if self.fgs == Fgs::MicAndProjection {
                    self.fgs = Fgs::Mic;
                    out.push(Action::StartForegroundMicOnly);
                }
                out.push(Action::NotifyAppAudioPaused);
            }
        }
    }

    fn drop_projection_type(&mut self, out: &mut Vec<Action>) {
        if self.fgs == Fgs::MicAndProjection {
            self.fgs = Fgs::Mic;
            out.push(Action::StartForegroundMicOnly);
        }
    }

    fn on_service_destroyed(&mut self, out: &mut Vec<Action>) {
        if self.locks {
            out.push(Action::ReleaseLocks);
        }
        if self.projection != Projection::Off {
            out.push(Action::ReleaseProjection);
        }
        self.fgs = Fgs::None;
        self.projection = Projection::Off;
        self.streaming = false;
        self.locks = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visible() -> Lifecycle {
        let mut l = Lifecycle::new();
        l.handle(Event::UiVisible(true));
        l
    }

    /// A phone with mic running and app audio fully active.
    fn running_with_projection() -> Lifecycle {
        let mut l = visible();
        l.handle(Event::StreamingWanted(true));
        l.handle(Event::ForegroundStarted {
            with_projection: false,
        });
        l.handle(Event::AppAudioRequested);
        l.handle(Event::ConsentResult { granted: true });
        l.handle(Event::ForegroundStarted {
            with_projection: true,
        });
        l.handle(Event::ProjectionReady);
        assert_eq!(l.projection(), Projection::Active);
        l
    }

    #[test]
    fn mic_service_starts_only_while_the_ui_is_visible() {
        let mut l = Lifecycle::new();
        assert_eq!(
            l.handle(Event::StreamingWanted(true)),
            vec![Action::Warn(Warning::NeedsUiVisible)]
        );
        assert!(!l.locks_held());
        l.handle(Event::UiVisible(true));
        assert_eq!(
            l.handle(Event::StreamingWanted(true)),
            vec![Action::StartMicService, Action::AcquireLocks]
        );
        assert!(l.locks_held());
    }

    #[test]
    fn hiding_the_ui_does_not_stop_a_running_service() {
        let mut l = visible();
        l.handle(Event::StreamingWanted(true));
        l.handle(Event::ForegroundStarted {
            with_projection: false,
        });
        assert!(l.handle(Event::UiVisible(false)).is_empty());
        assert_eq!(l.fgs(), Fgs::Mic);
        assert!(l.locks_held());
    }

    #[test]
    fn app_audio_follows_consent_then_foreground_then_projection() {
        let mut l = visible();
        l.handle(Event::StreamingWanted(true));
        l.handle(Event::ForegroundStarted {
            with_projection: false,
        });
        assert_eq!(
            l.handle(Event::AppAudioRequested),
            vec![Action::LaunchConsent]
        );
        // The projection type must not be requested before consent.
        assert_eq!(l.fgs(), Fgs::Mic);
        assert_eq!(
            l.handle(Event::ConsentResult { granted: true }),
            vec![Action::StartForegroundWithProjection]
        );
        // `getMediaProjection` only after `startForeground` returned.
        assert_eq!(
            l.handle(Event::ForegroundStarted {
                with_projection: true
            }),
            vec![Action::GetProjection]
        );
        assert_eq!(l.fgs(), Fgs::MicAndProjection);
        assert_eq!(
            l.handle(Event::ProjectionReady),
            vec![Action::NotifyRunning]
        );
        assert_eq!(l.projection(), Projection::Active);
    }

    #[test]
    fn app_audio_request_with_no_service_starts_the_mic_one_first() {
        let mut l = visible();
        assert_eq!(
            l.handle(Event::AppAudioRequested),
            vec![Action::StartMicService, Action::LaunchConsent]
        );
    }

    #[test]
    fn app_audio_request_from_the_background_is_refused() {
        let mut l = Lifecycle::new();
        assert_eq!(
            l.handle(Event::AppAudioRequested),
            vec![Action::Warn(Warning::NeedsUiVisible)]
        );
        assert_eq!(l.projection(), Projection::Off);
    }

    #[test]
    fn denied_consent_leaves_the_mic_running() {
        let mut l = visible();
        l.handle(Event::StreamingWanted(true));
        l.handle(Event::ForegroundStarted {
            with_projection: false,
        });
        l.handle(Event::AppAudioRequested);
        assert_eq!(
            l.handle(Event::ConsentResult { granted: false }),
            vec![Action::Warn(Warning::ConsentDenied)]
        );
        assert_eq!(l.projection(), Projection::Off);
        assert_eq!(l.fgs(), Fgs::Mic);
    }

    #[test]
    fn a_consent_result_nobody_asked_for_is_ignored() {
        let mut l = visible();
        assert!(l.handle(Event::ConsentResult { granted: true }).is_empty());
        assert_eq!(l.projection(), Projection::Off);
        assert!(!l.awaiting_consent());
    }

    #[test]
    fn the_token_is_single_use() {
        let mut l = running_with_projection();
        l.handle(Event::ProjectionStopped);
        assert_eq!(l.projection(), Projection::Paused);
        // A second result for the old dialog is stale, a fresh grant needs a fresh dialog.
        assert!(l.handle(Event::ConsentResult { granted: true }).is_empty());
        assert_eq!(
            l.handle(Event::AppAudioRequested),
            vec![Action::LaunchConsent]
        );
        assert!(l.awaiting_consent());
    }

    #[test]
    fn projection_stop_pauses_app_audio_and_keeps_the_mic() {
        let mut l = running_with_projection();
        assert_eq!(
            l.handle(Event::ProjectionStopped),
            vec![
                Action::ReleaseProjection,
                Action::StartForegroundMicOnly,
                Action::NotifyAppAudioPaused
            ]
        );
        assert!(l.app_audio_paused());
        assert_eq!(l.fgs(), Fgs::Mic);
        assert!(l.locks_held());
        // A second stop callback changes nothing.
        assert!(l.handle(Event::ProjectionStopped).is_empty());
    }

    #[test]
    fn resume_after_pause_goes_through_consent_again() {
        let mut l = running_with_projection();
        l.handle(Event::ProjectionStopped);
        assert_eq!(
            l.handle(Event::AppAudioRequested),
            vec![Action::LaunchConsent]
        );
        l.handle(Event::ConsentResult { granted: true });
        assert_eq!(
            l.handle(Event::ForegroundStarted {
                with_projection: true
            }),
            vec![Action::GetProjection]
        );
        l.handle(Event::ProjectionReady);
        assert_eq!(l.projection(), Projection::Active);
        assert!(!l.app_audio_paused());
    }

    #[test]
    fn a_failed_projection_drops_back_to_the_mic_type() {
        let mut l = visible();
        l.handle(Event::StreamingWanted(true));
        l.handle(Event::ForegroundStarted {
            with_projection: false,
        });
        l.handle(Event::AppAudioRequested);
        l.handle(Event::ConsentResult { granted: true });
        l.handle(Event::ForegroundStarted {
            with_projection: true,
        });
        assert_eq!(
            l.handle(Event::ProjectionFailed),
            vec![
                Action::StartForegroundMicOnly,
                Action::Warn(Warning::ProjectionFailed)
            ]
        );
        assert_eq!(l.fgs(), Fgs::Mic);
        assert_eq!(l.projection(), Projection::Off);
    }

    #[test]
    fn a_refused_foreground_start_goes_back_to_off() {
        let mut l = visible();
        l.handle(Event::StreamingWanted(true));
        l.handle(Event::ForegroundStarted {
            with_projection: false,
        });
        l.handle(Event::AppAudioRequested);
        l.handle(Event::ConsentResult { granted: true });
        assert_eq!(
            l.handle(Event::ForegroundRefused),
            vec![Action::Warn(Warning::ForegroundRefused)]
        );
        assert_eq!(l.projection(), Projection::Off);
    }

    #[test]
    fn dismissing_app_audio_returns_to_the_mic_type() {
        let mut l = running_with_projection();
        assert_eq!(
            l.handle(Event::AppAudioDismissed),
            vec![
                Action::ReleaseProjection,
                Action::StartForegroundMicOnly,
                Action::NotifyRunning
            ]
        );
        assert_eq!(l.projection(), Projection::Off);
    }

    #[test]
    fn stopping_streaming_releases_everything_in_order() {
        let mut l = running_with_projection();
        assert_eq!(
            l.handle(Event::StreamingWanted(false)),
            vec![
                Action::ReleaseLocks,
                Action::ReleaseProjection,
                Action::StopService
            ]
        );
        assert_eq!(l.fgs(), Fgs::None);
        assert!(!l.locks_held());
    }

    #[test]
    fn the_system_killing_the_service_releases_locks_and_the_token() {
        let mut l = running_with_projection();
        assert_eq!(
            l.handle(Event::ServiceDestroyed),
            vec![Action::ReleaseLocks, Action::ReleaseProjection]
        );
        assert_eq!(l, {
            let mut fresh = Lifecycle::new();
            fresh.handle(Event::UiVisible(true));
            fresh
        });
    }

    #[test]
    fn the_service_type_bits_match_the_manifest_and_the_platform() {
        // ServiceInfo constants (checked against android.jar, API 35).
        assert_eq!(Fgs::None.type_mask(), 0);
        assert_eq!(Fgs::Mic.type_mask(), 0x80);
        assert_eq!(Fgs::MicAndProjection.type_mask(), 0x80 | 0x20);
    }

    #[test]
    fn streaming_wanted_twice_takes_the_locks_once() {
        let mut l = visible();
        l.handle(Event::StreamingWanted(true));
        assert!(l.handle(Event::StreamingWanted(true)).is_empty());
    }
}
