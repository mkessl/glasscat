//! The full-screen terminal interface.

mod commands;
mod context;
mod input;
mod markdown;
mod screen;
mod text;
mod transcript;

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    MouseEventKind,
};
use crossterm::style::Color;
use crossterm::{execute, terminal};
use serde_json::Value;

use crate::Result;
use crate::agent::{Session, Shown};
use crate::context::Item;
use crate::openai::{Event as Stream, Served};
use input::Input;
use screen::{Screen, Style};
use transcript::{Block, Transcript};

/// At most one frame per this long; the first change after a pause is
/// drawn at once.
const FRAME: Duration = Duration::from_millis(16);
const INPUT_MAX_ROWS: usize = 8;
const WHEEL_LINES: usize = 3;
const PROMPT: &str = "› ";
const INPUT_POLL: Duration = Duration::from_millis(20);

enum Msg {
    Term(Event),
    Agent(AgentMsg),
    Done(Box<Session>, Option<String>),
    /// A result that arrived on its own, such as a policy lookup.
    Block(Block),
    /// The context window of a model, looked up on the side.
    Window(String, Option<u64>),
    /// Prompts from earlier conversations held here, for the input history.
    History(Vec<String>),
}

/// Where the provider is, for lookups made beside the conversation.
#[derive(Clone)]
struct Api {
    base_url: String,
    api_key: Option<String>,
    proxy: Option<String>,
}

enum AgentMsg {
    Request(Vec<u8>),
    Text(String),
    Reasoning(String),
    Calling(String),
    Tool {
        name: String,
        args: String,
        result: String,
        note: Option<String>,
    },
    Usage(Value),
    Served(Served),
    Timing(Option<Duration>, Duration),
    Retrying(String, Duration, u32),
    StartedOver,
}

enum View {
    Chat,
    Context { raw: bool, scroll: usize },
}

struct App {
    session: Option<Box<Session>>,
    model: String,
    transcript: Transcript,
    input: Input,
    screen: Screen,
    view: View,
    /// Lines scrolled up from the bottom of the transcript.
    scroll: usize,
    last_request: Vec<u8>,
    /// What a command shows over the chat, and how far it is scrolled.
    panel: Option<(String, usize)>,
    /// When the last request went out.
    sent_at: Instant,
    /// The last request that reported usage, and when it ended, to see
    /// why the cache missed.
    cache_last: Option<(crate::cache::Sent, Instant)>,
    busy: Option<String>,
    usage: Option<Value>,
    /// Cost of every request in this run, and how many reported one.
    spent: (f64, u32),
    /// Who answered last, as the stream said.
    served: Option<Served>,
    /// The last request's time to first token and in all.
    timing: Option<(Option<Duration>, Duration)>,
    api: Api,
    /// The model whose context window was looked up, and the window.
    window: Option<(String, Option<u64>)>,
    /// Whether the user was told the context is nearly full.
    warned_full: bool,
    cancel: Arc<AtomicBool>,
    tx: Sender<Msg>,
    /// Where the input was drawn: first screen row and first text row shown.
    input_at: (u16, usize),
    quit: bool,
    /// Whether the keyboard enhancement flags are on.
    keyboard: bool,
    /// Stops the input thread while another program has the terminal.
    paused: Arc<AtomicBool>,
    /// Whether the input thread may still be reading.
    reading: Arc<AtomicBool>,
    /// A hand-edited context that did not parse, to be opened again.
    kept_edit: Option<std::path::PathBuf>,
}

pub fn run(session: Session) -> Result<()> {
    // Before the input thread: the terminal queries here read input too.
    let term = TermGuard::enter()?;
    let (w, h) = terminal::size()?;
    let (tx, rx) = mpsc::channel();
    let input_tx = tx.clone();
    let paused = Arc::new(AtomicBool::new(false));
    let reading = Arc::new(AtomicBool::new(true));
    let (p, r) = (paused.clone(), reading.clone());
    std::thread::spawn(move || {
        loop {
            // Paused while another program has the terminal.
            if p.load(Ordering::Relaxed) {
                r.store(false, Ordering::Relaxed);
                std::thread::sleep(INPUT_POLL);
                continue;
            }
            r.store(true, Ordering::Relaxed);
            let e = match event::poll(INPUT_POLL) {
                Ok(true) => event::read(),
                Ok(false) => continue,
                Err(e) => Err(e),
            };
            let Ok(e) = e else { break };
            if input_tx.send(Msg::Term(e)).is_err() {
                break;
            }
        }
    });
    let cwd = session.tools.cwd().display().to_string();
    let api = Api {
        base_url: session.base_url.clone(),
        api_key: session.api_key.clone(),
        proxy: session.proxy.clone(),
    };
    let mut app = App {
        model: session.model.clone(),
        session: Some(Box::new(session)),
        transcript: Transcript::new(cwd),
        input: Input::default(),
        screen: Screen::new(w, h),
        view: View::Chat,
        scroll: 0,
        last_request: Vec::new(),
        panel: None,
        sent_at: Instant::now(),
        cache_last: None,
        busy: None,
        usage: None,
        spent: (0.0, 0),
        cancel: Arc::new(AtomicBool::new(false)),
        tx,
        input_at: (0, 0),
        quit: false,
        keyboard: term.keyboard,
        paused,
        reading,
        kept_edit: None,
        served: None,
        timing: None,
        api,
        window: None,
        warned_full: false,
    };
    let model = app.model.clone();
    app.look_up_window(&model);
    let (dir, tx) = (
        app.session.as_ref().map(|s| s.tools.cwd().to_path_buf()),
        app.tx.clone(),
    );
    std::thread::spawn(move || {
        if let Some(dir) = dir {
            let _ = tx.send(Msg::History(crate::history::recent_prompts(&dir, 20)));
        }
    });
    app.transcript.push(Block::Info(format!(
        "{} · {} · /help lists commands and keys",
        crate::NAME,
        app.model
    )));
    let items = &app.session.as_ref().expect("not started yet").items;
    if items.len() > 1 {
        replay(items, &mut app.transcript);
        app.hint_unanswered(false);
    }
    app.event_loop(rx)
}

