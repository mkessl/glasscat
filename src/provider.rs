//! Known providers that speak the OpenAI chat format.

use crate::Result;
use crate::openai::Prefill;

pub struct Preset {
    pub name: &'static str,
    pub base_url: &'static str,
    /// Environment variable holding the API key; `None` for local servers.
    pub key_env: Option<&'static str>,
    pub default_model: &'static str,
    pub prefill: Prefill,
}

pub const PRESETS: &[Preset] = &[
    Preset {
        name: "openrouter",
        base_url: "https://openrouter.ai/api/v1",
        key_env: Some("OPENROUTER_API_KEY"),
        // Routes only to models that cost nothing.
        default_model: "openrouter/free",
        prefill: Prefill::Plain,
    },
    Preset {
        name: "groq",
        base_url: "https://api.groq.com/openai/v1",
        key_env: Some("GROQ_API_KEY"),
        default_model: "openai/gpt-oss-120b",
        prefill: Prefill::Plain,
    },
    Preset {
        name: "deepseek",
        // The beta address is the one that accepts a reply prefix.
        base_url: "https://api.deepseek.com/beta",
        key_env: Some("DEEPSEEK_API_KEY"),
        default_model: "deepseek-flash",
        prefill: Prefill::PrefixFlag,
    },
    Preset {
        name: "mistral",
        base_url: "https://api.mistral.ai/v1",
        key_env: Some("MISTRAL_API_KEY"),
        default_model: "mistral-small-latest",
        prefill: Prefill::PrefixFlag,
    },
    Preset {
        name: "kimi",
        base_url: "https://api.moonshot.ai/v1",
        key_env: Some("MOONSHOT_API_KEY"),
        default_model: "kimi-k2.6",
        prefill: Prefill::PartialFlag,
    },
    Preset {
        name: "ollama",
        base_url: "http://localhost:11434/v1",
        key_env: None,
        default_model: "qwen3:1.7b",
        prefill: Prefill::None,
    },
    Preset {
        name: "llamacpp",
        base_url: "http://127.0.0.1:8080/v1",
        key_env: None,
        default_model: "default",
        prefill: Prefill::Plain,
    },
    Preset {
        name: "lmstudio",
        base_url: "http://localhost:1234/v1",
        key_env: None,
        default_model: "default",
        prefill: Prefill::None,
    },
];

pub fn preset(name: &str) -> Result<&'static Preset> {
    PRESETS
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| format!("unknown provider {name}").into())
}

impl Preset {
    pub fn api_key(&self) -> Result<Option<String>> {
        let Some(var) = self.key_env else {
            return Ok(None);
        };
        match std::env::var(var) {
            Ok(k) if !k.trim().is_empty() => Ok(Some(k.trim().to_string())),
            _ => Err(format!("{} needs an API key in {var}", self.name).into()),
        }
    }
}
