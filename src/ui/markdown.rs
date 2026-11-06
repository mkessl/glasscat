//! The markdown models write, laid out for the terminal: headings, lists,
//! quotes, rules, code blocks in colour, tables, and `code`, **bold** and
//! *italic* inside lines. Anything else stays as written.

use crossterm::style::Color;

use super::highlight;
use super::screen::Style;
use super::text::{Span, wrap_spans};
use super::transcript::Line;

pub const CODE: Style = Style::fg(Color::Green);
const QUOTE: Style = Style::dim();

pub fn lines(text: &str, width: usize) -> Vec<Line> {
    let mut out = Vec::new();
    let mut fence: Option<Fence> = None;
    let all: Vec<&str> = text
        .trim_end()
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let mut i = 0;
    while i < all.len() {
        let line = all[i];
        i += 1;
        if fence.is_none()
            && line.contains('|')
            && let Some(align) = all.get(i).and_then(|l| table_rule(l))
        {
            let mut rows = vec![cells(line)];
            i += 1;
            while i < all.len() && all[i].contains('|') && !all[i].trim().is_empty() {
                rows.push(cells(all[i]));
                i += 1;
            }
            table(&mut out, &rows, &align, width);
            continue;
        }
        let trimmed = line.trim_start();
        if let Some(f) = &mut fence {
            if trimmed.starts_with(f.marker.as_str())
                && trimmed.trim_matches(|c| c == '`' || c == '~').is_empty()
            {
                fence = None;
            } else {
                let mut spans = if f.diff {
                    highlight::diff_line(line)
                } else if let Some(lang) = f.lang {
                    highlight::line(line, lang, &mut f.in_comment)
                } else {
                    vec![(line.to_string(), CODE)]
                };
                if spans.is_empty() {
                    spans.push((String::new(), Style::default()));
                }
                block(&mut out, "", "", spans, width);
            }
            continue;
        }
        if let Some(marker) = fence_marker(trimmed) {
            let info = trimmed[marker.len()..].trim();
            fence = Some(Fence {
                lang: highlight::lang(info),
                diff: highlight::is_diff(info),
                marker,
                in_comment: false,
            });
            continue;
        }
        let indent = &line[..line.len() - trimmed.len()];
        if trimmed.is_empty() {
            out.push(Line::default());
        } else if let Some((level, title)) = heading(trimmed) {
            let style = if level <= 2 {
                Style::default().bold()
            } else {
                Style::dim().bold()
            };
            block(&mut out, "", "", inline(title, style), width);
        } else if is_rule(trimmed) {
            out.push(Line::new("─".repeat(width), Style::dim()));
        } else if let Some(rest) = trimmed.strip_prefix('>') {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            block(&mut out, "│ ", "│ ", inline(rest, QUOTE.italic()), width);
        } else if let Some((marker, rest)) = list_item(trimmed) {
            let first = format!("{indent}{marker} ");
            let next = " ".repeat(super::text::width(&first));
            block(
                &mut out,
                &first,
                &next,
                inline(rest, Style::default()),
                width,
            );
        } else {
            block(
                &mut out,
                indent,
                indent,
                inline(trimmed, Style::default()),
                width,
            );
        }
    }
    out
}

/// An open code block.
struct Fence {
    /// What closes it.
    marker: String,
    lang: Option<&'static highlight::Lang>,
    diff: bool,
    /// Inside a block comment that started on an earlier line.
    in_comment: bool,
}

/// Wraps styled text under a first-line prefix, with continuation lines
/// under `next` so list items and quotes hang.
fn block(out: &mut Vec<Line>, first: &str, next: &str, spans: Vec<Span>, width: usize) {
    let pw = super::text::width(first).max(super::text::width(next));
    let room = width.saturating_sub(pw).max(width.min(10));
    for (i, mut l) in wrap_spans(&spans, room).into_iter().enumerate() {
        let prefix = if i == 0 { first } else { next };
        if !prefix.is_empty() {
            let style = if prefix.starts_with('│') {
                QUOTE
            } else {
                Style::default()
            };
            l.insert(0, (prefix.to_string(), style));
        }
        out.push(Line { spans: l });
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Align {
    Left,
    Center,
    Right,
}

/// The line under a table's header, `|---|:-:|--:|`: how each column is
/// aligned.
fn table_rule(s: &str) -> Option<Vec<Align>> {
    if !s.contains('-') {
        return None;
    }
    cells(s)
        .iter()
        .map(|c| {
            let dashes = c.trim_matches(':');
            if dashes.is_empty() || !dashes.chars().all(|c| c == '-') {
                return None;
            }
            Some(match (c.starts_with(':'), c.ends_with(':')) {
                (true, true) => Align::Center,
                (false, true) => Align::Right,
                _ => Align::Left,
            })
        })
        .collect()
}

/// The cells of a table row; `\|` is a pipe inside a cell.
fn cells(s: &str) -> Vec<String> {
    let s = s.trim();
    let s = s.strip_prefix('|').unwrap_or(s);
    let s = s
        .strip_suffix('|')
        .filter(|r| !r.ends_with('\\'))
        .unwrap_or(s);
    let mut out = vec![String::new()];
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                out.last_mut().expect("never empty").push('|');
                chars.next();
            }
            '|' => out.push(String::new()),
            c => out.last_mut().expect("never empty").push(c),
        }
    }
    out.iter().map(|c| c.trim().to_string()).collect()
}

