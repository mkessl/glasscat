//! Why the provider's prompt cache gave back less than it could. Each
//! request repeats the one before it and adds to the end, so a provider
//! that caches prefixes should find nearly all of the last request again.
//! When it finds much less, what changed in between is told plainly: the
//! request itself, the provider that ran it, or nothing at all.

use std::time::Duration;

use serde_json::Value;

use crate::openai::Served;

/// What is kept of a request to compare the next one with.
pub struct Sent {
    pub body: Vec<u8>,
    pub prompt_tokens: u64,
    pub served: Option<Served>,
}

/// Below this many tokens providers do not cache at all.
const MIN_CACHED: u64 = 1024;

/// Why the cache missed, for the user.
#[derive(Debug, PartialEq)]
pub struct Miss {
    pub text: String,
    /// Nothing changed: the provider did not give the cache back. Said
    /// once per provider is enough.
    pub unchanged: bool,
}

/// What to tell the user when the cache gave back less than half of what
/// the last request sent, or None. `gap` is the time from the last
/// request's end to this one's start; `openrouter` adds how to keep to
/// one provider there.
pub fn explain_miss(
    last: &Sent,
    body: &[u8],
    usage: &Value,
    served: Option<&Served>,
    gap: Duration,
    openrouter: bool,
) -> Option<Miss> {
    // Only providers that report cached tokens say anything to compare.
    let cached = usage
        .pointer("/prompt_tokens_details/cached_tokens")?
        .as_u64()?;
    let prompt = usage.get("prompt_tokens")?.as_u64()?;
    let before = last.prompt_tokens;
    if before < MIN_CACHED || cached * 2 >= before {
        return None;
    }
    let (was, now) = (last.served.as_ref(), served);
    // Another model has its own cache; the status bar already says which ran.
    if was.map(|s| &s.model) != now.map(|s| &s.model) {
        return None;
    }
    let head = format!("The cache gave back {cached} of {prompt} tokens");
    let was_on = was.and_then(|s| s.provider.as_deref());
    let now_on = now.and_then(|s| s.provider.as_deref());
    let parse = |b: &[u8]| serde_json::from_slice::<Value>(b).ok();
    let new = parse(body);
    if let (Some(a), Some(b)) = (was_on, now_on)
        && a != b
    {
        let mut text = format!(
            "{head}: this request ran on {b}, the last one on {a}, and each provider keeps its \
             own cache."
        );
        if openrouter {
            // Kept with what the provider field already asks, such as zdr.
            let mut field = new
                .as_ref()
                .and_then(|n| n.get("provider"))
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            field.insert("order".into(), serde_json::json!([b]));
            text += &format!(
                " /set provider {} asks OpenRouter to keep to one.",
                Value::Object(field)
            );
        }
        return Some(Miss {
            text,
            unchanged: false,
        });
    }
    let (Some(old), Some(new)) = (parse(&last.body), new) else {
        return None;
    };
    if let Some(change) = first_change(&old, &new) {
        return Some(Miss {
            text: format!("{head}: {change} since the last request."),
            unchanged: false,
        });
    }
    Some(Miss {
        text: format!(
            "{head}, though the {before} tokens of the last request went again unchanged, {} \
             after it. The provider no longer had them, or did not look.",
            span(gap)
        ),
        unchanged: true,
    })
}

