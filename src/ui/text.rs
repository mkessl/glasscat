//! Display width and word wrapping.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::screen::Style;

pub fn width(s: &str) -> usize {
    s.width()
}

/// A run of text in one style.
pub type Span = (String, Style);

/// Wraps text to `width` columns at spaces, breaking words that do not
/// fit on a line of their own. Each `\n` starts a new line; tabs become
/// four spaces.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    wrap_spans(&[(text.to_string(), Style::default())], width)
        .into_iter()
        .map(|l| l.into_iter().map(|(t, _)| t).collect())
        .collect()
}

/// `wrap` for the agent's own lists: a wrapped line goes on under where
/// its text began, after its indent and, in a `name   what it does` row,
/// under the second column.
pub fn wrap_hanging(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.split('\n') {
        // As `wrap` sees it, so that its first line is a prefix of this one.
        let line = &line.trim_end_matches('\r').replace('\t', "    ");
        let mut pieces = wrap(line, width).into_iter();
        let first = pieces.next().unwrap_or_default();
        if pieces.len() == 0 {
            out.push(first);
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let body = &line[indent..];
        let digits = body.len() - body.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        // A list item goes on under its text, past "1. " or "- ".
        let marker = if digits > 0 && body[digits..].starts_with(". ") {
            Some(indent + digits + 2)
        } else if body.starts_with("- ") || body.starts_with("• ") {
            Some(indent + body.chars().next().map_or(0, char::len_utf8) + 1)
        } else {
            None
        };
        // The last column the first row starts, past a run of spaces, in
        // the left half of the screen.
        let fits = |&c: &usize| c < first.len() && self::width(&line[..c]) <= width / 2;
        let column = body
            .match_indices("  ")
            .map(|(i, _)| indent + i)
            .filter_map(|i| Some(i + line[i..].find(|c| c != ' ')?))
            .filter(fits)
            .last()
            .or(marker.filter(fits));
        let hang = column.map_or(indent, |c| self::width(&line[..c]));
        let hang = if hang <= width / 2 { hang } else { 0 };
        // `wrap` may drop an indent wider than the screen.
        let end = line.find(first.as_str()).unwrap_or(0) + first.len();
        let rest = line[end..].trim_start();
        out.push(first);
        for piece in wrap(rest, width - hang) {
            out.push(format!("{}{piece}", " ".repeat(hang)));
        }
    }
    out
}

/// `wrap` for styled text: each line keeps the styles of its pieces.
pub fn wrap_spans(spans: &[Span], width: usize) -> Vec<Vec<Span>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut para: Vec<(char, Style)> = Vec::new();
    for (text, style) in spans {
        for c in text.chars() {
            match c {
                '\n' => {
                    if para.last().is_some_and(|(c, _)| *c == '\r') {
                        para.pop();
                    }
                    wrap_para(&para, width, &mut lines);
                    para.clear();
                }
                '\t' => para.extend([(' ', *style); 4]),
                c => para.push((c, *style)),
            }
        }
    }
    if para.last().is_some_and(|(c, _)| *c == '\r') {
        para.pop();
    }
    wrap_para(&para, width, &mut lines);
    lines
}

fn wrap_para(para: &[(char, Style)], width: usize, lines: &mut Vec<Vec<Span>>) {
    let start = lines.len();
    let mut line: Vec<(char, Style)> = Vec::new();
    let mut used = 0;
    let mut rest = para;
    while let Some(&(first, _)) = rest.first() {
        // A run of spaces or a run of everything else.
        let space = first == ' ';
        let n = rest
            .iter()
            .position(|(c, _)| (*c == ' ') != space)
            .unwrap_or(rest.len());
        let (token, tail) = rest.split_at(n);
        rest = tail;
        let w: usize = token.iter().map(|(c, _)| c.width().unwrap_or(0)).sum();
        if used + w <= width {
            line.extend_from_slice(token);
            used += w;
            continue;
        }
        if space {
            // Spaces where a line breaks are dropped, and never make a
            // line of their own.
            if used > 0 {
                lines.push(spans_of(trim_end(std::mem::take(&mut line))));
                used = 0;
            }
            continue;
        }
        if used > 0 {
            lines.push(spans_of(trim_end(std::mem::take(&mut line))));
            used = 0;
        }
        for &(c, st) in token {
            let cw = c.width().unwrap_or(0);
            if used + cw > width && used > 0 {
                lines.push(spans_of(std::mem::take(&mut line)));
                used = 0;
            }
            line.push((c, st));
            used += cw;
        }
    }
    if !line.is_empty() || lines.len() == start {
        lines.push(spans_of(line));
    }
}

/// Drops the spaces a line ends with where it wraps.
fn trim_end(mut line: Vec<(char, Style)>) -> Vec<(char, Style)> {
    while line.last().is_some_and(|(c, _)| *c == ' ') {
        line.pop();
    }
    line
}