/// A table in columns as wide as their text, narrowed to `width` by
/// wrapping the widest cells.
fn table(out: &mut Vec<Line>, rows: &[Vec<String>], align: &[Align], width: usize) {
    const SEP: &str = " │ ";
    let n = align.len();
    let grid: Vec<Vec<Vec<Span>>> = rows
        .iter()
        .enumerate()
        .map(|(r, row)| {
            let base = if r == 0 {
                Style::default().bold()
            } else {
                Style::default()
            };
            (0..n)
                .map(|c| inline(row.get(c).map_or("", String::as_str), base))
                .collect()
        })
        .collect();
    let span_width = |s: &[Span]| s.iter().map(|(t, _)| super::text::width(t)).sum::<usize>();
    let natural: Vec<usize> = (0..n)
        .map(|c| {
            grid.iter()
                .map(|r| span_width(&r[c]))
                .max()
                .unwrap_or(0)
                .max(1)
        })
        .collect();
    let room = width.saturating_sub(super::text::width(SEP) * n.saturating_sub(1));
    if room < n * 3 {
        // Too narrow for columns: the rows as written.
        for row in rows {
            block(
                out,
                "",
                "",
                inline(&row.join(" | "), Style::default()),
                width,
            );
        }
        return;
    }
    // Narrow columns keep their width; the rest share what is left.
    let mut widths = natural.clone();
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&c| natural[c]);
    let mut left = room;
    for (k, &c) in order.iter().enumerate() {
        let fair = left / (n - k);
        widths[c] = natural[c].min(fair);
        left -= widths[c];
    }
    let rule = |out: &mut Vec<Line>| {
        let parts: Vec<String> = widths.iter().map(|&w| "─".repeat(w)).collect();
        out.push(Line::new(parts.join("─┼─"), Style::dim()));
    };
    for (r, row) in grid.iter().enumerate() {
        let wrapped: Vec<Vec<Vec<Span>>> = row
            .iter()
            .zip(&widths)
            .map(|(cell, &w)| wrap_spans(cell, w))
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        for l in 0..height {
            let mut line: Vec<Span> = Vec::new();
            for c in 0..n {
                if c > 0 {
                    line.push((SEP.into(), Style::dim()));
                }
                let piece = wrapped[c].get(l).cloned().unwrap_or_default();
                let gap = widths[c].saturating_sub(span_width(&piece));
                let (before, after) = match align[c] {
                    Align::Left => (0, gap),
                    Align::Right => (gap, 0),
                    Align::Center => (gap / 2, gap - gap / 2),
                };
                if before > 0 {
                    line.push((" ".repeat(before), Style::default()));
                }
                line.extend(piece);
                if after > 0 && c + 1 < n {
                    line.push((" ".repeat(after), Style::default()));
                }
            }
            out.push(Line { spans: line });
        }
        if r == 0 {
            rule(out);
        }
    }
}

fn fence_marker(s: &str) -> Option<String> {
    for c in ['`', '~'] {
        let n = s.chars().take_while(|&x| x == c).count();
        if n >= 3 {
            return Some(c.to_string().repeat(n));
        }
    }
    None
}

fn heading(s: &str) -> Option<(usize, &str)> {
    let level = s.chars().take_while(|&c| c == '#').count();
    let rest = &s[level..];
    ((1..=6).contains(&level) && (rest.is_empty() || rest.starts_with(' ')))
        .then(|| (level, rest.trim().trim_end_matches('#').trim_end()))
}

fn is_rule(s: &str) -> bool {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    s.len() >= 3 && ['-', '*', '_'].iter().any(|&m| s.chars().all(|c| c == m))
}

/// `- x`, `* x`, `+ x` become `• x`, `- [ ] x` and `- [x] x` a box;
/// `1. x` and `1) x` keep their number.
fn list_item(s: &str) -> Option<(String, &str)> {
    for m in ["- ", "* ", "+ "] {
        if let Some(rest) = s.strip_prefix(m) {
            // A task list's box takes the bullet's place.
            for (b, mark) in [("[ ] ", "☐"), ("[x] ", "☑"), ("[X] ", "☑")] {
                if let Some(rest) = rest.strip_prefix(b) {
                    return Some((mark.into(), rest));
                }
            }
            return Some(("•".into(), rest));
        }
    }
    let digits = s.chars().take_while(char::is_ascii_digit).count();
    if (1..=9).contains(&digits) {
        let after = &s[digits..];
        for m in [". ", ") "] {
            if let Some(rest) = after.strip_prefix(m) {
                return Some((s[..digits + 1].to_string(), rest));
            }
        }
    }
    None
}

