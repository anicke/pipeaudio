//! The output folder and the recordings in it.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub duration_secs: Option<u64>,
    pub size: u64,
    pub modified: SystemTime,
}

impl Entry {
    /// Reads the listing details of one recording.
    pub fn load(path: PathBuf) -> Option<Self> {
        let meta = fs::metadata(&path).ok()?;
        Some(Self {
            name: path.file_name()?.to_string_lossy().into_owned(),
            duration_secs: wav_duration_secs(&path).ok(),
            size: meta.len(),
            modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            path,
        })
    }
}

fn default_dir() -> PathBuf {
    dirs::audio_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join("Music")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("PipeAudio")
}

fn config_file() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("pipeaudio").join("output-dir"))
}

/// The output folder chosen last time, or the default one.
pub fn load_dir() -> PathBuf {
    config_file()
        .and_then(|file| fs::read_to_string(file).ok())
        .map(|dir| PathBuf::from(dir.trim()))
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(default_dir)
}

pub fn save_dir(dir: &Path) {
    if let Some(file) = config_file() {
        let _ = file.parent().map(fs::create_dir_all);
        let _ = fs::write(file, dir.to_string_lossy().as_bytes());
    }
}

/// WAV files in `dir`, newest first.
pub fn scan(dir: &Path) -> Vec<Entry> {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<Entry> = read_dir
        .flatten()
        .filter_map(|item| {
            let path = item.path();
            let name = path.file_name()?.to_str()?;
            if name.starts_with('.') || !name.to_lowercase().ends_with(".wav") {
                return None;
            }
            Entry::load(path)
        })
        .collect();
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.modified));
    entries
}

/// Length of the audio in a WAV file, in whole seconds.
pub fn wav_duration_secs(path: &Path) -> hound::Result<u64> {
    let reader = hound::WavReader::open(path)?;
    Ok(reader.duration() as u64 / reader.spec().sample_rate.max(1) as u64)
}

/// Moves a file to the desktop trash, so deleting can be undone.
pub fn trash(path: &Path) -> bool {
    ::trash::delete(path).is_ok()
}

pub fn format_clock(secs: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

pub fn format_size(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= MB {
        format!("{:.1} MB", bytes as f64 / MB)
    } else {
        format!("{} KB", (bytes / 1024).max(1))
    }
}

pub fn format_when(time: SystemTime) -> String {
    let time: chrono::DateTime<chrono::Local> = time.into();
    let today = chrono::Local::now().date_naive();
    match (today - time.date_naive()).num_days() {
        0 => format!("Today {}", time.format("%H:%M")),
        1 => format!("Yesterday {}", time.format("%H:%M")),
        _ => time.format("%b %-d, %H:%M").to_string(),
    }
}
