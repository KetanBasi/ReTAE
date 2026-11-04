//! Bus recorder (PLAN §8, C4): a writer thread that drains one bus's recorder ring into a file.
//!
//! The mixer already copies every block of a bus into a pre-allocated ring while `bus.record` is
//! on (`audio-engine::ports`, law 1: the RT thread only ever does a lock-free `push`). This
//! module is the other end of that ring. Nothing here runs on the RT thread, so it may allocate,
//! lock and do file I/O.
//!
//! * WAV through `hound` (16-bit, 24-bit or 32-bit float), header refreshed every few seconds so a
//!   crash leaves a playable file.
//! * FLAC (24-bit) through `flacenc`, *streamed*: one block is encoded and appended at a time, the
//!   `STREAMINFO` header is rewritten at the start and every few seconds. Memory stays flat for a
//!   multi-hour recording (`flacenc`'s own `Stream` type would keep every frame in RAM).
//!
//! Opus-in-Ogg is still queued (TODO.md); it needs `libopus` and so lives behind the `opus` feature.

use audio_engine::RecorderPort;
use flacenc::component::{BitRepr, Stream, StreamInfo};
use flacenc::config::Encoder as FlacConfig;
use flacenc::error::Verify;
use flacenc::source::{Fill, FrameBuf};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long the writer sleeps when the ring is empty. The ring holds 4096 frames (about 85 ms at
/// 48 kHz), so this must stay well below that.
const IDLE_WAIT: Duration = Duration::from_millis(10);
/// Seconds of audio between header refreshes (what a crash can lose is the tail, not the file).
const HEADER_REFRESH_SECS: u64 = 5;
/// FLAC block size in frames. 4096 is the reference encoder's default for 44.1/48 kHz.
const FLAC_BLOCK: usize = 4096;

// SECTION: Format
/// What a recording is written as. Chosen by the daemon (`--record-format`), not per take.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordFormat {
    Wav16,
    #[default]
    Wav24,
    WavFloat32,
    Flac,
}

impl RecordFormat {
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Flac => "flac",
            _ => "wav",
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Wav16 => "wav16",
            Self::Wav24 => "wav24",
            Self::WavFloat32 => "wav32f",
            Self::Flac => "flac",
        }
    }

    /// Parse the `--record-format` flag.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "wav16" => Some(Self::Wav16),
            "wav" | "wav24" => Some(Self::Wav24),
            "wav32f" | "wavf32" | "float" => Some(Self::WavFloat32),
            "flac" => Some(Self::Flac),
            _ => None,
        }
    }
}
// !SECTION: Format

// SECTION: Sample conversion
/// `f32` in -1..1 to a signed integer of `bits` bits, with clipping (the mixer can exceed 0 dBFS).
fn to_int(x: f32, bits: u32) -> i32 {
    let max = ((1i64 << (bits - 1)) - 1) as f32;
    // ? `as i32` on a NaN gives 0, so a stray NaN becomes silence rather than a panic.
    (x.clamp(-1.0, 1.0) * max).round() as i32
}
// !SECTION: Sample conversion

// SECTION: Sinks
/// One open file. `write` takes whole interleaved frames.
trait Sink: Send {
    fn write(&mut self, interleaved: &[f32]) -> io::Result<()>;
    /// Make the file valid as it stands (header sizes), without closing it.
    fn refresh(&mut self) -> io::Result<()>;
    fn finish(self: Box<Self>) -> io::Result<()>;
}

struct WavSink {
    writer: hound::WavWriter<BufWriter<File>>,
    format: RecordFormat,
}

fn hound_err(e: hound::Error) -> io::Error {
    match e {
        hound::Error::IoError(e) => e,
        other => io::Error::other(other.to_string()),
    }
}

