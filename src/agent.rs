//! The loop: send the context, stream the reply, run the tools it calls,
//! add the results and go again until the model answers without tools.

use std::sync::atomic::AtomicBool;

use crate::Result;
use crate::context::{Item, Reply, ToolSpec};
use crate::openai::{self, Event, Prefill};
use crate::tools::{self, Tools};

pub struct Session {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    /// How this provider continues a cut-off reply.
    pub prefill: Prefill,
    pub items: Vec<Item>,
    pub tools: Tools,
    specs: Vec<ToolSpec>,
}

/// What a turn shows as it goes.
pub enum Shown<'a> {
    /// The exact bytes about to be sent.
    Request(&'a [u8]),
    Stream(Event<'a>),
    /// The provider's usage report for the request just finished.
    Usage(&'a serde_json::Value),
    /// Asked to continue a cut-off reply, the model wrote a new one; it
    /// replaces the cut-off one.
    StartedOver,
    ToolDone {
        name: &'a str,
        args: &'a str,
        result: &'a str,
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
        Self {
            base_url,
            model,
            api_key,
            prefill,
            items,
            tools,
            specs: tools::specs(),
        }
    }

    /// Runs one user message to the model's final answer.
    pub fn turn(
        &mut self,
        prompt: String,
        cancel: &AtomicBool,
        on: impl FnMut(Shown),
    ) -> Result<()> {
        self.items.push(Item::User(prompt));
        self.run(cancel, on)
    }

    /// The last reply, if it was cut off before the model finished it.
    pub fn cut_reply(&self) -> Option<&Reply> {
        match self.items.last() {
            Some(Item::Assistant(r)) if r.cut => Some(r),
            _ => None,
        }
    }

    /// Whether a cut-off reply will continue from its last token rather
    /// than be asked for again.
    pub fn continues_from_token(&self) -> bool {
        self.prefill != Prefill::None && self.cut_reply().is_some_and(|r| !r.text.is_empty())
    }

    /// Picks up a cut-off reply: from its last token where the provider
    /// allows it, otherwise by asking for the reply again.
    pub fn resume(&mut self, cancel: &AtomicBool, on: impl FnMut(Shown)) -> Result<()> {
        if self.cut_reply().is_some() && !self.continues_from_token() {
            self.items.pop();
        }
        self.run(cancel, on)
    }

    fn run(&mut self, cancel: &AtomicBool, mut on: impl FnMut(Shown)) -> Result<()> {
        loop {
            let body = openai::request_body(&self.model, &self.items, &self.specs, self.prefill);
            on(Shown::Request(&body));
            let stream = openai::send(&self.base_url, self.api_key.as_deref(), &body)?;
            let mut continuation = self
                .continues_from_token()
                .then(|| self.cut_reply().cloned().map(Continuation::new))
                .flatten();
            let outcome = openai::read_stream(stream, cancel, |e| match &mut continuation {
                Some(c) => c.pass(e, &mut on),
                None => on(Shown::Stream(e)),
            });
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
            let results = self.run_calls(&calls);
            for (call, result) in calls.into_iter().zip(results) {
                on(Shown::ToolDone {
                    name: &call.name,
                    args: &call.args,
                    result: &result,
                });
                self.items.push(Item::ToolResult {
                    call_id: call.id,
                    content: result,
                });
            }
        }
    }

    /// Calls that change nothing run at the same time; otherwise in order.
    fn run_calls(&self, calls: &[crate::context::ToolCall]) -> Vec<String> {
        let tools = &self.tools;
        if calls.len() > 1 && calls.iter().all(|c| tools::read_only(&c.name)) {
            std::thread::scope(|s| {
                let handles: Vec<_> = calls
                    .iter()
                    .map(|c| s.spawn(move || tools.run(&c.name, &c.args)))
                    .collect();
                handles
                    .into_iter()
                    .map(|h| {
                        h.join()
                            .unwrap_or_else(|_| "error: the tool crashed".into())
                    })
                    .collect()
            })
        } else {
            calls.iter().map(|c| tools.run(&c.name, &c.args)).collect()
        }
    }
}

fn system_prompt(tools: &Tools) -> String {
    format!(
        "You are {name}, an AI agent working on the user's computer through your tools. \
The user owns you and can see everything you are given, this text included; \
answer questions about yourself plainly.\n\
Working directory: {cwd}\n\
Date (UTC): {date}",
        name = crate::NAME,
        cwd = tools.cwd().display(),
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