impl App {
    fn event_loop(&mut self, rx: Receiver<Msg>) -> Result<()> {
        let mut out = io::stdout();
        let mut dirty = true;
        let mut drawn = Instant::now() - FRAME;
        while !self.quit {
            let msg = if dirty {
                let wait = FRAME.saturating_sub(drawn.elapsed());
                if wait.is_zero() {
                    self.draw(&mut out)?;
                    dirty = false;
                    drawn = Instant::now();
                    continue;
                }
                match rx.recv_timeout(wait) {
                    Ok(m) => m,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match rx.recv() {
                    Ok(m) => m,
                    Err(_) => break,
                }
            };
            self.handle(msg);
            dirty = true;
        }
        Ok(())
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Term(Event::Key(k)) if k.kind != KeyEventKind::Release => match self.view {
                View::Chat => self.chat_key(k),
                View::Context { .. } => self.context_key(k),
            },
            Msg::Term(Event::Paste(s)) => {
                if matches!(self.view, View::Chat) {
                    self.input.insert(&s);
                }
            }
            Msg::Term(Event::Mouse(m)) => match m.kind {
                // The wheel scrolls the panel while one is open.
                MouseEventKind::ScrollUp => match &mut self.panel {
                    Some((_, s)) => *s = s.saturating_sub(WHEEL_LINES),
                    None => self.scroll_by(WHEEL_LINES as isize),
                },
                MouseEventKind::ScrollDown => match &mut self.panel {
                    Some((_, s)) => *s += WHEEL_LINES,
                    None => self.scroll_by(-(WHEEL_LINES as isize)),
                },
                MouseEventKind::Down(_) if matches!(self.view, View::Chat) => {
                    let (y0, top) = self.input_at;
                    let prompt = text::width(PROMPT) as u16;
                    if m.row >= y0 && m.row < self.screen.height() - 1 && m.column >= prompt {
                        let col = (m.column - prompt) as usize;
                        let row = top + (m.row - y0) as usize;
                        self.input.click(self.input_width(), col, row);
                    }
                }
                _ => {}
            },
            Msg::Term(Event::Resize(w, h)) => self.screen.resize(w, h),
            Msg::Term(_) => {}
            Msg::Agent(a) => self.agent(a),
            // A command's answer that took a while: shown like the quick ones.
            Msg::Block(Block::Info(t)) => self.panel = Some((t, 0)),
            Msg::Block(b) => {
                self.panel = None;
                self.transcript.push(b);
                self.scroll = 0;
            }
            Msg::History(h) => self.input.add_older(h),
            Msg::Window(model, n) => {
                if self.window.as_ref().is_some_and(|w| w.0 == model) {
                    self.window = Some((model, n));
                    self.warn_if_full();
                }
            }
            Msg::Done(session, err) => {
                self.session = Some(session);
                self.busy = None;
                let cancelled = self.cancel.swap(false, Ordering::Relaxed);
                if let Some(e) = err {
                    self.transcript.push(Block::Error(e));
                }
                self.hint_unanswered(cancelled);
            }
        }
    }

    /// Says once when the last request filled most of the model's window.
    fn warn_if_full(&mut self) {
        let used = self
            .usage
            .as_ref()
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(Value::as_u64);
        let Some((used, window)) = used.zip(self.window.as_ref().and_then(|w| w.1)) else {
            return;
        };
        let full = used * 100 / window.max(1);
        if full < 80 {
            self.warned_full = false;
        } else if !self.warned_full {
            self.warned_full = true;
            self.transcript.push(Block::Info(format!(
                "The context is at {full}% of the model's window. /edit can cut it down; \
                 /clear starts anew."
            )));
        }
    }