/// `code`, **bold**, __bold__, *italic*, _italic_, ~~struck~~ and
/// [links](url) in one line of text; a link shows its text, then its address dimmed.
fn inline(s: &str, base: Style) -> Vec<Span> {
    // A marker left open at the end of the line was no marker: it is
    // taken as text, and the line laid out again.
    let mut literal = Vec::new();
    loop {
        match spans(s, base, &literal) {
            (out, None) => return out,
            (_, Some(at)) => literal.push(at),
        }
    }
}

/// The spans of one line, with the markers at the byte offsets in
/// `literal` taken as text; and the offset of a marker left open, if any.
fn spans(s: &str, base: Style, literal: &[usize]) -> (Vec<Span>, Option<usize>) {
    let mut out: Vec<Span> = Vec::new();
    let mut push = |text: &str, style: Style| match out.last_mut() {
        Some((t, st)) if *st == style => t.push_str(text),
        _ => out.push((text.to_string(), style)),
    };
    let mut rest = s;
    // The marker that opened bold, `**` or `__`.
    let mut bold: Option<&str> = None;
    // The marker that opened italic, `*` or `_`.
    let mut italic: Option<char> = None;
    let mut strike = false;
    // Where each open marker opened.
    let (mut bold_at, mut italic_at, mut strike_at) = (0, 0, 0);
    let style = |bold: Option<&str>, italic: Option<char>, strike: bool| {
        let mut st = base;
        if bold.is_some() {
            st = st.bold();
        }
        if italic.is_some() {
            st = st.italic();
        }
        if strike {
            st = st.strike();
        }
        st
    };
    let word = |c: Option<char>| c.is_some_and(char::is_alphanumeric);
    while let Some(c) = rest.chars().next() {
        let at = s.len() - rest.len();
        let free = !literal.contains(&at);
        if c == '`' {
            let ticks = rest.chars().take_while(|&c| c == '`').count();
            let marker = &rest[..ticks];
            if let Some(end) = rest[ticks..].find(marker) {
                let code = &rest[ticks..ticks + end];
                let code = code
                    .strip_prefix(' ')
                    .and_then(|c| c.strip_suffix(' '))
                    .unwrap_or(code);
                push(code, CODE);
                rest = &rest[ticks + end + ticks..];
                continue;
            }
            push(marker, style(bold, italic, strike));
            rest = &rest[ticks..];
            continue;
        }
        if c == '['
            && let Some((text, url, len)) = link(rest)
        {
            push(text, style(bold, italic, strike).underline());
            if url != text {
                push(&format!(" ({url})"), Style::dim());
            }
            rest = &rest[len..];
            continue;
        }
        let toggles = |m: &str, on: bool, rest: &str| -> bool {
            let after = &rest[m.len()..];
            if on {
                true
            } else {
                // Opens only before text that closes it later on the line.
                !after.starts_with(char::is_whitespace) && !after.is_empty() && after.contains(m)
            }
        };
        if rest.starts_with("**") || rest.starts_with("__") {
            let m = &rest[..2];
            // `__` inside a word, as in `__init__`, is part of the word.
            let inside = m == "__"
                && if bold.is_some() {
                    word(rest[2..].chars().next())
                } else {
                    word(s[..s.len() - rest.len()].chars().next_back())
                };
            let fits = match bold {
                Some(open) => open == m,
                None => free && toggles(m, false, rest),
            };
            if fits && !inside {
                bold = if bold.is_some() { None } else { Some(m) };
                bold_at = at;
                rest = &rest[2..];
                continue;
            }
            push(m, style(bold, italic, strike));
            rest = &rest[2..];
            continue;
        } else if rest.starts_with("~~") && (strike || free) && toggles("~~", strike, rest) {
            strike = !strike;
            strike_at = at;
            rest = &rest[2..];
            continue;
        } else if c == '*'
            && italic != Some('_')
            && (italic.is_some() || free)
            && toggles("*", italic.is_some(), rest)
        {
            italic = if italic.is_some() { None } else { Some('*') };
            italic_at = at;
            rest = &rest[1..];
            continue;
        } else if c == '_' && italic != Some('*') {
            // `_` works only at the edge of a word: snake_case stays as it is.
            let before = s[..s.len() - rest.len()].chars().next_back();
            let after = rest[1..].chars().next();
            let fits = if italic.is_some() {
                !word(after) && !before.is_some_and(char::is_whitespace)
            } else {
                free && !word(before)
                    && after.is_some_and(|c| !c.is_whitespace())
                    && closes(&rest[1..])
            };
            if fits {
                italic = if italic.is_some() { None } else { Some('_') };
                italic_at = at;
                rest = &rest[1..];
                continue;
            }
        }
        push(&rest[..c.len_utf8()], style(bold, italic, strike));
        rest = &rest[c.len_utf8()..];
    }
    let open = [
        (bold.is_some(), bold_at),
        (italic.is_some(), italic_at),
        (strike, strike_at),
    ];
    (out, open.iter().find(|o| o.0).map(|o| o.1))
}

