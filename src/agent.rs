//! The loop: send the context, stream the reply, run the tools it calls,
//! add the results and go again until the model answers without tools.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::Result;
use crate::compact;
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
    /// The variable that holds the key, for a provider that needs one.
    pub key_env: Option<String>,
    /// How this provider continues a cut-off reply.
    pub prefill: Prefill,
    pub items: Vec<Item>,
    pub tools: Tools,
    specs: Vec<ToolSpec>,
    /// The model sent no tools, since it takes none.
    no_tools: Option<String>,
    /// What the model should learn with the next message, such as a
    /// change the user undid.
    notes: Vec<String>,
    /// Where the conversation is saved after each turn; `None` keeps it
    /// in memory only.
    pub log: Option<(PathBuf, String)>,
    /// What the command line set, which the settings do not override.
    pub launch: crate::settings::Launch,
    /// Input tokens of the last request, and how many items it held.
    last_input: Option<(u64, usize)>,
    /// A summary being written on the side before it is needed.
    prepared: Option<Prepared>,
    /// For each user message of this run still in `items`: where it
    /// stands there, and the number of the first file change made after
    /// it was sent, for `rewind`.
    marks: Vec<(usize, u64)>,
}

/// Share of the window, in percent, from which old tool results are
/// cleared, when that frees a tenth of the window or more.
const CLEAR_AT: u64 = 50;
/// From here a summary of the older part is written on the side, so it
/// is ready when needed and nobody waits for it.
const PREPARE_AT: u64 = 70;
/// From here the older part is replaced by that summary.
const COMPACT_AT: u64 = 85;
/// What `/compact` keeps of the recent part, in tokens.
const MANUAL_KEEP: u64 = 2_000;
/// Below this many tokens the context is never looked at for fitting.
const SMALL: u64 = 16_000;

/// A summary of `items[1..cut]` being written on a thread of its own.
struct Prepared {
    cut: usize,
    /// The request, to be shown when the summary is put in.
    body: Vec<u8>,
    cancel: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<Result<Summary>>>,
}

