//! The conversation as shown: blocks laid out into styled screen lines,
//! cached per block so a streaming reply only re-wraps its own block.

use crossterm::style::Color;
use serde_json::Value;

use super::markdown;
use super::screen::Style;
use super::text::{Span, clip, wrap};

pub enum Block {
    User(String),
    Text(String),
    Reasoning(String),
    Tool {
        name: String,
        args: String,
        result: String,
    },
    Error(String),
    Info(String),
}

/// One screen line: runs of styled text.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Line {
    pub spans: Vec<Span>,
}

impl Line {
    pub fn new(text: String, style: Style) -> Self {
        Self {
            spans: vec![(text, style)],
        }
    }

    #[cfg(test)]
    pub fn text(&self) -> String {
        self.spans.iter().map(|(t, _)| t.as_str()).collect()
    }
}

const USER: Style = Style::fg(Color::Cyan);
const TOOL: Style = Style::fg(Color::Yellow);
const ERROR: Style = Style::fg(Color::Red);

#[derive(Default)]
pub struct Transcript {
    blocks: Vec<Block>,
    cache: Vec<Option<Vec<Line>>>,
    width: usize,
    /// Reasoning shown in full rather than folded.
    reasoning_open: bool,
    /// The working directory, left out of paths in tool calls.
    cwd: String,
}

/// Lines of reasoning shown while the model is still thinking.
const THINKING_TAIL: usize = 3;

impl Transcript {
    pub fn new(cwd: String) -> Self {
        Self {
            cwd,
            ..Self::default()
        }
    }

    pub fn push(&mut self, b: Block) {
        // The block before was laid out as the last one.
        if let Some(c) = self.cache.last_mut() {
            *c = None;
        }
        self.blocks.push(b);
        self.cache.push(None);
    }

    pub fn clear(&mut self) {
        self.blocks.clear();
        self.cache.clear();
    }

    pub fn toggle_reasoning(&mut self) {
        self.reasoning_open ^= true;
        self.cache.iter_mut().for_each(|c| *c = None);
    }

    /// Appends streamed text to the last block if it is of the same kind.
    pub fn stream(&mut self, text: &str, reasoning: bool) {
        match self.blocks.last_mut() {
            Some(Block::Text(t)) if !reasoning => t.push_str(text),
            Some(Block::Reasoning(t)) if reasoning => t.push_str(text),
            _ => {
                let s = text.trim_start_matches('\n').to_string();
                self.push(if reasoning {
                    Block::Reasoning(s)
                } else {
                    Block::Text(s)
                });
                return;
            }
        }
        if let Some(c) = self.cache.last_mut() {
            *c = None;
        }
    }

    /// All lines at this width, one blank line between blocks.
    pub fn lines(&mut self, width: usize) -> Vec<&Line> {
        if width != self.width {
            self.cache.iter_mut().for_each(|c| *c = None);
            self.width = width;
        }
        let n = self.blocks.len();
        for (i, (b, c)) in self.blocks.iter().zip(self.cache.iter_mut()).enumerate() {
            if c.is_none() {
                let open = self.reasoning_open;
                *c = Some(layout(b, width, open, i + 1 == n, &self.cwd));
            }
        }
        let mut out = Vec::new();
        for c in &self.cache {
            let c = c.as_ref().expect("laid out above");
            if c.is_empty() {
                continue;
            }
            if !out.is_empty() {
                out.push(&BLANK);
            }
            out.extend(c);
        }
        out
    }
}

static BLANK: Line = Line { spans: Vec::new() };