    /// Asks the provider for the context window of `model` on a thread,
    /// unless that model's is known or on its way.
    fn look_up_window(&mut self, model: &str) {
        if self.window.as_ref().is_some_and(|w| w.0 == model) {
            return;
        }
        self.window = Some((model.to_string(), None));
        let (api, model, tx) = (self.api.clone(), model.to_string(), self.tx.clone());
        std::thread::spawn(move || {
            let n = crate::models::context_window(
                &api.base_url,
                api.api_key.as_deref(),
                api.proxy.as_deref(),
                &model,
            );
            let _ = tx.send(Msg::Window(model, n.ok().flatten()));
        });
    }

    /// Opens the settings file in the user's editor, then applies what can
    /// change while the agent runs.
    fn edit_settings(&mut self) {
        let path = crate::settings::path();
        if !path.exists() {
            // A start with the fields people change most, all empty.
            let start =
                "{\n  \"request\": {},\n  \"hide\": [],\n  \"allow\": [],\n  \"tools\": {}\n}\n";
            let made = std::fs::create_dir_all(crate::settings::dir())
                .and_then(|()| std::fs::write(&path, start));
            if let Err(e) = made {
                self.transcript
                    .push(Block::Error(format!("{}: {e}", path.display())));
                return;
            }
        }
        if let Err(e) = self.run_outside(|| editor(&path)) {
            self.transcript.push(Block::Error(e));
            return;
        }
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let applied = crate::settings::load().and_then(|s| crate::settings::apply(session, &s));
        self.transcript.push(match applied {
            Ok(()) => Block::Info(
                "Settings applied: request fields, proxy, allowed and hidden paths, and your \
                 tools. The provider, its address and key, and the default model change at the \
                 next start; /model switches now."
                    .into(),
            ),
            Err(e) => Block::Error(format!(
                "{e}\nNothing was changed; /settings opens the file again."
            )),
        });
    }

