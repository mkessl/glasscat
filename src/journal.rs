//! Every change the agent's tools make to a file is written down here, one
//! JSON line per change: the change itself, not a copy of the whole file.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::Value;

pub struct Journal {
    path: PathBuf,
    file: Mutex<Option<File>>,
}

impl Journal {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            file: Mutex::new(None),
        }
    }

    /// A journal for this run in the user's data directory.
    pub fn for_session() -> Self {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let name = format!("{secs}-{}.jsonl", std::process::id());
        Self::new(data_dir().join("edits").join(name))
    }

    pub fn record(&self, mut entry: Value) -> std::io::Result<()> {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64());
        entry["time"] = secs.into();
        let mut line = serde_json::to_vec(&entry)?;
        line.push(b'\n');
        let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        if file.is_none() {
            if let Some(dir) = self.path.parent() {
                fs::create_dir_all(dir)?;
            }
            *file = Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)?,
            );
        }
        file.as_mut().expect("opened above").write_all(&line)
    }
}

/// `$XDG_DATA_HOME/<name>`, `~/.local/share/<name>`, or `%LOCALAPPDATA%\<name>`.
pub fn data_dir() -> PathBuf {
    let env = |k| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let base = if cfg!(windows) {
        env("LOCALAPPDATA")
    } else {
        env("XDG_DATA_HOME").or_else(|| env("HOME").map(|h| h.join(".local/share")))
    };
    base.unwrap_or_else(std::env::temp_dir).join(crate::NAME)
}
