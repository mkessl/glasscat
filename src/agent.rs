//! The loop: send the context, stream the reply, run the tools it calls,
//! add the results and go again until the model answers without tools.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use crate::Result;
use crate::context::{Item, Reply, ToolSpec};
use crate::history;
use crate::openai::{self, Event, Prefill};
use crate::tools::Tools;

pub struct Session {
    pub base_url: String,
    /// The proxy for requests to the provider, if one is set.
    pub proxy: Option<String>,
    /// Fields set in request bodies, beyond what the agent builds.
    pub fields: Fields,
    pub model: String,
    pub api_key: Option<String>,
    /// How this provider continues a cut-off reply.
    pub prefill: Prefill,
    pub items: Vec<Item>,
    pub tools: Tools,
    specs: Vec<ToolSpec>,
    /// What the model should learn with the next message, such as a
    /// change the user undid.
    notes: Vec<String>,
    /// Where the conversation is saved after each turn; `None` keeps it
    /// in memory only.
    pub log: Option<(PathBuf, String)>,
    /// What the command line set, which the settings do not override.
    pub launch: crate::settings::Launch,
}

/// What a turn shows as it goes.
pub enum Shown<'a> {
    /// The exact bytes about to be sent.
    Request(&'a [u8]),
    Stream(Event<'a>),
    /// The provider's usage report for the request just finished.
    Usage(&'a serde_json::Value),
    /// Which model answered, and on which provider if a router says.
    Served(&'a openai::Served),
    /// The provider answered with an error that may pass; the same request
    /// goes again after `wait`.
    Retrying {
        error: &'a str,
        wait: Duration,
        attempt: u32,
    },
    /// How long the request took: to the first streamed token, if one
    /// came, and in all.
    Timing {
        first: Option<Duration>,
        total: Duration,
    },
    /// Asked to continue a cut-off reply, the model wrote a new one; it
    /// replaces the cut-off one.
    StartedOver,
    ToolDone {
        name: &'a str,
        args: &'a str,
        result: &'a str,
        /// For the user only: what the tool kept from the model (hidden
        /// paths, paths outside the allowed folders), or where a report
        /// was saved.
        note: Option<&'a str>,
        /// A file's text before the tool overwrote it.
        before: Option<&'a str>,
    },
}

impl Session {
    pub fn new(
        base_url: String,
        model: String,
        api_key: Option<String>,
        prefill: Prefill,
        tools: Tools,
    ) -> Self {
        let items = vec![Item::System(system_prompt(&tools))];
        let specs = tools.all_specs();
        Self {
            base_url,
            proxy: None,
            fields: Fields::default(),
            model,
            api_key,
            prefill,
            items,
            tools,
            specs,
            notes: Vec::new(),
            log: None,
            launch: Default::default(),
        }
    }

    /// Puts in the user's own tools, in place of any before.
    pub fn set_custom(
        &mut self,
        custom: std::collections::BTreeMap<String, crate::custom::CustomTool>,
    ) -> std::result::Result<(), String> {
        self.tools.set_custom(custom)?;
        self.specs = self.tools.all_specs();
        Ok(())
    }

    /// Puts in a context edited by hand, and saves it.
    pub fn set_items(&mut self, items: Vec<Item>) -> Result<()> {
        self.items = items;
        self.save(Ok(()))
    }

    /// Adds instruction files to the system prompt of a new conversation:
    /// the user's own, then each project file, as (shown path, text).
    pub fn add_instructions(
        &mut self,
        user: Option<(String, String)>,
        project: Vec<(String, String)>,
    ) {
        let Some(Item::System(prompt)) = self.items.first_mut() else {
            return;
        };
        for (whose, (path, text)) in user
            .map(|u| ("the user", u))
            .into_iter()
            .chain(project.into_iter().map(|p| ("this project", p)))
        {
            let text = text.trim();
            if !text.is_empty() {
                prompt.push_str(&format!(
                    "\n\nInstructions for {whose}, from {path}:\n{text}"
                ));
            }
        }
    }

    /// Forgets the conversation; the system prompt stays.
    pub fn clear(&mut self) {
        self.items.truncate(1);
        self.notes.clear();
        // The old conversation stays saved; the new one gets its own file.
        if let Some((path, _)) = &mut self.log {
            *path = history::new_path(self.tools.cwd());
        }
    }

    /// Drops all that came after the last user message, so the model is
    /// asked for its answer again; returns what was dropped, or None if
    /// there is no user message.
    pub fn drop_answer(&mut self) -> Option<Vec<Item>> {
        let last = self
            .items
            .iter()
            .rposition(|i| matches!(i, Item::User(_)))?;
        Some(self.items.split_off(last + 1))
    }

    /// Undoes the last file change the tools made; the model is told
    /// with the next message.
    pub fn undo(&mut self) -> std::result::Result<String, String> {
        let done = self.tools.undo()?;
        self.notes
            .push(format!("[The user undid one of your changes. {done}]"));
        Ok(done)
    }

    /// Runs one user message to the model's final answer.
    pub fn turn(
        &mut self,
        prompt: String,
        cancel: &AtomicBool,
        on: impl FnMut(Shown),
    ) -> Result<()> {
        let prompt = if self.notes.is_empty() {
            prompt
        } else {
            format!("{}\n\n{prompt}", std::mem::take(&mut self.notes).join("\n"))
        };
        self.items.push(Item::User(prompt));
        let r = self.run(cancel, on);
        self.save(r)
    }

    /// Saves the conversation where `log` says, after a turn that ended
    /// with `r`; a failed save turns into an error after a good turn.
    fn save(&self, r: Result<()>) -> Result<()> {
        let Some((path, provider)) = &self.log else {
            return r;
        };
        let saved = history::Saved {
            cwd: self.tools.cwd().to_path_buf(),
            provider: provider.clone(),
            model: self.model.clone(),
            items: self.items.clone(),
        };
        match history::save(path, &saved) {
            Err(e) if r.is_ok() => Err(format!("could not save the conversation: {e}").into()),
            _ => r,
        }
    }

    /// The last reply, if it was cut off before the model finished it.
    pub fn cut_reply(&self) -> Option<&Reply> {
        match self.items.last() {
            Some(Item::Assistant(r)) if r.cut => Some(r),
            _ => None,
        }
    }

    /// Whether the model owes a reply: the last turn was stopped, failed
    /// or was cut off before the model finished.
    pub fn unanswered(&self) -> bool {
        match self.items.last() {
            Some(Item::User(_) | Item::ToolResult { .. }) => true,
            Some(Item::Assistant(r)) => r.cut,
            _ => false,
        }
    }

    /// Whether a cut-off reply will continue from its last token rather
    /// than be asked for again.
    pub fn continues_from_token(&self) -> bool {
        self.prefill != Prefill::None && self.cut_reply().is_some_and(|r| !r.text.is_empty())
    }

    /// Picks up an unanswered turn: a cut-off reply from its last token
    /// where the provider allows it, otherwise by asking again.
    pub fn resume(&mut self, cancel: &AtomicBool, on: impl FnMut(Shown)) -> Result<()> {
        if self.cut_reply().is_some() && !self.continues_from_token() {
            self.items.pop();
        }
        let r = self.run(cancel, on);
        self.save(r)
    }

    /// The request body for the context as it stands.
    pub fn body(&self) -> Vec<u8> {
        openai::request_body(
            &self.model,
            &self.items,
            &self.specs,
            self.prefill,
            &self.fields.for_model(&self.model),
        )
    }

    fn run(&mut self, cancel: &AtomicBool, mut on: impl FnMut(Shown)) -> Result<()> {
        loop {
            let body = self.body();
            on(Shown::Request(&body));
            let mut attempt = 0;
            let (stream, start) = loop {
                let start = Instant::now();
                let sent = openai::send(
                    &self.base_url,
                    self.api_key.as_deref(),
                    self.proxy.as_deref(),
                    &body,
                    cancel,
                );
                let e = match sent {
                    Ok(Some(stream)) => break (stream, start),
                    Ok(None) => return Ok(()),
                    Err(e) => e,
                };
                let wait = e
                    .downcast_ref::<openai::HttpError>()
                    .and_then(|h| h.retry_wait(attempt));
                let Some(wait) = wait.filter(|_| attempt < RETRIES) else {
                    return Err(e);
                };
                attempt += 1;
                on(Shown::Retrying {
                    error: &e.to_string(),
                    wait,
                    attempt,
                });
                if !sleep_unless_cancelled(wait, cancel) {
                    return Ok(());
                }
            };
            let mut first = None;
            let mut continuation = self
                .continues_from_token()
                .then(|| self.cut_reply().cloned().map(Continuation::new))
                .flatten();
            let outcome = openai::read_stream(stream, cancel, |e| {
                first.get_or_insert_with(|| start.elapsed());
                match &mut continuation {
                    Some(c) => c.pass(e, &mut on),
                    None => on(Shown::Stream(e)),
                }
            });
            on(Shown::Timing {
                first,
                total: start.elapsed(),
            });
            if let Some(s) = &outcome.served {
                on(Shown::Served(s));
            }
            if let Some(u) = &outcome.usage {
                on(Shown::Usage(u));
            }
            let cut = outcome.cut();
            let mut reply = outcome.reply;
            if let Some(c) = continuation {
                self.items.pop();
                reply = c.finish(reply);
            }
            if cut {
                // A tool call may be cut mid-arguments; none is run or kept.
                reply.calls.clear();
            }
            reply.cut = cut;
            for (n, call) in reply.calls.iter_mut().enumerate() {
                if call.id.is_empty() {
                    call.id = format!("call_{}_{n}", self.items.len());
                }
            }
            let calls = reply.calls.clone();
            self.items.push(Item::Assistant(reply));
            if let Some(e) = outcome.error {
                return Err(e.into());
            }
            if cut || calls.is_empty() {
                return Ok(());
            }
            let results = self.run_calls(&calls, cancel);
            for (call, ran) in calls.into_iter().zip(results) {
                on(Shown::ToolDone {
                    name: &call.name,
                    args: &call.args,
                    result: &ran.result,
                    note: ran.note.as_deref(),
                    before: ran.before.as_deref(),
                });
                self.items.push(Item::ToolResult {
                    call_id: call.id,
                    content: ran.result,
                });
            }
        }
    }

    /// Calls that change nothing run at the same time; otherwise in order.
    fn run_calls(
        &self,
        calls: &[crate::context::ToolCall],
        cancel: &AtomicBool,
    ) -> Vec<crate::tools::Ran> {
        let tools = &self.tools;
        if calls.len() > 1 && calls.iter().all(|c| tools.reads_only(&c.name)) {
            std::thread::scope(|s| {
                let handles: Vec<_> = calls
                    .iter()
                    .map(|c| s.spawn(move || tools.run_noting(&c.name, &c.args, cancel)))
                    .collect();
                handles
                    .into_iter()
                    .map(|h| {
                        h.join().unwrap_or_else(|_| crate::tools::Ran {
                            result: "error: the tool crashed".into(),
                            note: None,
                            before: None,
                        })
                    })
                    .collect()
            })
        } else {
            calls
                .iter()
                .map(|c| tools.run_noting(&c.name, &c.args, cancel))
                .collect()
        }
    }
}

type Map = serde_json::Map<String, serde_json::Value>;

/// Tries of a request after the first, on errors that may pass.
pub const RETRIES: u32 = 3;

/// Waits unless cancelled first; false when cancelled.
fn sleep_unless_cancelled(wait: Duration, cancel: &AtomicBool) -> bool {
    let end = Instant::now() + wait;
    while Instant::now() < end {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    !cancel.load(std::sync::atomic::Ordering::Relaxed)
}

/// Request fields in layers; a later layer wins.
#[derive(Debug, Default)]
pub struct Fields {
    /// Built in for the provider: fields for the models whose id starts
    /// with the given text.
    pub builtin: Vec<(String, Map)>,
    /// From the settings: the general ones, then the provider's.
    pub settings: Map,
    /// From the settings, by model id.
    pub models: std::collections::BTreeMap<String, Map>,
    /// Set with /set in this session.
    pub session: Map,
}

impl Fields {
    /// Each field sent with `model`, and which layer it comes from.
    pub fn sources(&self, model: &str) -> Vec<(&str, &serde_json::Value, &'static str)> {
        let mut out: Vec<(&str, &serde_json::Value, &'static str)> = Vec::new();
        let id = model.trim_start_matches('~');
        let builtin = self
            .builtin
            .iter()
            .filter(|(start, _)| id.starts_with(start.as_str()))
            .map(|(_, m)| (Some(m), "the default for this model"));
        let layers = builtin.chain([
            (Some(&self.settings), "settings"),
            (self.models.get(model), "settings for this model"),
            (Some(&self.session), "this session"),
        ]);
        for (layer, from) in layers {
            for (k, v) in layer.into_iter().flatten() {
                match out.iter_mut().find(|f| f.0 == k) {
                    Some(f) => *f = (k, v, from),
                    None => out.push((k, v, from)),
                }
            }
        }
        out
    }

    pub fn for_model(&self, model: &str) -> Map {
        self.sources(model)
            .into_iter()
            .map(|(k, v, _)| (k.to_string(), v.clone()))
            .collect()
    }
}

fn system_prompt(tools: &Tools) -> String {
    format!(
        "You are {name}, an AI agent working on the user's computer through your tools. \
The user owns you and can see everything you are given, this text included; \
answer questions about yourself plainly. \
You may call several tools in one reply; those that only read run at the same time. \
You can run only what your tools offer; the built-in ones work on files, and cannot run \
programs, builds or tests; a script you write is not run. Say what is unchecked instead of \
claiming it works.\n\
Working directory: {cwd}\n\
System: {os}\n\
Date (UTC): {date}",
        name = crate::NAME,
        cwd = tools.cwd().display(),
        os = std::env::consts::OS,
        date = today(),
    )
}

/// Today's UTC date as YYYY-MM-DD.
fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Follows a reply asked to continue a cut-off one and works out what
/// the model did with the given start. Some models continue from the
/// last token; some repeat the start and then go on; some ignore it and
/// write a new reply. Streamed text is held back while it may still be
/// a copy of the start, so the screen never shows the start twice.
struct Continuation {
    before: Reply,
    held: String,
    how: Option<How>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum How {
    Continued,
    Repeated,
    StartedOver,
}

impl Continuation {
    fn new(before: Reply) -> Self {
        Self {
            before,
            held: String::new(),
            how: None,
        }
    }

    /// Passes a streamed event on, minus any copy of the start.
    fn pass(&mut self, e: Event, on: &mut impl FnMut(Shown)) {
        if self.how.is_none() {
            let start = &self.before.text;
            let how = match e {
                Event::Text(t) => {
                    self.held.push_str(t);
                    if start.starts_with(&self.held) {
                        return;
                    }
                    if self.held.starts_with(start.as_str()) {
                        How::Repeated
                    } else if common_chars(start, &self.held) >= 3 {
                        How::StartedOver
                    } else {
                        How::Continued
                    }
                }
                // A model going on from the last token cannot think first.
                Event::Reasoning(_) => How::StartedOver,
                _ if self.held.is_empty() => How::Continued,
                _ => How::StartedOver,
            };
            self.how = Some(how);
            let held = std::mem::take(&mut self.held);
            let shown = match how {
                How::Repeated => &held[start.len()..],
                How::StartedOver => {
                    on(Shown::StartedOver);
                    &held
                }
                How::Continued => &held,
            };
            if !shown.is_empty() {
                on(Shown::Stream(Event::Text(shown)));
            }
            if matches!(e, Event::Text(_)) {
                return;
            }
        }
        on(Shown::Stream(e));
    }

    /// The whole reply: the start and its continuation, or the new reply.
    fn finish(self, after: Reply) -> Reply {
        match self.how {
            Some(How::Repeated | How::StartedOver) => after,
            Some(How::Continued) => Reply {
                text: self.before.text + &after.text,
                reasoning: self.before.reasoning,
                calls: after.calls,
                cut: false,
            },
            // Nothing new came, or only part of the start again.
            None => Reply {
                calls: after.calls,
                ..self.before
            },
        }
    }
}

fn common_chars(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

/// Days since 1970-01-01 to a calendar date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::Access;
    use crate::journal::Journal;
    use crate::test_server::{serve, sse};
    use std::fs;

    #[test]
    fn runs_tools_until_the_model_answers() {
        let dir = std::env::temp_dir().join(format!("agent-loop-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.txt"), "alpha\n").unwrap();
        fs::write(dir.join("b.txt"), "beta\n").unwrap();
        let (url, server) = serve(vec![
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"r1","function":{"name":"read","arguments":"{\"path\":\"a.txt\"}"}},{"index":1,"id":"r2","function":{"name":"read","arguments":"{\"path\":\"b.txt\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}"#,
                "[DONE]",
            ]),
        ]);
        let access = Access::new(&dir, std::slice::from_ref(&dir), &[]).unwrap();
        let tools = Tools::new(access, Journal::new(dir.join("j.jsonl")));
        let mut session = Session::new(url, "m".into(), None, Prefill::None, tools);
        let mut done = Vec::new();
        session
            .turn("go".into(), &AtomicBool::new(false), |s| {
                if let Shown::ToolDone { name, result, .. } = s {
                    done.push(format!("{name}: {result}"));
                }
            })
            .unwrap();
        assert_eq!(done, ["read: alpha\n", "read: beta\n"]);
        let requests = server.join().unwrap();
        let second: serde_json::Value =
            serde_json::from_str(&requests[1][requests[1].find("\r\n\r\n").unwrap() + 4..])
                .unwrap();
        let msgs = second["messages"].as_array().unwrap();
        assert_eq!(
            msgs.len(),
            5,
            "system, user, assistant with calls, two results"
        );
        assert_eq!(msgs[3]["tool_call_id"], "r1");
        assert_eq!(msgs[4]["content"], "beta\n");
        assert!(matches!(session.items.last(), Some(Item::Assistant(r)) if r.text == "done"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn instruction_files_join_the_system_prompt_unless_hidden() {
        let dir =
            std::env::temp_dir().join(format!("{}-instr-{}", crate::NAME, std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("AGENTS.md"), "Use tabs.\n").unwrap();
        fs::write(dir.join("SECRET.md"), "hidden\n").unwrap();
        let access =
            Access::new(&dir, std::slice::from_ref(&dir), &[dir.join("SECRET.md")]).unwrap();
        let tools = Tools::new(access, Journal::new(dir.join("j.jsonl")));
        let mut session = Session::new("u".into(), "m".into(), None, Prefill::None, tools);
        let names = ["AGENTS.md", "SECRET.md", "MISSING.md"].map(String::from);
        let project = session.tools.read_visible(&names);
        session.add_instructions(Some(("~/u.md".into(), "Be brief.".into())), project);
        let Item::System(prompt) = &session.items[0] else {
            panic!()
        };
        assert!(
            prompt.ends_with(
                "\n\nInstructions for the user, from ~/u.md:\nBe brief.\n\n\
             Instructions for this project, from AGENTS.md:\nUse tabs."
            ),
            "{prompt}"
        );
        assert!(!prompt.contains("hidden"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tries_again_after_a_passing_error() {
        let busy =
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 4\r\n\r\nbusy";
        let (url, server) = serve(vec![
            busy.into(),
            sse(&[r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#]),
        ]);
        let (mut session, dir) = session(url, Prefill::None);
        let mut retried = Vec::new();
        session
            .turn("hi".into(), &AtomicBool::new(false), |s| {
                if let Shown::Retrying { error, attempt, .. } = s {
                    retried.push((error.to_string(), attempt));
                }
            })
            .unwrap();
        assert_eq!(retried.len(), 1);
        assert!(
            retried[0].0.ends_with("HTTP 429 Too Many Requests: busy"),
            "{:?}",
            retried
        );
        assert!(matches!(session.items.last(), Some(Item::Assistant(r)) if r.text == "ok"));
        assert_eq!(server.join().unwrap().len(), 2);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn drops_the_last_answer() {
        let dir = std::env::temp_dir().join(format!("agent-retry-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let access = Access::new(&dir, std::slice::from_ref(&dir), &[]).unwrap();
        let tools = Tools::new(access, Journal::new(dir.join("j.jsonl")));
        let mut session = Session::new("u".into(), "m".into(), None, Prefill::None, tools);
        assert_eq!(session.drop_answer(), None);
        let reply = |t: &str| {
            Item::Assistant(Reply {
                text: t.into(),
                ..Default::default()
            })
        };
        session.items.extend([
            Item::User("a".into()),
            reply("1"),
            Item::User("b".into()),
            reply("2"),
            Item::ToolResult {
                call_id: "c".into(),
                content: "r".into(),
            },
            reply("3"),
        ]);
        assert_eq!(session.drop_answer().map(|d| d.len()), Some(3));
        assert_eq!(session.items.len(), 4);
        assert_eq!(session.items.last(), Some(&Item::User("b".into())));
        assert!(session.unanswered());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn request_fields_in_layers() {
        let map = |v: serde_json::Value| v.as_object().unwrap().clone();
        let mut f = Fields {
            settings: map(serde_json::json!({"temperature": 0.6, "top_p": 0.9})),
            ..Fields::default()
        };
        f.models
            .insert("r1".into(), map(serde_json::json!({"temperature": 0.2})));
        f.session = map(serde_json::json!({"top_p": null}));
        let shown: Vec<String> = f
            .sources("r1")
            .iter()
            .map(|(k, v, from)| format!("{k}={v} ({from})"))
            .collect();
        assert_eq!(
            shown,
            [
                "temperature=0.2 (settings for this model)",
                "top_p=null (this session)"
            ]
        );
        assert_eq!(f.for_model("other")["temperature"], 0.6);
        f.builtin = vec![(
            "anthropic/".into(),
            map(serde_json::json!({"cache_control": 1})),
        )];
        assert_eq!(f.for_model("~anthropic/c")["cache_control"], 1);
        assert!(f.for_model("r1").get("cache_control").is_none());
        f.session
            .insert("cache_control".into(), serde_json::Value::Null);
        assert!(f.for_model("anthropic/c")["cache_control"].is_null());
    }

    fn session(url: String, prefill: Prefill) -> (Session, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "{}-agent-{}-{:?}",
            crate::NAME,
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let access = Access::new(&dir, std::slice::from_ref(&dir), &[]).unwrap();
        let tools = Tools::new(access, Journal::new(dir.join("j.jsonl")));
        (Session::new(url, "m".into(), None, prefill, tools), dir)
    }

    fn body(request: &str) -> serde_json::Value {
        serde_json::from_str(&request[request.find("\r\n\r\n").unwrap() + 4..]).unwrap()
    }

    #[test]
    fn continues_a_cut_reply_from_its_last_token() {
        let (url, server) = serve(vec![
            sse(&[
                r#"{"choices":[{"delta":{"content":"Hel","tool_calls":[{"index":0,"id":"x","function":{"name":"read","arguments":"{\"pa"}}]},"finish_reason":"length"}]}"#,
            ]),
            sse(&[r#"{"choices":[{"delta":{"content":"lo"},"finish_reason":"stop"}]}"#]),
        ]);
        let (mut s, dir) = session(url, Prefill::PrefixFlag);
        let cancel = AtomicBool::new(false);
        s.turn("hi".into(), &cancel, |_| {}).unwrap();
        let cut = s.cut_reply().expect("out of tokens is a cut");
        assert!(cut.calls.is_empty(), "a cut tool call is never kept");
        assert!(s.continues_from_token());
        s.resume(&cancel, |_| {}).unwrap();
        let requests = server.join().unwrap();
        let last = &body(&requests[1])["messages"].as_array().unwrap().clone();
        assert_eq!(last.len(), 3);
        assert_eq!(last[2]["content"], "Hel");
        assert_eq!(last[2]["prefix"], true);
        assert_eq!(s.items.len(), 3);
        assert!(matches!(s.items.last(), Some(Item::Assistant(r)) if r.text == "Hello" && !r.cut));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn asks_again_where_the_provider_cannot_continue() {
        let (url, server) = serve(vec![
            sse(&[r#"{"choices":[{"delta":{"content":"Hel"},"finish_reason":"length"}]}"#]),
            sse(&[r#"{"choices":[{"delta":{"content":"Hi"},"finish_reason":"stop"}]}"#]),
        ]);
        let (mut s, dir) = session(url, Prefill::None);
        let cancel = AtomicBool::new(false);
        s.turn("hi".into(), &cancel, |_| {}).unwrap();
        assert!(s.cut_reply().is_some() && !s.continues_from_token());
        s.resume(&cancel, |_| {}).unwrap();
        let requests = server.join().unwrap();
        assert_eq!(body(&requests[1])["messages"].as_array().unwrap().len(), 2);
        assert!(matches!(s.items.last(), Some(Item::Assistant(r)) if r.text == "Hi"));
        fs::remove_dir_all(dir).unwrap();
    }

    /// Streams `chunks` through a continuation of `start`; returns what
    /// was shown and the joined reply.
    fn continue_with(start: &str, chunks: &[Event]) -> (Vec<String>, String) {
        let mut c = Continuation::new(Reply {
            text: start.into(),
            cut: true,
            ..Reply::default()
        });
        let mut shown = Vec::new();
        let mut after = Reply::default();
        for e in chunks {
            if let Event::Text(t) = e {
                after.text.push_str(t);
            }
            c.pass(*e, &mut |s| match s {
                Shown::Stream(Event::Text(t)) => shown.push(t.to_string()),
                Shown::Stream(Event::Reasoning(t)) => shown.push(format!("({t})")),
                Shown::StartedOver => shown.push("[anew]".into()),
                _ => {}
            });
        }
        (shown, c.finish(after).text)
    }

    #[test]
    fn continuation_shows_each_part_once() {
        use Event::{Reasoning, Text};
        let (shown, text) = continue_with("one\ntw", &[Text("o\nth"), Text("ree")]);
        assert_eq!(
            (shown, text),
            (vec!["o\nth".into(), "ree".into()], "one\ntwo\nthree".into())
        );
        let (shown, text) = continue_with("one\ntw", &[Text("one"), Text("\ntwo\n"), Text("3")]);
        assert_eq!(
            (shown, text),
            (vec!["o\n".into(), "3".into()], "one\ntwo\n3".into())
        );
        let (shown, text) = continue_with("one\ntw", &[Text("one\nTWO")]);
        assert_eq!(
            (shown, text),
            (vec!["[anew]".into(), "one\nTWO".into()], "one\nTWO".into())
        );
        let (shown, text) = continue_with("one\ntw", &[Reasoning("hm"), Text("x")]);
        assert_eq!(
            (shown, text),
            (vec!["[anew]".into(), "(hm)".into(), "x".into()], "x".into())
        );
        let (shown, text) = continue_with("one\ntw", &[Text("on")]);
        assert_eq!((shown, text), (vec![], "one\ntw".into()), "nothing is lost");
    }

    #[test]
    fn dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(20_763), (2026, 11, 6));
    }
}
