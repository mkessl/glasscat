//! The markdown models write, laid out for the terminal: headings, lists,
//! quotes, rules, code blocks, and `code`, **bold** and *italic* inside
//! lines. Anything else stays as written.

use crossterm::style::Color;

use super::screen::Style;
use super::text::{Span, wrap_spans};
use super::transcript::Line;

pub const CODE: Style = Style::fg(Color::Green);
const QUOTE: Style = Style::dim();

pub fn lines(text: &str, width: usize) -> Vec<Line> {
    let mut out = Vec::new();
    let mut fence: Option<String> = None;
    for line in text.trim_end().split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = line.trim_start();
        if let Some(open) = &fence {
            if trimmed.starts_with(open.as_str())
                && trimmed.trim_matches(|c| c == '`' || c == '~').is_empty()
            {
                fence = None;
            } else {
                block(&mut out, "", "", vec![(line.to_string(), CODE)], width);
            }
            continue;
        }
        if let Some(marker) = fence_marker(trimmed) {
            fence = Some(marker);
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

/// `- x`, `* x`, `+ x` become `• x`; `1. x` and `1) x` keep their number.
fn list_item(s: &str) -> Option<(String, &str)> {
    for m in ["- ", "* ", "+ "] {
        if let Some(rest) = s.strip_prefix(m) {
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

/// `code`, **bold**, __bold__ and *italic* in one line of text.
fn inline(s: &str, base: Style) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    let mut push = |text: &str, style: Style| match out.last_mut() {
        Some((t, st)) if *st == style => t.push_str(text),
        _ => out.push((text.to_string(), style)),
    };
    let mut rest = s;
    let mut bold = false;
    let mut italic = false;
    let style = |bold: bool, italic: bool| {
        let mut st = base;
        if bold {
            st = st.bold();
        }
        if italic {
            st = st.italic();
        }
        st
    };
    while let Some(c) = rest.chars().next() {
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
            push(marker, style(bold, italic));
            rest = &rest[ticks..];
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
            if toggles(m, bold, rest) {
                bold = !bold;
                rest = &rest[2..];
                continue;
            }
        } else if c == '*' && toggles("*", italic, rest) {
            italic = !italic;
            rest = &rest[1..];
            continue;
        }
        push(&rest[..c.len_utf8()], style(bold, italic));
        rest = &rest[c.len_utf8()..];
    }
    out
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
    }
}
