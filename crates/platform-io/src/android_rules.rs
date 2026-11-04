//! What the Android backend decides, with no phone needed: device ids, the order in which AAudio
//! streams are tried, how an endpoint becomes a playback-capture request, how the app list is
//! shaped. Compiled and tested on every host (like `win_rules`); the code that actually talks
//! to AAudio and the JVM is in `android/` and only builds for Android.

use crate::backend::{AppInfo, BackendError};
use audio_types::{MAX_CHANNELS, PlaybackCaptureFilter, ProcessLoopbackMode, SourceEndpoint};

// SECTION: Device ids
/// What `HardwareInput` / `HardwareOutput` carry for "let Android pick" (AAudio device id 0).
pub const DEFAULT_DEVICE_ID: &str = "default";
const DEVICE_ID_PREFIX: &str = "aaudio:";
/// The one `DeviceLoopback` id: everything the phone plays (media, games, unknown).
pub const PLAYBACK_CAPTURE_ID: &str = "playback";

/// `AudioDeviceInfo.getId()` as the text stored in a profile.
pub fn device_id_text(aaudio_id: i32) -> String {
    if aaudio_id <= 0 {
        DEFAULT_DEVICE_ID.to_owned()
    } else {
        format!("{DEVICE_ID_PREFIX}{aaudio_id}")
    }
}

/// The AAudio device id for a profile's text. `0` means "unspecified, the primary device".
// ? Returns `None` for anything that is not ours, so a typo is `NoSuchDevice` and not silently
// ? the default microphone.
pub fn parse_device_id(text: &str) -> Option<i32> {
    if text.is_empty() || text == DEFAULT_DEVICE_ID {
        return Some(0);
    }
    let n: i32 = text.strip_prefix(DEVICE_ID_PREFIX)?.parse().ok()?;
    (n > 0).then_some(n)
}

/// `AudioDeviceInfo.getType()` values worth offering. `is_source` is true for an input.
// ? Android lists many routes that are not something a person picks: the telephony and FM tuner
// ? routes, `REMOTE_SUBMIX` (that is how screen recording works), the IP and bus types for cars.
// ? Types are the `AudioDeviceInfo.TYPE_*` numbers: 1 earpiece, 2 speaker, 3 wired headset,
// ? 4 wired headphones, 5 line analog, 7 Bluetooth SCO, 8 Bluetooth A2DP, 9 HDMI, 11 USB device,
// ? 15 built-in mic, 22 USB headset, 26 BLE headset, 27 BLE speaker.
pub fn device_type_offered(device_type: i32, is_source: bool) -> bool {
    if is_source {
        matches!(device_type, 3 | 5 | 7 | 11 | 15 | 22 | 26)
    } else {
        matches!(
            device_type,
            1 | 2 | 3 | 4 | 5 | 7 | 8 | 9 | 11 | 22 | 26 | 27
        )
    }
}

/// The name shown for a device: the product name, or the type when Android gives none.
pub fn device_label(product: &str, device_type: i32) -> String {
    let product = product.trim();
    if !product.is_empty() {
        return product.to_owned();
    }
    match device_type {
        1 => "Earpiece",
        2 => "Speaker",
        3 => "Wired headset",
        4 => "Wired headphones",
        5 => "Line in/out",
        7 | 8 | 26 | 27 => "Bluetooth",
        9 => "HDMI",
        11 | 22 => "USB audio",
        15 => "Built-in microphone",
        _ => "Audio device",
    }
    .to_owned()
}

/// A device's channel count, from `AudioDeviceInfo.getChannelCounts()` (empty means "any").
pub fn device_channels(counts: &[i32], is_source: bool) -> u8 {
    let best = counts.iter().copied().max().unwrap_or(0);
    if best < 1 {
        return if is_source { 1 } else { 2 };
    }
    u8::try_from(best.min(MAX_CHANNELS as i32)).unwrap_or(2)
}