    /// Opens the context in the user's editor and takes back the result.
    fn edit_context(&mut self) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let path = std::env::temp_dir().join(format!(
            "{}-context-{}.txt",
            crate::NAME,
            std::process::id()
        ));
        let kept = self.kept_edit.take().is_some_and(|p| p.exists());
        let written = if kept {
            Ok(())
        } else {
            std::fs::write(&path, crate::context::to_text(&session.items))
        };
        if let Err(e) = written {
            self.transcript
                .push(Block::Error(format!("{}: {e}", path.display())));
            return;
        }
        let ran = self.run_outside(|| editor(&path));
        let text = ran.and_then(|()| std::fs::read_to_string(&path).map_err(|e| e.to_string()));
        let session = self.session.as_mut().expect("checked above");
        if text.as_ref().is_ok_and(|t| t.trim().is_empty()) {
            let _ = std::fs::remove_file(&path);
            self.transcript.push(Block::Info(
                "The file was empty, so the context is unchanged.".into(),
            ));
            return;
        }
        let result = text.and_then(|t| {
            let items = crate::context::from_text(&t).map_err(|e| {
                self.kept_edit = Some(path.clone());
                format!(
                    "{e}. The context is unchanged; /edit opens your text again ({}).",
                    path.display()
                )
            })?;
            session.set_items(items).map_err(|e| e.to_string())
        });
        match result {
            Ok(()) => {
                let _ = std::fs::remove_file(&path);
                let items = session.items.clone();
                self.transcript.clear();
                replay(&items, &mut self.transcript);
                self.transcript
                    .push(Block::Info("The context was edited by hand.".into()));
                self.hint_unanswered(false);
            }
            Err(e) => self.transcript.push(Block::Error(e)),
        }
    }

    /// Gives the terminal to another program for the length of `f`.
    fn run_outside<T>(&mut self, f: impl FnOnce() -> T) -> T {
        self.paused.store(true, Ordering::Relaxed);
        while self.reading.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
        }
        restore(self.keyboard);
        let out = f();
        let _ = setup(self.keyboard);
        self.paused.store(false, Ordering::Relaxed);
        let (w, h) = terminal::size().unwrap_or((self.screen.width(), self.screen.height()));
        self.screen.resize(w, h);
        out
    }

    /// Says how to pick up a turn the model did not finish, if there is one.
    fn hint_unanswered(&mut self, cancelled: bool) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        if !session.unanswered() {
            return;
        }
        let why = if cancelled {
            "Stopped."
        } else if session.cut_reply().is_some() {
            "The reply was cut off."
        } else {
            "No reply."
        };
        let how = if session.continues_from_token() {
            "continues the reply from where it stopped"
        } else {
            "asks again"
        };
        self.transcript
            .push(Block::Info(format!("{why} Enter on an empty line {how}.")));
    }

    fn agent(&mut self, a: AgentMsg) {
        let before = (self.scroll > 0).then(|| self.transcript.lines(self.chat_width()).len());
        match a {
            AgentMsg::Request(body) => {
                self.last_request = body;
                self.sent_at = Instant::now();
                self.busy = Some("waiting for the model".into());
            }
            AgentMsg::Text(t) => {
                self.busy = Some("answering".into());
                self.transcript.stream(&t, false);
            }
            AgentMsg::Reasoning(t) => {
                self.busy = Some("thinking".into());
                self.transcript.stream(&t, true);
            }
            AgentMsg::Calling(name) => self.busy = Some(format!("calling {name}")),
            AgentMsg::Tool {
                name,
                args,
                result,
                note,
            } => {
                let report = name == crate::report::TOOL;
                self.transcript.push(Block::Tool { name, args, result });
                if let Some(mut n) = note {
                    if report {
                        n += " /report opens it as a new issue on the project's page, for you to send.";
                    }
                    self.transcript.push(Block::Info(n));
                }
            }
            AgentMsg::Usage(u) => {
                if let Some(c) = u.get("cost").and_then(Value::as_f64) {
                    self.spent = (self.spent.0 + c, self.spent.1 + 1);
                }
                if let Some((last, ended)) = &self.cache_last {
                    let gap = self.sent_at.saturating_duration_since(*ended);
                    let why = crate::cache::explain_miss(
                        last,
                        &self.last_request,
                        &u,
                        self.served.as_ref(),
                        gap,
                        crate::policy::known(&self.api.base_url),
                    );
                    if let Some(mut why) = why {
                        let on = self.served.as_ref().and_then(|s| s.provider.clone());
                        if why.contains("each provider keeps")
                            && let Some(on) = on.filter(|_| crate::policy::known(&self.api.base_url))
                        {
                            why += &format!(
                                " /set provider {{\"order\": [\"{on}\"]}} asks OpenRouter to \
                                 keep to one."
                            );
                        }
                        self.transcript.push(Block::Info(why));
                    }
                }
                if let Some(p) = u.get("prompt_tokens").and_then(Value::as_u64) {
                    let sent = crate::cache::Sent {
                        body: self.last_request.clone(),
                        prompt_tokens: p,
                        served: self.served.clone(),
                    };
                    self.cache_last = Some((sent, Instant::now()));
                }
                self.usage = Some(u);
                self.warn_if_full();
            }
            AgentMsg::Served(s) => {
                self.look_up_window(&s.model);
                self.served = Some(s);
            }
            AgentMsg::Timing(first, total) => self.timing = Some((first, total)),
            AgentMsg::Retrying(error, wait, attempt) => {
                let error = text::clip(error.lines().next().unwrap_or(""), 300);
                self.transcript.push(Block::Info(format!(
                    "{error}\nTrying again in {}s (try {} of {}); Esc stops.",
                    wait.as_secs(),
                    attempt + 1,
                    crate::agent::RETRIES + 1
                )));
                self.busy = Some(format!("waiting {}s to try again", wait.as_secs()));
            }
            AgentMsg::StartedOver => self.transcript.push(Block::Info(
                "The model wrote the reply anew instead of continuing it; only the new one is kept.".into(),
            )),
        }
        // Keep the lines being read in place while scrolled up.
        if let Some(before) = before {
            let after = self.transcript.lines(self.chat_width()).len();
            self.scroll += after.saturating_sub(before);
        }
    }

    /// Runs the input as a command if its first word names one.
    fn send(&mut self) {
        if self.command() {
            return;
        }
        let empty = self.input.text().trim().is_empty();
        let resume = empty && self.session.as_ref().is_some_and(|s| s.unanswered());
        if empty && !resume {
            return;
        }
        if self.session.is_none() {
            self.busy = Some("still working; Esc stops it".into());
            return;
        }
        let prompt = if resume {
            self.input.clear();
            None
        } else {
            let p = self.input.take();
            self.transcript.push(Block::User(p.clone()));
            Some(p)
        };
        self.start(prompt);
    }

    /// Runs a turn with `prompt`, or picks up the unanswered one.
    fn start(&mut self, prompt: Option<String>) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        self.scroll = 0;
        self.busy = Some("sending".into());
        let tx = self.tx.clone();
        let cancel = self.cancel.clone();
        std::thread::spawn(move || {
            let send = |m| {
                let _ = tx.send(Msg::Agent(m));
            };
            let on = |shown: Shown| match shown {
                Shown::Request(b) => send(AgentMsg::Request(b.to_vec())),
                Shown::Stream(Stream::Text(t)) => send(AgentMsg::Text(t.into())),
                Shown::Stream(Stream::Reasoning(t)) => send(AgentMsg::Reasoning(t.into())),
                Shown::Stream(Stream::Call(n)) => send(AgentMsg::Calling(n.into())),
                Shown::Stream(Stream::CallArgs(_)) => {}
                Shown::Usage(u) => send(AgentMsg::Usage(u.clone())),
                Shown::Served(s) => send(AgentMsg::Served(s.clone())),
                Shown::Timing { first, total } => send(AgentMsg::Timing(first, total)),
                Shown::Retrying {
                    error,
                    wait,
                    attempt,
                } => send(AgentMsg::Retrying(error.into(), wait, attempt)),
                Shown::StartedOver => send(AgentMsg::StartedOver),
                Shown::ToolDone {
                    name,
                    args,
                    result,
                    note,
                } => send(AgentMsg::Tool {
                    name: name.into(),
                    args: args.into(),
                    result: result.into(),
                    note: note.map(Into::into),
                }),
            };
            let r = match prompt {
                Some(p) => session.turn(p, &cancel, on),
                None => session.resume(&cancel, on),
            };
            let _ = tx.send(Msg::Done(session, r.err().map(|e| e.to_string())));
        });
    }

    fn chat_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        let width = self.input_width();
        let page = self.page();
        if let Some((_, scroll)) = &mut self.panel {
            match k.code {
                KeyCode::Esc => return self.panel = None,
                KeyCode::PageUp => return *scroll = scroll.saturating_sub(page),
                KeyCode::PageDown => return *scroll += page,
                // Anything else closes it and does what it does.
                _ => self.panel = None,
            }
        }
        match k.code {
            KeyCode::Enter if shift || alt => self.input.insert("\n"),
            KeyCode::Char('j') if ctrl => self.input.insert("\n"),
            KeyCode::Enter => self.send(),
            KeyCode::Tab if self.input.at_end() => {
                let rest = commands::complete(self.input.text());
                self.input.insert(&rest);
            }
            KeyCode::Esc => self.stop(),
            KeyCode::Char('c') if ctrl => {
                if self.session.is_none() {
                    self.stop();
                } else if !self.input.is_empty() {
                    self.input.clear();
                } else {
                    self.quit = true;
                }
            }
            KeyCode::Char('d') if ctrl && self.input.is_empty() && self.session.is_some() => {
                self.quit = true;
            }
            KeyCode::Char('t') if ctrl => {
                self.view = View::Context {
                    raw: false,
                    scroll: 0,
                }
            }
            KeyCode::Char('o') if ctrl => self.transcript.toggle_reasoning(),
            KeyCode::Char('a') if ctrl => self.input.line_start(),
            KeyCode::Char('e') if ctrl => self.input.line_end(),
            KeyCode::Char('u') if ctrl => self.input.kill_before(),
            KeyCode::Char('k') if ctrl => self.input.kill_after(),
            KeyCode::Char('w') if ctrl => self.input.kill_word(),
            KeyCode::Backspace if ctrl || alt => self.input.kill_word(),
            KeyCode::Char(c) if !ctrl => self.input.insert(c.encode_utf8(&mut [0; 4])),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left if ctrl || alt => self.input.word_left(),
            KeyCode::Right if ctrl || alt => self.input.word_right(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.line_start(),
            KeyCode::End => self.input.line_end(),
            KeyCode::Up => {
                if !self.input.vertical(width, false) {
                    self.input.history_prev();
                }
            }
            KeyCode::Down => {
                if !self.input.vertical(width, true) {
                    self.input.history_next();
                }
            }
            KeyCode::PageUp => self.scroll_by(self.page() as isize),
            KeyCode::PageDown => self.scroll_by(-(self.page() as isize)),
            _ => {}
        }
    }

    fn context_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.page() as isize;
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => self.view = View::Chat,
            KeyCode::Char('t') if ctrl => self.view = View::Chat,
            KeyCode::Char('c') if ctrl => self.view = View::Chat,
            KeyCode::Char('r') => {
                if let View::Context { raw, scroll } = &mut self.view {
                    *raw = !*raw;
                    *scroll = 0;
                }
            }
            KeyCode::Up => self.scroll_by(1),
            KeyCode::Down => self.scroll_by(-1),
            KeyCode::PageUp => self.scroll_by(page),
            KeyCode::PageDown => self.scroll_by(-page),
            KeyCode::Home => self.scroll_by(isize::MIN / 2),
            KeyCode::End => self.scroll_by(isize::MAX / 2),
            _ => {}
        }
    }

    fn stop(&mut self) {
        if self.session.is_none() {
            self.cancel.store(true, Ordering::Relaxed);
            self.busy = Some("stopping".into());
        }
    }

    /// Positive scrolls toward older lines. The chat counts from the
    /// bottom, the context view from the top.
    fn scroll_by(&mut self, n: isize) {
        match &mut self.view {
            View::Chat => self.scroll = self.scroll.saturating_add_signed(n),
            View::Context { scroll, .. } => *scroll = scroll.saturating_add_signed(-n),
        }
    }

    fn page(&self) -> usize {
        (self.screen.height() as usize).saturating_sub(4).max(1)
    }

    fn chat_width(&self) -> usize {
        self.screen.width() as usize
    }

    fn input_width(&self) -> usize {
        (self.screen.width() as usize)
            .saturating_sub(text::width(PROMPT))
            .max(1)
    }

    fn draw(&mut self, out: &mut impl Write) -> io::Result<()> {
        self.screen.clear();
        if self.screen.width() >= 4 && self.screen.height() >= 4 {
            match self.view {
                View::Chat => self.draw_chat(),
                View::Context { raw, scroll } => self.draw_context(raw, scroll),
            }
        }
        self.screen.flush(out)
    }

    fn draw_chat(&mut self) {
        let (w, h) = (self.screen.width(), self.screen.height());
        let iw = self.input_width();
        let rows = self.input.rows(iw);
        let (col, row) = self.input.cursor(iw);
        let shown = rows.len().clamp(1, INPUT_MAX_ROWS);
        let top = (row + 1).saturating_sub(shown);
        let status_y = h - 1;
        let input_y = status_y - shown as u16;
        let sep_y = input_y - 1;
        self.input_at = (input_y, top);

        // Transcript, bottom-aligned.
        let height = sep_y as usize;
        let lines = self.transcript.lines(w as usize);
        let max_scroll = lines.len().saturating_sub(height);
        self.scroll = self.scroll.min(max_scroll);
        let end = lines.len() - self.scroll;
        let start = end.saturating_sub(height);
        let pad = height - (end - start);
        for (i, line) in lines[start..end].iter().enumerate() {
            put_line(&mut self.screen, (pad + i) as u16, w, line);
        }

        // What a command shows, over the bottom of the chat.
        if let Some((text, scroll)) = &mut self.panel {
            let lines = text::wrap(text, (w as usize).saturating_sub(2).max(1));
            let room = (sep_y as usize).saturating_sub(1);
            let shown = lines.len().min(room);
            *scroll = (*scroll).min(lines.len() - shown);
            let top = sep_y - shown as u16 - 1;
            let more = if shown < lines.len() {
                " · PageUp PageDown for more"
            } else {
                ""
            };
            let head = format!("── Esc closes{more} ");
            let fill = "─".repeat((w as usize).saturating_sub(text::width(&head)));
            self.screen.put(0, top, w, &(head + &fill), Style::dim());
            for (i, line) in lines[*scroll..*scroll + shown].iter().enumerate() {
                let y = top + 1 + i as u16;
                self.screen
                    .put(0, y, w, &" ".repeat(w as usize), Style::default());
                self.screen.put(2, y, w, line, Style::default());
            }
        }

        // While a command name is typed, the commands it may become.
        let found = commands::completions(self.input.text());
        let room = (sep_y as usize).min(found.len());
        let nw = found.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
        for (i, (name, what)) in found[..room].iter().enumerate() {
            let y = sep_y - room as u16 + i as u16;
            self.screen
                .put(0, y, w, &" ".repeat(w as usize), Style::default());
            let x = self
                .screen
                .put(2, y, w, &format!("{name:<nw$}  "), Style::fg(Color::Cyan));
            self.screen.put(x, y, w, what, Style::dim());
        }

        let sep = if self.scroll > 0 {
            format!("── {} lines below · PageDown or wheel ", self.scroll)
        } else {
            String::new()
        };
        let x = self.screen.put(0, sep_y, w, &sep, Style::dim());
        self.screen
            .put(x, sep_y, w, &"─".repeat(w as usize), Style::dim());

        // Input.
        let prompt_style = Style::fg(Color::Cyan).bold();
        let px = text::width(PROMPT) as u16;
        self.screen.put(0, input_y, w, PROMPT, prompt_style);
        if self.input.is_empty() {
            self.screen.put(px, input_y, w, "Message…", Style::dim());
        }
        for (i, r) in rows.iter().skip(top).take(shown).enumerate() {
            let text = &self.input.text()[r.start..r.end];
            self.screen
                .put(px, input_y + i as u16, w, text, Style::default());
        }
        self.screen
            .set_cursor(Some((px + col as u16, input_y + (row - top) as u16)));

        // Status: who answers on the left, numbers on the right. When they
        // do not fit, the left drops the model asked for, then the right
        // drops the timing, the sum spent, and the usage, in that order.
        let mut lefts = vec![self.model.clone()];
        if let Some(s) = &self.served {
            let on = s
                .provider
                .as_ref()
                .map(|p| format!(" on {p}"))
                .unwrap_or_default();
            lefts = if s.model == self.model {
                vec![format!("{}{on}", s.model), s.model.clone()]
            } else {
                vec![
                    format!("{} → {}{on}", self.model, s.model),
                    format!("{}{on}", s.model),
                    s.model.clone(),
                ]
            };
        }
        if let Some(b) = &self.busy {
            lefts = lefts.into_iter().map(|l| format!("{l} · {b}")).collect();
        }
        let window = self.window.as_ref().and_then(|w| w.1);
        let spent = (self.spent.1 > 1 && self.spent.0 > 0.0).then_some(self.spent.0);
        let usage = self.usage.as_ref();
        let (all, one) = (
            usage_text(usage, window, spent),
            usage_text(usage, window, None),
        );
        let timing = timing_text(self.timing, usage);
        let hint = "ctrl+t context ";
        let rights = [
            format!("{all}{timing}{hint}"),
            format!("{all}{hint}"),
            format!("{one}{hint}"),
            hint.to_string(),
        ];
        let (left, right) = fit(&lefts, &rights, w as usize);
        let rx = w.saturating_sub(text::width(right) as u16);
        self.screen
            .put(0, status_y, rx.saturating_sub(1), left, Style::dim());
        self.screen.put(rx, status_y, w, right, Style::dim());
    }

    fn draw_context(&mut self, raw: bool, scroll: usize) {
        let (w, h) = (self.screen.width(), self.screen.height());
        // Before the first request, what the next one starts with.
        let preview = match &self.session {
            Some(s) if self.last_request.is_empty() => s.body(),
            _ => Vec::new(),
        };
        let lines = if preview.is_empty() {
            context::lines(&self.last_request, raw, w as usize, true)
        } else {
            context::lines(&preview, raw, w as usize, false)
        };
        let height = h as usize - 1;
        let scroll = scroll.min(lines.len().saturating_sub(height));
        if let View::Context { scroll: s, .. } = &mut self.view {
            *s = scroll;
        }
        for (i, line) in lines.iter().skip(scroll).take(height).enumerate() {
            put_line(&mut self.screen, i as u16, w, line);
        }
        let status = format!(
            "context · lines {}-{} of {} · r raw/parts · ↑↓ PgUp PgDn · Esc back",
            scroll + 1,
            (scroll + height).min(lines.len()),
            lines.len()
        );
        self.screen.put(0, h - 1, w, &status, Style::dim());
    }
}

