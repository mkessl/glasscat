//! MCP servers: their tools are given to the model beside its own. A
//! server is a program spoken to over its stdin and stdout, or an address
//! spoken to over HTTP, with JSON-RPC. Servers start in the background
//! when the agent does; the first request waits for those still starting,
//! so the tools stay the same for the whole conversation.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::context::ToolSpec;

/// One server in the settings, in the shape other agents use, so a
/// server's own instructions can be copied as they are.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// The program to start, for a server on stdin and stdout.
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// The address, for a server over HTTP.
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    /// "stdio" or "http"; known from `command` or `url` when not given.
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// Not started.
    pub disabled: bool,
    /// Built in: a failure to start is not told, only shown with the tools.
    #[serde(skip)]
    pub quiet: bool,
}

/// The server built in for web search and reading pages: Exa's, which
/// needs no key. One named `exa` in the settings takes its place.
pub fn with_builtin(config: &BTreeMap<String, Config>) -> BTreeMap<String, Config> {
    let mut all = config.clone();
    all.entry("exa".into()).or_insert_with(|| Config {
        url: Some("https://mcp.exa.ai/mcp".into()),
        quiet: true,
        ..Default::default()
    });
    all
}

const PROTOCOL: &str = "2025-06-18";
/// How long the first request waits for servers still starting.
const START_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Tool {
    /// The name the model calls it by: `mcp__server__tool`.
    pub name: String,
    /// The name the server knows it by.
    remote: String,
    description: String,
    schema: Value,
    pub read_only: bool,
    /// The server says it may destroy something.
    pub destructive: bool,
}

struct Ready {
    conn: Conn,
    tools: Vec<Tool>,
}

enum State {
    Starting,
    Ready(Arc<Ready>),
    Failed(String),
}

struct Server {
    name: String,
    quiet: bool,
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
pub struct Servers {
    config: BTreeMap<String, Config>,
    list: Vec<Arc<Server>>,
    started: Option<Instant>,
    /// Failures were told.
    told: AtomicBool,
}

impl Servers {
    /// Starts each server on a thread of its own.
    pub fn start(config: &BTreeMap<String, Config>, cwd: &Path) -> Arc<Self> {
        let mut list = Vec::new();
        for (name, c) in config.iter().filter(|(_, c)| !c.disabled) {
            let server = Arc::new(Server {
                name: name.clone(),
                quiet: c.quiet,
                state: Mutex::new(State::Starting),
                changed: Condvar::new(),
            });
            let (s, c, cwd) = (server.clone(), c.clone(), cwd.to_path_buf());
            std::thread::spawn(move || {
                let state = match connect(&s.name, &c, &cwd) {
                    Ok(r) => State::Ready(Arc::new(r)),
                    Err(e) => State::Failed(e),
                };
                *s.state.lock().unwrap_or_else(|e| e.into_inner()) = state;
                s.changed.notify_all();
            });
            list.push(server);
        }
        Arc::new(Self {
            config: config.clone(),
            list,
            started: Some(Instant::now()),
            told: AtomicBool::new(false),
        })
    }

    /// Whether these servers were started from `config`.
    pub fn same(&self, config: &BTreeMap<String, Config>) -> bool {
        self.config == *config
    }

    /// Waits for servers still starting; the first time, says which
    /// failed and why.
    pub fn wait(&self) -> Vec<String> {
        let deadline = self.started.unwrap_or_else(Instant::now) + START_TIMEOUT;
        for s in &self.list {
            let mut state = s.state.lock().unwrap_or_else(|e| e.into_inner());
            while matches!(*state, State::Starting) {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    *state = State::Failed(format!(
                        "did not start within {} seconds",
                        START_TIMEOUT.as_secs()
                    ));
                    break;
                }
                state = s
                    .changed
                    .wait_timeout(state, left)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        }
        if self.told.swap(true, Ordering::Relaxed) {
            return Vec::new();
        }
        self.failed()
    }

    fn failed(&self) -> Vec<String> {
        self.list
            .iter()
            .filter(|s| !s.quiet)
            .filter_map(
                |s| match &*s.state.lock().unwrap_or_else(|e| e.into_inner()) {
                    State::Failed(e) => Some(format!("MCP server {}: {e}", s.name)),
                    _ => None,
                },
            )
            .collect()
    }

    fn ready(&self) -> Vec<(String, Arc<Ready>)> {
        self.list
            .iter()
            .filter_map(
                |s| match &*s.state.lock().unwrap_or_else(|e| e.into_inner()) {
                    State::Ready(r) => Some((s.name.clone(), r.clone())),
                    _ => None,
                },
            )
            .collect()
    }