impl WavSink {
    fn create(file: File, channels: u16, rate: u32, format: RecordFormat) -> io::Result<Self> {
        let (bits, sample_format) = match format {
            RecordFormat::Wav16 => (16, hound::SampleFormat::Int),
            RecordFormat::WavFloat32 => (32, hound::SampleFormat::Float),
            _ => (24, hound::SampleFormat::Int),
        };
        let spec = hound::WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: bits,
            sample_format,
        };
        let writer = hound::WavWriter::new(BufWriter::new(file), spec).map_err(hound_err)?;
        Ok(Self { writer, format })
    }
}

impl Sink for WavSink {
    fn write(&mut self, interleaved: &[f32]) -> io::Result<()> {
        for &x in interleaved {
            match self.format {
                RecordFormat::WavFloat32 => self.writer.write_sample(x),
                RecordFormat::Wav16 => self.writer.write_sample(to_int(x, 16) as i16),
                _ => self.writer.write_sample(to_int(x, 24)),
            }
            .map_err(hound_err)?;
        }
        Ok(())
    }

    fn refresh(&mut self) -> io::Result<()> {
        // ? hound's `flush` also rewrites the RIFF/data sizes in the header.
        self.writer.flush().map_err(hound_err)
    }

    fn finish(self: Box<Self>) -> io::Result<()> {
        self.writer.finalize().map_err(hound_err)
    }
}

struct FlacSink {
    file: BufWriter<File>,
    config: flacenc::error::Verified<FlacConfig>,
    info: StreamInfo,
    buf: FrameBuf,
    ints: Vec<i32>,
    pending: usize,
    channels: usize,
    frame_number: usize,
}

fn flac_err(what: &str, e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("flac {what}: {e}"))
}

impl FlacSink {
    fn create(file: File, channels: usize, rate: u32) -> io::Result<Self> {
        let config = FlacConfig::default()
            .into_verified()
            .map_err(|e| flac_err("config", e.1))?;
        let mut info =
            StreamInfo::new(rate as usize, channels, 24).map_err(|e| flac_err("stream info", e))?;
        info.set_block_sizes(FLAC_BLOCK, FLAC_BLOCK)
            .map_err(|e| flac_err("block size", e))?;
        let buf = FrameBuf::with_size(channels, FLAC_BLOCK).map_err(|e| flac_err("buffer", e))?;
        let mut me = Self {
            file: BufWriter::new(file),
            config,
            info,
            buf,
            ints: Vec::with_capacity(FLAC_BLOCK * channels),
            pending: 0,
            channels,
            frame_number: 0,
        };
        me.write_header()?;
        Ok(me)
    }

    /// `fLaC` plus the STREAMINFO block, at the start of the file. The block has a fixed size,
    /// so it can be rewritten in place as the totals change.
    fn write_header(&mut self) -> io::Result<()> {
        let stream = Stream::with_stream_info(self.info.clone());
        let mut sink = flacenc::bitsink::ByteSink::new();
        stream.write(&mut sink).map_err(|e| flac_err("header", e))?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(sink.as_slice())?;
        self.file.seek(SeekFrom::End(0))?;
        Ok(())
    }

    /// Encode `frames` whole frames from `self.ints` (a full block, or the short last one).
    fn emit_block(&mut self, frames: usize) -> io::Result<()> {
        self.buf
            .fill_interleaved(&self.ints[..frames * self.channels])
            .map_err(|e| flac_err("fill", e))?;
        let frame = flacenc::encode_fixed_size_frame(
            &self.config,
            &self.buf,
            self.frame_number,
            &self.info,
        )
        .map_err(|e| flac_err("encode", e))?;
        self.info.update_frame_info(&frame);
        let mut sink = flacenc::bitsink::ByteSink::new();
        frame.write(&mut sink).map_err(|e| flac_err("frame", e))?;
        self.file.write_all(sink.as_slice())?;
        self.frame_number += 1;
        self.ints.clear();
        Ok(())
    }
}

