//! The conversation as shown: blocks laid out into styled screen lines,
//! cached per block so a streaming reply only re-wraps its own block.

use crossterm::style::Color;
use serde_json::Value;

use super::screen::Style;
use super::text::{clip, wrap};

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

pub struct Line {
    pub text: String,
    pub style: Style,
}

const USER: Style = Style::fg(Color::Cyan);
const CODE: Style = Style::fg(Color::Green);
const TOOL: Style = Style::fg(Color::Yellow);
const ERROR: Style = Style::fg(Color::Red);

#[derive(Default)]
pub struct Transcript {
    blocks: Vec<Block>,
    cache: Vec<Option<Vec<Line>>>,
    width: usize,
    /// Reasoning shown in full rather than folded.
    reasoning_open: bool,
}

/// Lines of reasoning shown while the model is still thinking.
const THINKING_TAIL: usize = 3;

impl Transcript {
    pub fn push(&mut self, b: Block) {
        // The block before was laid out as the last one.
        if let Some(c) = self.cache.last_mut() {
            *c = None;
        }
        self.blocks.push(b);
        self.cache.push(None);
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
                *c = Some(layout(b, width, open, i + 1 == n));
            }
        }
        let mut out = Vec::new();
        for (i, c) in self.cache.iter().enumerate() {
            if i > 0 {
                out.push(&BLANK);
            }
            out.extend(c.as_ref().expect("laid out above"));
        }
        out
    }
}

static BLANK: Line = Line {
    text: String::new(),
    style: Style::dim(),
};

fn layout(b: &Block, width: usize, reasoning_open: bool, last: bool) -> Vec<Line> {
    let mut out = Vec::new();
    let mut add = |text: String, style: Style| out.push(Line { text, style });
    match b {
        Block::User(t) => {
            for (i, l) in wrap(t, width.saturating_sub(2)).into_iter().enumerate() {
                add(format!("{} {l}", if i == 0 { '›' } else { ' ' }), USER);
            }
        }
        Block::Text(t) => {
            let mut code = false;
            for line in t.trim_end().split('\n') {
                let fence = line.trim_start().starts_with("```");
                let style = if fence {
                    Style::dim()
                } else if code {
                    CODE
                } else {
                    Style::default()
                };
                for l in wrap(line, width) {
                    add(l, style);
                }
                code ^= fence;
            }
        }
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
            add(clip(&format!("▸ {name} {}", summary(args)), width), TOOL);
            let failed = result.starts_with("error:");
            let mut lines = result.lines();
            let first = lines.next().unwrap_or("");
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
fn summary(args: &str) -> String {
    let Ok(Value::Object(m)) = serde_json::from_str::<Value>(args) else {
        return args.replace('\n', " ");
    };
    let get = |k: &str| m.get(k).and_then(Value::as_str);
    match (get("pattern"), get("path")) {
        (Some(p), Some(path)) => format!("{p} in {path}"),
        (Some(p), None) => p.to_string(),
        (None, Some(path)) => path.to_string(),
        (None, None) => Value::Object(m.clone()).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(t: &mut Transcript, w: usize) -> Vec<String> {
        t.lines(w).iter().map(|l| l.text.clone()).collect()
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
    fn tool_block_summarises() {
        let mut t = Transcript::default();
        t.push(Block::Tool {
            name: "read".into(),
            args: r#"{"path":"src/main.rs"}"#.into(),
            result: "a\nb\nc".into(),
        });
        assert_eq!(texts(&mut t, 40), ["▸ read src/main.rs", "  a  (+2 lines)"]);
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

    #[test]
    fn code_fences_are_styled() {
        let mut t = Transcript::default();
        t.stream("x\n```\ncode\n```\ny", false);
        let styles: Vec<Style> = t.lines(20).iter().map(|l| l.style).collect();
        assert_eq!(
            styles,
            [
                Style::default(),
                Style::dim(),
                CODE,
                Style::dim(),
                Style::default()
            ]
        );
    }
}
