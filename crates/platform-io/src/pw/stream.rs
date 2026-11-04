//! The PipeWire side of a stream: the audio format we ask the graph for.
//!
//! The callback state (`CaptureState`, `PlaybackState`, `Slot`) is shared with the ALSA backend
//! and lives in `crate::ring_io`; it is re-exported here so the rest of `pw` keeps one import.

pub(crate) use crate::ring_io::{CaptureState, Counters, PlaybackState, Slot};
use audio_types::ENGINE_SAMPLE_RATE;
use pipewire::spa;

// SECTION: Format
/// The channel layout PipeWire should use for `channels` channels.
fn channel_positions(channels: u8) -> [u32; spa::param::audio::MAX_CHANNELS] {
    use spa::sys::{
        SPA_AUDIO_CHANNEL_FC, SPA_AUDIO_CHANNEL_FL, SPA_AUDIO_CHANNEL_FR, SPA_AUDIO_CHANNEL_LFE,
        SPA_AUDIO_CHANNEL_MONO, SPA_AUDIO_CHANNEL_RL, SPA_AUDIO_CHANNEL_RR, SPA_AUDIO_CHANNEL_SL,
        SPA_AUDIO_CHANNEL_SR,
    };
    let layout: &[u32] = match channels {
        1 => &[SPA_AUDIO_CHANNEL_MONO],
        2 => &[SPA_AUDIO_CHANNEL_FL, SPA_AUDIO_CHANNEL_FR],
        3 => &[
            SPA_AUDIO_CHANNEL_FL,
            SPA_AUDIO_CHANNEL_FR,
            SPA_AUDIO_CHANNEL_FC,
        ],
        4 => &[
            SPA_AUDIO_CHANNEL_FL,
            SPA_AUDIO_CHANNEL_FR,
            SPA_AUDIO_CHANNEL_RL,
            SPA_AUDIO_CHANNEL_RR,
        ],
        6 => &[
            SPA_AUDIO_CHANNEL_FL,
            SPA_AUDIO_CHANNEL_FR,
            SPA_AUDIO_CHANNEL_FC,
            SPA_AUDIO_CHANNEL_LFE,
            SPA_AUDIO_CHANNEL_RL,
            SPA_AUDIO_CHANNEL_RR,
        ],
        8 => &[
            SPA_AUDIO_CHANNEL_FL,
            SPA_AUDIO_CHANNEL_FR,
            SPA_AUDIO_CHANNEL_FC,
            SPA_AUDIO_CHANNEL_LFE,
            SPA_AUDIO_CHANNEL_RL,
            SPA_AUDIO_CHANNEL_RR,
            SPA_AUDIO_CHANNEL_SL,
            SPA_AUDIO_CHANNEL_SR,
        ],
        // 5 and 7 channels have no standard layout; leave the positions to the server.
        _ => &[],
    };
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[..layout.len()].copy_from_slice(layout);
    position
}

/// The one format we ask for: 48 kHz interleaved `f32`. If the graph runs at another rate the
/// stream's own converter handles it, so the engine only ever sees 48 kHz (NOTES A2).
pub(crate) fn format_pod(channels: u8) -> Vec<u8> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(ENGINE_SAMPLE_RATE);
    info.set_channels(u32::from(channels));
    info.set_position(channel_positions(channels));
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .unwrap_or_default()
}
// !SECTION: Format

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_format_pod_exists_for_every_channel_count() {
        for ch in 1..=8 {
            assert!(!format_pod(ch).is_empty(), "{ch} channels");
        }
        let pos = channel_positions(2);
        assert_ne!(pos[0], 0);
        assert_ne!(pos[1], pos[0]);
        assert_eq!(pos[2], 0);
        assert!(
            channel_positions(5).iter().all(|&p| p == 0),
            "5 ch: server chooses"
        );
    }
}