/// The refusal text of a playback capture that has no screen-capture grant. The UI looks for it
/// to show "app audio paused, tap to resume" instead of a generic error.
pub const NEEDS_PROJECTION: &str = "app audio needs the screen-capture permission";
// !SECTION: Device ids

// SECTION: AAudio open order
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sharing {
    Exclusive,
    Shared,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputPreset {
    /// Low-latency voice capture without the call-quality processing (API 29).
    VoicePerformance,
    /// No processing at all.
    Unprocessed,
    Camcorder,
    VoiceRecognition,
    /// What AAudio picks when nothing is asked for.
    Generic,
}

impl InputPreset {
    /// The contract's preset (`audio_types::AndroidInputPreset`) as the backend's own.
    pub fn from_contract(p: audio_types::AndroidInputPreset) -> Self {
        use audio_types::AndroidInputPreset as P;
        match p {
            P::VoicePerformance => Self::VoicePerformance,
            P::Unprocessed => Self::Unprocessed,
            P::Camcorder => Self::Camcorder,
            P::VoiceRecognition => Self::VoiceRecognition,
        }
    }
}

/// One way to try opening a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attempt {
    pub sharing: Sharing,
    /// `None` for an output stream.
    pub preset: Option<InputPreset>,
}

/// Microphone attempts, best first (PLAN §6.3): exclusive `VoicePerformance`, then shared
/// `VoicePerformance`, then shared with the default preset.
// ? Exclusive (MMAP) is the lowest latency but many phones refuse it, or refuse 48 kHz in it,
// ? so a refusal is expected and just moves down the list. The last entry exists because some
// ? HALs reject a preset they advertise; a working microphone beats a perfect one.
pub fn input_attempts() -> [Attempt; 3] {
    input_attempts_for(InputPreset::VoicePerformance)
}

/// The same list for a preset the strip asked for (`SourceEndpoint::AndroidMic`).
pub fn input_attempts_for(preset: InputPreset) -> [Attempt; 3] {
    [
        Attempt {
            sharing: Sharing::Exclusive,
            preset: Some(preset),
        },
        Attempt {
            sharing: Sharing::Shared,
            preset: Some(preset),
        },
        Attempt {
            sharing: Sharing::Shared,
            preset: Some(InputPreset::Generic),
        },
    ]
}

/// Speaker/headphone attempts, best first: exclusive, then shared.
pub fn output_attempts() -> [Attempt; 2] {
    [
        Attempt {
            sharing: Sharing::Exclusive,
            preset: None,
        },
        Attempt {
            sharing: Sharing::Shared,
            preset: None,
        },
    ]
}

/// A line for `AudioStream::detail`.
pub fn describe_open(a: Attempt, burst_frames: i32, buffer_frames: i32, rate: i32) -> String {
    let share = match a.sharing {
        Sharing::Exclusive => "exclusive",
        Sharing::Shared => "shared",
    };
    let preset = match a.preset {
        Some(InputPreset::VoicePerformance) => ", voice-performance",
        Some(InputPreset::Unprocessed) => ", unprocessed",
        Some(InputPreset::Camcorder) => ", camcorder",
        Some(InputPreset::VoiceRecognition) => ", voice-recognition",
        Some(InputPreset::Generic) => ", default preset",
        None => "",
    };
    format!(
        "AAudio {share}{preset}, low latency, burst {burst_frames} frames, buffer {buffer_frames} frames, {rate} Hz"
    )
}

/// How many frames an output keeps queued in AAudio's own buffer: two bursts, the usual
/// double-buffer, but never less than one mixer block's worth so a 96-frame burst cannot starve.
// ? AAudio's buffer starts at one burst for a LowLatency stream and grows only when told to.
// ? Two bursts is the standard starting point (Google's low-latency guide); `x_run_count`
// ? then says whether it was enough.
pub fn output_buffer_frames(burst_frames: i32, capacity_frames: i32) -> i32 {
    let want = burst_frames.max(1).saturating_mul(2);
    want.min(capacity_frames.max(burst_frames.max(1)))
}
// !SECTION: AAudio open order