/// The first part of the old request that the new one does not repeat.
fn first_change(old: &Value, new: &Value) -> Option<String> {
    if old["tools"] != new["tools"] {
        return Some("the tool definitions changed".into());
    }
    let (Some(a), Some(b)) = (old.as_object(), new.as_object()) else {
        return None;
    };
    let mut fields: Vec<&str> = a
        .keys()
        .chain(b.keys())
        .map(String::as_str)
        .filter(|k| !matches!(*k, "messages" | "tools") && a.get(*k) != b.get(*k))
        .collect();
    fields.sort_unstable();
    fields.dedup();
    let empty = Vec::new();
    let list = |v: &Value| v["messages"].as_array().unwrap_or(&empty).clone();
    let (am, bm) = (list(old), list(new));
    let changed = am.iter().zip(&bm).position(|(x, y)| x != y);
    let message = match changed {
        Some(0) if am[0]["role"] == "system" => Some("the system prompt changed".to_string()),
        Some(i) => Some(format!(
            "message {} of the {} sent last time changed",
            i + 1,
            am.len()
        )),
        None if bm.len() < am.len() => Some("messages were taken out".to_string()),
        None => None,
    };
    let fields =
        (!fields.is_empty()).then(|| format!("the request fields {} changed", fields.join(", ")));
    match (message, fields) {
        (Some(m), Some(f)) => Some(format!("{m}, and {f}")),
        (m, f) => m.or(f),
    }
}

fn span(d: Duration) -> String {
    match d.as_secs() {
        s if s < 120 => format!("{s} seconds"),
        s => format!("{} minutes", s / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sent(body: &Value, tokens: u64, provider: &str) -> Sent {
        Sent {
            body: body.to_string().into_bytes(),
            prompt_tokens: tokens,
            served: Some(served(provider)),
        }
    }

    fn served(provider: &str) -> Served {
        Served {
            model: "m".into(),
            provider: Some(provider.into()),
        }
    }

    fn usage(prompt: u64, cached: u64) -> Value {
        json!({"prompt_tokens": prompt, "prompt_tokens_details": {"cached_tokens": cached}})
    }

    #[test]
    fn says_what_changed() {
        let old = json!({"model": "m", "provider": {"zdr": true}, "tools": [1], "messages": [
            {"role": "system", "content": "s"}, {"role": "user", "content": "a"}]});
        let last = sent(&old, 5000, "P");
        let gap = Duration::from_secs(400);
        let explain = |new: &Value, u: Value, on: &str| {
            explain_miss(
                &last,
                new.to_string().as_bytes(),
                &u,
                Some(&served(on)),
                gap,
                on == "R",
            )
            .map(|m| m.text)
        };
        let mut grown = old.clone();
        grown["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role": "assistant", "content": "b"}));
        assert_eq!(explain(&grown, usage(5100, 4900), "P"), None);
        assert_eq!(
            explain(&grown, usage(5100, 0), "Q").unwrap(),
            "The cache gave back 0 of 5100 tokens: this request ran on Q, the last one on P, \
             and each provider keeps its own cache."
        );
        assert!(explain(&grown, usage(5100, 0), "R").unwrap().ends_with(
            "/set provider {\"zdr\":true,\"order\":[\"R\"]} asks OpenRouter to keep to one."
        ));
        assert_eq!(
            explain(&grown, usage(5100, 0), "P").unwrap(),
            "The cache gave back 0 of 5100 tokens, though the 5000 tokens of the last request \
             went again unchanged, 6 minutes after it. The provider no longer had them, or did \
             not look."
        );
        let mut system = grown.clone();
        system["messages"][0]["content"] = json!("t");
        system["temperature"] = json!(0.3);
        assert_eq!(
            explain(&system, usage(5100, 0), "P").unwrap(),
            "The cache gave back 0 of 5100 tokens: the system prompt changed, and the request \
             fields temperature changed since the last request."
        );
        let mut tools = grown.clone();
        tools["tools"] = json!([2]);
        assert!(
            explain(&tools, usage(5100, 0), "P")
                .unwrap()
                .contains("tool definitions")
        );
        let mut edited = grown.clone();
        edited["messages"][1]["content"] = json!("x");
        assert!(
            explain(&edited, usage(5100, 0), "P")
                .unwrap()
                .contains("message 2 of the 2")
        );
        // No cached count reported, or too little sent before: nothing to say.
        assert_eq!(explain(&grown, json!({"prompt_tokens": 5100}), "P"), None);
        let small = sent(&old, 900, "P");
        assert_eq!(
            explain_miss(
                &small,
                b"{}",
                &usage(1000, 0),
                Some(&served("P")),
                gap,
                true
            ),
            None
        );
    }
}