impl Sink for FlacSink {
    fn write(&mut self, interleaved: &[f32]) -> io::Result<()> {
        for frame in interleaved.chunks_exact(self.channels) {
            self.ints.extend(frame.iter().map(|&x| to_int(x, 24)));
            self.pending += 1;
            if self.pending == FLAC_BLOCK {
                self.emit_block(FLAC_BLOCK)?;
                self.pending = 0;
            }
        }
        Ok(())
    }

    fn refresh(&mut self) -> io::Result<()> {
        self.write_header()?;
        self.file.flush()
    }

    fn finish(mut self: Box<Self>) -> io::Result<()> {
        if self.pending > 0 {
            let n = self.pending;
            self.emit_block(n)?;
            self.pending = 0;
        }
        self.write_header()?;
        self.file.flush()
    }
}
// !SECTION: Sinks

// SECTION: Recorder
/// What the writer thread reports while it runs.
#[derive(Debug, Default)]
struct Progress {
    frames: AtomicU64,
    error: Mutex<Option<String>>,
}

/// A snapshot for the UI.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordingState {
    pub path: String,
    pub format: RecordFormat,
    pub frames: u64,
    pub seconds: f32,
    /// Set when a write failed (disk full, folder gone). The file so far stays on disk.
    pub error: Option<String>,
}

/// How a finished recording ended.
#[derive(Debug)]
pub struct RecordingReport {
    pub path: PathBuf,
    pub frames: u64,
    pub error: Option<String>,
}

/// A running recording. Dropping it stops the thread and finalizes the file.
// ? The ring's consumer half lives in `RecorderPort`. It is moved into the thread through the
// ? `handoff` cell (not captured by the closure) so a failed `spawn` can still take it back out.
pub struct Recorder {
    stop: Arc<AtomicBool>,
    progress: Arc<Progress>,
    join: Option<JoinHandle<RecorderPort>>,
    path: PathBuf,
    format: RecordFormat,
    channels: u8,
    rate: u32,
}