// SECTION: Channel adaption
/// Convert interleaved `src` (`src_ch` channels) to `dst` (`dst_ch` channels) and return how many
/// frames were written. Only as many frames as both slices hold are converted.
///
/// * same count: copy; mono to anything: the sample on every channel;
/// * anything to mono: the average; otherwise the first channels are kept and any extra
///   destination channels are silent.
// ? A phone's microphone is often mono while a strip is stereo, and a mono earpiece may sit
// ? behind a stereo bus. AAudio can ask the system to convert, but not every device honours it,
// ? so the stream asks for the strip's count first and, when refused, takes the device's own and
// ? converts here. No allocation: both slices belong to the caller (a stack stage in the
// ? real-time callback).
pub fn adapt_channels(src: &[f32], src_ch: usize, dst: &mut [f32], dst_ch: usize) -> usize {
    if src_ch == 0 || dst_ch == 0 {
        return 0;
    }
    let frames = (src.len() / src_ch).min(dst.len() / dst_ch);
    for f in 0..frames {
        let s = &src[f * src_ch..(f + 1) * src_ch];
        let d = &mut dst[f * dst_ch..(f + 1) * dst_ch];
        if src_ch == dst_ch {
            d.copy_from_slice(s);
        } else if src_ch == 1 {
            d.fill(s[0]);
        } else if dst_ch == 1 {
            d[0] = s.iter().sum::<f32>() / src_ch as f32;
        } else {
            for (c, v) in d.iter_mut().enumerate() {
                *v = s.get(c).copied().unwrap_or(0.0);
            }
        }
    }
    frames
}
// !SECTION: Channel adaption

// SECTION: Playback capture
// ? Values of `android.media.AudioAttributes.USAGE_*` that playback capture can match.
pub const USAGE_UNKNOWN: i32 = 0;
pub const USAGE_MEDIA: i32 = 1;
pub const USAGE_GAME: i32 = 14;

/// `AudioFormat.CHANNEL_IN_MONO` / `CHANNEL_IN_STEREO`.
pub fn capture_channel_mask(channels: u8) -> Result<i32, BackendError> {
    match channels {
        1 => Ok(0x10),
        2 => Ok(0x0C),
        n => Err(BackendError::BadChannels(n)),
    }
}

/// What to ask `AudioPlaybackCaptureConfiguration.Builder` for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturePlan {
    pub usages: Vec<i32>,
    /// `addMatchingUid`: only these apps are captured. Empty = every app.
    pub include_uids: Vec<i32>,
    /// `excludeUid`: every app but these.
    pub exclude_uids: Vec<i32>,
}

/// The "all media and games" filter of PLAN §6.3.
pub const ALL_MEDIA_AND_GAMES: [i32; 3] = [USAGE_MEDIA, USAGE_GAME, USAGE_UNKNOWN];