impl Drop for Prepared {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// A summary the model wrote, with what its request took.
struct Summary {
    text: String,
    usage: Option<serde_json::Value>,
    served: Option<openai::Served>,
    first: Option<Duration>,
    total: Duration,
}

/// Asks for a summary with the request `body`.
fn summarise(
    base_url: &str,
    api_key: Option<&str>,
    proxy: Option<&str>,
    body: &[u8],
    cancel: &AtomicBool,
) -> Result<Summary> {
    let start = Instant::now();
    let Some(stream) = openai::send(base_url, api_key, proxy, body, cancel)? else {
        return Err("stopped".into());
    };
    let mut first = None;
    let outcome = openai::read_stream(stream, cancel, |_| {
        first.get_or_insert_with(|| start.elapsed());
    });
    if let Some(e) = outcome.error {
        return Err(e.into());
    }
    if outcome.cancelled {
        return Err("stopped".into());
    }
    let text = outcome.reply.text.trim().to_string();
    if text.is_empty() {
        return Err("the model wrote no summary".into());
    }
    Ok(Summary {
        text,
        usage: outcome.usage,
        served: outcome.served,
        first,
        total: start.elapsed(),
    })
}

/// Whether a provider's error says the context is longer than the model takes.
pub fn too_long(error: &str) -> bool {
    let e = error.to_lowercase();
    [
        "context length",
        "context_length",
        "context window",
        "maximum context",
        "prompt is too long",
        "too many tokens",
        "context size",
        "input token count",
    ]
    .iter()
    .any(|s| e.contains(s))
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
    /// The images in the context were taken out, since the provider
    /// refused them with the error `why`, or its model list says the
    /// model takes none; the request goes (again) without them.
    ImagesDropped {
        why: &'a str,
        refused: bool,
    },
    /// The model is sent no tools from now on, since the provider refused
    /// them with the error `why`, or its model list says the model takes
    /// none.
    ToolsOff {
        why: &'a str,
        refused: bool,
    },
    /// Something the user should know, such as an MCP server that failed.
    Notice(&'a str),
    /// The older part of the conversation is being summarised, or the
    /// summary written on the side is awaited.
    Compacting,
    /// Old tool results were cleared to free about `saved` tokens.
    Cleared {
        results: usize,
        saved: u64,
    },
    /// The older part was replaced by a summary; the context went from
    /// about `before` tokens to `after`. The whole conversation as it was
    /// is kept in `kept`.
    Compacted {
        before: u64,
        after: u64,
        kept: Option<&'a std::path::Path>,
    },
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
        let specs = all_specs(&tools);
        Self {
            base_url,
            proxy: None,
            fields: Fields::default(),
            model,
            api_key,
            key_env: None,
            prefill,
            items,
            tools,
            specs,
            no_tools: None,
            notes: Vec::new(),
            log: None,
            launch: Default::default(),
            last_input: None,
            prepared: None,
            marks: Vec::new(),
        }
    }

    /// Puts in the user's own tools, in place of any before.
    pub fn set_custom(
        &mut self,
        custom: std::collections::BTreeMap<String, crate::custom::CustomTool>,
    ) -> std::result::Result<(), String> {
        self.tools.set_custom(custom)?;
        self.specs = all_specs(&self.tools);
        Ok(())
    }

    /// Puts in a context edited by hand, and saves it.
    pub fn set_items(&mut self, items: Vec<Item>) -> Result<()> {
        self.items = items;
        self.marks.clear();
        self.prepared = None;
        self.last_input = None;
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
        self.marks.clear();
        self.prepared = None;
        self.last_input = None;
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
        if self.prepared.as_ref().is_some_and(|p| p.cut > last + 1) {
            self.prepared = None;
        }
        Some(self.items.split_off(last + 1))
    }

    /// Takes back the file changes made since the last user message was
    /// sent: what was undone, and what could not be; None when it was sent
    /// before this run, so its changes are not known.
    pub fn undo_answer(&mut self) -> Option<(Vec<String>, Vec<String>)> {
        let last = self
            .items
            .iter()
            .rposition(|i| matches!(i, Item::User(_)))?;
        let &(at, from) = self.marks.last()?;
        (at == last).then(|| self.tools.rewind(from))
    }

    /// The user messages `rewind` can go back to, latest first: the
    /// message, and how many file changes not yet undone came after it.
    pub fn rewind_points(&self) -> Vec<(&str, usize)> {
        self.marks
            .iter()
            .rev()
            .filter_map(|&(at, from)| match self.items.get(at) {
                Some(Item::User(text)) => Some((text.as_str(), self.tools.changes_since(from))),
                _ => None,
            })
            .collect()
    }

    /// Goes back to just before the `n`th latest user message (1 is the
    /// last): the conversation as it was then, and every file change made
    /// since taken back. Returns the message, and what was undone and what
    /// could not be, as lines for the user.
    pub fn rewind(&mut self, n: usize) -> Option<(String, Vec<String>, Vec<String>)> {
        let at = self.marks.len().checked_sub(n).filter(|_| n > 0)?;
        let (item, from) = self.marks[at];
        let Some(Item::User(text)) = self.items.get(item) else {
            return None;
        };
        let text = text.clone();
        let (done, failed) = self.tools.rewind(from);
        self.marks.truncate(at);
        self.items.truncate(item);
        self.prepared = None;
        self.last_input = None;
        self.notes.clear();
        // Saved as it now is; what was cut off is gone from the file too.
        let _ = self.save(Ok(()));
        Some((text, done, failed))
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
        self.marks
            .push((self.items.len(), self.tools.next_change()));
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
            Some(Item::User(_) | Item::ToolResult { .. } | Item::Image { .. }) => true,
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
            if self.no_tools.as_ref() == Some(&self.model) {
                &[]
            } else {
                &self.specs
            },
            self.prefill,
            &self.fields.for_model(&self.model),
        )
    }

