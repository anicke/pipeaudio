//! Captures whatever is playing on the default PipeWire output.
//!
//! `pw-record` is asked to capture the default sink's monitor
//! (`stream.capture.sink=true`) and stream raw PCM to stdout. A reader thread
//! writes that PCM into a WAV file and keeps peak levels for the UI meter.
//! Optionally the file isn't started until audio rises above silence, keeping
//! a short pre-roll so the first sound isn't clipped. The WAV header is kept up to date as audio arrives, so a file left behind
//! by a crash is still playable.

use std::collections::VecDeque;
use std::fs;
use std::io::{BufWriter, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};

use crate::library::{format_clock, wav_duration_secs};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;
pub const BITS_PER_SAMPLE: u16 = 16;
const BYTES_PER_FRAME: usize = CHANNELS as usize * BITS_PER_SAMPLE as usize / 8;
/// One meter value is produced per 20 ms of audio.
const FRAMES_PER_LEVEL: usize = SAMPLE_RATE as usize / 50;
const BLOCK_BYTES: usize = FRAMES_PER_LEVEL * BYTES_PER_FRAME;
/// How many meter values the waveform view keeps.
pub const HISTORY_LEN: usize = 96;
/// Peaks below this (about -50 dBFS) count as silence.
const SILENCE_THRESHOLD: f32 = 0.003;
/// Silence kept before the first sound when skipping leading silence (500 ms).
const PRE_ROLL_BYTES: usize = SAMPLE_RATE as usize / 2 * BYTES_PER_FRAME;
const SPEC: hound::WavSpec = hound::WavSpec {
    channels: CHANNELS,
    sample_rate: SAMPLE_RATE,
    bits_per_sample: BITS_PER_SAMPLE,
    sample_format: hound::SampleFormat::Int,
};

type Writer = hound::WavWriter<BufWriter<fs::File>>;

/// In-progress recordings are hidden files named after their start time.
const PART_PREFIX: &str = ".recording-";
const PART_SUFFIX: &str = ".wav.part";
const PART_TIME_FORMAT: &str = "%Y%m%d-%H%M%S";

#[derive(Default)]
struct Levels {
    /// Most recent per-channel peaks, 0.0..=1.0.
    peak: [f32; 2],
    /// Recent combined peaks, oldest first.
    history: VecDeque<f32>,
    bytes_written: u64,
    error: Option<String>,
    /// The file being written, once audio has started.
    part: Option<Part>,
}

struct Part {
    path: PathBuf,
    started_at: chrono::DateTime<chrono::Local>,
}

pub struct LevelSnapshot {
    pub peak: [f32; 2],
    pub history: Vec<f32>,
    pub bytes_written: u64,
    /// Audio recorded so far; zero while waiting for audio.
    pub elapsed: Duration,
    /// True while leading silence is being skipped.
    pub waiting: bool,
}

pub struct Recording {
    child: Child,
    reader: Option<JoinHandle<()>>,
    levels: Arc<Mutex<Levels>>,
}

impl Recording {
    /// Starts capturing into `dir`. With `skip_silence`, the file starts when
    /// audio does instead of right away.
    pub fn start(dir: &Path, skip_silence: bool) -> Result<Self> {
        fs::create_dir_all(dir)
            .with_context(|| format!("Could not create folder {}", dir.display()))?;

        let mut child = Command::new("pw-record")
            .args(["-P", "{ stream.capture.sink=true }"])
            .args(["--media-category", "Capture", "--media-role", "Production"])
            .args(["--rate", &SAMPLE_RATE.to_string()])
            .args(["--channels", &CHANNELS.to_string()])
            .args(["--format", "s16", "--raw", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("Could not start pw-record. Is PipeWire (pipewire-bin) installed?")?;

        let stdout = child.stdout.take().expect("stdout is piped");
        let levels = Arc::new(Mutex::new(Levels::default()));
        let reader = std::thread::Builder::new()
            .name("pw-record-reader".into())
            .spawn({
                let levels = levels.clone();
                let dir = dir.to_path_buf();
                move || {
                    if let Err(error) = pump(stdout, &dir, skip_silence, &levels) {
                        levels.lock().unwrap().error =
                            Some(format!("Failed to write recording: {error}"));
                    }
                }
            })?;

        Ok(Self {
            child,
            reader: Some(reader),
            levels,
        })
    }

    pub fn levels(&self) -> LevelSnapshot {
        let levels = self.levels.lock().unwrap();
        LevelSnapshot {
            peak: levels.peak,
            history: levels.history.iter().copied().collect(),
            bytes_written: levels.bytes_written,
            elapsed: Duration::from_secs_f64(
                levels.bytes_written as f64 / (SAMPLE_RATE as usize * BYTES_PER_FRAME) as f64,
            ),
            waiting: levels.part.is_none(),
        }
    }

    /// Returns an error message if capture died on its own.
    pub fn failure(&mut self) -> Option<String> {
        if let Some(error) = self.levels.lock().unwrap().error.clone() {
            return Some(error);
        }
        match self.child.try_wait() {
            Ok(Some(status)) => Some(format!("pw-record stopped unexpectedly ({status})")),
            _ => None,
        }
    }

    /// Stops capture, finalizes the WAV file and returns its final path, or
    /// `None` if no audio ever started.
    pub fn stop(mut self) -> Result<Option<PathBuf>> {
        self.shutdown();
        let mut levels = self.levels.lock().unwrap();
        if let Some(error) = levels.error.take() {
            return Err(anyhow!(error));
        }
        levels
            .part
            .take()
            .map(|part| finish(&part.path, part.started_at.naive_local()))
            .transpose()
    }

    fn shutdown(&mut self) {
        let Some(reader) = self.reader.take() else {
            return;
        };
        // SIGINT lets pw-record flush; fall back to SIGKILL if it hangs.
        unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGINT) };
        let deadline = Instant::now() + Duration::from_secs(2);
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = reader.join();
    }
}

