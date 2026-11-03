//! Saved mixer setups: TOML files in `<config dir>/profiles/<name>.toml`.
//!
//! A profile holds what the user set up (sources, sinks, gains, routing, UI layout, who may
//! send to us). It never holds runtime state such as ports or sockets; loading one makes the
//! daemon re-open the devices and streams it names.

use crate::paths;
use crate::ui_state::UiLayout;
use audio_engine::{sanitize_bands, sanitize_dsp};
use audio_types::{
    BusConfig, EqBand, FixedStr, MAX_BUSES, MAX_CHANNELS, MAX_STRIPS, RouteSend, SinkEndpoint,
    SourceEndpoint, StripConfig, StripDsp,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::Path;

/// Bumped when a field changes meaning. Older files keep loading through `#[serde(default)]`.
pub const PROFILE_VERSION: u32 = 1;

const GAIN_RANGE: (f32, f32) = (-200.0, 24.0);

// SECTION: Defaults
pub fn default_strip(index: usize) -> StripConfig {
    StripConfig {
        label: FixedStr::from_str_trunc(&format!("Strip {}", index + 1)),
        source: SourceEndpoint::Unassigned,
        channels: 2,
        gain_db: 0.0,
        pan: 0.0,
        mute: false,
        solo: false,
        dsp: StripDsp::default(),
        sends: [RouteSend::default(); MAX_BUSES],
    }
}

pub fn default_bus(index: usize) -> BusConfig {
    BusConfig {
        label: FixedStr::from_str_trunc(&format!("Bus {}", index + 1)),
        sink: SinkEndpoint::Unassigned,
        channels: 2,
        gain_db: 0.0,
        mute: false,
        eq: [EqBand::default(); 3],
        record: false,
    }
}
// !SECTION: Defaults

// SECTION: The profile
/// Who may send audio to a LAN input that does not name a source address.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkProfile {
    pub accept_any: bool,
    pub allowed: Vec<[u8; 4]>,
    pub trust_discovered: bool,
    pub beacon: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub version: u32,
    pub strips: Vec<StripConfig>,
    pub buses: Vec<BusConfig>,
    #[serde(default)]
    pub network: NetworkProfile,
    #[serde(default)]
    pub ui: UiLayout,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            version: PROFILE_VERSION,
            strips: (0..MAX_STRIPS).map(default_strip).collect(),
            buses: (0..MAX_BUSES).map(default_bus).collect(),
            network: NetworkProfile::default(),
            ui: UiLayout::default(),
        }
    }
}

fn fix(v: f32, default: f32, lo: f32, hi: f32) -> f32 {
    if v.is_finite() {
        v.clamp(lo, hi)
    } else {
        default
    }
}

impl Profile {
    /// Make a profile from a file (or from a client) safe to apply: exactly 16 strips and 16
    /// buses, channel counts 1 to 8, finite values in range.
    // ? The engine rejects bad numbers anyway (and counts them), but the control plane's own
    // ? copy of the config would then disagree with what the engine does. Fixing it here keeps
    // ? both the same.
    pub fn normalize(mut self) -> Self {
        self.strips.truncate(MAX_STRIPS);
        self.buses.truncate(MAX_BUSES);
        for i in self.strips.len()..MAX_STRIPS {
            self.strips.push(default_strip(i));
        }
        for i in self.buses.len()..MAX_BUSES {
            self.buses.push(default_bus(i));
        }
        for s in &mut self.strips {
            s.channels = s.channels.clamp(1, MAX_CHANNELS as u8);
            s.gain_db = fix(s.gain_db, 0.0, GAIN_RANGE.0, GAIN_RANGE.1);
            s.pan = fix(s.pan, 0.0, -1.0, 1.0);
            s.dsp = sanitize_dsp(&s.dsp).unwrap_or_default();
            for send in &mut s.sends {
                send.gain_db = fix(send.gain_db, 0.0, GAIN_RANGE.0, GAIN_RANGE.1);
            }
        }
        for b in &mut self.buses {
            b.channels = b.channels.clamp(1, MAX_CHANNELS as u8);
            b.gain_db = fix(b.gain_db, 0.0, GAIN_RANGE.0, GAIN_RANGE.1);
            b.eq = sanitize_bands(&b.eq).unwrap_or([EqBand::default(); 3]);
        }
        self.ui = self.ui.sanitized();
        self.version = PROFILE_VERSION;
        self
    }
}
// !SECTION: The profile

// SECTION: Files
#[derive(Debug)]
pub enum ProfileError {
    Name(String),
    NotFound(String),
    Io(io::Error),
    Parse(String),
    /// Written by a newer version of the program.
    Version(u32),
}

