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
    Ok(window_in(&*list(base_url, api_key, proxy)?, model))
}

/// The provider's `/models` list, fetched once per run.
pub fn list(base_url: &str, api_key: Option<&str>, proxy: Option<&str>) -> Result<Arc<Value>> {
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
    Ok(list)
}

/// A token count in a few characters: 950, 8.2k, 131k, 1M, 1.5M.
pub fn short(n: u64) -> String {
    let f = |v: f64, unit: &str| {
        let s = if v < 10.0 {
            format!("{v:.1}")
        } else {
            format!("{v:.0}")
        };
        format!("{}{unit}", s.trim_end_matches(".0"))
    };
    match n {
        0..1000 => n.to_string(),
        1000..1_000_000 => f(n as f64 / 1000.0, "k"),
        _ => f(n as f64 / 1_000_000.0, "M"),
    }
}

/// The models whose id contains every word of `filter`, one per line with
/// the context window and price where the list gives them; at most `max`.
pub fn matching(list: &Value, filter: &str, max: usize) -> String {
    let words: Vec<String> = filter.split_whitespace().map(str::to_lowercase).collect();
    let mut found: Vec<&Value> = list["data"]
        .as_array()
        .map(|a| a.iter())
        .into_iter()
        .flatten()
        .filter(|m| {
            let id = m["id"].as_str().unwrap_or_default().to_lowercase();
            words.iter().all(|w| id.contains(w.as_str()))
        })
        .collect();
    found.sort_by_key(|m| m["id"].as_str().unwrap_or_default().to_string());
    if found.is_empty() {
        return format!("No model id here contains {filter:?}.");
    }
    let mut out: Vec<String> = found
        .iter()
        .take(max)
        .map(|m| {
            let mut line = m["id"].as_str().unwrap_or_default().to_string();
            if let Some(w) = WINDOW_KEYS.iter().find_map(|k| m[k].as_u64()) {
                line += &format!(" · {}", short(w));
            }
            let per_m = |k: &str| {
                let p: f64 = m["pricing"][k].as_str()?.parse().ok()?;
                Some(p * 1e6)
            };
            match (per_m("prompt"), per_m("completion")) {
                (Some(i), Some(o)) if i == 0.0 && o == 0.0 => line += " · free",
                (Some(i), Some(o)) => line += &format!(" · ${i:.2} in, ${o:.2} out per M"),
                _ => {}
            }
            line
        })
        .collect();
    if found.len() > max {
        out.push(format!(
            "and {} more; add words to narrow it down.",
            found.len() - max
        ));
    }
    out.join("\n")
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
    fn lists_matching_models() {
        let list = json!({"data": [
            {"id": "z/free-one", "context_length": 32000,
             "pricing": {"prompt": "0", "completion": "0"}},
            {"id": "a/big", "context_length": 200000,
             "pricing": {"prompt": "0.000003", "completion": "0.000015"}},
            {"id": "a/small"},
        ]});
        assert_eq!(
            matching(&list, "a/", 5),
            "a/big · 200k · $3.00 in, $15.00 out per M\na/small"
        );
        assert_eq!(matching(&list, "FREE one", 5), "z/free-one · 32k · free");
        assert_eq!(
            matching(&list, "", 1),
            "a/big · 200k · $3.00 in, $15.00 out per M\nand 2 more; add words to narrow it down."
        );
        assert!(matching(&list, "nope", 5).starts_with("No model"));
    }

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