/// Turn a strip's source into a capture request, or say why it is not one.
// ? Android has no process *ids* for capture, only app *uids*, so `ProcessLoopback.pid` carries
// ? the uid here. The two ranges never meet: uids of installed apps start at 10000, and the UI
// ? gets the number from `apps()`, never from a person.
pub fn capture_plan(source: &SourceEndpoint) -> Result<CapturePlan, BackendError> {
    match source {
        SourceEndpoint::DeviceLoopback { device_id } => {
            let id = device_id.as_str();
            if id.is_empty() || id == PLAYBACK_CAPTURE_ID {
                Ok(CapturePlan {
                    usages: ALL_MEDIA_AND_GAMES.to_vec(),
                    include_uids: Vec::new(),
                    exclude_uids: Vec::new(),
                })
            } else {
                Err(BackendError::NoSuchDevice(id.to_owned()))
            }
        }
        SourceEndpoint::ProcessLoopback { pid, mode, .. } => {
            let uid = i32::try_from(*pid)
                .map_err(|_| BackendError::NoSuchDevice(format!("app uid {pid}")))?;
            let (include_uids, exclude_uids) = match mode {
                ProcessLoopbackMode::IncludeTree => (vec![uid], Vec::new()),
                ProcessLoopbackMode::ExcludeTree => (Vec::new(), vec![uid]),
            };
            Ok(CapturePlan {
                usages: ALL_MEDIA_AND_GAMES.to_vec(),
                include_uids,
                exclude_uids,
            })
        }
        // ? The contract's own Android variant: same plan, the filter says which apps.
        SourceEndpoint::AndroidPlaybackCapture { filter } => {
            let (include_uids, exclude_uids) = match *filter {
                PlaybackCaptureFilter::AllMediaAndGames => (Vec::new(), Vec::new()),
                PlaybackCaptureFilter::OnlyUid(uid) => (vec![uid_to_i32(uid)?], Vec::new()),
                PlaybackCaptureFilter::ExcludeUid(uid) => (Vec::new(), vec![uid_to_i32(uid)?]),
            };
            Ok(CapturePlan {
                usages: ALL_MEDIA_AND_GAMES.to_vec(),
                include_uids,
                exclude_uids,
            })
        }
        other => Err(BackendError::NotSupported(format!(
            "{other:?} is not a playback-capture source"
        ))),
    }
}

fn uid_to_i32(uid: u32) -> Result<i32, BackendError> {
    i32::try_from(uid).map_err(|_| BackendError::NoSuchDevice(format!("app uid {uid}")))
}

/// Frames moved per blocking `AudioRecord.read`: one engine block.
pub const CAPTURE_READ_FRAMES: usize = audio_types::ENGINE_BLOCK_FRAMES;
const BYTES_PER_FLOAT: usize = 4;

/// Bytes of one read for `channels` channels of `f32`.
pub fn capture_read_bytes(channels: u8) -> usize {
    CAPTURE_READ_FRAMES * usize::from(channels).min(MAX_CHANNELS) * BYTES_PER_FLOAT
}

/// The `AudioRecord` buffer to request: the system minimum, and at least four reads so a late
/// wake-up of the capture thread does not lose audio.
pub fn capture_buffer_bytes(system_minimum: i32, channels: u8) -> i32 {
    let floor = i32::try_from(capture_read_bytes(channels) * 4).unwrap_or(i32::MAX);
    system_minimum.max(floor)
}
// !SECTION: Playback capture

// SECTION: App list
/// One installed app as Java reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawApp {
    pub uid: i32,
    pub label: String,
    pub package: String,
    /// Has a launcher icon (so a person knows it).
    pub launchable: bool,
}

/// The apps worth offering: launchable ones, not ourselves, once per uid, sorted by name.
// ? `AppInfo.pid` carries the uid (see `capture_plan`). `playing` is always false: telling which
// ? app is making sound needs `AudioPlaybackConfiguration.getClientUid`, a hidden API, so the
// ? list says "can be captured" and not "is playing".
pub fn shape_app_list(raw: Vec<RawApp>, own_uid: i32) -> Vec<AppInfo> {
    let mut seen = std::collections::HashSet::new();
    let mut apps: Vec<AppInfo> = raw
        .into_iter()
        .filter(|a| a.launchable && a.uid != own_uid && a.uid >= 0 && seen.insert(a.uid))
        .map(|a| AppInfo {
            pid: u32::try_from(a.uid).unwrap_or_default(),
            name: if a.label.is_empty() {
                a.package
            } else {
                a.label
            },
            playing: false,
        })
        .collect();
    apps.sort_by_cached_key(|a| a.name.to_lowercase());
    apps
}
// !SECTION: App list

#[cfg(test)]
mod tests {
    use super::*;
    use audio_types::FixedStr;

