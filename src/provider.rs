//! Known providers that speak the OpenAI chat format.

use crate::Result;
use crate::openai::Prefill;
use crate::settings::Settings;

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

/// A provider to talk to: a built-in one or one from the settings.
#[derive(Clone, Debug, PartialEq)]
pub struct Provider {
    pub name: String,
    pub base_url: String,
    pub key_env: Option<String>,
    pub default_model: String,
    pub prefill: Prefill,
    pub proxy: Option<String>,
    /// Request fields: the general ones from the settings, then this
    /// provider's own over them.
    pub request: serde_json::Map<String, serde_json::Value>,
}

/// The provider called `name`; the settings may add providers or
/// change built-in ones.
pub fn find(name: &str, settings: &Settings) -> Result<Provider> {
    if let Some(p) = settings.providers.get(name) {
        return Ok(Provider {
            name: name.into(),
            base_url: p.base_url.clone(),
            key_env: p.key_env.clone(),
            default_model: p.model.clone(),
            prefill: p.prefill,
            proxy: p.proxy.clone().or_else(|| settings.proxy.clone()),
            request: settings
                .request
                .clone()
                .into_iter()
                .chain(p.request.clone())
                .collect(),
        });
    }
    let p = PRESETS.iter().find(|p| p.name == name).ok_or_else(|| {
        format!(
            "unknown provider {name}; add it under \"providers\" in {}",
            crate::settings::path().display()
        )
    })?;
    Ok(Provider {
        name: p.name.into(),
        base_url: p.base_url.into(),
        key_env: p.key_env.map(Into::into),
        default_model: p.default_model.into(),
        prefill: p.prefill,
        proxy: settings.proxy.clone(),
        request: settings.request.clone(),
    })
}

impl Provider {
    /// Request fields sent unless the settings change them: OpenRouter
    /// caches Claude's prompt only when asked to, with `cache_control`;
    /// it then keeps the cache at the end of each request.
    pub fn builtin_fields(&self) -> Vec<(String, serde_json::Map<String, serde_json::Value>)> {
        if !self.base_url.contains("openrouter.ai") {
            return Vec::new();
        }
        let mut cache = serde_json::Map::new();
        cache.insert(
            "cache_control".into(),
            serde_json::json!({"type": "ephemeral"}),
        );
        vec![("anthropic/".into(), cache)]
    }

    pub fn api_key(&self) -> Result<Option<String>> {
        let Some(var) = &self.key_env else {
            return Ok(None);
        };
        match std::env::var(var) {
            Ok(k) if !k.trim().is_empty() => Ok(Some(k.trim().to_string())),
            _ => {
                let mut e = format!("{} needs an API key in {var}.", self.name);
                if let Some(page) = keys_page(&self.name) {
                    e += &format!("\nGet one at {page}");
                    if self.name == "openrouter" {
                        e += "; its free models, the default, need no credit";
                    }
                    e += ".";
                }
                let set = if cfg!(windows) {
                    format!("set {var}=...")
                } else {
                    format!("export {var}=...")
                };
                e += &format!(
                    "\nThen: {set}\nOr use a model on this computer, which needs no key: \
                     --provider ollama, llamacpp or lmstudio."
                );
                Err(e.into())
            }
        }
    }
}

/// Where a preset provider hands out API keys.
fn keys_page(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "openrouter" => "https://openrouter.ai/keys",
        "groq" => "https://console.groq.com/keys",
        "deepseek" => "https://platform.deepseek.com/api_keys",
        "mistral" => "https://console.mistral.ai/api-keys",
        "kimi" => "https://platform.kimi.ai/console/api-keys",
        _ => return None,
    })
}
