//! What the model is given, kept in the agent's own form. Each provider
//! format is built from this, so the same context can go to any provider.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum Item {
    System(String),
    User(String),
    Assistant(Reply),
    ToolResult {
        call_id: String,
        content: String,
    },
    /// An image a tool gave, shown to the model after the tool results;
    /// `data` is base64.
    Image {
        name: String,
        mime: String,
        data: String,
    },
}

/// One model turn: text, reasoning and the tool calls it made.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct Reply {
    pub text: String,
    pub reasoning: String,
    pub calls: Vec<ToolCall>,
    /// Stopped before the model finished: cancelled, broken off, or out of
    /// output tokens.
    pub cut: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Arguments as the model wrote them; usually JSON, not checked here.
    pub args: String,
}

/// A tool as the model sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments.
    pub parameters: Value,
}

/// The context as plain text for editing by hand: each part under a
/// header line, `=== system`, `=== user`, `=== assistant` (with
/// `--- reasoning` and `--- call ID NAME` inside), `=== tool CALL_ID`,
/// `=== image MEDIA_TYPE NAME` over base64 lines.
/// A line of content that starts with `===`, `---` or `\` gets a `\`
/// in front, so headers stay unambiguous.
pub fn to_text(items: &[Item]) -> String {
    let mut out = String::new();
    let mut part = |header: &str, body: &str| {
        out.push_str(header);
        out.push('\n');
        for line in body.split('\n') {
            if line.starts_with("===") || line.starts_with("---") || line.starts_with('\\') {
                out.push('\\');
            }
            out.push_str(line);
            out.push('\n');
        }
    };
    for item in items {
        match item {
            Item::System(t) => part("=== system", t),
            Item::User(t) => part("=== user", t),
            Item::Assistant(r) => {
                part(
                    if r.cut {
                        "=== assistant (cut)"
                    } else {
                        "=== assistant"
                    },
                    &r.text,
                );
                if !r.reasoning.is_empty() {
                    part("--- reasoning", &r.reasoning);
                }
                for c in &r.calls {
                    part(&format!("--- call {} {}", c.id, c.name), &c.args);
                }
            }
            Item::ToolResult { call_id, content } => part(&format!("=== tool {call_id}"), content),
            Item::Image { name, mime, data } => {
                let lines: Vec<&str> = data
                    .as_bytes()
                    .chunks(76)
                    .map(|c| std::str::from_utf8(c).unwrap_or_default())
                    .collect();
                part(&format!("=== image {mime} {name}"), &lines.join("\n"));
            }
        }
    }
    out
}

/// Reads text written by `to_text`, perhaps edited.
pub fn from_text(text: &str) -> Result<Vec<Item>, String> {
    // Header lines with the content lines under each.
    let mut parts: Vec<(usize, &str, Vec<String>)> = Vec::new();
    let lines = text.strip_suffix('\n').unwrap_or(text).split('\n');
    for (n, line) in lines.enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with("===") || line.starts_with("---") {
            parts.push((n + 1, line, Vec::new()));
        } else if let Some((_, _, body)) = parts.last_mut() {
            body.push(line.strip_prefix('\\').unwrap_or(line).to_string());
        } else if !line.trim().is_empty() {
            return Err(format!("line {}: text before the first === header", n + 1));
        }
    }
    let mut items = Vec::new();
    for (n, header, body) in parts {
        let body = body.join("\n");
        let words: Vec<&str> = header.split_whitespace().collect();
        let err = |what: &str| Err(format!("line {n}: {what}: {header}"));
        match words.as_slice() {
            ["===", "system"] => items.push(Item::System(body)),
            ["===", "user"] => items.push(Item::User(body)),
            ["===", "assistant"] | ["===", "assistant", "(cut)"] => {
                items.push(Item::Assistant(Reply {
                    text: body,
                    cut: words.len() == 3,
                    ..Reply::default()
                }));
            }
            ["===", "image", mime, name @ ..] if !name.is_empty() => items.push(Item::Image {
                name: name.join(" "),
                mime: mime.to_string(),
                data: body.split_whitespace().collect(),
            }),
            ["===", "tool", id] => items.push(Item::ToolResult {
                call_id: id.to_string(),
                content: body,
            }),
            ["---", rest @ ..] => {
                let Some(Item::Assistant(r)) = items.last_mut() else {
                    return err("--- belongs inside an assistant part");
                };
                match rest {
                    ["reasoning"] => r.reasoning = body,
                    ["call", id, name] => r.calls.push(ToolCall {
                        id: id.to_string(),
                        name: name.to_string(),
                        args: body,
                    }),
                    _ => return err("expected --- reasoning or --- call ID NAME"),
                }
            }
            _ => return err("unknown header"),
        }
    }
    if !matches!(items.first(), Some(Item::System(_))) {
        return Err("the first part must be === system (it may be empty)".into());
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trip() {
        let items = vec![
            Item::System("sys\n".into()),
            Item::User("=== not a header\n--- nor this\n\\ back\n\n".into()),
            Item::Assistant(Reply {
                text: String::new(),
                reasoning: "hm".into(),
                calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    args: "{\"path\":\"a\"}".into(),
                }],
                cut: false,
            }),
            Item::ToolResult {
                call_id: "c1".into(),
                content: "x\ny".into(),
            },
            Item::Image {
                name: "a b.png".into(),
                mime: "image/png".into(),
                data: "QUJD".repeat(40),
            },
            Item::Assistant(Reply {
                text: "par".into(),
                cut: true,
                ..Reply::default()
            }),
        ];
        let text = to_text(&items);
        assert_eq!(from_text(&text).unwrap(), items);
        assert!(text.contains("\\=== not a header"));
    }

    #[test]
    fn starts_with_the_system_prompt() {
        assert!(from_text("=== user\nhi\n").is_err());
        assert_eq!(
            from_text("=== system\n").unwrap(),
            [Item::System(String::new())]
        );
    }

    #[test]
    fn errors_name_the_line() {
        assert_eq!(
            from_text("=== user\nhi\n=== bogus\n").unwrap_err(),
            "line 3: unknown header: === bogus"
        );
        assert!(
            from_text("--- reasoning\nx")
                .unwrap_err()
                .contains("inside an assistant")
        );
        assert!(
            from_text("stray\n=== user\n")
                .unwrap_err()
                .contains("line 1")
        );
    }
}
