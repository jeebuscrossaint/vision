pub mod color;
pub mod drm;

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use color::Adjust;

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/tmp".into())
}

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".config"))
        .join("vision")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

/// Modification time of the config file, used to notice outside edits.
pub fn config_mtime() -> Option<std::time::SystemTime> {
    fs::metadata(config_path()).and_then(|m| m.modified()).ok()
}

pub fn runtime_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/vision-{}", unsafe { libc::getuid() })));
    base.join("vision")
}

pub fn status_path() -> PathBuf {
    runtime_dir().join("status.json")
}

pub fn lock_path() -> PathBuf {
    runtime_dir().join("visiond.lock")
}

/// Write via temp file + rename so readers never see a half-written file.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
    }
    fs::rename(tmp, path)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Master switch; when off every display is restored to stock.
    pub enabled: bool,
    /// Per-connector settings, keyed by connector name ("eDP-1", "HDMI-A-1").
    pub outputs: BTreeMap<String, Adjust>,
}

impl Default for Config {
    fn default() -> Self {
        Config { enabled: true, outputs: BTreeMap::new() }
    }
}

impl Config {
    pub fn load() -> io::Result<Config> {
        match fs::read_to_string(config_path()) {
            Ok(s) => serde_json::from_str(&s).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self) -> io::Result<()> {
        let s = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        write_atomic(&config_path(), s.as_bytes())
    }

    pub fn effective(&self, output: &str) -> Adjust {
        if self.enabled {
            self.outputs.get(output).copied().unwrap_or_default()
        } else {
            Adjust::default()
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub enum State {
    #[default]
    Starting,
    Ok,
    Waiting,
    NeedsPrivilege,
    Error,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OutputInfo {
    pub name: String,
    pub model: Option<String>,
    pub card: String,
    pub driver: String,
    pub supported: bool,
    pub active: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Status {
    pub pid: u32,
    pub state: State,
    pub message: String,
    pub outputs: Vec<OutputInfo>,
}

impl Status {
    pub fn load() -> Option<Status> {
        serde_json::from_slice(&fs::read(status_path()).ok()?).ok()
    }
}

/// True when a visiond instance currently holds the lock.
pub fn daemon_running() -> bool {
    let _ = fs::create_dir_all(runtime_dir());
    let Ok(f) = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(lock_path())
    else {
        return false;
    };
    use std::os::fd::AsRawFd;
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    r != 0
}
