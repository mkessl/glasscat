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
            lines.push(spans_of(trim_end(std::mem::take(&mut line))));
            used = 0;
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
    lines.push(spans_of(line));
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

#[cfg(test)]
mod tests {
    use super::*;

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