fn layout(b: &Block, width: usize, reasoning_open: bool, last: bool, cwd: &str) -> Vec<Line> {
    let mut out = Vec::new();
    let mut add = |text: String, style: Style| out.push(Line::new(text, style));
    match b {
        Block::User(t) => {
            for (i, l) in wrap(t, width.saturating_sub(2)).into_iter().enumerate() {
                add(format!("{} {l}", if i == 0 { '›' } else { ' ' }), USER);
            }
        }
        // A block that so far holds only whitespace takes no room.
        Block::Text(t) | Block::Reasoning(t) if t.trim().is_empty() => {}
        Block::Text(t) => return markdown::lines(t, width),
        Block::Reasoning(t) => {
            let lines = wrap(t.trim(), width);
            let skip = if reasoning_open {
                0
            } else {
                let (verb, keep) = if last {
                    ("thinking", THINKING_TAIL)
                } else {
                    ("thought", 0)
                };
                let n = lines.len();
                let s = if n == 1 { "" } else { "s" };
                add(
                    clip(&format!("{verb} · {n} line{s} · ctrl+o shows"), width),
                    Style::dim(),
                );
                n.saturating_sub(keep)
            };
            for l in lines.into_iter().skip(skip) {
                add(l, Style::dim().italic());
            }
        }
        Block::Tool { name, args, result } => {
            add(
                clip(&format!("▸ {name} {}", summary(args, cwd)), width),
                TOOL,
            );
            let failed = result.starts_with("error:");
            let mut lines = result.lines();
            let first = lines.next().unwrap_or("").trim();
            let more = lines.count();
            let tail = if more > 0 {
                format!("  (+{more} lines)")
            } else {
                String::new()
            };
            let style = if failed { ERROR } else { Style::dim() };
            add(clip(&format!("  {first}{tail}"), width), style);
        }
        Block::Error(t) => {
            for l in wrap(t, width) {
                add(l, ERROR);
            }
        }
        Block::Info(t) => {
            for l in wrap(t, width) {
                add(l, Style::dim());
            }
        }
    }
    out
}

/// A tool call's arguments in a few words: the path or pattern when there
/// is one, otherwise the JSON on one line.
fn summary(args: &str, cwd: &str) -> String {
    let Ok(Value::Object(m)) = serde_json::from_str::<Value>(args) else {
        return args.replace('\n', " ");
    };
    let get = |k: &str| m.get(k).and_then(Value::as_str);
    let rel = |p: &str| -> String {
        if cwd.is_empty() || p == cwd {
            return if p == cwd { ".".into() } else { p.into() };
        }
        p.strip_prefix(cwd)
            .and_then(|r| r.strip_prefix(['/', '\\']))
            .unwrap_or(p)
            .into()
    };
    match (get("pattern"), get("path")) {
        (Some(p), Some(path)) => format!("{p} in {}", rel(path)),
        (Some(p), None) => p.to_string(),
        (None, Some(path)) => rel(path),
        (None, None) => Value::Object(m.clone()).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(t: &mut Transcript, w: usize) -> Vec<String> {
        t.lines(w).iter().map(|l| l.text()).collect()
    }

    #[test]
    fn streams_into_one_block_and_rewraps() {
        let mut t = Transcript::default();
        t.push(Block::User("hi".into()));
        t.stream("hello ", false);
        t.stream("world", false);
        assert_eq!(texts(&mut t, 20), ["› hi", "", "hello world"]);
        assert_eq!(texts(&mut t, 6), ["› hi", "", "hello", "world"]);
    }

    #[test]
    fn empty_blocks_take_no_room() {
        let mut t = Transcript::default();
        t.push(Block::User("hi".into()));
        t.stream("\n\n", true);
        t.stream("  ", false);
        t.push(Block::Info("x".into()));
        assert_eq!(texts(&mut t, 20), ["› hi", "", "x"]);
    }

    #[test]
    fn tool_block_summarises() {
        let mut t = Transcript::default();
        t.push(Block::Tool {
            name: "read".into(),
            args: r#"{"path":"src/main.rs"}"#.into(),
            result: "a\nb\nc".into(),
        });
        assert_eq!(texts(&mut t, 40), ["▸ read src/main.rs", "  a  (+2 lines)"]);
        t.cwd = "/p".into();
        t.push(Block::Tool {
            name: "find".into(),
            args: r#"{"pattern":"*.rs","path":"/p/src"}"#.into(),
            result: String::new(),
        });
        assert_eq!(texts(&mut t, 40)[3], "▸ find *.rs in src");
    }

    #[test]
    fn reasoning_folds_once_the_model_moves_on() {
        let mut t = Transcript::default();
        t.stream("a\nb\nc\nd", true);
        assert_eq!(
            texts(&mut t, 40),
            ["thinking · 4 lines · ctrl+o shows", "b", "c", "d"]
        );
        t.stream("answer", false);
        assert_eq!(
            texts(&mut t, 40),
            ["thought · 4 lines · ctrl+o shows", "", "answer"]
        );
        t.toggle_reasoning();
        assert_eq!(texts(&mut t, 40), ["a", "b", "c", "d", "", "answer"]);
    }
}
