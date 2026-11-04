//! Linux fallback on ALSA through `cpal`, for machines without PipeWire (PLAN.md §6.2).
//!
//! Hardware inputs and outputs only. ALSA has no way to create a virtual sink or microphone,
//! to capture another program's audio, or to monitor an output, so those endpoints answer
//! `NotSupported` and `Capabilities` says so (the UI greys them out).
//!
//! Like every non-master device, an ALSA device runs on its own clock. Inputs are steered by
//! the drift resampler inside the engine; outputs run the same `ClockDomainAdapter` here, in
//! the device callback (CLAUDE.md law 2).

use crate::backend::{
    AppInfo, AudioBackend, AudioStream, BackendError, Capabilities, ClosedPort, DeviceInfo,
    DeviceKind, StreamStats,
};
use crate::ring_io::{CaptureState, Counters, PlaybackState, Slot, take_input, take_output};
use audio_engine::{InputPort, OutputPort};
use audio_types::{
    ENGINE_BLOCK_FRAMES, ENGINE_SAMPLE_RATE, MAX_CHANNELS, SinkEndpoint, SourceEndpoint,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, ErrorKind, StreamConfig};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

/// Samples converted per step when the device wants 16-bit integers.
const I16_STAGE: usize = 1024;

#[derive(Clone, Copy, Debug)]
pub struct AlsaOptions {
    /// Audio kept queued between the mixer and an output device. Same meaning as the PipeWire
    /// option of the same name.
    pub output_target_ms: f32,
}

impl Default for AlsaOptions {
    fn default() -> Self {
        Self {
            output_target_ms: 8.0,
        }
    }
}

pub struct AlsaBackend {
    host: cpal::Host,
    options: AlsaOptions,
}

impl AlsaBackend {
    /// Always succeeds on Linux; a machine with no sound card just lists no devices.
    pub fn connect(options: AlsaOptions) -> Result<Self, BackendError> {
        Ok(Self {
            host: cpal::default_host(),
            options,
        })
    }

    /// Find a device by the id `devices()` printed. The id also has to name a device of the
    /// right direction, or the stream would fail later with a less helpful message.
    fn find(&self, id: &str, kind: DeviceKind) -> Result<cpal::Device, BackendError> {
        let parsed: cpal::DeviceId = id
            .parse()
            .map_err(|_| BackendError::NoSuchDevice(id.to_owned()))?;
        let device = self
            .host
            .device_by_id(&parsed)
            .ok_or_else(|| BackendError::NoSuchDevice(id.to_owned()))?;
        let usable = match kind {
            DeviceKind::Input => device.supports_input(),
            DeviceKind::Output => device.supports_output(),
        };
        if usable {
            Ok(device)
        } else {
            Err(BackendError::NoSuchDevice(id.to_owned()))
        }
    }
}

fn map_error(e: &cpal::Error, what: &str) -> BackendError {
    match e.kind() {
        ErrorKind::DeviceBusy => BackendError::Busy(what.to_owned()),
        ErrorKind::DeviceNotAvailable => BackendError::NoSuchDevice(what.to_owned()),
        _ => BackendError::Io(e.to_string()),
    }
}

// SECTION: Backend
impl AudioBackend for AlsaBackend {
    fn name(&self) -> &'static str {
        "alsa"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            hardware_input: true,
            hardware_output: true,
            ..Capabilities::default()
        }
    }

    fn devices(&self) -> Vec<DeviceInfo> {
        // ? Asking a device whether it can record or play opens it briefly, so a refresh of
        // ? this list is not free. The control plane only does it when the user asks.
        let default_in = self.host.default_input_device().and_then(|d| d.id().ok());
        let default_out = self.host.default_output_device().and_then(|d| d.id().ok());
        let Ok(all) = self.host.devices() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for device in all {
            let Ok(id) = device.id() else { continue };
            let name = device
                .description()
                .map(|d| d.name().to_owned())
                .unwrap_or_else(|_| id.to_string());
            let mut push = |kind, channels: Option<u16>, default: &Option<cpal::DeviceId>| {
                out.push(DeviceInfo {
                    id: id.to_string(),
                    name: name.clone(),
                    kind,
                    channels: channels.map_or(2, |c| c.clamp(1, MAX_CHANNELS as u16) as u8),
                    is_default: default.as_ref() == Some(&id),
                });
            };
            if device.supports_input() {
                let ch = device.default_input_config().ok().map(|c| c.channels());
                push(DeviceKind::Input, ch, &default_in);
            }
            if device.supports_output() {
                let ch = device.default_output_config().ok().map(|c| c.channels());
                push(DeviceKind::Output, ch, &default_out);
            }
        }
        out.sort_by(|a, b| {
            (a.kind as u8, !a.is_default, &a.name).cmp(&(b.kind as u8, !b.is_default, &b.name))
        });
        out
    }

    fn apps(&self) -> Vec<AppInfo> {
        Vec::new()
    }

    fn open_input(
        &mut self,
        source: &SourceEndpoint,
        channels: u8,
        port: InputPort,
    ) -> Result<Box<dyn AudioStream>, (BackendError, InputPort)> {
        let slot = Slot::new(CaptureState::new(port, usize::from(channels)));
        let fail = |e| Err((e, take_input(&slot)));
        if channels == 0 || usize::from(channels) > MAX_CHANNELS {
            return fail(BackendError::BadChannels(channels));
        }
        let SourceEndpoint::HardwareInput { device_id } = source else {
            return fail(BackendError::NotSupported(format!(
                "{source:?}: ALSA can only record from a hardware input"
            )));
        };
        let id = device_id.as_str();
        let device = match self.find(id, DeviceKind::Input) {
            Ok(d) => d,
            Err(e) => return fail(e),
        };
        match build_capture(&device, channels, &slot) {
            Ok(stream) => Ok(Box::new(AlsaStream {
                stream: Some(stream),
                slot: Slotted::Capture(slot),
            })),
            Err(e) => fail(map_error(&e, id)),
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
        if channels == 0 || usize::from(channels) > MAX_CHANNELS {
            return fail(BackendError::BadChannels(channels));
        }
        let SinkEndpoint::HardwareOutput { device_id } = sink else {
            return fail(BackendError::NotSupported(format!(
                "{sink:?}: ALSA cannot create virtual devices"
            )));
        };
        let id = device_id.as_str();
        let device = match self.find(id, DeviceKind::Output) {
            Ok(d) => d,
            Err(e) => return fail(e),
        };
        match build_playback(&device, channels, &slot) {
            Ok(stream) => Ok(Box::new(AlsaStream {
                stream: Some(stream),
                slot: Slotted::Playback(slot),
            })),
            Err(e) => fail(map_error(&e, id)),
        }
    }
}
// !SECTION: Backend

