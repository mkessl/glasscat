//! The user's settings: a JSON file in the config directory. Every
//! field is optional; options given on the command line win.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::Result;
use crate::openai::Prefill;

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// The provider used when none is given.
    pub provider: Option<String>,
    /// The model used with that provider when none is given.
    pub model: Option<String>,
    /// More providers, or changes to built-in ones, by name.
    pub providers: BTreeMap<String, Provider>,
    /// More folders the agent may work in.
    pub allow: Vec<PathBuf>,
    /// Paths the model must not see.
    pub hide: Vec<PathBuf>,
    /// A proxy for every provider that sets none of its own.
    pub proxy: Option<String>,
    /// Fields set in every request body, such as `temperature`; `null`
    /// removes one.
    pub request: Map<String, Value>,
    /// Settings for one model, by its id.
    pub models: BTreeMap<String, Model>,
    /// Tools of the user's own: programs the model may run.
    pub tools: BTreeMap<String, crate::custom::CustomTool>,
    /// Files in the working directory whose text goes into the system
    /// prompt; `AGENTS.md` when not set.
    pub instructions: Option<Vec<String>>,
}

impl Settings {
    pub fn instruction_files(&self) -> Vec<String> {
        self.instructions
            .clone()
            .unwrap_or_else(|| vec!["AGENTS.md".into()])
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub base_url: String,
    /// Environment variable holding the API key; none for local servers.
    pub key_env: Option<String>,
    pub model: String,
    #[serde(default = "no_prefill")]
    pub prefill: Prefill,
    /// A proxy for this provider only, such as `socks5://127.0.0.1:1080`.
    pub proxy: Option<String>,
    /// Request fields for this provider, over the general ones.
    #[serde(default)]
    pub request: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Model {
    /// Request fields for this model, over the general and provider ones.
    pub request: Map<String, Value>,
}

fn no_prefill() -> Prefill {
    Prefill::None
}

/// `$XDG_CONFIG_HOME/<name>/settings.json`, `~/.config/<name>/...`, or
/// `%APPDATA%\<name>\...`.
pub fn path() -> PathBuf {
    dir().join("settings.json")
}

/// The folder holding the settings and the user's own `AGENTS.md`.
pub fn dir() -> PathBuf {
    let env = |k| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let base = if cfg!(windows) {
        env("APPDATA")
    } else {
        env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|h| h.join(".config")))
    };
    base.unwrap_or_else(std::env::temp_dir).join(crate::NAME)
}

/// The settings, or the defaults when there is no file.
pub fn load() -> Result<Settings> {
    let path = path();
    match std::fs::read(&path) {
        Ok(bytes) => parse(&bytes).map_err(|e| format!("{}: {e}", path.display()).into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(format!("{}: {e}", path.display()).into()),
    }
}

fn parse(bytes: &[u8]) -> serde_json::Result<Settings> {
    serde_json::from_slice(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_providers_and_rejects_typos() {
        let s = parse(
            br#"{
                "provider": "local",
                "providers": {
                    "local": { "base_url": "http://127.0.0.1:8000/v1", "model": "qwen", "prefill": "plain",
                               "request": { "top_k": 20 } }
                },
                "request": { "temperature": 0.6 },
                "hide": [".env"]
            }"#,
        )
        .unwrap();
        assert_eq!(s.provider.as_deref(), Some("local"));
        assert_eq!(s.providers["local"].prefill, Prefill::Plain);
        assert_eq!(s.providers["local"].key_env, None);
        assert_eq!(s.hide, [PathBuf::from(".env")]);
        assert_eq!(s.request["temperature"], 0.6);
        assert_eq!(s.providers["local"].request["top_k"], 20);
        let typo = parse(br#"{"provder": "x"}"#).unwrap_err().to_string();
        assert!(typo.contains("unknown field `provder`"), "{typo}");
        assert_eq!(parse(b"{}").unwrap(), Settings::default());
    }
}
