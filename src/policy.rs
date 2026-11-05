//! What is published about the provider that served a reply. Only
//! OpenRouter has an API for it so far: the quantization of each endpoint,
//! which endpoints keep no data (zero data retention), and where each
//! provider is based. None of it says whether a provider trains on data.

use serde_json::Value;

use crate::Result;
use crate::openai::{Served, get_json};

/// Whether `base_url` has a policy API this module reads.
pub fn known(base_url: &str) -> bool {
    base_url.contains("openrouter.ai")
}

/// What OpenRouter publishes about the provider that served `s`.
pub fn describe(base_url: &str, proxy: Option<&str>, s: &Served) -> Result<String> {
    let Some(provider) = &s.provider else {
        return Ok(format!(
            "The reply came from {}; the stream did not say which provider ran it.",
            s.model
        ));
    };
    let base = base_url.trim_end_matches('/');
    let endpoints = get_json(&format!("{base}/models/{}/endpoints", s.model), None, proxy)?;
    let providers = get_json(&format!("{base}/providers"), None, proxy)?;
    let zdr = get_json(&format!("{base}/endpoints/zdr"), None, proxy)?;
    Ok(lines(&s.model, provider, &endpoints, &providers, &zdr))
}

fn lines(model: &str, provider: &str, endpoints: &Value, providers: &Value, zdr: &Value) -> String {
    let list = |v: &Value, path: &[&str]| -> Vec<Value> {
        let mut v = v;
        for k in path {
            v = &v[k];
        }
        v.as_array().cloned().unwrap_or_default()
    };
    let str_of = |v: &Value, k: &str| v[k].as_str().unwrap_or_default().to_string();
    let ours: Vec<Value> = list(endpoints, &["data", "endpoints"])
        .into_iter()
        .filter(|e| e["provider_name"] == provider)
        .collect();
    let mut quant: Vec<String> = ours
        .iter()
        .map(|e| str_of(e, "quantization"))
        .filter(|q| !q.is_empty() && q != "unknown")
        .collect();
    quant.sort();
    quant.dedup();
    let zdr_count = list(zdr, &["data"])
        .iter()
        .filter(|e| e["model_id"] == model && e["provider_name"] == provider)
        .count();
    let mut out = format!("{model} was served by {provider}, as OpenRouter reports.");
    let mut line = |k: &str, v: String| out.push_str(&format!("\n  {k}: {v}"));
    line(
        "Quantization",
        if quant.is_empty() {
            "not stated".into()
        } else {
            quant.join(" or ")
        },
    );
    line(
        "Zero data retention",
        match zdr_count {
            0 => "no".into(),
            n if n >= ours.len() => "yes".into(),
            _ => "on some of its endpoints for this model; OpenRouter does not say which one ran"
                .into(),
        },
    );
    line(
        "Training on your data",
        "not in OpenRouter's API; see the privacy policy".into(),
    );
    let info = list(providers, &["data"])
        .into_iter()
        .find(|p| p["name"] == provider);
    if let Some(p) = info {
        let hq = str_of(&p, "headquarters");
        let dcs: Vec<String> = list(&p, &["datacenters"])
            .iter()
            .filter_map(|d| d.as_str().map(String::from))
            .collect();
        if !hq.is_empty() {
            line("Based in", hq);
        }
        if !dcs.is_empty() {
            line("Data centers", dcs.join(", "));
        }
        for (k, name) in [
            ("privacy_policy_url", "Privacy policy"),
            ("terms_of_service_url", "Terms"),
        ] {
            let url = str_of(&p, k);
            if !url.is_empty() {
                line(name, url);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_what_openrouter_publishes() {
        let endpoints = json!({"data": {"endpoints": [
            {"provider_name": "DeepInfra", "quantization": "bf16"},
            {"provider_name": "DeepInfra", "quantization": "fp4"},
            {"provider_name": "Other", "quantization": "fp8"},
        ]}});
        let providers = json!({"data": [{"name": "DeepInfra", "headquarters": "US",
            "datacenters": ["US"], "privacy_policy_url": "https://p", "terms_of_service_url": null}]});
        let zdr = json!({"data": [{"model_id": "m", "provider_name": "DeepInfra"}]});
        let text = lines("m", "DeepInfra", &endpoints, &providers, &zdr);
        assert_eq!(
            text,
            "m was served by DeepInfra, as OpenRouter reports.\n  Quantization: bf16 or fp4\n  \
             Zero data retention: on some of its endpoints for this model; OpenRouter does not \
             say which one ran\n  Training on your data: not in OpenRouter's API; see the \
             privacy policy\n  Based in: US\n  Data centers: US\n  Privacy policy: https://p"
        );
        let none = lines("m", "Other", &endpoints, &providers, &json!({"data": []}));
        assert!(
            none.contains("Quantization: fp8\n  Zero data retention: no"),
            "{none}"
        );
    }
}
