//! The OpenAI chat completions format, which OpenRouter, Groq, DeepSeek,
//! Mistral and local servers (Ollama, llama.cpp, LM Studio, vLLM) speak.

use std::io::{BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::Result;
use crate::context::{Item, Reply, ToolCall, ToolSpec};
use crate::sse::Events;

/// How a provider continues a cut-off assistant message from its last
/// token. The message is sent last, as the start of the reply.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Prefill {
    /// Not supported; the reply is asked for again instead.
    None,
    /// A trailing assistant message is continued as it is (OpenRouter,
    /// Groq, llama.cpp).
    Plain,
    /// `"prefix": true` on the message (DeepSeek beta, Mistral).
    PrefixFlag,
    /// `"partial": true` on the message (Kimi).
    PartialFlag,
}

/// The exact request body. The bytes built here are the bytes sent.
pub fn request_body(model: &str, items: &[Item], tools: &[ToolSpec], prefill: Prefill) -> Vec<u8> {
    let mut messages: Vec<Value> = items.iter().map(message).collect();
    let continuing = prefill != Prefill::None
        && matches!(items.last(), Some(Item::Assistant(r)) if r.cut && !r.text.is_empty());
    if continuing && let Some(last) = messages.last_mut() {
        match prefill {
            Prefill::PrefixFlag => last["prefix"] = true.into(),
            Prefill::PartialFlag => last["partial"] = true.into(),
            _ => {}
        }
    }
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
#[derive(Clone, Copy, Debug, PartialEq)]
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
    /// Why the stream broke off, if it did: a network error, an error the
    /// provider sent inside the stream, or a chunk that could not be read.
    pub error: Option<String>,
    /// Whether the stream said it was finished (`[DONE]` or a finish
    /// reason) rather than just ending.
    pub done: bool,
    /// The usage object exactly as the provider sent it.
    pub usage: Option<Value>,
}

impl Outcome {
    /// Whether the reply stopped before the model finished it, so it can be
    /// continued from where it stopped.
    pub fn cut(&self) -> bool {
        self.cancelled
            || self.error.is_some()
            || !self.done
            || self.finish.as_deref() == Some("length")
    }
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

/// How often a waiting stream checks whether it was cancelled.
const CANCEL_POLL: Duration = Duration::from_millis(30);

/// Reads a streamed response to its end, calling `on` for each piece.
/// Reading happens on its own thread, so a cancel takes effect at once even
/// while the provider sends nothing; the partial reply is kept. A broken
/// stream is not an `Err` either: the outcome keeps what arrived and says
/// why it stopped.
pub fn read_stream(
    body: impl Read + Send + 'static,
    cancel: &AtomicBool,
    mut on: impl FnMut(Event),
) -> Outcome {
    let (tx, rx) = mpsc::sync_channel::<Result<String, String>>(64);
    std::thread::spawn(move || {
        let mut events = Events::new(BufReader::new(body));
        loop {
            let next = match events.next_data() {
                Ok(Some(data)) => Ok(data),
                Ok(None) => break,
                Err(e) => Err(format!("connection broke: {e}")),
            };
            let stop = next.is_err();
            if tx.send(next).is_err() || stop {
                break;
            }
        }
    });
    let mut out = Outcome::default();
    // Provider call index -> position in `out.reply.calls`.
    let mut slots: Vec<(usize, usize)> = Vec::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            out.cancelled = true;
            break;
        }
        let data = match rx.recv_timeout(CANCEL_POLL) {
            Ok(Ok(data)) => data,
            Ok(Err(e)) => {
                out.error = Some(e);
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if data == "[DONE]" {
            out.done = true;
            break;
        }
        let chunk: Chunk = match serde_json::from_str(&data) {
            Ok(c) => c,
            Err(e) => {
                out.error = Some(format!("unreadable stream chunk ({e}): {data}"));
                break;
            }
        };
        if let Some(err) = chunk.error {
            out.error = Some(format!("provider error: {err}"));
            break;
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
                out.done = true;
                out.finish = choice.finish_reason;
            }
        }
    }
    out
}