/// Runs `$VISUAL` or `$EDITOR` on a file and waits for it.
fn editor(path: &std::path::Path) -> std::result::Result<(), String> {
    let cmd = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .ok()
        .filter(|c| !c.trim().is_empty())
        .unwrap_or_else(|| if cfg!(windows) { "notepad" } else { "vi" }.into());
    let mut words = cmd.split_whitespace();
    let program = words.next().expect("not empty");
    let status = std::process::Command::new(program)
        .args(words)
        .arg(path)
        .status()
        .map_err(|e| format!("could not run the editor {cmd}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "the editor {cmd} ended with {status}; the context is unchanged"
        ))
    }
}

/// Shows a saved conversation as it went.
fn replay(items: &[Item], t: &mut Transcript) {
    let results: std::collections::HashMap<&str, &str> = items
        .iter()
        .filter_map(|i| match i {
            Item::ToolResult { call_id, content } => Some((call_id.as_str(), content.as_str())),
            _ => None,
        })
        .collect();
    for item in items {
        match item {
            Item::User(text) => t.push(Block::User(text.clone())),
            Item::Assistant(r) => {
                if !r.reasoning.trim().is_empty() {
                    t.push(Block::Reasoning(r.reasoning.clone()));
                }
                if !r.text.trim().is_empty() {
                    t.push(Block::Text(r.text.clone()));
                }
                for c in &r.calls {
                    t.push(Block::Tool {
                        name: c.name.clone(),
                        args: c.args.clone(),
                        result: results.get(c.id.as_str()).unwrap_or(&"").to_string(),
                    });
                }
            }
            Item::System(_) | Item::ToolResult { .. } => {}
        }
    }
}

