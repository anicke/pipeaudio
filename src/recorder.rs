//! Captures whatever is playing on the default PipeWire output.
//!
//! `pw-record` is asked to capture the default sink's monitor
//! (`stream.capture.sink=true`) and stream raw PCM to stdout. A reader thread
//! writes that PCM into a WAV file and keeps peak levels for the UI meter.
//! The WAV header is kept up to date as audio arrives, so a file left behind
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
/// How many meter values the waveform view keeps.
pub const HISTORY_LEN: usize = 96;

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
}

pub struct LevelSnapshot {
    pub peak: [f32; 2],
    pub history: Vec<f32>,
    pub bytes_written: u64,
}

pub struct Recording {
    child: Child,
    reader: Option<JoinHandle<()>>,
    levels: Arc<Mutex<Levels>>,
    started: Instant,
    started_at: chrono::DateTime<chrono::Local>,
    temp_path: PathBuf,
}

impl Recording {
    pub fn start(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)
            .with_context(|| format!("Could not create folder {}", dir.display()))?;

        let started_at = chrono::Local::now();
        let temp_path = dir.join(format!(
            "{PART_PREFIX}{}{PART_SUFFIX}",
            started_at.format(PART_TIME_FORMAT)
        ));

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
        let spec = hound::WavSpec {
            channels: CHANNELS,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: BITS_PER_SAMPLE,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = hound::WavWriter::new(BufWriter::new(fs::File::create(&temp_path)?), spec)?;

        let levels = Arc::new(Mutex::new(Levels::default()));
        let reader = std::thread::Builder::new()
            .name("pw-record-reader".into())
            .spawn({
                let levels = levels.clone();
                move || {
                    if let Err(error) = pump(stdout, writer, &levels) {
                        levels.lock().unwrap().error =
                            Some(format!("Failed to write recording: {error}"));
                    }
                }
            })?;

        Ok(Self {
            child,
            reader: Some(reader),
            levels,
            started: Instant::now(),
            started_at,
            temp_path,
        })
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn levels(&self) -> LevelSnapshot {
        let levels = self.levels.lock().unwrap();
        LevelSnapshot {
            peak: levels.peak,
            history: levels.history.iter().copied().collect(),
            bytes_written: levels.bytes_written,
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

    /// Stops capture, finalizes the WAV file and returns its final path.
    pub fn stop(mut self) -> Result<PathBuf> {
        self.shutdown();
        if let Some(error) = self.levels.lock().unwrap().error.take() {
            return Err(anyhow!(error));
        }

        finish(&self.temp_path, self.started_at.naive_local())
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
    mut writer: hound::WavWriter<BufWriter<fs::File>>,
    levels: &Mutex<Levels>,
) -> Result<()> {
    let mut buf = vec![0u8; FRAMES_PER_LEVEL * BYTES_PER_FRAME];
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
        let peak = write_block(&mut writer, &buf)?;
        // Keeps the header valid in case the app dies mid-recording.
        writer.flush()?;
        filled = 0;

        let mut levels = levels.lock().unwrap();
        levels.peak = peak;
        levels.history.push_back(peak[0].max(peak[1]));
        if levels.history.len() > HISTORY_LEN {
            levels.history.pop_front();
        }
        levels.bytes_written += buf.len() as u64;
    }
    // Flush any trailing whole frames.
    let tail = filled - filled % BYTES_PER_FRAME;
    write_block(&mut writer, &buf[..tail])?;
    writer.finalize()?;
    Ok(())
}

/// Writes interleaved s16le frames and returns the per-channel peak.
fn write_block(
    writer: &mut hound::WavWriter<BufWriter<fs::File>>,
    bytes: &[u8],
) -> Result<[f32; 2]> {
    let mut peak = [0u16; 2];
    for (i, sample) in bytes.chunks_exact(2).enumerate() {
        let sample = i16::from_le_bytes([sample[0], sample[1]]);
        writer.write_sample(sample)?;
        let channel = &mut peak[i % 2];
        *channel = (*channel).max(sample.unsigned_abs());
    }
    Ok(peak.map(|p| (p as f32 / i16::MAX as f32).min(1.0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a running PipeWire session: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn records_desktop_audio() {
        let dir = std::env::temp_dir().join(format!("pipeaudio-test-{}", std::process::id()));
        let tone = dir.join("tone.wav");
        fs::create_dir_all(&dir).unwrap();
        let status = Command::new("ffmpeg")
            .args(["-loglevel", "error", "-y", "-f", "lavfi"])
            .args(["-i", "sine=frequency=440:duration=2"])
            .arg(&tone)
            .status()
            .unwrap();
        assert!(status.success());

        let mut recording = Recording::start(&dir).unwrap();
        let played = Command::new("pw-play").arg(&tone).status().unwrap();
        assert!(played.success());
        assert!(recording.failure().is_none());
        let peak = recording.levels().history.into_iter().fold(0f32, f32::max);
        let path = recording.stop().unwrap();

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