/// Whether `s` holds a `_` that ends italic: after text, before no word.
fn closes(s: &str) -> bool {
    s.char_indices().any(|(i, c)| {
        c == '_'
            && i > 0
            && !s[..i].ends_with(char::is_whitespace)
            && !s[i + 1..].starts_with(char::is_alphanumeric)
    })
}

/// `[text](url)` at the start of `s`: the text, the address, and the
/// length of the whole.
fn link(s: &str) -> Option<(&str, &str, usize)> {
    let close = s.find("](")?;
    let text = &s[1..close];
    let after = &s[close + 2..];
    let end = after.find(')')?;
    let url = &after[..end];
    (!text.is_empty() && !text.contains('[') && !url.contains(char::is_whitespace)).then_some((
        text,
        url,
        close + 2 + end + 1,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(md: &str, width: usize) -> Vec<String> {
        lines(md, width).iter().map(Line::text).collect()
    }

    #[test]
    fn inline_styles() {
        let b = Style::default().bold();
        assert_eq!(
            inline("a **b** `c` *d* 2*3", Style::default()),
            [
                ("a ".to_string(), Style::default()),
                ("b".to_string(), b),
                (" ".to_string(), Style::default()),
                ("c".to_string(), CODE),
                (" ".to_string(), Style::default()),
                ("d".to_string(), Style::default().italic()),
                (" 2*3".to_string(), Style::default()),
            ]
        );
        assert_eq!(inline("snake_case_name", Style::default()).len(), 1);
        // A marker that never closes is text.
        for open in ["_a__ and more", "_a `b_` c", "**a `b**` c", "~~a `~~`"] {
            assert!(
                inline(open, Style::default())
                    .iter()
                    .all(|(_, st)| *st == Style::default() || *st == CODE),
                "{open}"
            );
        }
        let i = Style::default().italic();
        assert_eq!(
            inline(
                "**bold _it_** _x_y_ a_b _ c_ ~~gone~~ ~x~",
                Style::default()
            ),
            [
                ("bold ".to_string(), b),
                ("it".to_string(), b.italic()),
                (" ".to_string(), Style::default()),
                ("x_y".to_string(), i),
                (" a_b _ c_ ".to_string(), Style::default()),
                ("gone".to_string(), Style::default().strike()),
                (" ~x~".to_string(), Style::default()),
            ]
        );
        let b = Style::default().bold();
        assert_eq!(
            inline("**src/__init__.py** __x__", Style::default()),
            [
                ("src/__init__.py".to_string(), b),
                (" ".to_string(), Style::default()),
                ("x".to_string(), b),
            ]
        );
        let l: String = inline(
            "see [the docs](https://d.x) or [a](b c) [u](u)",
            Style::default(),
        )
        .into_iter()
        .map(|(t, _)| t)
        .collect();
        assert_eq!(l, "see the docs (https://d.x) or [a](b c) u");
    }

    #[test]
    fn tables() {
        let md = "| Tool | What |\n|---|--:|\n| `read` | reads a file |\n| a \\| b | 2 |";
        assert_eq!(
            texts(md, 40),
            [
                "Tool  │         What",
                "──────┼─────────────",
                "read  │ reads a file",
                "a | b │            2",
            ]
        );
        // Narrower than the table: the widest column wraps.
        assert_eq!(
            texts("|a|b|\n|-|-|\n|x|one two three|", 12),
            ["a │ b", "──┼─────────", "x │ one two", "  │ three"]
        );
        // A pipe without a rule under it is plain text.
        assert_eq!(texts("a | b", 20), ["a | b"]);
    }

    #[test]
    fn blocks() {
        let md = "# Title\n\n- one two three\n  - nested\n1. first\n> quoted\n---\n```rust\nlet *x* = 1;\n```";
        assert_eq!(
            texts(md, 12),
            [
                "Title",
                "",
                "• one two",
                "  three",
                "  • nested",
                "1. first",
                "│ quoted",
                "────────────",
                "let *x* = 1;",
            ]
        );
        assert_eq!(
            texts("- [ ] to do\n- [x] done\n- [y] not a box", 20),
            ["☐ to do", "☑ done", "• [y] not a box"]
        );
    }
}
