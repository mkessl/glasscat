//! The full-screen terminal interface.

mod context;
mod input;
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
use crate::openai::Event as Stream;
use input::Input;
use screen::{Screen, Style};
use transcript::{Block, Transcript};

/// At most one frame per this long; the first change after a pause is
/// drawn at once.
const FRAME: Duration = Duration::from_millis(16);
const INPUT_MAX_ROWS: usize = 8;
const WHEEL_LINES: usize = 3;
const PROMPT: &str = "› ";

enum Msg {
    Term(Event),
    Agent(AgentMsg),
    Done(Box<Session>, Option<String>),
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
    },
    Usage(Value),
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
    busy: Option<String>,
    usage: Option<Value>,
    cancel: Arc<AtomicBool>,
    tx: Sender<Msg>,
    /// Where the input was drawn: first screen row and first text row shown.
    input_at: (u16, usize),
    quit: bool,
}

pub fn run(session: Session) -> Result<()> {
    // Before the input thread: the terminal queries here read input too.
    let _term = TermGuard::enter()?;
    let (w, h) = terminal::size()?;
    let (tx, rx) = mpsc::channel();
    let input_tx = tx.clone();
    std::thread::spawn(move || {
        while let Ok(e) = event::read() {
            if input_tx.send(Msg::Term(e)).is_err() {
                break;
            }
        }
    });
    let mut app = App {
        model: session.model.clone(),
        session: Some(Box::new(session)),
        transcript: Transcript::default(),
        input: Input::default(),
        screen: Screen::new(w, h),
        view: View::Chat,
        scroll: 0,
        last_request: Vec::new(),
        busy: None,
        usage: None,
        cancel: Arc::new(AtomicBool::new(false)),
        tx,
        input_at: (0, 0),
        quit: false,
    };
    app.transcript.push(Block::Info(format!(
        "{} · {} · Enter sends, Shift+Enter adds a line, Ctrl+T shows the exact context, Ctrl+C quits",
        crate::NAME,
        app.model
    )));
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
                MouseEventKind::ScrollUp => self.scroll_by(WHEEL_LINES as isize),
                MouseEventKind::ScrollDown => self.scroll_by(-(WHEEL_LINES as isize)),
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
            Msg::Done(session, err) => {
                self.session = Some(session);
                self.busy = None;
                if self.cancel.swap(false, Ordering::Relaxed) {
                    self.transcript.push(Block::Info("cancelled".into()));
                }
                if let Some(e) = err {
                    self.transcript.push(Block::Error(e));
                }
            }
        }
    }

    fn agent(&mut self, a: AgentMsg) {
        let before = (self.scroll > 0).then(|| self.transcript.lines(self.chat_width()).len());
        match a {
            AgentMsg::Request(body) => {
                self.last_request = body;
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
            AgentMsg::Tool { name, args, result } => {
                self.transcript.push(Block::Tool { name, args, result });
            }
            AgentMsg::Usage(u) => self.usage = Some(u),
        }
        // Keep the lines being read in place while scrolled up.
        if let Some(before) = before {
            let after = self.transcript.lines(self.chat_width()).len();
            self.scroll += after.saturating_sub(before);
        }
    }

    fn send(&mut self) {
        if self.input.text().trim().is_empty() {
            return;
        }
        let Some(mut session) = self.session.take() else {
            self.busy = Some("still working; Esc stops it".into());
            return;
        };
        let prompt = self.input.take();
        self.transcript.push(Block::User(prompt.clone()));
        self.scroll = 0;
        self.busy = Some("sending".into());
        let tx = self.tx.clone();
        let cancel = self.cancel.clone();
        std::thread::spawn(move || {
            let send = |m| {
                let _ = tx.send(Msg::Agent(m));
            };
            let r = session.turn(prompt, &cancel, |shown| match shown {
                Shown::Request(b) => send(AgentMsg::Request(b.to_vec())),
                Shown::Stream(Stream::Text(t)) => send(AgentMsg::Text(t.into())),
                Shown::Stream(Stream::Reasoning(t)) => send(AgentMsg::Reasoning(t.into())),
                Shown::Stream(Stream::Call(n)) => send(AgentMsg::Calling(n.into())),
                Shown::Stream(Stream::CallArgs(_)) => {}
                Shown::Usage(u) => send(AgentMsg::Usage(u.clone())),
                Shown::ToolDone { name, args, result } => send(AgentMsg::Tool {
                    name: name.into(),
                    args: args.into(),
                    result: result.into(),
                }),
            });
            let _ = tx.send(Msg::Done(session, r.err().map(|e| e.to_string())));
        });
    }

    fn chat_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        let width = self.input_width();
        match k.code {
            KeyCode::Enter if shift || alt => self.input.insert("\n"),
            KeyCode::Char('j') if ctrl => self.input.insert("\n"),
            KeyCode::Enter => self.send(),
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
            self.screen
                .put(0, (pad + i) as u16, w, &line.text, line.style);
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

        // Status.
        let left = match &self.busy {
            Some(b) => format!("{} · {b}", self.model),
            None => self.model.clone(),
        };
        let right = format!("{}ctrl+t context ", usage_text(self.usage.as_ref()));
        let rx = w.saturating_sub(text::width(&right) as u16);
        self.screen
            .put(0, status_y, rx.saturating_sub(1), &left, Style::dim());
        self.screen.put(rx, status_y, w, &right, Style::dim());
    }

    fn draw_context(&mut self, raw: bool, scroll: usize) {
        let (w, h) = (self.screen.width(), self.screen.height());
        let lines = context::lines(&self.last_request, raw, w as usize);
        let height = h as usize - 1;
        let scroll = scroll.min(lines.len().saturating_sub(height));
        if let View::Context { scroll: s, .. } = &mut self.view {
            *s = scroll;
        }
        for (i, line) in lines.iter().skip(scroll).take(height).enumerate() {
            self.screen.put(0, i as u16, w, &line.text, line.style);
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

/// Tokens and cost from the provider's usage report.
fn usage_text(u: Option<&Value>) -> String {
    let Some(u) = u else { return String::new() };
    let n = |k: &str| u.get(k).and_then(Value::as_u64);
    let mut parts = Vec::new();
    if let Some(p) = n("prompt_tokens") {
        let cached = u
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64);
        match cached {
            Some(c) if c > 0 => parts.push(format!("in {p} ({c} cached)")),
            _ => parts.push(format!("in {p}")),
        }
    }
    if let Some(c) = n("completion_tokens") {
        parts.push(format!("out {c}"));
    }
    if let Some(cost) = u.get("cost").and_then(Value::as_f64) {
        parts.push(format!("${cost:.4}"));
    }
    if parts.is_empty() {
        return String::new();
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
            usage_text(Some(&u)),
            "in 1200 (1000 cached) · out 30 · $0.0001 · "
        );
        assert_eq!(usage_text(None), "");
    }
}