    #[test]
    fn device_ids_round_trip_and_reject_strangers() {
        assert_eq!(parse_device_id("default"), Some(0));
        assert_eq!(parse_device_id(""), Some(0));
        assert_eq!(parse_device_id("aaudio:37"), Some(37));
        assert_eq!(device_id_text(37), "aaudio:37");
        assert_eq!(device_id_text(0), "default");
        // A typo or another platform's id must not become the default microphone.
        for bad in [
            "aaudio:",
            "aaudio:0",
            "aaudio:-3",
            "aaudio:x",
            "alsa:hw:0",
            "wasapi",
        ] {
            assert_eq!(parse_device_id(bad), None, "{bad}");
        }
        for id in [1, 2, 99, 1234] {
            assert_eq!(parse_device_id(&device_id_text(id)), Some(id));
        }
    }

    #[test]
    fn only_devices_a_person_would_pick_are_offered() {
        // Inputs: the built-in mic and headsets yes; telephony (18), submix (25), FM (16) no.
        for t in [15, 3, 22, 11, 7] {
            assert!(device_type_offered(t, true), "input type {t}");
        }
        for t in [18, 25, 16, 2, 0] {
            assert!(!device_type_offered(t, true), "input type {t}");
        }
        // Outputs: speaker, earpiece, headphones, Bluetooth; not the mic types or submix.
        for t in [2, 1, 4, 8, 9] {
            assert!(device_type_offered(t, false), "output type {t}");
        }
        for t in [15, 25, 18] {
            assert!(!device_type_offered(t, false), "output type {t}");
        }
        assert_eq!(device_label("  Pixel Buds ", 8), "Pixel Buds");
        assert_eq!(device_label("", 15), "Built-in microphone");
        assert_eq!(device_label("", 999), "Audio device");
    }

    #[test]
    fn a_device_without_a_channel_list_gets_a_sensible_count() {
        assert_eq!(device_channels(&[], true), 1);
        assert_eq!(device_channels(&[], false), 2);
        assert_eq!(device_channels(&[1, 2], true), 2);
        assert_eq!(
            device_channels(&[2, 6, 12], false),
            8,
            "capped at the engine's maximum"
        );
    }

    #[test]
    fn channels_are_adapted_without_losing_frames() {
        let mut out = [9.0_f32; 8];
        // Mono to stereo: the sample on both channels.
        assert_eq!(adapt_channels(&[0.5, -0.5], 1, &mut out, 2), 2);
        assert_eq!(&out[..4], &[0.5, 0.5, -0.5, -0.5]);
        // Stereo to mono: the average.
        let mut mono = [0.0_f32; 2];
        assert_eq!(adapt_channels(&[1.0, 0.0, 0.5, 0.5], 2, &mut mono, 1), 2);
        assert_eq!(mono, [0.5, 0.5]);
        // Stereo to 4: the extra channels are silent. 4 to stereo: the first two are kept.
        let mut four = [9.0_f32; 4];
        assert_eq!(adapt_channels(&[0.1, 0.2], 2, &mut four, 4), 1);
        assert_eq!(four, [0.1, 0.2, 0.0, 0.0]);
        let mut two = [0.0_f32; 2];
        assert_eq!(adapt_channels(&[1.0, 2.0, 3.0, 4.0], 4, &mut two, 2), 1);
        assert_eq!(two, [1.0, 2.0]);
        // Same count is a copy; the shorter slice limits the frames; nonsense counts do nothing.
        let mut same = [0.0_f32; 3];
        assert_eq!(adapt_channels(&[1.0, 2.0, 3.0, 4.0], 2, &mut same, 2), 1);
        assert_eq!(&same[..2], &[1.0, 2.0]);
        assert_eq!(adapt_channels(&[1.0], 0, &mut same, 2), 0);
        assert_eq!(adapt_channels(&[1.0], 1, &mut same, 0), 0);
    }

