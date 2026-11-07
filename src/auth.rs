//! Stored logins: one JSON file in the data dir, readable only by the user (mode 0600 on Unix). Environment variables override it.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::Result;

/// One stored login for a provider.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Account {
    /// The API key.
    pub key: String,
    /// Optional human-readable name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// Unix seconds when the key was added.
    #[serde(default)]
    pub added: u64,
}

/// Accounts per provider name, newest first.
type Store = BTreeMap<String, Vec<Account>>;

/// Path of the auth file in the data dir.
pub fn path() -> PathBuf {
    crate::journal::data_dir().join("auth.json")
}

/// First stored key for a provider, or `None` if there is none or the file cannot be read.
pub fn key(provider: &str) -> Option<String> {
    key_in(&path(), provider)
}

/// Store a key for a provider as its newest account.
pub fn add(provider: &str, key: &str, label: &str) -> Result<()> {
    add_in(&path(), provider, key, label)
}

/// Remove all accounts of a provider. Returns how many were removed.
pub fn remove(provider: &str) -> Result<usize> {
    remove_in(&path(), provider)
}

/// Read the store. A missing file is an empty store.
fn load_from(path: &Path) -> Result<Store> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Store::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display()).into()),
    };
    serde_json::from_slice(&bytes)
        .map_err(|e| format!("cannot parse {}: {e}", path.display()).into())
}

/// Write the store atomically: a temp file in the same dir, then rename over the target.
fn save_to(path: &Path, store: &Store) -> Result<()> {
    let mut data = serde_json::to_vec_pretty(store)?;
    data.push(b'\n');
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_file_name(format!("auth.json.tmp-{}", std::process::id()));
    let result =
        write_tmp(&tmp, &data).and_then(|()| std::fs::rename(&tmp, path).map_err(Into::into));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Create (or truncate) the temp file with private permissions and write the data durably.
fn write_tmp(tmp: &Path, data: &[u8]) -> Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(tmp)?;
    file.write_all(data)?;
    file.sync_all()?;
    Ok(())
}

/// Current time in Unix seconds, or 0 if the clock is before the epoch.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Add a key as the newest account of a provider, replacing any account with the same key.
fn add_in(path: &Path, provider: &str, key: &str, label: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() {
        return Err("API key is empty".into());
    }
    let mut store = load_from(path)?;
    let accounts = store.entry(provider.to_string()).or_default();
    accounts.retain(|a| a.key != key);
    accounts.insert(
        0,
        Account {
            key: key.to_string(),
            label: label.to_string(),
            added: now(),
        },
    );
    save_to(path, &store)
}

/// Remove a provider's entry. Writes the file only if something was removed.
fn remove_in(path: &Path, provider: &str) -> Result<usize> {
    let mut store = load_from(path)?;
    let removed = store.remove(provider).map_or(0, |accounts| accounts.len());
    if removed > 0 {
        save_to(path, &store)?;
    }
    Ok(removed)
}

/// First stored key for a provider. Any read or parse error gives `None`.
fn key_in(path: &Path, provider: &str) -> Option<String> {
    load_from(path)
        .ok()?
        .get(provider)?
        .first()
        .map(|a| a.key.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique scratch dir for one test.
    fn test_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("agent-auth-test-{}-{name}", std::process::id()))
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = test_dir("missing");
        let file = dir.join("auth.json");
        assert!(load_from(&file).unwrap().is_empty());
        assert_eq!(key_in(&file, "openrouter"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_orders_newest_first_and_dedups() {
        let dir = test_dir("add");
        let file = dir.join("auth.json");
        add_in(&file, "openrouter", "sk-a", "first").unwrap();
        add_in(&file, "openrouter", "sk-b", "second").unwrap();
        assert_eq!(key_in(&file, "openrouter").as_deref(), Some("sk-b"));
        add_in(&file, "openrouter", "  sk-a  ", "again").unwrap();
        let store = load_from(&file).unwrap();
        let accounts = &store["openrouter"];
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].key, "sk-a");
        assert_eq!(key_in(&file, "openrouter").as_deref(), Some("sk-a"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_drops_whole_provider() {
        let dir = test_dir("remove");
        let file = dir.join("auth.json");
        add_in(&file, "openrouter", "sk-a", "").unwrap();
        add_in(&file, "openrouter", "sk-b", "").unwrap();
        assert_eq!(remove_in(&file, "openrouter").unwrap(), 2);
        assert_eq!(key_in(&file, "openrouter"), None);
        assert_eq!(remove_in(&file, "openrouter").unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = test_dir("mode");
        let file = dir.join("auth.json");
        add_in(&file, "openrouter", "sk-a", "").unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_json_error_names_path() {
        let dir = test_dir("bad");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("auth.json");
        std::fs::write(&file, "{not json").unwrap();
        let err = load_from(&file).unwrap_err().to_string();
        assert!(err.contains(&file.display().to_string()), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
