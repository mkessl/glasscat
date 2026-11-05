//! Conversations are saved as they go, one JSON file each, so one can be
//! picked up later with `--continue`. Each working directory has its own
//! folder, named after its path.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::context::Item;

#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub struct Saved {
    pub cwd: PathBuf,
    pub provider: String,
    pub model: String,
    pub items: Vec<Item>,
}

/// Where conversations held in `cwd` are saved.
pub fn dir_for(cwd: &Path) -> PathBuf {
    let name: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    crate::journal::data_dir().join("conversations").join(name)
}

/// A file for a new conversation in `cwd`.
pub fn new_path(cwd: &Path) -> PathBuf {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    dir_for(cwd).join(format!("{secs:012}-{}.json", std::process::id()))
}

/// The conversation in `cwd` saved last.
pub fn latest(cwd: &Path) -> Option<PathBuf> {
    fs::read_dir(dir_for(cwd))
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .max_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok())
}

/// What the user wrote in the last `files` conversations held in `cwd`,
/// oldest first, without the agent's bracketed notes in front.
pub fn recent_prompts(cwd: &Path, files: usize) -> Vec<String> {
    let Ok(dir) = fs::read_dir(dir_for(cwd)) else {
        return Vec::new();
    };
    let mut paths: Vec<(std::time::SystemTime, PathBuf)> = dir
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| Some((fs::metadata(&p).and_then(|m| m.modified()).ok()?, p)))
        .collect();
    paths.sort();
    let start = paths.len().saturating_sub(files);
    let mut out = Vec::new();
    for (_, p) in &paths[start..] {
        let Ok(saved) = load(p) else { continue };
        for item in saved.items {
            if let Item::User(text) = item {
                out.push(without_notes(&text).to_string());
            }
        }
    }
    out
}

/// A prompt without the `[...]` note lines the agent put in front of it.
fn without_notes(text: &str) -> &str {
    if let Some((head, rest)) = text.split_once("\n\n")
        && head.lines().all(|l| l.starts_with('[') && l.ends_with(']'))
    {
        return rest;
    }
    text
}

pub fn save(path: &Path, saved: &Saved) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let bytes = serde_json::to_vec(saved)?;
    crate::tools::write_atomic(path, &bytes)?;
    Ok(())
}

pub fn load(path: &Path) -> Result<Saved> {
    let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| format!("{} is not a saved conversation: {e}", path.display()).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{Reply, ToolCall};

    #[test]
    fn saves_and_loads() {
        let dir = std::env::temp_dir().join(format!("history-{}", std::process::id()));
        let path = dir.join("a/b.json");
        let saved = Saved {
            cwd: "/p".into(),
            provider: "openrouter".into(),
            model: "m".into(),
            items: vec![
                Item::System("s".into()),
                Item::User("hi".into()),
                Item::Assistant(Reply {
                    text: "t".into(),
                    calls: vec![ToolCall {
                        id: "1".into(),
                        name: "read".into(),
                        args: "{}".into(),
                    }],
                    ..Reply::default()
                }),
                Item::ToolResult {
                    call_id: "1".into(),
                    content: "x".into(),
                },
            ],
        };
        save(&path, &saved).unwrap();
        assert_eq!(load(&path).unwrap(), saved);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prompts_without_notes() {
        assert_eq!(without_notes("[The user undid it.]\n\nfix it"), "fix it");
        assert_eq!(without_notes("plain\n\ntext"), "plain\n\ntext");
    }

    #[test]
    fn a_folder_per_directory() {
        let d = dir_for(Path::new("/home/me/my project"));
        assert!(d.ends_with("conversations/-home-me-my-project"));
    }
}
