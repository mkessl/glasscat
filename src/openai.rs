//! The OpenAI chat completions format, which OpenRouter, Groq, DeepSeek,
//! Mistral and local servers (Ollama, llama.cpp, LM Studio, vLLM) speak.

use std::io::{BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::Result;
use crate::context::{Item, Reply, ToolCall, ToolSpec};
use crate::sse::Events;

/// The exact request body. The bytes built here are the bytes sent.
pub fn request_body(model: &str, items: &[Item], tools: &[ToolSpec]) -> Vec<u8> {
    let messages: Vec<Value> = items.iter().map(message).collect();
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if !tools.is_empty() {
        let tools: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    },
                })
            })
            .collect();
        body["tools"] = Value::Array(tools);
    }
    serde_json::to_vec(&body).expect("a JSON value always serializes")
}

fn message(item: &Item) -> Value {
    match item {
        Item::System(text) => json!({ "role": "system", "content": text }),
        Item::User(text) => json!({ "role": "user", "content": text }),
        Item::Assistant(reply) => {
            let mut m = json!({ "role": "assistant", "content": reply.text });
            if !reply.calls.is_empty() {
                let calls: Vec<Value> = reply
                    .calls
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "type": "function",
                            "function": { "name": c.name, "arguments": c.args },
                        })
                    })
                    .collect();
                m["tool_calls"] = Value::Array(calls);
            }
            m
        }
        Item::ToolResult { call_id, content } => {
            json!({ "role": "tool", "tool_call_id": call_id, "content": content })
        }
    }
}