    /// The definitions of the tools of the servers ready now.
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut out = Vec::new();
        for (_, r) in self.ready() {
            out.extend(r.tools.iter().map(|t| ToolSpec {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.schema.clone(),
            }));
        }
        out
    }

    /// The tool called `name`, and its server's name.
    pub fn tool(&self, name: &str) -> Option<(String, bool, bool)> {
        self.ready().into_iter().find_map(|(server, r)| {
            r.tools
                .iter()
                .find(|t| t.name == name)
                .map(|t| (server, t.read_only, t.destructive))
        })
    }

    /// Calls a tool; None when no server has it.
    pub fn call(&self, name: &str, args: &str, cancel: &AtomicBool) -> Option<Called> {
        let (r, remote) = self.ready().into_iter().find_map(|(_, r)| {
            let remote = r.tools.iter().find(|t| t.name == name)?.remote.clone();
            Some((r, remote))
        })?;
        let args: Value = match args.trim() {
            "" => json!({}),
            a => match serde_json::from_str(a) {
                Ok(v) => v,
                Err(e) => {
                    return Some(Called::text(format!(
                        "error: arguments are not valid JSON: {e}"
                    )));
                }
            },
        };
        let params = json!({ "name": remote, "arguments": args });
        Some(match r.conn.request("tools/call", params, cancel) {
            Ok(v) => result(&v, name),
            Err(e) => Called::text(format!("error: {e}")),
        })
    }

    /// One line for each server: its state and tool count.
    pub fn summary(&self) -> Vec<String> {
        self.list
            .iter()
            .map(
                |s| match &*s.state.lock().unwrap_or_else(|e| e.into_inner()) {
                    State::Starting => format!("{}: starting", s.name),
                    State::Ready(r) => format!("{}: {} tools", s.name, r.tools.len()),
                    State::Failed(e) => format!("{}: failed: {e}", s.name),
                },
            )
            .collect()
    }
}

/// What a tool call gave.
pub struct Called {
    pub text: String,
    pub image: Option<crate::tools::Image>,
}

impl Called {
    fn text(text: String) -> Self {
        Self { text, image: None }
    }
}

/// A tool result as text for the model, with the first image it holds.
fn result(v: &Value, tool: &str) -> Called {
    use base64::Engine;
    let mut parts = Vec::new();
    let mut image = None;
    for c in v["content"].as_array().into_iter().flatten() {
        match c["type"].as_str().unwrap_or_default() {
            "text" => parts.push(c["text"].as_str().unwrap_or_default().to_string()),
            "image" if image.is_none() => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(c["data"].as_str().unwrap_or_default())
                    .unwrap_or_default();
                image = Some(crate::tools::Image {
                    name: format!("the image from {tool}"),
                    mime: c["mimeType"].as_str().unwrap_or("image/png").into(),
                    bytes,
                });
            }
            "resource" => {
                let r = &c["resource"];
                parts.push(match r["text"].as_str() {
                    Some(t) => t.to_string(),
                    None => format!("[resource {}]", r["uri"].as_str().unwrap_or_default()),
                });
            }
            "resource_link" => {
                parts.push(format!("[link {}]", c["uri"].as_str().unwrap_or_default()))
            }
            other => parts.push(format!("[{other} content]")),
        }
    }
    if parts.is_empty() && !v["structuredContent"].is_null() {
        parts.push(v["structuredContent"].to_string());
    }
    let mut text = parts.join("\n");
    if image.is_some() {
        text += "; it follows as an image";
    }
    if v["isError"].as_bool() == Some(true) {
        text = format!("error: {text}");
    }
    Called { text, image }
}

/// A name fit for a tool name: letters, digits, `_` and `-`.
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' => c,
            _ => '_',
        })
        .collect()
}