    #[test]
    fn the_microphone_tries_exclusive_voice_performance_first_and_always_ends_shared() {
        let a = input_attempts();
        assert_eq!(a[0].sharing, Sharing::Exclusive);
        assert_eq!(a[0].preset, Some(InputPreset::VoicePerformance));
        assert_eq!(a[1].sharing, Sharing::Shared);
        assert_eq!(a[1].preset, Some(InputPreset::VoicePerformance));
        let last = a[a.len() - 1];
        assert_eq!(last.sharing, Sharing::Shared);
        assert_eq!(last.preset, Some(InputPreset::Generic));
        // The same attempt is never tried twice.
        for (i, x) in a.iter().enumerate() {
            assert!(a[i + 1..].iter().all(|y| y != x));
        }
    }

    #[test]
    fn the_speaker_falls_back_from_exclusive_to_shared_without_a_preset() {
        let a = output_attempts();
        assert_eq!(a[0].sharing, Sharing::Exclusive);
        assert_eq!(a[1].sharing, Sharing::Shared);
        assert!(a.iter().all(|x| x.preset.is_none()));
    }

    #[test]
    fn the_open_line_says_what_the_stream_got() {
        let s = describe_open(input_attempts()[1], 96, 192, 48_000);
        assert!(
            s.contains("shared") && s.contains("voice-performance"),
            "{s}"
        );
        assert!(s.contains("burst 96") && s.contains("48000"), "{s}");
        assert!(describe_open(output_attempts()[0], 192, 384, 48_000).contains("exclusive"));
    }

    #[test]
    fn the_output_buffer_is_two_bursts_within_the_capacity() {
        assert_eq!(output_buffer_frames(96, 1024), 192);
        assert_eq!(
            output_buffer_frames(192, 256),
            256,
            "never above the capacity"
        );
        assert_eq!(
            output_buffer_frames(0, 0),
            1,
            "bad numbers still give a usable size"
        );
        assert_eq!(output_buffer_frames(i32::MAX, i32::MAX), i32::MAX);
    }

    fn loopback(id: &str) -> SourceEndpoint {
        SourceEndpoint::DeviceLoopback {
            device_id: FixedStr::from_str_trunc(id),
        }
    }

    #[test]
    fn all_audio_is_media_games_and_unknown_with_no_uid_filter() {
        for id in ["", PLAYBACK_CAPTURE_ID] {
            let p = capture_plan(&loopback(id)).unwrap();
            assert_eq!(p.usages, vec![USAGE_MEDIA, USAGE_GAME, USAGE_UNKNOWN]);
            assert!(p.include_uids.is_empty() && p.exclude_uids.is_empty());
        }
        assert!(matches!(
            capture_plan(&loopback("speaker")),
            Err(BackendError::NoSuchDevice(_))
        ));
    }

    #[test]
    fn the_contracts_android_capture_variant_maps_each_filter() {
        let plan = |filter| capture_plan(&SourceEndpoint::AndroidPlaybackCapture { filter });
        let all = plan(PlaybackCaptureFilter::AllMediaAndGames).unwrap();
        assert_eq!(all.usages, vec![USAGE_MEDIA, USAGE_GAME, USAGE_UNKNOWN]);
        assert!(all.include_uids.is_empty() && all.exclude_uids.is_empty());
        let only = plan(PlaybackCaptureFilter::OnlyUid(10_123)).unwrap();
        assert_eq!(only.include_uids, vec![10_123]);
        assert!(only.exclude_uids.is_empty());
        let but = plan(PlaybackCaptureFilter::ExcludeUid(10_123)).unwrap();
        assert_eq!(but.exclude_uids, vec![10_123]);
        assert!(but.include_uids.is_empty());
        assert!(matches!(
            plan(PlaybackCaptureFilter::OnlyUid(u32::MAX)),
            Err(BackendError::NoSuchDevice(_))
        ));
    }