/// Create `<dir>/<stem>.<ext>`, or `<stem>-2.<ext>` and so on, never overwriting a file.
fn create_unique(dir: &Path, stem: &str, ext: &str) -> io::Result<(File, PathBuf)> {
    std::fs::create_dir_all(dir)?;
    for n in 1..1000u32 {
        let name = if n == 1 {
            format!("{stem}.{ext}")
        } else {
            format!("{stem}-{n}.{ext}")
        };
        let path = dir.join(name);
        match OpenOptions::new()
            .write(true)
            .read(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((file, path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("too many recordings with the same name"))
}

impl Recorder {
    /// Open the file and start draining `port`. On failure the port comes back so the caller can
    /// return the ring to the pool (rings are never freed).
    // ? Why the tuple error: `RecorderPort` owns the ring's consumer half. Moving it into the
    // ? thread and then failing would lose it; handing it back keeps the pool whole.
    pub fn start(
        port: RecorderPort,
        dir: &Path,
        stem: &str,
        format: RecordFormat,
        channels: u8,
        rate: u32,
    ) -> Result<Self, (io::Error, RecorderPort)> {
        let (file, path) = match create_unique(dir, stem, format.extension()) {
            Ok(v) => v,
            Err(e) => return Err((e, port)),
        };
        let sink: io::Result<Box<dyn Sink>> = match format {
            RecordFormat::Flac => {
                FlacSink::create(file, usize::from(channels), rate).map(|s| Box::new(s) as _)
            }
            _ => WavSink::create(file, u16::from(channels), rate, format).map(|s| Box::new(s) as _),
        };
        let sink = match sink {
            Ok(s) => s,
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                return Err((e, port));
            }
        };
        let stop = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Progress::default());
        let handoff = Arc::new(Mutex::new(Some(port)));
        let (t_stop, t_progress, t_handoff) = (stop.clone(), progress.clone(), handoff.clone());
        let join = std::thread::Builder::new()
            .name(format!("recorder-{stem}"))
            .spawn(move || {
                let port = t_handoff
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take()
                    .expect("the port is in the cell until the thread takes it");
                drain(port, sink, usize::from(channels), rate, t_stop, t_progress)
            });
        match join {
            Ok(join) => Ok(Self {
                stop,
                progress,
                join: Some(join),
                path,
                format,
                channels,
                rate,
            }),
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                let port = handoff
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take()
                    .expect("a thread that never started never took the port");
                Err((e, port))
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn channels(&self) -> u8 {
        self.channels
    }

    pub fn state(&self) -> RecordingState {
        let frames = self.progress.frames.load(Ordering::Relaxed);
        RecordingState {
            path: self.path.display().to_string(),
            format: self.format,
            frames,
            seconds: frames as f32 / self.rate as f32,
            error: self
                .progress
                .error
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        }
    }

    /// Stop, drain what is left in the ring, finalize the file and hand the ring back.
    /// The port is `None` only if the writer thread panicked (the ring is then lost for the rest
    /// of the session; nothing here panics on purpose).
    pub fn stop(mut self) -> (Option<RecorderPort>, RecordingReport) {
        let port = self.finish();
        let state = self.state();
        (
            port,
            RecordingReport {
                path: self.path.clone(),
                frames: state.frames,
                error: state.error,
            },
        )
    }

    fn finish(&mut self) -> Option<RecorderPort> {
        self.stop.store(true, Ordering::Release);
        self.join.take()?.join().ok()
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // ? Dropping without `stop` loses the ring (no pool to hand it to); the daemon always
        // ? calls `stop`. This still closes the file properly.
        let _ = self.finish();
    }
}

/// The writer thread body. Returns the port so `stop` can give it back.
fn drain(
    mut port: RecorderPort,
    mut sink: Box<dyn Sink>,
    channels: usize,
    rate: u32,
    stop: Arc<AtomicBool>,
    progress: Arc<Progress>,
) -> RecorderPort {
    let mut buf = vec![0.0f32; audio_engine::PORT_RING_SAMPLES];
    // ? The engine pushes whole blocks, so the ring holds whole frames. A ring that was emptied by
    // ? `release_recorder` mid-block can still leave a fraction, hence the carry.
    let mut carry: Vec<f32> = Vec::with_capacity(channels);
    let mut failed = false;
    let mut since_refresh = 0u64;
    loop {
        // Read the flag BEFORE the pop: if it was set and the pop still finds nothing, the ring is
        // empty for good (the engine stopped writing before `stop` was called).
        let stopping = stop.load(Ordering::Acquire);
        let n = port.pop(&mut buf);
        if n > 0 && !failed {
            let data: &[f32] = if carry.is_empty() {
                &buf[..n]
            } else {
                carry.extend_from_slice(&buf[..n]);
                &carry
            };
            let whole = data.len() - data.len() % channels;
            let result = sink.write(&data[..whole]);
            let frames = (whole / channels) as u64;
            let tail: Vec<f32> = data[whole..].to_vec();
            carry = tail;
            progress.frames.fetch_add(frames, Ordering::Relaxed);
            since_refresh += frames;
            match result {
                Ok(()) if since_refresh >= u64::from(rate) * HEADER_REFRESH_SECS => {
                    since_refresh = 0;
                    if let Err(e) = sink.refresh() {
                        fail(&progress, e, &mut failed);
                    }
                }
                Ok(()) => {}
                Err(e) => fail(&progress, e, &mut failed),
            }
        }
        if n == 0 {
            if stopping {
                break;
            }
            std::thread::sleep(IDLE_WAIT);
        }
    }
    if let Err(e) = sink.finish() {
        // ? A failure to finalize after an earlier error is the same story; keep the first message.
        if !failed {
            fail(&progress, e, &mut failed);
        }
    }
    port
}

fn fail(progress: &Progress, e: io::Error, failed: &mut bool) {
    *failed = true;
    *progress
        .error
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(e.to_string());
}
// !SECTION: Recorder

// SECTION: File names
/// `20261104-153007` in UTC, from the system clock. No time-zone database is needed for a file
/// name, and UTC sorts correctly across daylight-saving changes.
pub fn utc_stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    stamp_from_unix(secs)
}

pub fn stamp_from_unix(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant's algorithm), valid for any date after 1970.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Keep a bus label usable as part of a file name on Windows, Linux and Android.
pub fn file_safe(label: &str) -> String {
    let s: String = label
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let s = s.trim_matches('_');
    if s.is_empty() {
        "bus".to_owned()
    } else {
        s.chars().take(24).collect()
    }
}
// !SECTION: File names

#[cfg(test)]
mod tests {
    use super::*;
    use audio_engine::create_port_pool;
    use audio_types::BusId;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("retae-rec-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn utc_stamp_matches_known_dates() {
        assert_eq!(stamp_from_unix(0), "19700101-000000");
        assert_eq!(stamp_from_unix(951_782_400), "20000229-000000"); // leap day
        assert_eq!(stamp_from_unix(1_700_000_000), "20231114-221320");
    }

    #[test]
    fn file_safe_strips_path_characters() {
        assert_eq!(file_safe("Main / Mix: 1"), "Main___Mix__1");
        assert_eq!(file_safe("///"), "bus");
        assert_eq!(file_safe("é"), "é");
    }

    #[test]
    fn format_names_round_trip() {
        for f in [
            RecordFormat::Wav16,
            RecordFormat::Wav24,
            RecordFormat::WavFloat32,
            RecordFormat::Flac,
        ] {
            assert_eq!(RecordFormat::parse(f.name()), Some(f));
        }
        assert_eq!(RecordFormat::parse("mp3"), None);
    }

    #[test]
    fn wav_sinks_write_what_they_are_given() {
        let dir = scratch("wav");
        for (fmt, tol) in [
            (RecordFormat::Wav16, 1.0 / 32_000.0),
            (RecordFormat::Wav24, 1.0 / 8_000_000.0),
            (RecordFormat::WavFloat32, 0.0),
        ] {
            let (file, path) = create_unique(&dir, fmt.name(), "wav").unwrap();
            let mut sink = Box::new(WavSink::create(file, 2, 48_000, fmt).unwrap());
            let data: Vec<f32> = (0..2000).map(|i| ((i as f32) * 0.01).sin() * 0.7).collect();
            sink.write(&data).unwrap();
            sink.finish().unwrap();
            let mut r = hound::WavReader::open(&path).unwrap();
            assert_eq!(r.spec().channels, 2);
            assert_eq!(r.duration(), 1000);
            let back: Vec<f32> = match fmt {
                RecordFormat::WavFloat32 => r.samples::<f32>().map(|s| s.unwrap()).collect(),
                RecordFormat::Wav16 => r
                    .samples::<i16>()
                    .map(|s| f32::from(s.unwrap()) / 32767.0)
                    .collect(),
                _ => r
                    .samples::<i32>()
                    .map(|s| s.unwrap() as f32 / 8_388_607.0)
                    .collect(),
            };
            for (a, b) in data.iter().zip(&back) {
                assert!((a - b).abs() <= tol + f32::EPSILON, "{fmt:?}: {a} vs {b}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wav_clips_instead_of_wrapping() {
        assert_eq!(to_int(2.0, 16), 32_767);
        assert_eq!(to_int(-2.0, 16), -32_767);
        assert_eq!(to_int(f32::NAN, 24), 0);
    }

    #[test]
    fn create_unique_never_overwrites() {
        let dir = scratch("unique");
        let (_, a) = create_unique(&dir, "take", "wav").unwrap();
        let (_, b) = create_unique(&dir, "take", "wav").unwrap();
        assert_ne!(a, b);
        assert!(b.file_name().unwrap().to_string_lossy().contains("-2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flac_sink_writes_a_valid_stream() {
        let dir = scratch("flac");
        let (file, path) = create_unique(&dir, "t", "flac").unwrap();
        let mut sink = Box::new(FlacSink::create(file, 2, 48_000).unwrap());
        // 2.5 blocks, so the short last block is exercised.
        let frames = FLAC_BLOCK * 2 + FLAC_BLOCK / 2;
        let data: Vec<f32> = (0..frames * 2)
            .map(|i| ((i / 2) as f32 * 0.02).sin() * 0.5)
            .collect();
        sink.write(&data[..1000]).unwrap();
        sink.refresh().unwrap();
        sink.write(&data[1000..]).unwrap();
        sink.finish().unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..4], b"fLaC");
        // STREAMINFO: block type 0 with the "last metadata block" bit set, length 34.
        assert_eq!(bytes[4], 0x80);
        assert_eq!(&bytes[5..8], &[0, 0, 34]);
        // Total samples is a 36-bit field at bits 108..144 of the STREAMINFO payload.
        let info = &bytes[8..42];
        let total = (u64::from(info[13] & 0x0F) << 32)
            | (u64::from(info[14]) << 24)
            | (u64::from(info[15]) << 16)
            | (u64::from(info[16]) << 8)
            | u64::from(info[17]);
        assert_eq!(total, frames as u64);
        // Channels - 1 and bits - 1 sit in bits 20..28 of the packed word at byte 12..14.
        let channels = ((info[12] >> 1) & 0x07) + 1;
        assert_eq!(channels, 2);
        assert!(bytes.len() > 42 + 3 * 100, "frames were appended");

        // Decode with the system `flac` when it is installed (not required to pass).
        if let Ok(out) = std::process::Command::new("flac")
            .arg("--test")
            .arg("--silent")
            .arg(&path)
            .output()
        {
            assert!(out.status.success(), "flac --test: {out:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ? The ring's producer half is private to audio-engine, so the end-to-end test (real engine
    // ? feeding the ring, file contents, overruns) lives in `tests/local.rs` and `tests/record.rs`.
    #[test]
    fn an_idle_recorder_makes_a_valid_empty_file_and_returns_the_ring() {
        let dir = scratch("thread");
        let (_engine, mut ctl) = create_port_pool();
        let port = ctl.acquire_recorder(BusId(0)).unwrap();
        let rec = Recorder::start(port, &dir, "t", RecordFormat::WavFloat32, 2, 48_000)
            .map_err(|e| e.0)
            .unwrap();
        assert_eq!(rec.channels(), 2);
        let path = rec.path().to_path_buf();
        assert_eq!(rec.state().frames, 0);
        assert!(path.exists());
        let (back, report) = rec.stop();
        assert_eq!(report.frames, 0);
        assert!(report.error.is_none());
        let back = back.expect("the writer thread did not panic");
        assert_eq!(back.slot().0, 32);
        ctl.release_recorder(back);
        assert!(ctl.acquire_recorder(BusId(0)).is_some(), "ring is reusable");
        let r = hound::WavReader::open(&path).unwrap();
        assert_eq!(r.duration(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_open_returns_the_ring() {
        // A file where the directory should be makes `create_dir_all` fail.
        let dir = scratch("blocked");
        std::fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("file");
        std::fs::write(&blocker, b"x").unwrap();
        let (_engine, mut ctl) = create_port_pool();
        let port = ctl.acquire_recorder(BusId(0)).unwrap();
        let (err, port) = match Recorder::start(port, &blocker, "t", RecordFormat::Wav24, 2, 48_000)
        {
            Ok(_) => panic!("opening under a file must fail"),
            Err(e) => e,
        };
        let _ = err;
        ctl.release_recorder(port);
        assert!(ctl.acquire_recorder(BusId(0)).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
