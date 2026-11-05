//! The context view: the exact last request, either as labelled parts
//! with their sizes or as the raw bytes.

use crossterm::style::Color;
use serde_json::Value;

use super::screen::Style;
use super::text::wrap;
use super::transcript::Line;

const HEAD: Style = Style::fg(Color::Yellow);

pub fn lines(body: &[u8], raw: bool, width: usize) -> Vec<Line> {
    let mut out = Vec::new();
    let mut add = |text: String, style: Style| out.push(Line::new(text, style));
    if body.is_empty() {
        add("Nothing has been sent yet.".into(), Style::dim());
        return out;
    }
    add(
        format!(
            "Exact request: {} · about {} tokens (bytes / 4) · {} for {}",
            bytes(body.len()),
            body.len() / 4,
            if raw { "raw bytes" } else { "parts" },
            if raw { "parts: r" } else { "raw bytes: r" },
        ),
        Style::dim(),
    );
    add(String::new(), Style::default());
    let text = String::from_utf8_lossy(body);
    if raw {
        for l in wrap(&text, width) {
            add(l, Style::default());
        }
        return out;
    }
    let Ok(Value::Object(req)) = serde_json::from_str::<Value>(&text) else {
        add(
            "The request is not a JSON object; press r for the raw bytes.".into(),
            Style::dim(),
        );
        return out;
    };
    for m in req
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("?");
        add(header(role, size(m), width), HEAD);
        if let Some(c) = m.get("content") {
            let c = match c {
                Value::String(s) => s.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            for l in wrap(&c, width) {
                add(l, Style::default());
            }
        }
        for call in m
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let f = &call["function"];
            let line = format!(
                "call {} {} [{}]",
                f["name"].as_str().unwrap_or("?"),
                f["arguments"].as_str().unwrap_or(""),
                call["id"].as_str().unwrap_or("")
            );
            for l in wrap(&line, width) {
                add(l, Style::fg(Color::Cyan));
            }
        }
    }
    if let Some(tools) = req.get("tools").and_then(Value::as_array) {
        let total = tools.iter().map(size).sum();
        add(
            header(&format!("tools ({})", tools.len()), total, width),
            HEAD,
        );
        for t in tools {
            let f = &t["function"];
            let line = format!(
                "{}: {}",
                f["name"].as_str().unwrap_or("?"),
                f["description"].as_str().unwrap_or("")
            );
            for l in wrap(&line, width) {
                add(l, Style::default());
            }
            for l in wrap(&format!("  {}", f["parameters"]), width) {
                add(l, Style::dim());
            }
        }
    }
    let rest: Vec<String> = req
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "messages" | "tools"))
        .map(|(k, v)| format!("{k}: {v}"))
        .collect();
    let rest_size = rest.iter().map(String::len).sum();
    add(header("other fields", rest_size, width), HEAD);
    for r in rest {
        for l in wrap(&r, width) {
            add(l, Style::default());
        }
    }
    out
}

fn header(name: &str, size: usize, width: usize) -> String {
    let label = format!("── {name} · {} ", bytes(size));
    let fill = width.saturating_sub(super::text::width(&label));
    label + &"─".repeat(fill)
}

fn size(v: &Value) -> usize {
    serde_json::to_vec(v).map_or(0, |b| b.len())
}

fn bytes(n: usize) -> String {
    if n < 10_000 {
        format!("{n} B")
    } else {
        format!("{:.1} KB", n as f64 / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_parts_with_sizes() {
        let body = br#"{"model":"m","messages":[{"role":"system","content":"be brief"},{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"read","description":"Read a file","parameters":{}}}],"stream":true}"#;
        let text: Vec<String> = lines(body, false, 40)
            .into_iter()
            .map(|l| l.text())
            .collect();
        assert!(text[0].starts_with(&format!("Exact request: {} B", body.len())));
        assert!(
            text.iter().any(|l| l.starts_with("── system · 38 B ")),
            "{text:#?}"
        );
        assert!(text.iter().any(|l| l == "be brief"));
        assert!(text.iter().any(|l| l.starts_with("── tools (1)")));
        assert!(text.iter().any(|l| l == "read: Read a file"));
        assert!(text.iter().any(|l| l == "stream: true"));
    }

    #[test]
    fn raw_shows_the_bytes() {
        let text: Vec<String> = lines(b"{\"a\":1}", true, 40)
            .into_iter()
            .map(|l| l.text())
            .collect();
        assert_eq!(text[2], "{\"a\":1}");
    }
}