    #[test]
    fn a_requested_microphone_preset_leads_the_attempt_list() {
        use audio_types::AndroidInputPreset as P;
        for (contract, own) in [
            (P::VoicePerformance, InputPreset::VoicePerformance),
            (P::Unprocessed, InputPreset::Unprocessed),
            (P::Camcorder, InputPreset::Camcorder),
            (P::VoiceRecognition, InputPreset::VoiceRecognition),
        ] {
            assert_eq!(InputPreset::from_contract(contract), own);
            let a = input_attempts_for(own);
            assert_eq!(a[0].preset, Some(own));
            assert_eq!(a[1].preset, Some(own));
            // A HAL that rejects the preset still gets a working microphone.
            assert_eq!(a[2].preset, Some(InputPreset::Generic));
        }
    }

    #[test]
    fn one_app_is_a_matching_uid_and_everything_but_one_is_an_excluded_uid() {
        let src = |mode| SourceEndpoint::ProcessLoopback {
            pid: 10_123,
            exe_name: FixedStr::from_str_trunc("player"),
            mode,
        };
        let inc = capture_plan(&src(ProcessLoopbackMode::IncludeTree)).unwrap();
        assert_eq!((inc.include_uids, inc.exclude_uids), (vec![10_123], vec![]));
        let exc = capture_plan(&src(ProcessLoopbackMode::ExcludeTree)).unwrap();
        // Android refuses a configuration that both matches and excludes uids, so only one is set.
        assert_eq!((exc.include_uids, exc.exclude_uids), (vec![], vec![10_123]));
    }

    #[test]
    fn a_uid_that_does_not_fit_is_refused_and_other_sources_are_not_capture() {
        let huge = SourceEndpoint::ProcessLoopback {
            pid: u32::MAX,
            exe_name: FixedStr::from_str_trunc("x"),
            mode: ProcessLoopbackMode::IncludeTree,
        };
        assert!(matches!(
            capture_plan(&huge),
            Err(BackendError::NoSuchDevice(_))
        ));
        for s in [
            SourceEndpoint::Unassigned,
            SourceEndpoint::VirtualInput { index: 0 },
        ] {
            assert!(matches!(
                capture_plan(&s),
                Err(BackendError::NotSupported(_))
            ));
        }
    }

    #[test]
    fn playback_capture_is_mono_or_stereo() {
        assert_eq!(capture_channel_mask(1).unwrap(), 0x10);
        assert_eq!(capture_channel_mask(2).unwrap(), 0x0C);
        for n in [0, 3, 6, 8] {
            assert_eq!(capture_channel_mask(n), Err(BackendError::BadChannels(n)));
        }
    }

    #[test]
    fn reads_are_one_engine_block_and_the_buffer_holds_four_of_them() {
        assert_eq!(capture_read_bytes(2), 128 * 2 * 4);
        assert_eq!(
            capture_buffer_bytes(100, 2),
            4 * 1024,
            "a tiny minimum is raised"
        );
        assert_eq!(
            capture_buffer_bytes(65_536, 2),
            65_536,
            "a big minimum is kept"
        );
    }

    fn raw(uid: i32, label: &str, package: &str, launchable: bool) -> RawApp {
        RawApp {
            uid,
            label: label.into(),
            package: package.into(),
            launchable,
        }
    }

    #[test]
    fn the_app_list_has_launchable_apps_once_sorted_without_ourselves() {
        let list = shape_app_list(
            vec![
                raw(10_300, "Spotify", "com.spotify.music", true),
                raw(10_001, "ReTAE", "dev.f.lanrouter", true),
                raw(10_200, "alarm", "com.android.deskclock", true),
                raw(1000, "System UI", "com.android.systemui", false),
                // Two packages sharing a uid are one capture target.
                raw(10_300, "Spotify Lite", "com.spotify.lite", true),
                raw(10_400, "", "org.example.nolabel", true),
            ],
            10_001,
        );
        let names: Vec<_> = list.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["alarm", "org.example.nolabel", "Spotify"]);
        assert_eq!(list[2].pid, 10_300);
        assert!(list.iter().all(|a| !a.playing));
    }
}