fn put_line(screen: &mut Screen, y: u16, w: u16, line: &transcript::Line) {
    let mut x = 0;
    for (text, style) in &line.spans {
        x = screen.put(x, y, w, text, *style);
    }
}

/// The first pair, richest right first and then richest left, that fits
/// in `width` with a space between; else the shortest of each, the left
/// cut by the screen.
fn fit<'a>(lefts: &'a [String], rights: &'a [String], width: usize) -> (&'a str, &'a str) {
    for r in rights {
        for l in lefts {
            if text::width(l) + 1 + text::width(r) <= width {
                return (l, r);
            }
        }
    }
    let last = |v: &'a [String]| v.last().map_or("", String::as_str);
    (last(lefts), last(rights))
}

/// Tokens and cost from the provider's usage report.
/// `in 1200/128k (1000 cached) · out 30 · $0.0001 ($0.0040 in all)`; the
/// window is the model's context window, when the provider says, and the
/// sum is what the requests of this run cost together.
fn usage_text(u: Option<&Value>, window: Option<u64>, spent: Option<f64>) -> String {
    let Some(u) = u else { return String::new() };
    let n = |k: &str| u.get(k).and_then(Value::as_u64);
    let mut parts = Vec::new();
    if let Some(p) = n("prompt_tokens") {
        let cached = u
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64);
        let of = window
            .map(|w| format!("/{}", crate::models::short(w)))
            .unwrap_or_default();
        match cached {
            Some(c) if c > 0 => parts.push(format!("in {p}{of} ({c} cached)")),
            _ => parts.push(format!("in {p}{of}")),
        }
    }
    if let Some(c) = n("completion_tokens") {
        parts.push(format!("out {c}"));
    }
    if let Some(cost) = u.get("cost").and_then(Value::as_f64) {
        match spent {
            Some(all) => parts.push(format!("${cost:.4} (${all:.4} in all)")),
            None => parts.push(format!("${cost:.4}")),
        }
    }
    if parts.is_empty() {
        return String::new();
    }
    parts.join(" · ") + " · "
}

