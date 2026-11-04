//! The Android backend (PLAN.md §6.3). Only builds for Android.
//!
//! | Endpoint | How |
//! |---|---|
//! | `HardwareInput` | AAudio microphone (`VoicePerformance`, LowLatency, exclusive then shared) |
//! | `HardwareOutput` | AAudio output (LowLatency, exclusive then shared) |
//! | `DeviceLoopback{"playback"}` | `AudioRecord` playback capture of media, games and unknown audio |
//! | `ProcessLoopback{uid}` | the same, filtered to one app (`pid` carries the Android **uid**) |
//! | `VirtualInput` / `VirtualMic` | not possible without root: `NotSupported` |
//!
//! Like every non-master device, an Android stream runs on its own clock: inputs are steered by
//! the drift resampler inside the engine, outputs run a `ClockDomainAdapter` in the callback
//! (CLAUDE.md law 2). Decisions that need no phone live in `android_rules`.

mod aaudio;
mod capture;
mod devices;
pub mod jvm;

use crate::android_rules::{DEFAULT_DEVICE_ID, InputPreset};
use crate::backend::{AppInfo, AudioBackend, AudioStream, BackendError, Capabilities, DeviceInfo};
use crate::ring_io::{CaptureState, PlaybackState, Slot, take_input, take_output};
use audio_engine::{InputPort, OutputPort};
use audio_types::{MAX_CHANNELS, SinkEndpoint, SourceEndpoint};

#[derive(Clone, Copy, Debug)]
pub struct AndroidOptions {
    /// Audio kept queued between the mixer and an output stream (the `ClockDomainAdapter` target).
    pub output_target_ms: f32,
    /// Try exclusive (MMAP) streams first. Off, only shared streams are opened.
    pub exclusive: bool,
}

impl Default for AndroidOptions {
    fn default() -> Self {
        Self {
            output_target_ms: 8.0,
            exclusive: true,
        }
    }
}

pub struct AndroidBackend {
    options: AndroidOptions,
}

impl AndroidBackend {
    /// Needs `jvm::install` to have run (the app does that at start-up).
    pub fn connect(options: AndroidOptions) -> Result<Self, BackendError> {
        if !jvm::is_installed() {
            return Err(BackendError::Io(
                "the Android context was not installed (call platform_io::android::jvm::install)"
                    .into(),
            ));
        }
        Ok(Self { options })
    }
}

fn channels_ok(channels: u8) -> Result<(), BackendError> {
    if channels == 0 || usize::from(channels) > MAX_CHANNELS {
        Err(BackendError::BadChannels(channels))
    } else {
        Ok(())
    }
}

// SECTION: Backend
impl AudioBackend for AndroidBackend {
    fn name(&self) -> &'static str {
        "aaudio"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            hardware_input: true,
            hardware_output: true,
            // ? Playback capture of everything, and of one app. Whether it works *now* depends
            // ? on the screen-capture grant; without it opening answers `NEEDS_PROJECTION`.
            device_loopback: true,
            process_loopback: true,
            // Virtual devices for other apps need root.
            virtual_inputs: 0,
            virtual_mics: 0,
        }
    }

    fn devices(&self) -> Vec<DeviceInfo> {
        devices::list_devices()
    }

    fn apps(&self) -> Vec<AppInfo> {
        devices::list_apps()
    }

    fn open_input(
        &mut self,
        source: &SourceEndpoint,
        channels: u8,
        port: InputPort,
    ) -> Result<Box<dyn AudioStream>, (BackendError, InputPort)> {
        let slot = Slot::new(CaptureState::new(port, usize::from(channels)));
        let fail = |e| Err((e, take_input(&slot)));
        if let Err(e) = channels_ok(channels) {
            return fail(e);
        }
        let opened: Result<Box<dyn AudioStream>, BackendError> = match source {
            SourceEndpoint::HardwareInput { device_id } => aaudio::open_input(
                device_id.as_str(),
                InputPreset::VoicePerformance,
                channels,
                &slot,
                &self.options,
            )
            .map(|s| Box::new(s) as Box<dyn AudioStream>),
            // * The contract's own Android microphone: the default device with the preset the
            // * strip asked for.
            SourceEndpoint::AndroidMic { preset } => aaudio::open_input(
                DEFAULT_DEVICE_ID,
                InputPreset::from_contract(*preset),
                channels,
                &slot,
                &self.options,
            )
            .map(|s| Box::new(s) as Box<dyn AudioStream>),
            SourceEndpoint::DeviceLoopback { .. }
            | SourceEndpoint::ProcessLoopback { .. }
            | SourceEndpoint::AndroidPlaybackCapture { .. } => {
                capture::open(source, channels, &slot).map(|s| Box::new(s) as Box<dyn AudioStream>)
            }
            other => Err(BackendError::NotSupported(format!(
                "{other:?}: Android cannot create virtual inputs without root"
            ))),
        };
        match opened {
            Ok(stream) => Ok(stream),
            Err(e) => fail(e),
        }
    }

    fn open_output(
        &mut self,
        sink: &SinkEndpoint,
        channels: u8,
        port: OutputPort,
    ) -> Result<Box<dyn AudioStream>, (BackendError, OutputPort)> {
        let slot = Slot::new(PlaybackState::new(
            port,
            usize::from(channels),
            self.options.output_target_ms,
        ));
        let fail = |e| Err((e, take_output(&slot)));
        if let Err(e) = channels_ok(channels) {
            return fail(e);
        }
        let SinkEndpoint::HardwareOutput { device_id } = sink else {
            return fail(BackendError::NotSupported(format!(
                "{sink:?}: Android cannot create a virtual microphone without root"
            )));
        };
        match aaudio::open_output(device_id.as_str(), channels, &slot, &self.options) {
            Ok(stream) => Ok(Box::new(stream)),
            Err(e) => fail(e),
        }
    }
}
// !SECTION: Backend