impl Drop for Recording {
    /// Makes sure the WAV header is finalized even if the app quits mid-recording.
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Renames a finished temp file to its final, duration-stamped name.
fn finish(temp_path: &Path, started_at: chrono::NaiveDateTime) -> Result<PathBuf> {
    // Use the real amount of captured audio for the duration in the name.
    let secs = wav_duration_secs(temp_path).context("The recording file is unreadable")?;
    let name = format!(
        "recording-{}-[{}].wav",
        started_at.format("%Y-%m-%d_%H-%M-%S"),
        format_clock(secs)
    );
    let final_path = temp_path.with_file_name(name);
    fs::rename(temp_path, &final_path)?;
    Ok(final_path)
}

/// Finishes recordings left behind by a crash or kill. Returns the paths of
/// the recovered files.
pub fn recover(dir: &Path) -> Vec<PathBuf> {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return Vec::new();
    };
    read_dir
        .flatten()
        .filter_map(|item| {
            let path = item.path();
            let name = path.file_name()?.to_str()?;
            let stamp = name.strip_prefix(PART_PREFIX)?.strip_suffix(PART_SUFFIX)?;
            let started_at = chrono::NaiveDateTime::parse_from_str(stamp, PART_TIME_FORMAT).ok()?;
            finish(&path, started_at).ok()
        })
        .collect()
}

fn pump(
    mut stdout: impl Read,
    dir: &Path,
    skip_silence: bool,
    levels: &Mutex<Levels>,
) -> Result<()> {
    let mut writer = if skip_silence {
        None
    } else {
        Some(open_part(dir, levels)?)
    };
    // The most recent silence, written ahead of the first sound.
    let mut pre_roll: VecDeque<u8> = VecDeque::with_capacity(PRE_ROLL_BYTES + BLOCK_BYTES);
    let mut buf = vec![0u8; BLOCK_BYTES];
    let mut filled = 0;
    loop {
        let n = stdout.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
        if filled < buf.len() {
            continue;
        }
        filled = 0;
        let peak = block_peak(&buf);
        let mut written = 0;

        if writer.is_none() && peak[0].max(peak[1]) < SILENCE_THRESHOLD {
            pre_roll.extend(&buf);
            pre_roll.drain(..pre_roll.len().saturating_sub(PRE_ROLL_BYTES));
        } else {
            let writer = match &mut writer {
                Some(writer) => writer,
                None => {
                    let mut new_writer = open_part(dir, levels)?;
                    write_block(&mut new_writer, pre_roll.make_contiguous())?;
                    written += pre_roll.len() as u64;
                    pre_roll = VecDeque::new();
                    writer.insert(new_writer)
                }
            };
            write_block(writer, &buf)?;
            // Keeps the header valid in case the app dies mid-recording.
            writer.flush()?;
            written += buf.len() as u64;
        }

        let mut levels = levels.lock().unwrap();
        levels.peak = peak;
        levels.history.push_back(peak[0].max(peak[1]));
        if levels.history.len() > HISTORY_LEN {
            levels.history.pop_front();
        }
        levels.bytes_written += written;
    }
    if let Some(mut writer) = writer {
        // Flush any trailing whole frames.
        let tail = filled - filled % BYTES_PER_FRAME;
        write_block(&mut writer, &buf[..tail])?;
        writer.finalize()?;
    }
    Ok(())
}