/// `first 0.4s · 2.1s · 52 tok/s`: time to the first token, in all, and
/// the output speed after the first token.
fn timing_text(t: Option<(Option<Duration>, Duration)>, usage: Option<&Value>) -> String {
    let Some((first, total)) = t else {
        return String::new();
    };
    let mut parts = Vec::new();
    if let Some(f) = first {
        parts.push(format!("first {:.1}s", f.as_secs_f64()));
    }
    parts.push(format!("{:.1}s", total.as_secs_f64()));
    let out = usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(Value::as_u64);
    if let (Some(f), Some(n)) = (first, out) {
        let secs = (total - f).as_secs_f64();
        if secs >= 0.1 && n > 1 {
            parts.push(format!("{:.0} tok/s", (n - 1) as f64 / secs));
        }
    }
    parts.join(" · ") + " · "
}

/// Puts the terminal into full-screen raw mode and restores it on drop,
/// also when the program panics.
struct TermGuard {
    keyboard: bool,
}

impl TermGuard {
    fn enter() -> io::Result<Self> {
        let keyboard = terminal::supports_keyboard_enhancement().unwrap_or(false);
        setup(keyboard)?;
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore(keyboard);
            hook(info);
        }));
        Ok(Self { keyboard })
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        restore(self.keyboard);
    }
}