/// Characters back into runs of one style.
fn spans_of(chars: Vec<(char, Style)>) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    for (c, st) in chars {
        match out.last_mut() {
            Some((t, s)) if *s == st => t.push(c),
            _ => out.push((c.to_string(), st)),
        }
    }
    out
}

/// Dollars to four places, or to two significant digits when that would
/// show a cost as nothing.
pub fn money(dollars: f64) -> String {
    if dollars > 0.0 && dollars < 0.000_05 {
        let places = (-dollars.log10()).ceil() as usize + 1;
        format!("${dollars:.places$}")
    } else {
        format!("${dollars:.4}")
    }
}

/// Parts of a status line joined by " · " in `width` columns: the parts
/// that do not fit go from before the last one, which stays.
pub fn fit_parts(parts: &[&str], width: usize) -> String {
    let Some((last, rest)) = parts.split_last() else {
        return String::new();
    };
    let mut out = String::new();
    for p in rest {
        if out.width() + p.width() + 3 + last.width() > width {
            break;
        }
        out = out + p + " · ";
    }
    clip(&(out + last), width)
}

/// Cuts text to at most `width` columns, ending with … when cut.
pub fn clip(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if used + cw + 1 > width {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push('…');
    out
}

/// Cuts text to at most `width` columns from the start, beginning with …
/// when cut: for paths, whose end says the most.
pub fn clip_start(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut used = 1;
    let mut start = s.len();
    for (i, c) in s.char_indices().rev() {
        let cw = c.width().unwrap_or(0);
        if used + cw > width {
            break;
        }
        used += cw;
        start = i;
    }
    format!("…{}", &s[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_parts_keep_the_last() {
        let parts = ["a move", "b open", "Esc back"];
        assert_eq!(fit_parts(&parts, 80), "a move · b open · Esc back");
        assert_eq!(fit_parts(&parts, 20), "a move · Esc back");
        assert_eq!(fit_parts(&parts, 8), "Esc back");
        assert_eq!(fit_parts(&parts, 5), "Esc …");
    }

    #[test]
    fn wraps_at_spaces() {
        assert_eq!(wrap("the quick brown fox", 10), ["the quick", "brown fox"]);
        assert_eq!(wrap("a\n\nb", 10), ["a", "", "b"]);
    }

    #[test]
    fn breaks_long_words_and_wide_chars() {
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("界界界", 5), ["界界", "界"]);
    }

    #[test]
    fn clips_from_either_end() {
        assert_eq!(clip("/home/me/project", 8), "/home/m…");
        assert_eq!(clip_start("/home/me/project", 8), "…project");
        assert_eq!(clip_start("/a/界界", 4), "…界");
        assert_eq!(clip_start("short", 8), "short");
        assert_eq!(clip_start("abc", 0), "");
    }

    #[test]
    fn hangs_wrapped_lines_under_their_column() {
        assert_eq!(
            wrap_hanging(
                "  /clear   start a new conversation\n  plain words go on and on",
                24
            ),
            [
                "  /clear   start a new",
                "           conversation",
                "  plain words go on and",
                "  on"
            ]
        );
        assert_eq!(wrap_hanging("short\n\nx", 10), ["short", "", "x"]);
        assert_eq!(wrap_hanging("abc ", 3), ["abc"]);
        assert_eq!(
            wrap_hanging("  12. one two three", 12),
            ["  12. one", "      two", "      three"]
        );
        assert_eq!(wrap_hanging("• one two", 6), ["• one", "  two"]);
        assert_eq!(
            wrap_hanging("  a  b   one two three four", 24),
            ["  a  b   one two three", "         four"]
        );
        assert_eq!(
            wrap_hanging("  a  b   one two three", 16),
            ["  a  b   one two", "     three"]
        );
        assert_eq!(wrap_hanging("  a b", 1), ["a", "b"]);
        assert_eq!(
            wrap_hanging("界 \tab cd ef", 14),
            ["界     ab cd", "       ef"]
        );
    }

    #[test]
    fn money_never_shows_a_cost_as_nothing() {
        assert_eq!(money(0.0012), "$0.0012");
        assert_eq!(money(0.000034), "$0.000034");
        assert_eq!(money(0.0000051), "$0.0000051");
        assert_eq!(money(0.0), "$0.0000");
        assert_eq!(money(1.5), "$1.5000");
    }

    #[test]
    fn keeps_indentation() {
        assert_eq!(wrap("    code();", 20), ["    code();"]);
    }

    #[test]
    fn styled_wrap_keeps_styles_across_breaks() {
        let b = Style::default().bold();
        let lines = wrap_spans(
            &[("a ".into(), Style::default()), ("bold word".into(), b)],
            6,
        );
        assert_eq!(
            lines,
            [
                vec![
                    ("a ".to_string(), Style::default()),
                    ("bold".to_string(), b)
                ],
                vec![("word".to_string(), b)],
            ]
        );
    }

    #[test]
    fn clips_with_ellipsis() {
        assert_eq!(clip("hello world", 6), "hello…");
        assert_eq!(clip("hi", 6), "hi");
    }
}