fn connect(name: &str, c: &Config, cwd: &Path) -> Result<Ready, String> {
    let conn =
        match (&c.command, &c.url) {
            (Some(cmd), _) if c.kind.as_deref() != Some("http") => {
                Conn::Stdio(Stdio::start(name, c, cmd, cwd)?)
            }
            (_, Some(url)) if c.kind.as_deref() != Some("sse") => Conn::Http(Http::new(url, c)?),
            (_, Some(_)) => return Err(
                "the old SSE transport is not supported; give the server's streamable HTTP address"
                    .into(),
            ),
            _ => return Err("give a `command` or a `url`".into()),
        };
    let never = AtomicBool::new(false);
    let init = conn.request(
        "initialize",
        json!({
            "protocolVersion": PROTOCOL,
            "capabilities": {},
            "clientInfo": { "name": crate::NAME, "version": env!("CARGO_PKG_VERSION") },
        }),
        &never,
    )?;
    if let Conn::Http(h) = &conn {
        let v = init["protocolVersion"].as_str().unwrap_or(PROTOCOL);
        *h.version.lock().unwrap_or_else(|e| e.into_inner()) = Some(v.into());
    }
    conn.notify("notifications/initialized");
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let params = match &cursor {
            Some(c) => json!({ "cursor": c }),
            None => json!({}),
        };
        let page = conn.request("tools/list", params, &never)?;
        for t in page["tools"].as_array().into_iter().flatten() {
            let Some(remote) = t["name"].as_str() else {
                continue;
            };
            let full: String = format!("mcp__{}__{}", clean(name), clean(remote))
                .chars()
                .take(64)
                .collect();
            let hints = &t["annotations"];
            let mut schema = t["inputSchema"].clone();
            if !schema.is_object() {
                schema = json!({ "type": "object", "properties": {} });
            }
            tools.push(Tool {
                name: full,
                remote: remote.into(),
                description: t["description"].as_str().unwrap_or_default().into(),
                schema,
                read_only: hints["readOnlyHint"].as_bool() == Some(true),
                destructive: hints["destructiveHint"].as_bool() == Some(true),
            });
        }
        cursor = page["nextCursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    Ok(Ready { conn, tools })
}

enum Conn {
    Stdio(Stdio),
    Http(Http),
}

impl Conn {
    fn request(&self, method: &str, params: Value, cancel: &AtomicBool) -> Result<Value, String> {
        let v = match self {
            Conn::Stdio(s) => s.request(method, params, cancel)?,
            Conn::Http(h) => h.request(method, params)?,
        };
        match v.get("error") {
            Some(e) => Err(e["message"]
                .as_str()
                .map_or_else(|| e.to_string(), str::to_string)),
            None => Ok(v["result"].clone()),
        }
    }

    fn notify(&self, method: &str) {
        let msg = json!({ "jsonrpc": "2.0", "method": method });
        match self {
            Conn::Stdio(s) => s.send(&msg),
            Conn::Http(h) => {
                let _ = h.post(&msg);
            }
        }
    }
}

type Pending = Arc<Mutex<HashMap<u64, mpsc::Sender<Value>>>>;

/// A server started as a program, one JSON message per line.
struct Stdio {
    stdin: Arc<Mutex<std::process::ChildStdin>>,
    pending: Pending,
    next: AtomicU64,
    child: Mutex<std::process::Child>,
}

impl Stdio {
    fn start(name: &str, c: &Config, cmd: &str, cwd: &Path) -> Result<Self, String> {
        let mut command = std::process::Command::new(cmd);
        command
            .args(&c.args)
            .envs(&c.env)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped());
        // What it writes to stderr is kept for the user to read.
        let log = log_path(name);
        match std::fs::File::create(&log) {
            Ok(f) => command.stderr(f),
            Err(_) => command.stderr(std::process::Stdio::null()),
        };
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command
            .spawn()
            .map_err(|e| format!("could not start {cmd}: {e}"))?;
        let stdin = Arc::new(Mutex::new(child.stdin.take().ok_or("no stdin")?));
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let pending: Pending = Arc::default();
        let (p, w) = (pending.clone(), stdin.clone());
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if v.get("method").is_some() {
                    // A request of the server's own: only `ping` is known.
                    if let Some(id) = v.get("id") {
                        let reply = match v["method"].as_str() {
                            Some("ping") => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
                            _ => json!({ "jsonrpc": "2.0", "id": id,
                                "error": { "code": -32601, "message": "not supported" } }),
                        };
                        write_line(&w, &reply);
                    }
                    continue;
                }
                let Some(id) = v.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                if let Some(tx) = p.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) {
                    let _ = tx.send(v);
                }
            }
            // The server ended: those waiting hear so.
            p.lock().unwrap_or_else(|e| e.into_inner()).clear();
        });
        Ok(Self {
            stdin,
            pending,
            next: AtomicU64::new(1),
            child: Mutex::new(child),
        })
    }

    fn send(&self, msg: &Value) {
        write_line(&self.stdin, msg);
    }

    fn request(&self, method: &str, params: Value, cancel: &AtomicBool) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let started = Instant::now();
        loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(v) => return Ok(v),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("the server ended".into());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            let starting = method == "initialize" && started.elapsed() > START_TIMEOUT;
            if cancel.load(Ordering::Relaxed) || starting {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                self.send(
                    &json!({ "jsonrpc": "2.0", "method": "notifications/cancelled",
                    "params": { "requestId": id, "reason": "stopped by the user" } }),
                );
                return Err("stopped".into());
            }
        }
    }
}

