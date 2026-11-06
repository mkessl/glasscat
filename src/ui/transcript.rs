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
        /// A file's text before a write overwrote it.
        before: Option<String>,
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
const ADDED: Style = Style::fg(Color::Green);

/// Lines of an edit's change shown under the call.
const DIFF_LINES: usize = 12;

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
/// Lines of a long message of the user's shown while it is folded.
const USER_LINES: usize = 8;

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

    pub fn has_user_message(&self) -> bool {
        self.blocks.iter().any(|b| matches!(b, Block::User(_)))
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
            let mut lines = wrap(t, width.saturating_sub(2));
            // A long paste is folded like reasoning; all of it was sent.
            let more = lines.len().saturating_sub(USER_LINES);
            if !reasoning_open && more > 1 {
                lines.truncate(USER_LINES);
            }
            for (i, l) in lines.into_iter().enumerate() {
                add(format!("{} {l}", if i == 0 { '›' } else { ' ' }), USER);
            }
            if !reasoning_open && more > 1 {
                add(
                    clip(&format!("  … {more} more lines · ctrl+o shows"), width),
                    Style::dim(),
                );
            }
        }
        // A block that so far holds only whitespace takes no room.
        Block::Text(t) | Block::Reasoning(t) if t.trim().is_empty() => {}
        Block::Text(t) => return markdown::lines(t, width),
        Block::Reasoning(t) => {
            let (verb, keep) = if last {
                ("thinking", THINKING_TAIL)
            } else {
                ("thought", 0)
            };
            // A short thought fits beside its label.
            let label = format!("{verb} · ");
            let t = t.trim();
            if !reasoning_open
                && !t.contains('\n')
                && super::text::width(&label) + super::text::width(t) <= width
            {
                out.push(Line {
                    spans: vec![
                        (label, Style::dim()),
                        (t.to_string(), Style::dim().italic()),
                    ],
                });
                return out;
            }
            let lines = wrap(t, width);
            let skip = if reasoning_open {
                0
            } else {
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
        Block::Tool {
            name,
            args,
            result,
            before,
        } => {
            add(
                clip(&format!("▸ {name} {}", summary(args, cwd)), width),
                TOOL,
            );
            let failed = result.starts_with("error:");
            let style = if failed { ERROR } else { Style::dim() };
            add(
                clip(&format!("  {}", outcome(name, result, failed)), width),
                style,
            );
            if !failed {
                for (text, style) in change(name, args, result, before.as_deref()) {
                    add(clip(&text, width), style);
                }
            }
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

/// A tool's result in one line: how much a read gave, otherwise the first
/// line and how many follow.
pub fn outcome(name: &str, result: &str, failed: bool) -> String {
    let lines: Vec<&str> = result.lines().collect();
    if name == "read" && !failed && lines.len() > 1 {
        // "[lines 1-2000 of 5000; continue with offset 2001]"
        return match lines.last().and_then(|l| l.strip_prefix('[')) {
            Some(part) if part.starts_with("lines ") => {
                part.split(';').next().unwrap_or(part).to_string()
            }
            _ => format!("{} lines", lines.len()),
        };
    }
    if !failed && lines.len() > 1 {
        // A note such as "[3 more entries not shown]" ends a long answer.
        let (items, more) = match lines.split_last() {
            Some((last, items)) if last.starts_with('[') => (items, true),
            _ => (&lines[..], false),
        };
        let plus = if more { "+" } else { "" };
        let n = items.len();
        match name {
            "list" => return format!("{n}{plus} entries: {}", items.join(", ")),
            "find" => return format!("{n}{plus} files: {}", items.join(", ")),
            "search" => {
                let found: Vec<&str> = items.iter().filter_map(|l| match_file(l)).collect();
                let n = found.len();
                let mut files = found;
                files.dedup();
                let f = files.len();
                let fs = if f == 1 { "" } else { "s" };
                let plain = if lines[0].starts_with("[not a regular expression") {
                    ", as plain text"
                } else {
                    ""
                };
                let es = if n == 1 && !more { "" } else { "es" };
                return format!("{n}{plus} match{es} in {f} file{fs}{plain}");
            }
            _ => {}
        }
    }
    let first = lines.first().copied().unwrap_or("").trim();
    match lines.len().saturating_sub(1) {
        0 => first.to_string(),
        1 => format!("{first}  (+1 line)"),
        n => format!("{first}  (+{n} lines)"),
    }
}

/// The path of a search result line, "path:line: text", where the path
/// may hold a colon too.
fn match_file(l: &str) -> Option<&str> {
    l.match_indices(':').map(|(i, _)| &l[..i]).find(|p| {
        let rest = &l[p.len() + 1..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        digits > 0 && rest[digits..].starts_with(": ")
    })
}

/// The lines an edit or a write took out and put in, as `- ` and `+ `
/// lines, cut short after a screenful.
pub fn change(name: &str, args: &str, result: &str, before: Option<&str>) -> Vec<(String, Style)> {
    let Ok(v) = serde_json::from_str::<Value>(args) else {
        return Vec::new();
    };
    let text = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    let (old, new) = match name {
        "edit" => (text("old"), text("new")),
        "write" if result.starts_with("created") => (String::new(), text("content")),
        "write" => match before {
            Some(b) => (b.to_string(), text("content")),
            None => return Vec::new(),
        },
        _ => return Vec::new(),
    };
    let (mut old, mut new): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    // Lines the model repeated for context did not change; leave them out.
    let same = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    old.drain(..same);
    new.drain(..same);
    let same = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    old.truncate(old.len() - same);
    new.truncate(new.len() - same);
    let mut lines: Vec<(String, Style)> = old
        .iter()
        .map(|l| (format!("  - {l}"), ERROR))
        .chain(new.iter().map(|l| (format!("  + {l}"), ADDED)))
        .collect();
    if lines.len() > DIFF_LINES {
        let more = lines.len() - (DIFF_LINES - 1);
        lines.truncate(DIFF_LINES - 1);
        lines.push((format!("  … {more} more lines"), Style::dim()));
    }
    lines
}

/// A tool call's arguments in a few words: the path or pattern when there
/// is one, otherwise the JSON on one line.
pub fn summary(args: &str, cwd: &str) -> String {
    let Ok(Value::Object(m)) = serde_json::from_str::<Value>(args) else {
        return args.replace('\n', " ");
    };
    let get = |k: &str| m.get(k).and_then(Value::as_str);
    let rel = |p: &str| -> String {
        if p.is_empty() || p == "." || p == "./" || p == cwd {
            return ".".into();
        }
        let p = (!cwd.is_empty())
            .then(|| p.strip_prefix(cwd))
            .flatten()
            .and_then(|r| r.strip_prefix(['/', '\\']))
            .unwrap_or(p);
        match p.strip_prefix("./").unwrap_or(p) {
            "" => ".".into(),
            p => p.into(),
        }
    };
    match (get("pattern"), get("path")) {
        (Some(p), Some(path)) if rel(path) != "." => format!("{p} in {}", rel(path)),
        (Some(p), _) => p.to_string(),
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
            before: None,
        });
        assert_eq!(texts(&mut t, 40), ["▸ read src/main.rs", "  3 lines"]);
        assert_eq!(summary(r#"{"pattern":"x","path":""}"#, "/p"), "x");
        assert_eq!(
            summary(r#"{"pattern":"x","path":"./src"}"#, "/p"),
            "x in src"
        );
        assert_eq!(summary(r#"{"path":"/p/src/a.rs"}"#, "/p"), "src/a.rs");
        assert_eq!(summary(r#"{"path":"/p"}"#, ""), "/p");
        assert_eq!(summary(r#"{"pattern":"x","path":"/p/"}"#, "/p"), "x");
        assert_eq!(
            outcome(
                "read",
                "a\nb\n[lines 1-2 of 9; continue with offset 3]",
                false
            ),
            "lines 1-2 of 9"
        );
        assert_eq!(outcome("read", "x is empty", false), "x is empty");
        assert_eq!(
            outcome("list", "src/\nREADME.md", false),
            "2 entries: src/, README.md"
        );
        assert_eq!(
            outcome("find", "a.rs\nb.rs\n[3 more files not shown]", false),
            "2+ files: a.rs, b.rs"
        );
        assert_eq!(
            outcome("search", "C:\\a.rs:1: x\nC:\\a.rs:4: y\nb.rs:2: z\n", false),
            "3 matches in 2 files"
        );
        assert_eq!(
            outcome(
                "search",
                "[not a regular expression (x), so searched as plain text]\na.rs:1: f(\n",
                false
            ),
            "1 match in 1 file, as plain text"
        );
        t.cwd = "/p".into();
        t.push(Block::Tool {
            name: "find".into(),
            args: r#"{"pattern":"*.rs","path":"/p/src"}"#.into(),
            result: String::new(),
            before: None,
        });
        assert_eq!(texts(&mut t, 40)[3], "▸ find *.rs in src");
        t.push(Block::Tool {
            name: "search".into(),
            args: r#"{"pattern":"fn","path":"."}"#.into(),
            result: String::new(),
            before: None,
        });
        assert_eq!(texts(&mut t, 40)[6], "▸ search fn");
    }

    #[test]
    fn edit_shows_its_change() {
        let mut t = Transcript::default();
        t.push(Block::Tool {
            name: "edit".into(),
            args: r#"{"path":"a.rs","old":"fn f() {\nlet x = 1;\nfoo();\n}","new":"fn f() {\nlet x = 2;\n}"}"#.into(),
            result: "edited a.rs at line 3".into(),
            before: None,
        });
        assert_eq!(
            texts(&mut t, 40),
            [
                "▸ edit a.rs",
                "  edited a.rs at line 3",
                "  - let x = 1;",
                "  - foo();",
                "  + let x = 2;"
            ]
        );
        let long = "x\n".repeat(20);
        t.push(Block::Tool {
            name: "edit".into(),
            args: serde_json::json!({"path": "b", "old": long, "new": "y"}).to_string(),
            result: "edited b at line 1".into(),
            before: None,
        });
        let text = texts(&mut t, 40);
        assert_eq!(text.len(), 5 + 1 + 2 + DIFF_LINES);
        assert_eq!(text.last().unwrap(), "  … 10 more lines");
        t.push(Block::Tool {
            name: "edit".into(),
            args: r#"{"path":"c","old":"a","new":"b"}"#.into(),
            result: "error: old text not found".into(),
            before: None,
        });
        assert_eq!(
            texts(&mut t, 40).last().unwrap(),
            "  error: old text not found"
        );
        let mut t = Transcript::default();
        for (result, before) in [
            ("created n.md (2 lines)", None),
            ("wrote n.md (2 lines)", None),
            ("wrote n.md (2 lines)", Some("# N\nold\n")),
        ] {
            t.push(Block::Tool {
                name: "write".into(),
                args: r##"{"path":"n.md","content":"# N\nok\n"}"##.into(),
                result: result.into(),
                before: before.map(Into::into),
            });
        }
        assert_eq!(
            texts(&mut t, 40),
            [
                "▸ write n.md",
                "  created n.md (2 lines)",
                "  + # N",
                "  + ok",
                "",
                "▸ write n.md",
                "  wrote n.md (2 lines)",
                "",
                "▸ write n.md",
                "  wrote n.md (2 lines)",
                "  - old",
                "  + ok"
            ]
        );
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
        // A short thought is shown beside its label.
        let mut t = Transcript::default();
        t.stream("short", true);
        t.stream("answer", false);
        assert_eq!(texts(&mut t, 40), ["thought · short", "", "answer"]);
    }

    #[test]
    fn long_messages_fold() {
        let mut t = Transcript::default();
        let pasted: Vec<String> = (1..=20).map(|n| n.to_string()).collect();
        t.push(Block::User(pasted.join("\n")));
        let text = texts(&mut t, 40);
        assert_eq!(text.len(), USER_LINES + 1);
        assert_eq!(text[0], "› 1");
        assert_eq!(text[USER_LINES], "  … 12 more lines · ctrl+o shows");
        t.toggle_reasoning();
        assert_eq!(texts(&mut t, 40).len(), 20);
        // One line more than fits is shown rather than folded.
        let mut t = Transcript::default();
        t.push(Block::User(pasted[..USER_LINES + 1].join("\n")));
        assert_eq!(texts(&mut t, 40).len(), USER_LINES + 1);
    }
}