// SECTION: Streams
/// The stream settings to try, best first: our block size (lowest latency), then whatever the
/// device prefers. 48 kHz is always asked for; ALSA's `plug` layer converts if the card cannot.
fn configs(channels: u8) -> [StreamConfig; 2] {
    let base = StreamConfig {
        channels: u16::from(channels),
        sample_rate: ENGINE_SAMPLE_RATE,
        buffer_size: BufferSize::Default,
    };
    [
        StreamConfig {
            buffer_size: BufferSize::Fixed(ENGINE_BLOCK_FRAMES as u32),
            ..base
        },
        base,
    ]
}

fn build_capture(
    device: &cpal::Device,
    channels: u8,
    slot: &Arc<Slot<CaptureState>>,
) -> Result<cpal::Stream, cpal::Error> {
    let mut last = None;
    for config in configs(channels) {
        // ? A callback is moved into the stream, so each attempt gets its own copy of the
        // ? `Arc` (a shared pointer to the slot) and the closure owns that.
        let (a, b) = (slot.clone(), slot.clone());
        let on_f32 = move |data: &[f32], _: &cpal::InputCallbackInfo| {
            if let Ok(mut guard) = a.state.try_lock() {
                if let Some(state) = guard.as_mut() {
                    state.feed_samples(data, &a.counters);
                }
            }
        };
        let on_error = {
            let c = slot.clone();
            move |_: cpal::Error| {
                c.counters.xruns.fetch_add(1, Relaxed);
            }
        };
        let built = device
            .build_input_stream::<f32, _, _>(config, on_f32, on_error.clone(), None)
            .or_else(|_| {
                // Some `hw:` devices only speak 16-bit integers.
                let on_i16 = move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    let mut stage = [0.0_f32; I16_STAGE];
                    let channels = usize::from(config.channels).max(1);
                    for piece in data.chunks(I16_STAGE / channels * channels) {
                        for (dst, s) in stage.iter_mut().zip(piece) {
                            *dst = f32::from(*s) / 32768.0;
                        }
                        if let Ok(mut guard) = b.state.try_lock() {
                            if let Some(state) = guard.as_mut() {
                                state.feed_samples(&stage[..piece.len()], &b.counters);
                            }
                        }
                    }
                };
                device.build_input_stream::<i16, _, _>(config, on_i16, on_error, None)
            });
        match built {
            Ok(stream) => {
                stream.play()?;
                return Ok(stream);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.expect("configs() always yields at least one attempt"))
}

fn build_playback(
    device: &cpal::Device,
    channels: u8,
    slot: &Arc<Slot<PlaybackState>>,
) -> Result<cpal::Stream, cpal::Error> {
    let mut last = None;
    for config in configs(channels) {
        let (a, b) = (slot.clone(), slot.clone());
        let on_f32 = move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let channels = usize::from(config.channels).max(1);
            match a.state.try_lock() {
                Ok(mut guard) => match guard.as_mut() {
                    Some(state) => state.render_samples(data, data.len() / channels, &a.counters),
                    None => data.fill(0.0),
                },
                // The control thread holds the state (closing): play silence this cycle.
                Err(_) => data.fill(0.0),
            }
        };
        let on_error = {
            let c = slot.clone();
            move |_: cpal::Error| {
                c.counters.xruns.fetch_add(1, Relaxed);
            }
        };
        let built = device
            .build_output_stream::<f32, _, _>(config, on_f32, on_error.clone(), None)
            .or_else(|_| {
                let on_i16 = move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    let channels = usize::from(config.channels).max(1);
                    let mut stage = [0.0_f32; I16_STAGE];
                    for piece in data.chunks_mut(I16_STAGE / channels * channels) {
                        let frames = piece.len() / channels;
                        let samples = &mut stage[..piece.len()];
                        match b.state.try_lock() {
                            Ok(mut guard) => match guard.as_mut() {
                                Some(state) => state.render_samples(samples, frames, &b.counters),
                                None => samples.fill(0.0),
                            },
                            Err(_) => samples.fill(0.0),
                        }
                        for (dst, v) in piece.iter_mut().zip(samples.iter()) {
                            // ? `as i16` saturates, and render_samples already limited to +-1.
                            *dst = (*v * 32767.0) as i16;
                        }
                    }
                };
                device.build_output_stream::<i16, _, _>(config, on_i16, on_error, None)
            });
        match built {
            Ok(stream) => {
                stream.play()?;
                return Ok(stream);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.expect("configs() always yields at least one attempt"))
}

enum Slotted {
    Capture(Arc<Slot<CaptureState>>),
    Playback(Arc<Slot<PlaybackState>>),
}

struct AlsaStream {
    // ? `Option` so `close` can drop (and thereby stop) the stream before it takes the port out
    // ? of the slot: once the stream is gone no callback can still be using it.
    stream: Option<cpal::Stream>,
    slot: Slotted,
}

impl AlsaStream {
    fn counters(&self) -> &Counters {
        match &self.slot {
            Slotted::Capture(s) => &s.counters,
            Slotted::Playback(s) => &s.counters,
        }
    }
}

impl AudioStream for AlsaStream {
    fn stats(&self) -> StreamStats {
        let c = self.counters();
        StreamStats {
            frames: c.frames.load(Relaxed),
            xruns: c.xruns.load(Relaxed),
            clock_ppm: f32::from_bits(c.ppm_bits.load(Relaxed)),
        }
    }

    fn close(mut self: Box<Self>) -> Option<ClosedPort> {
        drop(self.stream.take());
        match &self.slot {
            Slotted::Capture(s) => s.take().map(|st| ClosedPort::Input(st.into_port())),
            Slotted::Playback(s) => s.take().map(|st| ClosedPort::Output(st.into_port())),
        }
    }
}
// !SECTION: Streams

#[cfg(test)]
mod tests {
    use super::*;
    use audio_engine::create_engine;
    use audio_types::FixedStr;

    // ? These tests need no sound card: a CI box has none. They check what the backend promises
    // ? when there is nothing to open, which is the common case there.
    fn backend() -> AlsaBackend {
        AlsaBackend::connect(AlsaOptions::default()).expect("connect")
    }

    #[test]
    fn it_says_what_alsa_cannot_do() {
        let b = backend();
        let caps = b.capabilities();
        assert!(caps.hardware_input && caps.hardware_output);
        assert!(!caps.device_loopback && !caps.process_loopback);
        assert_eq!((caps.virtual_inputs, caps.virtual_mics), (0, 0));
        assert_eq!(b.name(), "alsa");
        assert!(b.apps().is_empty());
        // Listing must not panic, with or without cards.
        let _ = b.devices();
    }

    #[test]
    fn refused_opens_hand_the_port_back() {
        let (_engine, mut h) = create_engine();
        let mut b = backend();

        let port = h.ports.acquire_input().unwrap();
        let (err, port) = b
            .open_input(&SourceEndpoint::VirtualInput { index: 0 }, 2, port)
            .err()
            .expect("no virtual inputs on ALSA");
        assert!(matches!(err, BackendError::NotSupported(_)), "{err:?}");
        let (err, port) = b
            .open_input(
                &SourceEndpoint::HardwareInput {
                    device_id: FixedStr::from_str_trunc("alsa:no-such-card"),
                },
                2,
                port,
            )
            .err()
            .expect("unknown device");
        assert!(matches!(err, BackendError::NoSuchDevice(_)), "{err:?}");
        let (err, port) = b
            .open_input(&SourceEndpoint::Unassigned, 9, port)
            .err()
            .expect("bad channel count");
        assert_eq!(err, BackendError::BadChannels(9));
        h.ports.release_input(port);
        assert_eq!(h.ports.free_inputs(), 16, "no port may be lost");

        let out = h.ports.acquire_output().unwrap();
        let (err, out) = b
            .open_output(&SinkEndpoint::VirtualMic { index: 0 }, 2, out)
            .err()
            .expect("no virtual mics on ALSA");
        assert!(matches!(err, BackendError::NotSupported(_)), "{err:?}");
        let (err, out) = b
            .open_output(
                &SinkEndpoint::HardwareOutput {
                    device_id: FixedStr::from_str_trunc("not even an id"),
                },
                2,
                out,
            )
            .err()
            .expect("unparsable id");
        assert!(matches!(err, BackendError::NoSuchDevice(_)), "{err:?}");
        h.ports.release_output(out);
        assert_eq!(h.ports.free_outputs(), 16);
    }
}