impl core::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Name(m) => f.write_str(m),
            Self::NotFound(n) => write!(f, "no profile called {n:?}"),
            Self::Io(e) => write!(f, "file error: {e}"),
            Self::Parse(m) => write!(f, "cannot read the profile: {m}"),
            Self::Version(v) => write!(
                f,
                "the profile is version {v}; this program understands up to {PROFILE_VERSION}"
            ),
        }
    }
}

impl std::error::Error for ProfileError {}

impl From<io::Error> for ProfileError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

pub fn to_toml(profile: &Profile) -> Result<String, ProfileError> {
    toml::to_string_pretty(profile).map_err(|e| ProfileError::Parse(e.to_string()))
}

pub fn from_toml(text: &str) -> Result<Profile, ProfileError> {
    let profile: Profile = toml::from_str(text).map_err(|e| ProfileError::Parse(e.to_string()))?;
    if profile.version > PROFILE_VERSION {
        return Err(ProfileError::Version(profile.version));
    }
    Ok(profile.normalize())
}

/// Write `<config>/profiles/<name>.toml`. The file appears complete or not at all.
// ? Write to a sibling temp file and rename: a crash or a full disk in the middle leaves the
// ? old profile untouched instead of a half-written one.
pub fn save(config_dir: &Path, name: &str, profile: &Profile) -> Result<(), ProfileError> {
    paths::validate_profile_name(name).map_err(ProfileError::Name)?;
    let text = to_toml(profile)?;
    let dir = paths::profiles_dir(config_dir);
    fs::create_dir_all(&dir)?;
    let target = paths::profile_path(config_dir, name);
    let tmp = dir.join(format!("{name}.{}.tmp", paths::profile_ext()));
    fs::write(&tmp, text)?;
    fs::rename(&tmp, &target).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;
    Ok(())
}

pub fn load(config_dir: &Path, name: &str) -> Result<Profile, ProfileError> {
    paths::validate_profile_name(name).map_err(ProfileError::Name)?;
    match fs::read_to_string(paths::profile_path(config_dir, name)) {
        Ok(text) => from_toml(&text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(ProfileError::NotFound(name.into())),
        Err(e) => Err(e.into()),
    }
}

pub fn delete(config_dir: &Path, name: &str) -> Result<(), ProfileError> {
    paths::validate_profile_name(name).map_err(ProfileError::Name)?;
    match fs::remove_file(paths::profile_path(config_dir, name)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(ProfileError::NotFound(name.into())),
        Err(e) => Err(e.into()),
    }
}

/// Names of the saved profiles, sorted. A missing directory is an empty list.
pub fn list(config_dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(paths::profiles_dir(config_dir)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            (path.extension()? == paths::profile_ext())
                .then(|| path.file_stem()?.to_str().map(str::to_owned))?
        })
        .filter(|n| paths::validate_profile_name(n).is_ok())
        .collect();
    names.sort();
    names
}
// !SECTION: Files

#[cfg(test)]
mod tests {
    use super::*;
    use audio_types::{LanRxConfig, LanTxConfig, NetworkBufferPreset, WireCodec};
    use std::path::PathBuf;