/// Sends a request body to `{base_url}/chat/completions` and returns the
/// response stream. A non-success status becomes an error with the body.
/// Sends a request and waits for the response to start, on a thread so a
/// cancel takes effect at once; `None` when cancelled. The thread left
/// behind ends with the connection or its timeout.
pub fn send(
    base_url: &str,
    api_key: Option<&str>,
    body: &[u8],
    cancel: &AtomicBool,
) -> Result<Option<impl Read + Send + use<>>> {
    let (tx, rx) = mpsc::sync_channel(1);
    let (url, key, body) = (
        base_url.to_string(),
        api_key.map(str::to_string),
        body.to_vec(),
    );
    std::thread::spawn(move || {
        let _ = tx.send(post(&url, key.as_deref(), &body));
    });
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        match rx.recv_timeout(CANCEL_POLL) {
            Ok(r) => return r.map(Some),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("the request thread crashed".into());
            }
        }
    }
}

/// Giving up on a silent connection. Generous: a local model may read a
/// long prompt for minutes before its first byte, and a stop works at
/// any time anyway.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(600);
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);

fn post(base_url: &str, api_key: Option<&str>, body: &[u8]) -> Result<ureq::BodyReader<'static>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_per_read(Some(READ_TIMEOUT))
        .timeout_per_write(Some(WRITE_TIMEOUT))
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
        let body = request_body("m", &[Item::User("hi".into())], &[], Prefill::None);
        let stream = send(&url, Some("k"), &body, &AtomicBool::new(false))
            .unwrap()
            .unwrap();
        let out = read_stream(stream, &AtomicBool::new(false), |_| {});
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
    fn cancel_works_while_the_server_is_silent() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let held = std::thread::spawn(move || listener.accept().map(|(conn, _)| conn));
        let cancel = AtomicBool::new(false);
        let started = std::time::Instant::now();
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                cancel.store(true, Ordering::Relaxed);
            });
            assert!(send(&url, None, b"{}", &cancel).unwrap().is_none());
        });
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(held.join());
    }

    #[test]
    fn error_status_carries_the_body() {
        let msg = r#"{"error":{"message":"No auth credentials found"}}"#;
        let (url, server) = crate::test_server::serve_once(format!(
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\n\r\n{msg}",
            msg.len()
        ));
        let err = send(&url, None, b"{}", &AtomicBool::new(false))
            .err()
            .unwrap()
            .to_string();
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
        });
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
                cut: false,
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
        let body: Value =
            serde_json::from_slice(&request_body("m", &items, &tools, Prefill::None)).unwrap();
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
    fn flags_only_a_cut_reply_being_continued() {
        let cut = |cut| {
            Item::Assistant(Reply {
                text: "Hel".into(),
                cut,
                ..Reply::default()
            })
        };
        let last = |items: &[Item], prefill| {
            let body: Value =
                serde_json::from_slice(&request_body("m", items, &[], prefill)).unwrap();
            body["messages"].as_array().unwrap().last().unwrap().clone()
        };
        let items = [Item::User("hi".into()), cut(true)];
        assert_eq!(last(&items, Prefill::PrefixFlag)["prefix"], true);
        assert_eq!(last(&items, Prefill::PartialFlag)["partial"], true);
        assert_eq!(
            last(&items, Prefill::Plain),
            json!({ "role": "assistant", "content": "Hel" })
        );
        let done = [Item::User("hi".into()), cut(false)];
        assert!(last(&done, Prefill::PrefixFlag).get("prefix").is_none());
    }

    #[test]
    fn no_tools_key_without_tools() {
        let body: Value =
            serde_json::from_slice(&request_body("m", &[], &[], Prefill::None)).unwrap();
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
    fn error_inside_stream_keeps_the_partial_reply() {
        let (out, _) = run(&[
            r#"{"choices":[{"delta":{"content":"so far"}}]}"#,
            r#"{"error":{"message":"overloaded","code":502}}"#,
        ]);
        assert!(out.error.unwrap().contains("overloaded"));
        assert_eq!(out.reply.text, "so far");
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
        );
        assert!(out.cancelled);
        assert_eq!(out.reply.text, "part");
    }
}