    fn run(&mut self, cancel: &AtomicBool, mut on: impl FnMut(Shown)) -> Result<()> {
        // Compacted once already for a "too long" error in this turn.
        let mut compacted = false;
        // The first request waits for MCP servers still starting.
        for e in self.tools.mcp.wait() {
            on(Shown::Notice(&e));
        }
        self.specs = all_specs(&self.tools);
        'request: loop {
            self.fit(cancel, &mut on)?;
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            let images = self.items.iter().any(|i| matches!(i, Item::Image { .. }));
            if images
                && self.takes_images() == Some(false)
                && self.drop_images("this model takes no images")
            {
                let why = format!(
                    "{} takes no images, by its provider's model list.",
                    self.model
                );
                on(Shown::ImagesDropped {
                    why: &why,
                    refused: false,
                });
            }
            let sends_tools = self.no_tools.as_ref() != Some(&self.model);
            // Only a list at hand is asked, so the request never waits for one;
            // a refusal from the provider tells the rest.
            let listed = crate::models::fetched(&self.base_url);
            if sends_tools
                && listed.and_then(|l| crate::models::takes_tools(&l, &self.model)) == Some(false)
            {
                self.no_tools = Some(self.model.clone());
                let why = format!(
                    "{} takes no tools, by its provider's model list",
                    self.model
                );
                on(Shown::ToolsOff {
                    why: &why,
                    refused: false,
                });
            }
            let body = self.body();
            let held = self.items.len();
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
                    // Every later request would fail the same way.
                    if refuses_images(&*e) && self.drop_images("the provider refused it") {
                        on(Shown::ImagesDropped {
                            why: &e.to_string(),
                            refused: true,
                        });
                        continue 'request;
                    }
                    if too_long(&e.to_string()) && !compacted {
                        compacted = true;
                        self.compact(None, cancel, &mut on)?;
                        continue 'request;
                    }
                    if sends_tools && refuses_tools(&*e) {
                        self.no_tools = Some(self.model.clone());
                        on(Shown::ToolsOff {
                            why: &e.to_string(),
                            refused: true,
                        });
                        continue 'request;
                    }
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
                if let Some(t) = u.get("prompt_tokens").and_then(serde_json::Value::as_u64) {
                    self.last_input = Some((t, held));
                }
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
            // Images go after all the results: each tool call must be
            // answered right after the call.
            let mut images = Vec::new();
            for (call, mut ran) in calls.into_iter().zip(results) {
                if let Some(image) = ran.image.take() {
                    if self.takes_images() == Some(false) {
                        ran.result +=
                            "; not shown: this model takes no images, by its provider's model list";
                    } else {
                        ran.result += IMAGE_FOLLOWS;
                        images.push(Item::Image {
                            data: crate::image::base64(&image.bytes),
                            name: image.name,
                            mime: image.mime,
                        });
                    }
                }
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
            self.items.append(&mut images);
        }
    }

    /// About how many tokens the context holds: the last request's count
    /// plus an estimate of what came after it.
    pub fn context_tokens(&self) -> u64 {
        match self.last_input {
            Some((t, n)) if n <= self.items.len() => t + compact::tokens(&self.items[n..]),
            _ => compact::tokens(&self.items),
        }
    }

    /// The model's context window, from the provider's model list.
    fn window(&self) -> Option<u64> {
        let list = crate::models::fetched(&self.base_url).or_else(|| {
            crate::models::list(
                &self.base_url,
                self.api_key.as_deref(),
                self.proxy.as_deref(),
            )
            .ok()
        })?;
        crate::models::window_in(&list, &self.model)
    }

    /// Before a request, makes the context fit the window: clears old tool
    /// results, writes a summary on the side, and puts it in place of the
    /// older part when the context nears the end of the window.
    fn fit(&mut self, cancel: &AtomicBool, on: &mut impl FnMut(Shown)) -> Result<()> {
        let used = self.context_tokens();
        if used < SMALL {
            return Ok(());
        }
        let Some(window) = self.window() else {
            return Ok(());
        };
        let share = |t: u64| t * 100 / window.max(1);
        let mut used = used;
        if share(used) >= CLEAR_AT {
            let before = compact::tokens(&self.items);
            let full = self.items.clone();
            let cleared = compact::clear_old_results(&mut self.items, window / 4);
            let saved = before.saturating_sub(compact::tokens(&self.items));
            // Each clearing costs the provider's cache, so only a large one.
            if saved >= window / 10 {
                self.keep_whole(&full);
                self.last_input = None;
                self.prepared = None;
                on(Shown::Cleared {
                    results: cleared.len(),
                    saved,
                });
                used = self.context_tokens();
            } else {
                self.items = full;
            }
        }
        let keep = (window / 5).min(30_000);
        if share(used) >= PREPARE_AT
            && self.prepared.is_none()
            && let Some(cut) = compact::cut_point(&self.items, keep)
        {
            self.prepare(cut);
        }
        if share(used) >= COMPACT_AT {
            self.compact(None, cancel, on)?;
        }
        Ok(())
    }