fn setup(keyboard: bool) -> io::Result<()> {
    terminal::enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(
        out,
        terminal::EnterAlternateScreen,
        event::EnableBracketedPaste,
        event::EnableMouseCapture
    )?;
    if keyboard {
        execute!(
            out,
            event::PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        )?;
    }
    Ok(())
}

fn restore(keyboard: bool) {
    let mut out = io::stdout();
    if keyboard {
        let _ = execute!(out, event::PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        out,
        event::DisableMouseCapture,
        event::DisableBracketedPaste,
        terminal::LeaveAlternateScreen,
        crossterm::cursor::Show
    );
    let _ = terminal::disable_raw_mode();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_line() {
        let u = json!({"prompt_tokens": 1200, "completion_tokens": 30, "cost": 0.00012,
                       "prompt_tokens_details": {"cached_tokens": 1000}});
        assert_eq!(
            usage_text(Some(&u), Some(131_072), None),
            "in 1200/131k (1000 cached) · out 30 · $0.0001 · "
        );
        assert_eq!(
            usage_text(Some(&u), None, Some(0.004)),
            "in 1200 (1000 cached) · out 30 · $0.0001 ($0.0040 in all) · "
        );
        assert_eq!(usage_text(None, None, None), "");
        let shorts: Vec<String> = [950, 8192, 131_072, 1_000_000, 1_500_000]
            .into_iter()
            .map(crate::models::short)
            .collect();
        assert_eq!(shorts, ["950", "8.2k", "131k", "1M", "1.5M"]);
    }

    #[test]
    fn status_fits_the_most_it_can() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let lefts = s(&["free → m on P", "m on P", "m"]);
        let rights = s(&["in 1 · first 1s · hint", "in 1 · hint", "hint"]);
        assert_eq!(
            fit(&lefts, &rights, 40),
            ("free → m on P", "in 1 · first 1s · hint")
        );
        assert_eq!(
            fit(&lefts, &rights, 30),
            ("m on P", "in 1 · first 1s · hint")
        );
        assert_eq!(fit(&lefts, &rights, 20), ("m on P", "in 1 · hint"));
        assert_eq!(fit(&lefts, &rights, 8), ("m", "hint"));
        assert_eq!(fit(&lefts, &rights, 3), ("m", "hint"));
    }

    #[test]
    fn timing_line() {
        let t = Some((
            Some(Duration::from_millis(400)),
            Duration::from_millis(2400),
        ));
        let u = json!({"completion_tokens": 101});
        assert_eq!(timing_text(t, Some(&u)), "first 0.4s · 2.4s · 50 tok/s · ");
        let none = Some((None, Duration::from_millis(1000)));
        assert_eq!(timing_text(none, None), "1.0s · ");
    }
}
