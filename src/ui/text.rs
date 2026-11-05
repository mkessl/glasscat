//! Display width and word wrapping.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn width(s: &str) -> usize {
    s.width()
}

/// Wraps text to `width` columns at spaces, breaking words that do not
/// fit on a line of their own. Each `\n` starts a new line; tabs become
/// four spaces.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for para in text.split('\n') {
        let para = para
            .strip_suffix('\r')
            .unwrap_or(para)
            .replace('\t', "    ");
        let mut line = String::new();
        let mut used = 0;
        for token in tokens(&para) {
            let w = token.width();
            let space = token.starts_with(' ');
            if used + w <= width {
                line.push_str(token);
                used += w;
                continue;
            }
            if space {
                trim_end(&mut line);
                lines.push(std::mem::take(&mut line));
                used = 0;
                continue;
            }
            if used > 0 {
                trim_end(&mut line);
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            for c in token.chars() {
                let cw = c.width().unwrap_or(0);
                if used + cw > width && used > 0 {
                    lines.push(std::mem::take(&mut line));
                    used = 0;
                }
                line.push(c);
                used += cw;
            }
        }
        lines.push(line);
    }
    lines
}

/// Drops the spaces a line ends with where it wraps.
fn trim_end(line: &mut String) {
    line.truncate(line.trim_end_matches(' ').len());
}

/// Runs of spaces and runs of everything else.
fn tokens(s: &str) -> impl Iterator<Item = &str> {
    let mut rest = s;
    std::iter::from_fn(move || {
        let first = rest.chars().next()?;
        let space = first == ' ';
        let end = rest
            .find(|c: char| (c == ' ') != space)
            .unwrap_or(rest.len());
        let (token, tail) = rest.split_at(end);
        rest = tail;
        Some(token)
    })
}

/// Cuts text to at most `width` columns, ending with … when cut.
pub fn clip(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
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
    fn clips_with_ellipsis() {
        assert_eq!(clip("hello world", 6), "hello…");
        assert_eq!(clip("hi", 6), "hi");
    }
}