/// Something the stream produced, passed on as soon as it arrives.
#[derive(Debug, PartialEq)]
pub enum Event<'a> {
    Text(&'a str),
    Reasoning(&'a str),
    /// The model started a tool call.
    Call(&'a str),
    /// A piece of the current tool call's arguments.
    CallArgs(&'a str),
}

/// How a streamed turn ended.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    pub reply: Reply,
    /// The provider's finish reason; `None` if the stream ended without one
    /// or was cancelled.
    pub finish: Option<String>,
    pub cancelled: bool,
    /// The usage object exactly as the provider sent it.
    pub usage: Option<Value>,
}

#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    usage: Option<Value>,
    error: Option<Value>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    content: Option<String>,
    /// OpenRouter's name for reasoning text.
    reasoning: Option<String>,
    /// DeepSeek's, vLLM's and others' name for it.
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<CallDelta>,
}

#[derive(Deserialize)]
struct CallDelta {
    index: Option<usize>,
    id: Option<String>,
    function: Option<FunctionDelta>,
}

#[derive(Deserialize)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

/// Reads a streamed response to its end, calling `on` for each piece.
/// When `cancel` is set the partial reply is returned as it stands.
pub fn read_stream(
    body: impl Read,
    cancel: &AtomicBool,
    mut on: impl FnMut(Event),
) -> Result<Outcome> {
    let mut events = Events::new(BufReader::new(body));
    let mut out = Outcome::default();
    // Provider call index -> position in `out.reply.calls`.
    let mut slots: Vec<(usize, usize)> = Vec::new();
    while let Some(data) = events.next_data()? {
        if cancel.load(Ordering::Relaxed) {
            out.cancelled = true;
            break;
        }
        if data == "[DONE]" {
            break;
        }
        let chunk: Chunk =
            serde_json::from_str(&data).map_err(|e| format!("bad stream chunk ({e}): {data}"))?;
        if let Some(err) = chunk.error {
            return Err(format!("provider error in stream: {err}").into());
        }
        if chunk.usage.is_some() {
            out.usage = chunk.usage;
        }
        for choice in chunk.choices {
            let d = choice.delta;
            for r in [d.reasoning, d.reasoning_content].into_iter().flatten() {
                if !r.is_empty() {
                    out.reply.reasoning.push_str(&r);
                    on(Event::Reasoning(&r));
                }
            }
            if let Some(t) = d.content.filter(|t| !t.is_empty()) {
                out.reply.text.push_str(&t);
                on(Event::Text(&t));
            }
            for c in d.tool_calls {
                let calls = &mut out.reply.calls;
                let index = c.index.unwrap_or(slots.len());
                let at = match slots.iter().find(|s| s.0 == index) {
                    Some(&(_, at)) => at,
                    None => {
                        slots.push((index, calls.len()));
                        calls.push(ToolCall::default());
                        calls.len() - 1
                    }
                };
                let call = &mut calls[at];
                if let Some(id) = c.id.filter(|id| !id.is_empty()) {
                    call.id = id;
                }
                let Some(f) = c.function else { continue };
                if let Some(name) = f.name.filter(|n| !n.is_empty()) {
                    call.name.push_str(&name);
                    on(Event::Call(&call.name));
                }
                if let Some(a) = f.arguments.filter(|a| !a.is_empty()) {
                    call.args.push_str(&a);
                    on(Event::CallArgs(&a));
                }
            }
            if choice.finish_reason.is_some() {
                out.finish = choice.finish_reason;
            }
        }
    }
    Ok(out)
}

/// Sends a request body to `{base_url}/chat/completions` and returns the
/// response stream. A non-success status becomes an error with the body.
pub fn send(base_url: &str, api_key: Option<&str>, body: &[u8]) -> Result<impl Read + use<>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let mut req = agent.post(&url).content_type("application/json");
    if let Some(key) = api_key {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    let resp = req.send(body)?;
    let status = resp.status();
    let reader = resp.into_body().into_reader();
    if !status.is_success() {
        let mut text = String::new();
        reader.take(64 * 1024).read_to_string(&mut text).ok();
        return Err(format!("{url}: HTTP {status}: {}", text.trim()).into());
    }
    Ok(reader)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    fn stream_of(chunks: &[&str]) -> impl BufRead + use<> {
        let mut s = String::new();
        for c in chunks {
            s.push_str("data: ");
            s.push_str(c);
            s.push_str("\n\n");
        }
        std::io::Cursor::new(s.into_bytes())
    }

    #[test]
    fn sends_exact_bytes_and_streams_back() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
        let (url, server) = crate::test_server::serve_once(format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{sse}",
            sse.len()
        ));
        let body = request_body("m", &[Item::User("hi".into())], &[]);
        let stream = send(&url, Some("k"), &body).unwrap();
        let out = read_stream(stream, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(out.reply.text, "ok");
        let req = server.join().unwrap();
        assert!(req.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
        assert!(
            req.contains("authorization: Bearer k\r\n")
                || req.contains("Authorization: Bearer k\r\n")
        );
        assert!(req.ends_with(std::str::from_utf8(&body).unwrap()));
    }

    #[test]
    fn error_status_carries_the_body() {
        let msg = r#"{"error":{"message":"No auth credentials found"}}"#;
        let (url, server) = crate::test_server::serve_once(format!(
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\n\r\n{msg}",
            msg.len()
        ));
        let err = send(&url, None, b"{}").err().unwrap().to_string();
        server.join().unwrap();
        assert!(
            err.contains("401") && err.contains("No auth credentials"),
            "{err}"
        );
    }

    fn run(chunks: &[&str]) -> (Outcome, Vec<String>) {
        let mut seen = Vec::new();
        let out = read_stream(stream_of(chunks), &AtomicBool::new(false), |e| {
            seen.push(format!("{e:?}"))
        })
        .unwrap();
        (out, seen)
    }

    #[test]
    fn builds_messages_and_tools() {
        let items = vec![
            Item::System("sys".into()),
            Item::User("hi".into()),
            Item::Assistant(Reply {
                text: String::new(),
                reasoning: "hidden".into(),
                calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    args: "{}".into(),
                }],
            }),
            Item::ToolResult {
                call_id: "c1".into(),
                content: "data".into(),
            },
        ];
        let tools = vec![ToolSpec {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: json!({ "type": "object" }),
        }];
        let body: Value = serde_json::from_slice(&request_body("m", &items, &tools)).unwrap();
        assert_eq!(body["model"], "m");
        assert_eq!(body["stream"], true);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0], json!({ "role": "system", "content": "sys" }));
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "read");
        assert_eq!(
            msgs[3],
            json!({ "role": "tool", "tool_call_id": "c1", "content": "data" })
        );
        assert_eq!(body["tools"][0]["function"]["name"], "read");
    }

    #[test]
    fn no_tools_key_without_tools() {
        let body: Value = serde_json::from_slice(&request_body("m", &[], &[])).unwrap();
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn streams_text_reasoning_and_usage() {
        let (out, seen) = run(&[
            r#"{"choices":[{"delta":{"role":"assistant","reasoning":"think"}}]}"#,
            r#"{"choices":[{"delta":{"content":"Hel"}}]}"#,
            r#"{"choices":[{"delta":{"content":"lo"},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
            "[DONE]",
        ]);
        assert_eq!(out.reply.text, "Hello");
        assert_eq!(out.reply.reasoning, "think");
        assert_eq!(out.finish.as_deref(), Some("stop"));
        assert_eq!(out.usage.unwrap()["prompt_tokens"], 5);
        assert_eq!(
            seen,
            ["Reasoning(\"think\")", "Text(\"Hel\")", "Text(\"lo\")"]
        );
    }

    #[test]
    fn assembles_parallel_tool_calls() {
        let (out, _) = run(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","type":"function","function":{"name":"read","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"find","arguments":"{\"q\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"x\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"\"y\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        ]);
        let calls = &out.reply.calls;
        assert_eq!(calls.len(), 2);
        assert_eq!(
            (calls[0].id.as_str(), calls[0].name.as_str()),
            ("a", "read")
        );
        assert_eq!(calls[0].args, r#"{"path":"x"}"#);
        assert_eq!(calls[1].args, r#"{"q":"y"}"#);
        assert_eq!(out.finish.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn error_inside_stream_is_an_error() {
        let r = read_stream(
            stream_of(&[r#"{"error":{"message":"overloaded","code":502}}"#]),
            &AtomicBool::new(false),
            |_| {},
        );
        assert!(r.unwrap_err().to_string().contains("overloaded"));
    }

    #[test]
    fn cancel_keeps_partial_reply() {
        let cancel = AtomicBool::new(false);
        let out = read_stream(
            stream_of(&[
                r#"{"choices":[{"delta":{"content":"part"}}]}"#,
                r#"{"choices":[{"delta":{"content":"never"}}]}"#,
            ]),
            &cancel,
            |_| cancel.store(true, Ordering::Relaxed),
        )
        .unwrap();
        assert!(out.cancelled);
        assert_eq!(out.reply.text, "part");
    }
}