/// Creates the in-progress file in `dir`.
fn open_part(dir: &Path, levels: &Mutex<Levels>) -> Result<Writer> {
    let started_at = chrono::Local::now();
    let path = dir.join(format!(
        "{PART_PREFIX}{}{PART_SUFFIX}",
        started_at.format(PART_TIME_FORMAT)
    ));
    let writer = hound::WavWriter::new(BufWriter::new(fs::File::create(&path)?), SPEC)?;
    levels.lock().unwrap().part = Some(Part { path, started_at });
    Ok(writer)
}

/// Writes interleaved s16le frames.
fn write_block(writer: &mut Writer, bytes: &[u8]) -> Result<()> {
    for sample in bytes.chunks_exact(2) {
        writer.write_sample(i16::from_le_bytes([sample[0], sample[1]]))?;
    }
    Ok(())
}

/// The per-channel peak of interleaved s16le frames, 0.0..=1.0.
fn block_peak(bytes: &[u8]) -> [f32; 2] {
    let mut peak = [0u16; 2];
    for (i, sample) in bytes.chunks_exact(2).enumerate() {
        let sample = i16::from_le_bytes([sample[0], sample[1]]);
        let channel = &mut peak[i % 2];
        *channel = (*channel).max(sample.unsigned_abs());
    }
    peak.map(|p| (p as f32 / i16::MAX as f32).min(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pipeaudio-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `secs` of stereo s16le, silent or a loud square wave.
    fn pcm(secs: f32, loud: bool) -> Vec<u8> {
        let frames = (secs * SAMPLE_RATE as f32) as usize;
        (0..frames * CHANNELS as usize)
            .flat_map(|i| {
                let sample: i16 = if loud && (i / 100) % 2 == 0 {
                    10_000
                } else {
                    0
                };
                sample.to_le_bytes()
            })
            .collect()
    }

    fn run_pump(dir: &Path, input: Vec<u8>, skip_silence: bool) -> Option<PathBuf> {
        let levels = Mutex::new(Levels::default());
        pump(std::io::Cursor::new(input), dir, skip_silence, &levels).unwrap();
        levels.into_inner().unwrap().part.map(|part| part.path)
    }

    #[test]
    fn skips_leading_silence_but_keeps_pre_roll() {
        let dir = test_dir("skip");
        let input = [pcm(3.0, false), pcm(1.0, true)].concat();
        let path = run_pump(&dir, input, true).unwrap();

        let mut reader = hound::WavReader::open(&path).unwrap();
        let frames = reader.duration() as usize;
        let lead = SAMPLE_RATE as usize / 2;
        assert_eq!(frames, lead + SAMPLE_RATE as usize);
        let samples: Vec<i16> = reader.samples().map(Result::unwrap).collect();
        assert!(samples[..lead * 2].iter().all(|&s| s == 0));
        assert_eq!(samples[lead * 2], 10_000);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn silence_only_writes_nothing() {
        let dir = test_dir("silent");
        assert!(run_pump(&dir, pcm(2.0, false), true).is_none());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keeps_silence_when_not_skipping() {
        let dir = test_dir("keep");
        let path = run_pump(&dir, pcm(2.0, false), false).unwrap();
        let reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.duration(), 2 * SAMPLE_RATE);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Needs a running PipeWire session: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn records_desktop_audio() {
        let dir = test_dir("test");
        let tone = dir.join("tone.wav");
        let status = Command::new("ffmpeg")
            .args(["-loglevel", "error", "-y", "-f", "lavfi"])
            .args(["-i", "sine=frequency=440:duration=2"])
            .arg(&tone)
            .status()
            .unwrap();
        assert!(status.success());

        let mut recording = Recording::start(&dir, false).unwrap();
        let played = Command::new("pw-play").arg(&tone).status().unwrap();
        assert!(played.success());
        assert!(recording.failure().is_none());
        let peak = recording.levels().history.into_iter().fold(0f32, f32::max);
        let path = recording.stop().unwrap().unwrap();

        let reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().sample_rate, SAMPLE_RATE);
        assert!(
            reader.duration() >= SAMPLE_RATE,
            "at least a second captured"
        );
        assert!(
            peak > 0.05,
            "tone should be audible in the capture, peak {peak}"
        );
        assert!(
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with("].wav")
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