    /// The request that asks the model for a summary of `items[1..cut]`:
    /// the conversation itself, so the provider's cache serves it, then
    /// the summary prompt.
    fn summary_body(&self, cut: usize) -> Vec<u8> {
        let mut items = self.items[..cut].to_vec();
        items.push(Item::User(compact::PROMPT.into()));
        openai::request_body(
            &self.model,
            &items,
            if self.no_tools.as_ref() == Some(&self.model) {
                &[]
            } else {
                &self.specs
            },
            self.prefill,
            &self.fields.for_model(&self.model),
        )
    }

    /// Starts writing the summary of `items[1..cut]` on a thread.
    fn prepare(&mut self, cut: usize) {
        let body = self.summary_body(cut);
        let (url, key, proxy) = (
            self.base_url.clone(),
            self.api_key.clone(),
            self.proxy.clone(),
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        let sent = body.clone();
        let handle = std::thread::spawn(move || {
            summarise(&url, key.as_deref(), proxy.as_deref(), &sent, &stop)
        });
        self.prepared = Some(Prepared {
            cut,
            body,
            cancel,
            handle: Some(handle),
        });
    }

    /// Replaces the older part of the conversation with a summary the
    /// model writes, keeping the recent part and the user's own messages
    /// word for word. Uses the summary written on the side if there is one.
    /// `keep` is how many tokens of the recent part stay as they are.
    fn compact(
        &mut self,
        keep: Option<u64>,
        cancel: &AtomicBool,
        on: &mut impl FnMut(Shown),
    ) -> Result<()> {
        let window = self.window();
        let keep = keep.unwrap_or_else(|| window.map_or(20_000, |w| (w / 5).min(30_000)));
        on(Shown::Compacting);
        let prepared = self.prepared.take().filter(|p| p.cut < self.items.len());
        let ready = match prepared {
            Some(mut p) => {
                let handle = p.handle.take();
                while handle.as_ref().is_some_and(|h| !h.is_finished()) {
                    if !sleep_unless_cancelled(Duration::from_millis(50), cancel) {
                        return Ok(());
                    }
                }
                let got = handle.map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err("the summary thread failed".into()))
                });
                if got.as_ref().is_some_and(|r| r.is_ok()) {
                    on(Shown::Request(&p.body));
                }
                got.and_then(|r| r.ok()).map(|s| (p.cut, s))
            }
            None => None,
        };
        let (cut, summary) = match ready {
            Some(r) => r,
            None => {
                let cut = compact::cut_point(&self.items, keep)
                    .ok_or("nothing to compact: all of the conversation is recent")?;
                let body = self.summary_body(cut);
                on(Shown::Request(&body));
                let s = summarise(
                    &self.base_url,
                    self.api_key.as_deref(),
                    self.proxy.as_deref(),
                    &body,
                    cancel,
                );
                match s {
                    Ok(s) => (cut, s),
                    Err(_) if cancel.load(Ordering::Relaxed) => return Ok(()),
                    Err(e) => return Err(format!("compacting failed: {e}").into()),
                }
            }
        };
        on(Shown::Timing {
            first: summary.first,
            total: summary.total,
        });
        if let Some(s) = &summary.served {
            on(Shown::Served(s));
        }
        if let Some(u) = &summary.usage {
            on(Shown::Usage(u));
        }
        let before = self.context_tokens();
        let full = self.items.clone();
        let kept = self.keep_whole(&full);
        // The user's own words may take a tenth of the window.
        let max_user = window.map_or(40_000, |w| (w * 4 / 10) as usize);
        let removed = compact::apply(&mut self.items, cut, &summary.text, max_user);
        if !removed.is_empty() {
            // A message folded into the summary can no longer be gone back to.
            self.marks.retain(|m| m.0 >= cut);
            for m in &mut self.marks {
                m.0 = m.0 + 1 - removed.len();
            }
        }
        self.last_input = None;
        on(Shown::Compacted {
            before,
            after: self.context_tokens(),
            kept: kept.as_deref(),
        });
        Ok(())
    }

    /// `/compact`: compacts now, keeping only about the last exchange as
    /// it was, and saves.
    pub fn compact_now(&mut self, cancel: &AtomicBool, mut on: impl FnMut(Shown)) -> Result<()> {
        let r = self.compact(Some(MANUAL_KEEP), cancel, &mut on);
        self.save(r)
    }

    /// Saves `items`, the whole conversation before it was made shorter,
    /// next to the conversation's file; returns where.
    fn keep_whole(&self, items: &[Item]) -> Option<PathBuf> {
        let (path, provider) = self.log.as_ref()?;
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let stem = path.file_stem()?.to_string_lossy();
        let whole = path.with_file_name(format!("{stem}.before-{n}.json"));
        let saved = history::Saved {
            cwd: self.tools.cwd().to_path_buf(),
            provider: provider.clone(),
            model: self.model.clone(),
            items: items.to_vec(),
        };
        history::save(&whole, &saved).ok()?;
        Some(whole)
    }

    /// Takes every image out of the context, and tells the model in the
    /// tool result it came with `why`; false if there was none.
    fn drop_images(&mut self, why: &str) -> bool {
        let before = self.items.len();
        self.items.retain(|i| !matches!(i, Item::Image { .. }));
        for item in &mut self.items {
            if let Item::ToolResult { content, .. } = item
                && let Some(head) = content.strip_suffix(IMAGE_FOLLOWS)
            {
                *content = format!("{head}; not shown: {why}");
            }
        }
        self.items.len() < before
    }

    /// Whether the model takes images, when the provider's model list
    /// says.
    fn takes_images(&self) -> Option<bool> {
        let list = crate::models::list(
            &self.base_url,
            self.api_key.as_deref(),
            self.proxy.as_deref(),
        );
        crate::models::takes_images(&*list.ok()?, &self.model)
    }

    /// Calls that change nothing run at the same time; otherwise in order.
    fn run_calls(
        &self,
        calls: &[crate::context::ToolCall],
        cancel: &AtomicBool,
    ) -> Vec<crate::tools::Ran> {
        let tools = &self.tools;
        let one = |c: &crate::context::ToolCall| match c.name.as_str() {
            "agent" => self.run_agent(&c.args, cancel),
            _ => tools.run_noting(&c.name, &c.args, cancel),
        };
        if calls.len() > 1
            && calls
                .iter()
                .all(|c| c.name == "agent" || tools.reads_only(&c.name))
        {
            std::thread::scope(|s| {
                let handles: Vec<_> = calls.iter().map(|c| s.spawn(move || one(c))).collect();
                handles
                    .into_iter()
                    .map(|h| {
                        h.join().unwrap_or_else(|_| crate::tools::Ran {
                            result: "error: the tool crashed".into(),
                            note: None,
                            before: None,
                            image: None,
                        })
                    })
                    .collect()
            })
        } else {
            calls.iter().map(one).collect()
        }
    }

    /// Runs another agent on the task in `args`, with a fresh context and
    /// the same tools; what comes back is its last answer.
    fn run_agent(&self, args: &str, cancel: &AtomicBool) -> crate::tools::Ran {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let ran = |result: String, note: Option<String>| crate::tools::Ran {
            result,
            note,
            before: None,
            image: None,
        };
        let task = serde_json::from_str::<serde_json::Value>(args)
            .ok()
            .and_then(|v| v.get("task")?.as_str().map(str::to_string))
            .filter(|t| !t.trim().is_empty());
        let Some(task) = task else {
            return ran("error: give the agent its task in `task`".into(), None);
        };
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut child = Session::new(
            self.base_url.clone(),
            self.model.clone(),
            self.api_key.clone(),
            self.prefill,
            self.tools.child(),
        );
        child.proxy = self.proxy.clone();
        child.fields = self.fields.clone();
        child.key_env = self.key_env.clone();
        child.launch = self.launch.clone();
        child.no_tools = self.no_tools.clone();
        if let Some(Item::System(system)) = self.items.first() {
            child.items[0] = Item::System(format!("{system}\n\n{SUBAGENT}"));
        }
        // Kept where this conversation is kept, if it is.
        child.log = self.log.as_ref().map(|(_, provider)| {
            let dir = crate::journal::data_dir().join("agents");
            let _ = std::fs::create_dir_all(&dir);
            let name = format!(
                "{}-{}-{id}.json",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs()),
                std::process::id()
            );
            (dir.join(name), provider.clone())
        });
        let sink = self.tools.sub.clone();
        let tell = |s: Sub| {
            if let Some(sink) = &sink {
                sink(s)
            }
        };
        tell(Sub::Started {
            agent: id,
            task: task.clone(),
        });
        let mut steps = 0;
        let r = child.turn(task, cancel, |shown| match shown {
            Shown::Usage(u) => tell(Sub::Usage(u.clone())),
            Shown::ToolDone { name, args, .. } => {
                steps += 1;
                tell(Sub::Tool {
                    agent: id,
                    name: name.into(),
                    args: args.into(),
                })
            }
            _ => {}
        });
        tell(Sub::Done { agent: id });
        let answer = child
            .items
            .iter()
            .rev()
            .find_map(|i| match i {
                Item::Assistant(r) if !r.text.trim().is_empty() => Some(r.text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let result = match (r, answer.is_empty()) {
            (Ok(()), false) => answer,
            (Ok(()), true) => "error: the agent ended without an answer".into(),
            (Err(e), true) => format!("error: the agent stopped: {e}"),
            (Err(e), false) => format!("error: the agent stopped: {e}; its last words:\n{answer}"),
        };
        let mut note = format!("{steps} tool call{}", if steps == 1 { "" } else { "s" });
        if let Some((path, _)) = &child.log {
            note += &format!("; the agent's conversation: {}", path.display());
        }
        ran(result, Some(note))
    }
}

/// What an agent started by the model does, told as it happens.
pub enum Sub {
    Started {
        agent: u64,
        task: String,
    },
    Tool {
        agent: u64,
        name: String,
        args: String,
    },
    /// The provider's usage report for one of its requests.
    Usage(serde_json::Value),
    Done {
        agent: u64,
    },
}

/// The tools given to the model: the built-in ones, the user's, and
/// starting another agent.
fn all_specs(tools: &Tools) -> Vec<ToolSpec> {
    let mut specs = tools.all_specs();
    specs.push(ToolSpec {
        name: "agent".into(),
        description: "Start another agent on a task: a fresh context with your tools, this one \
             included. It sees nothing of this conversation, so `task` must hold everything it \
             needs: the goal, what you already know, the paths involved, and what to report. \
             Only its final answer comes back, so your context stays small. Worth it for work \
             of many steps whose details you will not need: a wide search, studying an \
             unfamiliar part, an independent change. Several in one reply run at the same time; \
             give them parts that do not touch the same files. Not worth it for what a few tool \
             calls do: each agent pays for its own context. Its file changes undo like yours."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "task": { "type": "string" } },
            "required": ["task"],
        }),
    });
    specs
}