    /// A fresh directory per test, removed at the end.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "retae-test-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn busy_profile() -> Profile {
        let mut p = Profile::default();
        p.strips[0].label = FixedStr::from_str_trunc("Mic");
        p.strips[0].source = SourceEndpoint::HardwareInput {
            device_id: FixedStr::from_str_trunc("mock-in-1"),
        };
        p.strips[0].gain_db = -6.5;
        p.strips[0].pan = -0.25;
        p.strips[0].mute = true;
        p.strips[0].dsp.eq[1].enabled = true;
        p.strips[0].dsp.eq[1].gain_db = 3.0;
        p.strips[0].sends[2] = RouteSend {
            enabled: true,
            gain_db: -3.0,
            pre_fader: true,
        };
        p.strips[1].source = SourceEndpoint::LanIn(LanRxConfig {
            source_ipv4: [192, 168, 1, 20],
            stream_name: FixedStr::from_str_trunc("Stream1"),
            buffer: NetworkBufferPreset::Custom { target_ms: 30 },
        });
        p.strips[2].source = SourceEndpoint::ProcessLoopback {
            pid: 4242,
            exe_name: FixedStr::from_str_trunc("firefox.exe"),
            mode: audio_types::ProcessLoopbackMode::ExcludeTree,
        };
        p.buses[0].sink = SinkEndpoint::LanOut(LanTxConfig {
            peer_ipv4: [10, 0, 0, 7],
            port: 6981,
            codec: WireCodec::Pcm {
                data_type: audio_types::VbanDataType::Float32,
            },
            duplicate: true,
            ..LanTxConfig::default()
        });
        p.buses[1].sink = SinkEndpoint::HardwareOutput {
            device_id: FixedStr::from_str_trunc("mock-out-1"),
        };
        p.buses[1].eq[0].enabled = true;
        p.buses[1].record = true;
        p.network = NetworkProfile {
            accept_any: false,
            allowed: vec![[192, 168, 1, 20], [10, 0, 0, 7]],
            trust_discovered: true,
            beacon: true,
        };
        p.ui.mode = crate::ui_state::UiMode::Expert;
        p.ui.layout = crate::ui_state::LayoutMode::Matrix;
        p.ui.graphics = Some(crate::ui_state::GraphicsSettings::ECO);
        p.ui.strips[3].collapsed_in_strips = true;
        p.ui.buses[1].card_order = 7;
        p
    }

    #[test]
    fn a_profile_round_trips_through_toml_unchanged() {
        let p = busy_profile();
        let text = to_toml(&p).unwrap();
        let back = from_toml(&text).unwrap();
        assert_eq!(back, p, "TOML was:\n{text}");
    }

    #[test]
    fn the_default_profile_round_trips_too() {
        let p = Profile::default();
        assert_eq!(from_toml(&to_toml(&p).unwrap()).unwrap(), p);
    }

    #[test]
    fn files_save_list_load_and_delete() {
        let dir = TempDir::new("files");
        assert!(list(&dir.0).is_empty());
        let p = busy_profile();
        save(&dir.0, "Living room", &p).unwrap();
        save(&dir.0, "alpha", &Profile::default()).unwrap();
        assert_eq!(list(&dir.0), ["Living room", "alpha"]);
        assert_eq!(load(&dir.0, "Living room").unwrap(), p);
        // Saving again replaces the file.
        save(&dir.0, "alpha", &p).unwrap();
        assert_eq!(load(&dir.0, "alpha").unwrap(), p);
        assert!(
            !paths::profiles_dir(&dir.0).read_dir().unwrap().any(|e| e
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "tmp")),
            "no temp file may be left behind"
        );
        delete(&dir.0, "alpha").unwrap();
        assert!(matches!(
            load(&dir.0, "alpha"),
            Err(ProfileError::NotFound(_))
        ));
        assert!(matches!(
            delete(&dir.0, "alpha"),
            Err(ProfileError::NotFound(_))
        ));
    }

    #[test]
    fn bad_names_never_touch_the_disk() {
        let dir = TempDir::new("names");
        for bad in ["../evil", "a/b", ".x", ""] {
            assert!(matches!(
                save(&dir.0, bad, &Profile::default()),
                Err(ProfileError::Name(_))
            ));
            assert!(matches!(load(&dir.0, bad), Err(ProfileError::Name(_))));
        }
        assert!(!paths::profiles_dir(&dir.0).exists());
    }

    #[test]
    fn a_hand_edited_profile_is_made_safe() {
        let mut p = busy_profile();
        p.strips.truncate(3);
        p.buses.clear();
        p.strips[0].gain_db = f32::NAN;
        p.strips[0].pan = 9.0;
        p.strips[0].channels = 0;
        p.strips[1].channels = 200;
        p.strips[1].dsp.eq[0].freq_hz = f32::INFINITY;
        p.ui.graphics = Some(crate::ui_state::GraphicsSettings {
            active_fps: 0,
            ..crate::ui_state::GraphicsSettings::RICH
        });
        // NaN cannot be written to TOML in a readable way, so go through `normalize` directly,
        // the way `from_toml` does after parsing.
        let fixed = p.normalize();
        assert_eq!(fixed.strips.len(), MAX_STRIPS);
        assert_eq!(fixed.buses.len(), MAX_BUSES);
        assert_eq!(fixed.strips[0].gain_db, 0.0);
        assert_eq!(fixed.strips[0].pan, 1.0);
        assert_eq!(fixed.strips[0].channels, 1);
        assert_eq!(fixed.strips[1].channels, MAX_CHANNELS as u8);
        assert_eq!(fixed.strips[1].dsp, StripDsp::default());
        assert_eq!(fixed.ui.graphics.unwrap().active_fps, 1);
        assert_eq!(fixed.buses[5].label.as_str(), "Bus 6");
    }

    #[test]
    fn a_newer_profile_is_refused_not_misread() {
        let text = to_toml(&Profile {
            version: PROFILE_VERSION + 1,
            ..Profile::default()
        })
        .unwrap();
        assert!(matches!(from_toml(&text), Err(ProfileError::Version(_))));
    }

    #[test]
    fn garbage_is_a_parse_error() {
        assert!(matches!(
            from_toml("this is = [not a profile"),
            Err(ProfileError::Parse(_))
        ));
    }
}