impl Drop for Stdio {
    fn drop(&mut self) {
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(unix)]
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGTERM);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn write_line(w: &Mutex<std::process::ChildStdin>, msg: &Value) {
    let mut w = w.lock().unwrap_or_else(|e| e.into_inner());
    let _ = writeln!(w, "{msg}").and_then(|_| w.flush());
}

/// Where a server's stderr is kept.
pub fn log_path(name: &str) -> PathBuf {
    let dir = crate::journal::data_dir().join("mcp");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("{}.log", clean(name)))
}

/// A server over streamable HTTP.
struct Http {
    url: String,
    headers: BTreeMap<String, String>,
    session: Mutex<Option<String>>,
    version: Mutex<Option<String>>,
    next: AtomicU64,
    agent: ureq::Agent,
}

impl Http {
    fn new(url: &str, c: &Config) -> Result<Self, String> {
        Ok(Self {
            url: url.into(),
            headers: c.headers.clone(),
            session: Mutex::default(),
            version: Mutex::default(),
            next: AtomicU64::new(1),
            agent: crate::openai::agent(None).map_err(|e| e.to_string())?,
        })
    }

    fn post(&self, msg: &Value) -> Result<ureq::http::Response<ureq::Body>, String> {
        let mut req = self
            .agent
            .post(&self.url)
            .content_type("application/json")
            .header("Accept", "application/json, text/event-stream");
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        if let Some(s) = &*self.session.lock().unwrap_or_else(|e| e.into_inner()) {
            req = req.header("Mcp-Session-Id", s);
        }
        if let Some(v) = &*self.version.lock().unwrap_or_else(|e| e.into_inner()) {
            req = req.header("MCP-Protocol-Version", v);
        }
        req.send(msg.to_string())
            .map_err(|e| format!("{}: {e}", self.url))
    }

    fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let mut resp = self.post(&msg)?;
        let status = resp.status();
        if let Some(s) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *self.session.lock().unwrap_or_else(|e| e.into_inner()) = Some(s.into());
        }
        if !status.is_success() {
            let text = resp.body_mut().read_to_string().unwrap_or_default();
            let text: String = text.trim().chars().take(300).collect();
            return Err(format!("HTTP {status}: {text}"));
        }
        let stream = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| t.contains("event-stream"));
        if !stream {
            let bytes = resp
                .body_mut()
                .with_config()
                .limit(32 << 20)
                .read_to_vec()
                .map_err(|e| e.to_string())?;
            return serde_json::from_slice(&bytes).map_err(|e| e.to_string());
        }
        let reader = BufReader::new(resp.into_body().into_reader());
        let mut events = crate::sse::Events::new(reader);
        while let Some(data) = events.next_data().map_err(|e| e.to_string())? {
            if let Ok(v) = serde_json::from_str::<Value>(&data)
                && v.get("id").and_then(Value::as_u64) == Some(id)
            {
                return Ok(v);
            }
        }
        Err("the server closed the stream without an answer".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn a_server_on_stdin_gives_its_tools() {
        // A tiny server in the shell: answers by request number.
        let script = r#"
while read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{}}}" ;;
    *'"tools/list"'*) echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"echo it\",\"description\":\"Echoes.\",\"inputSchema\":{\"type\":\"object\"},\"annotations\":{\"readOnlyHint\":true}}]}}" ;;
    *'"tools/call"'*) echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"heard\"}]}}" ;;
  esac
done"#;
        let config = BTreeMap::from([(
            "t".to_string(),
            Config {
                command: Some("sh".into()),
                args: vec!["-c".into(), script.into()],
                ..Default::default()
            },
        )]);
        let servers = Servers::start(&config, &std::env::temp_dir());
        assert!(servers.wait().is_empty(), "{:?}", servers.summary());
        let specs = servers.specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "mcp__t__echo_it");
        assert_eq!(servers.tool("mcp__t__echo_it").map(|t| t.1), Some(true));
        let called = servers
            .call("mcp__t__echo_it", "{}", &AtomicBool::new(false))
            .unwrap();
        assert_eq!(called.text, "heard");
        assert!(
            servers
                .call("nope", "{}", &AtomicBool::new(false))
                .is_none()
        );
    }

    #[test]
    fn a_failing_server_is_told_once() {
        let config = BTreeMap::from([(
            "gone".to_string(),
            Config {
                command: Some("/nonexistent/server".into()),
                ..Default::default()
            },
        )]);
        let servers = Servers::start(&config, &std::env::temp_dir());
        let failed = servers.wait();
        assert_eq!(failed.len(), 1);
        assert!(failed[0].starts_with("MCP server gone: could not start"));
        assert!(servers.wait().is_empty());
    }
}
