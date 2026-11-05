//! What a provider's model list says about a model: for now, its context
//! window. OpenAI-compatible servers name that field differently.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::Result;
use crate::openai::get_json;

/// OpenRouter and Kimi, Groq, Mistral and LM Studio, vLLM.
const WINDOW_KEYS: &[&str] = &[
    "context_length",
    "context_window",
    "max_context_length",
    "max_model_len",
];

/// The context window of `model` in tokens, if the provider's `/models`
/// list gives one. The list is fetched once per run: OpenRouter's is
/// most of a megabyte, and its free router picks a new model each time.
pub fn context_window(
    base_url: &str,
    api_key: Option<&str>,
    proxy: Option<&str>,
    model: &str,
) -> Result<Option<u64>> {
    static LISTS: Mutex<Option<HashMap<String, Arc<Value>>>> = Mutex::new(None);
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let cached = LISTS
        .lock()
        .unwrap()
        .get_or_insert_default()
        .get(&url)
        .cloned();
    let list = match cached {
        Some(list) => list,
        None => {
            let list = Arc::new(get_json(&url, api_key, proxy)?);
            let mut lists = LISTS.lock().unwrap();
            lists.get_or_insert_default().insert(url, list.clone());
            list
        }
    };
    Ok(window_in(&list, model))
}

fn window_in(list: &Value, model: &str) -> Option<u64> {
    let entry = list["data"].as_array()?.iter().find(|m| m["id"] == model)?;
    WINDOW_KEYS.iter().find_map(|k| entry[k].as_u64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finds_the_window_under_any_name() {
        let list = json!({"data": [
            {"id": "a", "context_length": 200000},
            {"id": "b", "context_window": 131072},
            {"id": "c", "max_model_len": 8192},
            {"id": "d"},
        ]});
        assert_eq!(window_in(&list, "a"), Some(200000));
        assert_eq!(window_in(&list, "b"), Some(131072));
        assert_eq!(window_in(&list, "c"), Some(8192));
        assert_eq!(window_in(&list, "d"), None);
        assert_eq!(window_in(&list, "e"), None);
    }
}
