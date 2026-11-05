//! The loop: send the context, stream the reply, run the tools it calls,
//! add the results and go again until the model answers without tools.

use std::sync::atomic::AtomicBool;

use crate::Result;
use crate::context::{Item, ToolSpec};
use crate::openai::{self, Event};
use crate::tools::{self, Tools};

pub struct Session {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
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
    ToolDone {
        name: &'a str,
        args: &'a str,
        result: &'a str,
    },
}

impl Session {
    pub fn new(base_url: String, model: String, api_key: Option<String>, tools: Tools) -> Self {
        let items = vec![Item::System(system_prompt(&tools))];
        Self {
            base_url,
            model,
            api_key,
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
        mut on: impl FnMut(Shown),
    ) -> Result<()> {
        self.items.push(Item::User(prompt));
        loop {
            let body = openai::request_body(&self.model, &self.items, &self.specs);
            on(Shown::Request(&body));
            let stream = openai::send(&self.base_url, self.api_key.as_deref(), &body)?;
            let outcome = openai::read_stream(stream, cancel, |e| on(Shown::Stream(e)))?;
            if let Some(u) = &outcome.usage {
                on(Shown::Usage(u));
            }
            let mut reply = outcome.reply;
            for (n, call) in reply.calls.iter_mut().enumerate() {
                if call.id.is_empty() {
                    call.id = format!("call_{}_{n}", self.items.len());
                }
            }
            let calls = reply.calls.clone();
            self.items.push(Item::Assistant(reply));
            if outcome.cancelled || calls.is_empty() {
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
        let mut session = Session::new(url, "m".into(), None, tools);
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
    fn dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(20_763), (2026, 11, 6));
    }
}
