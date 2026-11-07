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
    /// MCP servers whose tools the model gets.
    pub mcp: BTreeMap<String, crate::mcp::Config>,
    /// Files in the working directory whose text goes into the system
    /// prompt; when not set, the first of `INSTRUCTIONS` there is.
    pub instructions: Option<Vec<String>>,
    /// The model's shell: `landlock`, `unrestricted` or `off`.
    pub shell: crate::shell::Mode,
    /// Whether the most dangerous commands wait for the user's yes.
    pub confirm: Option<bool>,
}

impl Settings {
    /// The instruction files to read, and whether only the first found
    /// is taken.
    pub fn instruction_files(&self) -> (Vec<String>, bool) {
        match &self.instructions {
            Some(list) => (list.clone(), false),
            None => (INSTRUCTIONS.iter().map(|s| s.to_string()).collect(), true),
        }
    }
}

/// Instruction files of a project, as agents name them: `AGENTS.md`,
/// which most read, then those of single agents. Projects often keep the
/// same text under several names, so only the first found is taken.
pub const INSTRUCTIONS: &[&str] = &[
    "AGENTS.md",
    "CLAUDE.md",
    "GEMINI.md",
    ".github/copilot-instructions.md",
    ".cursorrules",
    ".windsurfrules",
    ".clinerules",
    "CONVENTIONS.md",
];

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

/// What the command line gave, kept so the settings can be applied again
/// without overriding it.
#[derive(Clone, Debug, Default)]
pub struct Launch {
    pub provider: String,
    pub allow: Vec<PathBuf>,
    pub hide: Vec<PathBuf>,
    pub proxy: Option<String>,
}

/// Applies the settings that can change while the agent runs: request
/// fields, the proxy, allowed and hidden paths, and the user's own tools.
/// Nothing changes unless all of them are valid.
pub fn apply(session: &mut crate::agent::Session, s: &Settings) -> Result<()> {
    let launch = &session.launch;
    let provider = crate::provider::find(&launch.provider, s)?;
    let cwd = session.tools.cwd().to_path_buf();
    let allowed: Vec<PathBuf> = std::iter::once(cwd.clone())
        .chain(launch.allow.iter().cloned())
        .chain(s.allow.iter().cloned())
        .collect();
    let hidden: Vec<PathBuf> = launch.hide.iter().chain(&s.hide).cloned().collect();
    let access = crate::access::Access::new(&cwd, &allowed, &hidden)
        .map_err(|e| format!("a folder to allow: {e}"))?;
    let builtin = provider.builtin_fields();
    let proxy = launch.proxy.clone().or(provider.proxy);
    if let Some(p) = &proxy {
        crate::openai::parse_proxy(p)?;
    }
    session.tools.shell = s.shell;
    session.tools.confirm = s.confirm.unwrap_or(true);
    session
        .set_custom(s.tools.clone())
        .map_err(|e| format!("{}: {e}", path().display()))?;
    session.tools.access = access;
    let mcp = crate::mcp::with_builtin(&s.mcp);
    if !session.tools.mcp.same(&mcp) {
        session.tools.mcp = crate::mcp::Servers::start(&mcp, &cwd);
    }
    session.proxy = proxy;
    session.fields.builtin = builtin;
    session.fields.settings = provider.request;
    session.fields.models = s
        .models
        .iter()
        .map(|(id, m)| (id.clone(), m.request.clone()))
        .collect();
    Ok(())
}

/// Writes the paths hidden now into the settings file as its `hide`,
/// leaving the rest of the file as it was; returns what was written.
pub fn keep_hidden(session: &crate::agent::Session) -> Result<String> {
    let path = path();
    let mut file: Map<String, Value> = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| format!("{}: {e}; /settings fixes it", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Map::new(),
        Err(e) => return Err(format!("{}: {e}", path.display()).into()),
    };
    let saved: Vec<PathBuf> = match file.get("hide") {
        Some(v) => serde_json::from_value(v.clone()).unwrap_or_default(),
        None => Vec::new(),
    };
    let list = hide_list(&session.tools.access, &saved, &session.launch.hide);
    if list.is_empty() {
        file.remove("hide");
    } else {
        file.insert("hide".into(), serde_json::to_value(&list)?);
    }
    std::fs::create_dir_all(dir()).map_err(|e| format!("{}: {e}", dir().display()))?;
    let text = serde_json::to_string_pretty(&file)? + "\n";
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    let shown: Vec<String> = list.iter().map(|p| p.display().to_string()).collect();
    Ok(if shown.is_empty() {
        format!("{} hides nothing now.", path.display())
    } else {
        format!("{} hides {} now.", path.display(), shown.join(", "))
    })
}

/// The `hide` list for what `access` hides: entries the file already has
/// stay as written while they still hide something, so a rule like `.env`
/// keeps applying in every folder; paths hidden since are added in full.
/// What the command line hid is left to the command line.
fn hide_list(
    access: &crate::access::Access,
    saved: &[PathBuf],
    launch: &[PathBuf],
) -> Vec<PathBuf> {
    let mut list: Vec<PathBuf> = Vec::new();
    for h in access.hidden() {
        if launch.iter().any(|l| access.rule(l) == *h) {
            continue;
        }
        let entry = saved
            .iter()
            .find(|e| access.rule(e) == *h)
            .unwrap_or(h)
            .clone();
        if !list.contains(&entry) {
            list.push(entry);
        }
    }
    list
}

fn parse(bytes: &[u8]) -> serde_json::Result<Settings> {
    serde_json::from_slice(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_hidden_paths_as_written() {
        let cwd = std::env::temp_dir().canonicalize().unwrap();
        let mut a = crate::access::Access::new(
            &cwd,
            std::slice::from_ref(&cwd),
            &[".env".into(), "old".into(), "flag".into()],
        )
        .unwrap();
        a.unhide(std::path::Path::new("old"));
        a.hide(std::path::Path::new("new"));
        let saved = [PathBuf::from(".env"), PathBuf::from("old")];
        assert_eq!(
            hide_list(&a, &saved, &[PathBuf::from("flag")]),
            [PathBuf::from(".env"), cwd.join("new")]
        );
    }

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