/// Added to the system prompt of an agent started by the model.
const SUBAGENT: &str = "Another agent started you on the task in the user message; it is the \
only one who reads your answer, and it sees nothing else of your work. Do the task, then end \
with a complete, plain report: what you found or changed, with paths and line numbers, and \
what is left unchecked.";

type Map = serde_json::Map<String, serde_json::Value>;

/// Ends the result of a tool whose image follows the results.
const IMAGE_FOLLOWS: &str = "; it follows as an image";

/// Whether a provider's error refuses the images in a request.
fn refuses_images(e: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
    let Some(h) = e.downcast_ref::<openai::HttpError>() else {
        return false;
    };
    let text = h.message.to_lowercase();
    matches!(h.status, 400 | 404 | 415 | 422)
        && ["image", "vision", "multimodal"]
            .iter()
            .any(|w| text.contains(w))
}

/// Whether a provider's error refuses tools for the model, as OpenRouter's
/// "No endpoints found that support tool use" or Ollama's "does not
/// support tools".
fn refuses_tools(e: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
    let Some(h) = e.downcast_ref::<openai::HttpError>() else {
        return false;
    };
    let text = h.message.to_lowercase();
    matches!(h.status, 400 | 404 | 422)
        && [
            "support tool",
            "support function",
            "tool choice",
            "tool_choice",
        ]
        .iter()
        .any(|w| text.contains(w))
}

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
#[derive(Clone, Debug, Default)]
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
{run}Say what is unchecked instead of claiming it works.\n\
Working directory: {cwd}\n\
System: {os}\n\
Date (UTC): {date}",
        name = crate::NAME,
        run = if tools.has_shell() {
            ""
        } else {
            "You can run only what your tools offer; the built-in ones work on files, and cannot \
             run programs, builds or tests; a script you write is not run. "
        },
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
    fn an_agent_works_on_its_own_and_its_changes_undo() {
        let (url, server) = serve(vec![
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a1","function":{"name":"agent","arguments":"{\"task\":\"make c.txt\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"w1","function":{"name":"write","arguments":"{\"path\":\"c.txt\",\"content\":\"c\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"made c.txt"},"finish_reason":"stop"}]}"#,
                "[DONE]",
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#,
                "[DONE]",
            ]),
        ]);
        let (mut session, dir) = session(url, Prefill::None);
        let mut done = Vec::new();
        session
            .turn("go".into(), &AtomicBool::new(false), |s| {
                if let Shown::ToolDone { name, result, .. } = s {
                    done.push(format!("{name}: {result}"));
                }
            })
            .unwrap();
        assert_eq!(
            done,
            ["agent: made c.txt"],
            "only the agent's answer comes back"
        );
        let requests: Vec<serde_json::Value> =
            server.join().unwrap().iter().map(|r| body(r)).collect();
        let child = requests[1]["messages"].as_array().unwrap();
        assert!(child[0]["content"].as_str().unwrap().ends_with(SUBAGENT));
        assert_eq!(child[1]["content"], "make c.txt", "the task is all it sees");
        assert_eq!(fs::read_to_string(dir.join("c.txt")).unwrap(), "c");
        session.undo().unwrap();
        assert!(!dir.join("c.txt").exists(), "its change undoes from here");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rewind_takes_back_the_conversation_and_the_files() {
        let dir = std::env::temp_dir().join(format!("agent-rewind-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.txt"), "one\n").unwrap();
        let done = |t: &str| {
            sse(&[
                &format!(
                    r#"{{"choices":[{{"delta":{{"content":"{t}"}},"finish_reason":"stop"}}]}}"#
                ),
                "[DONE]",
            ])
        };
        let edit = |id: &str, old: &str, new: &str| {
            sse(&[&format!(
                r#"{{"choices":[{{"delta":{{"tool_calls":[{{"index":0,"id":"{id}","function":{{"name":"edit","arguments":"{{\"path\":\"a.txt\",\"old\":\"{old}\",\"new\":\"{new}\"}}"}}}}]}},"finish_reason":"tool_calls"}}]}}"#
            )])
        };
        let (url, server) = serve(vec![
            edit("e1", "one", "two"),
            done("first"),
            edit("e2", "two", "three"),
            done("second"),
        ]);
        let access = Access::new(&dir, std::slice::from_ref(&dir), &[]).unwrap();
        let tools = Tools::new(access, Journal::new(dir.join("j.jsonl")));
        let mut session = Session::new(url, "m".into(), None, Prefill::None, tools);
        let stop = AtomicBool::new(false);
        session.turn("make it two".into(), &stop, |_| {}).unwrap();
        session.turn("make it three".into(), &stop, |_| {}).unwrap();
        server.join().unwrap();
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "three\n");
        assert_eq!(
            session.rewind_points(),
            [("make it three", 1), ("make it two", 2)]
        );
        assert!(session.rewind(0).is_none() && session.rewind(3).is_none());
        let (message, done, failed) = session.rewind(2).unwrap();
        assert_eq!(message, "make it two");
        assert_eq!((done.len(), failed.len()), (2, 0));
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
        assert_eq!(session.items.len(), 1, "only the system prompt is left");
        assert!(session.rewind_points().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tools_the_provider_refuses_are_left_out() {
        let dir = std::env::temp_dir().join(format!("agent-no-tools-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let body = r#"{"error":{"message":"No endpoints found that support tool use."}}"#;
        let (url, server) = serve(vec![
            format!(
                "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
            sse(&[
                r#"{"choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}]}"#,
                "[DONE]",
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"again"},"finish_reason":"stop"}]}"#,
                "[DONE]",
            ]),
        ]);
        let access = Access::new(&dir, std::slice::from_ref(&dir), &[]).unwrap();
        let tools = Tools::new(access, Journal::new(dir.join("j.jsonl")));
        let mut session = Session::new(url, "m".into(), None, Prefill::None, tools);
        let mut off = 0;
        for say in ["hello", "more"] {
            session
                .turn(say.into(), &AtomicBool::new(false), |s| {
                    if let Shown::ToolsOff { refused: true, .. } = s {
                        off += 1;
                    }
                })
                .unwrap();
        }
        assert_eq!(off, 1);
        let requests = server.join().unwrap();
        assert!(requests[0].contains(r#""tools":"#));
        assert!(!requests[1].contains(r#""tools":"#));
        assert!(!requests[2].contains(r#""tools":"#));
        // Another model is sent the tools again.
        session.model = "n".into();
        assert!(
            String::from_utf8(session.body())
                .unwrap()
                .contains(r#""tools":"#)
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_image_the_provider_refuses_is_taken_out() {
        let dir = std::env::temp_dir().join(format!("agent-image-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.gif"), b"GIF89a\x02\x00\x03\x00").unwrap();
        let reply = |status: &str, body: &str| {
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
        };
        let (url, server) = serve(vec![
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"r1","function":{"name":"read","arguments":"{\"path\":\"a.gif\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            ]),
            // The model list does not say whether the model takes images.
            reply("200 OK", r#"{"data":[]}"#),
            reply(
                "400 Bad Request",
                r#"{"error":{"message":"image input is not supported"}}"#,
            ),
            sse(&[
                r#"{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}"#,
                "[DONE]",
            ]),
        ]);
        let access = Access::new(&dir, std::slice::from_ref(&dir), &[]).unwrap();
        let tools = Tools::new(access, Journal::new(dir.join("j.jsonl")));
        let mut session = Session::new(url, "m".into(), None, Prefill::None, tools);
        let mut dropped = Vec::new();
        session
            .turn("look".into(), &AtomicBool::new(false), |s| {
                if let Shown::ImagesDropped { why, .. } = s {
                    dropped.push(why.to_string());
                }
            })
            .unwrap();
        assert_eq!(dropped.len(), 1);
        assert!(dropped[0].contains("image input is not supported"));
        let requests = server.join().unwrap();
        assert!(requests[2].contains("data:image/gif;base64,"));
        assert!(!requests[3].contains("image_url"));
        assert!(requests[3].contains("GIF image, 2×3, 1 KB; not shown: the provider refused it"));
        assert!(
            !session
                .items
                .iter()
                .any(|i| matches!(i, Item::Image { .. }))
        );
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
    fn compacts_when_the_provider_says_the_context_is_too_long() {
        let long = "HTTP/1.1 400 Bad Request\r\nContent-Length: 46\r\n\r\nThis model's maximum context length is 1000 t.";
        let models = r#"{"data":[{"id":"m","context_length":1000}]}"#;
        let list = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{models}",
            models.len()
        );
        let (url, server) = serve(vec![
            long.into(),
            list,
            sse(&[r#"{"choices":[{"delta":{"content":"Goal: sum"},"finish_reason":"stop"}]}"#]),
            sse(&[r#"{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}"#]),
        ]);
        let (mut session, dir) = session(url, Prefill::None);
        session
            .items
            .push(Item::User("first, keep my words".into()));
        session.items.push(Item::Assistant(Reply {
            text: "an answer".repeat(50),
            ..Default::default()
        }));
        let mut shown = Vec::new();
        session
            .turn(
                "second ".repeat(300),
                &AtomicBool::new(false),
                |s| match s {
                    Shown::Compacting => shown.push("compacting".to_string()),
                    Shown::Compacted { .. } => shown.push("compacted".to_string()),
                    _ => {}
                },
            )
            .unwrap();
        assert_eq!(shown, ["compacting", "compacted"]);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[1].starts_with("GET "), "{}", requests[1]);
        assert!(requests[2].contains("Write the summary"), "{}", requests[2]);
        let last = &requests[3];
        assert!(last.contains("was compacted"), "{last}");
        assert!(last.contains("first, keep my words"), "{last}");
        assert!(!last.contains("an answeran answer"), "{last}");
        assert!(matches!(session.items.last(), Some(Item::Assistant(r)) if r.text == "done"));
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
