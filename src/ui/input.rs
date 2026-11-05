//! The message editor: multi-line text, a cursor, and history.

use unicode_width::UnicodeWidthChar;

#[derive(Default)]
pub struct Input {
    text: String,
    /// Byte offset of the cursor.
    cur: usize,
    /// Column kept while moving up and down through short rows.
    goal: Option<usize>,
    history: Vec<String>,
    hist: Option<usize>,
    draft: String,
}

/// One screen row of the text: a byte range without the newline.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Row {
    pub start: usize,
    pub end: usize,
}

impl Input {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Puts earlier prompts before the history of this run.
    pub fn add_older(&mut self, mut older: Vec<String>) {
        older.dedup();
        if self.hist.is_some() {
            return;
        }
        older.append(&mut self.history);
        self.history = older;
    }

    /// Takes the text out for sending and remembers it in history.
    pub fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        if !text.trim().is_empty() && self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        self.cur = 0;
        self.goal = None;
        self.hist = None;
        text
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cur = 0;
        self.goal = None;
        self.hist = None;
    }

    pub fn insert(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        self.text.insert_str(self.cur, &s);
        self.cur += s.len();
        self.goal = None;
    }

    pub fn backspace(&mut self) {
        if let Some(p) = self.prev(self.cur) {
            self.text.replace_range(p..self.cur, "");
            self.cur = p;
        }
        self.goal = None;
    }

    pub fn delete(&mut self) {
        if let Some(n) = self.next(self.cur) {
            self.text.replace_range(self.cur..n, "");
        }
        self.goal = None;
    }

    pub fn left(&mut self) {
        self.cur = self.prev(self.cur).unwrap_or(0);
        self.goal = None;
    }

    pub fn right(&mut self) {
        self.cur = self.next(self.cur).unwrap_or(self.cur);
        self.goal = None;
    }

    pub fn word_left(&mut self) {
        self.cur = self.word_start(self.cur);
        self.goal = None;
    }

    pub fn word_right(&mut self) {
        let rest = &self.text[self.cur..];
        let skip_space = rest.len() - rest.trim_start().len();
        let word = rest[skip_space..]
            .find(char::is_whitespace)
            .unwrap_or(rest.len() - skip_space);
        self.cur += skip_space + word;
        self.goal = None;
    }

    pub fn line_start(&mut self) {
        self.cur = self.text[..self.cur].rfind('\n').map_or(0, |i| i + 1);
        self.goal = None;
    }

    pub fn line_end(&mut self) {
        self.cur += self.text[self.cur..]
            .find('\n')
            .unwrap_or(self.text.len() - self.cur);
        self.goal = None;
    }

    /// Deletes from the start of the line to the cursor.
    pub fn kill_before(&mut self) {
        let start = self.text[..self.cur].rfind('\n').map_or(0, |i| i + 1);
        self.text.replace_range(start..self.cur, "");
        self.cur = start;
    }

    /// Deletes from the cursor to the end of the line.
    pub fn kill_after(&mut self) {
        let end = self.cur
            + self.text[self.cur..]
                .find('\n')
                .unwrap_or(self.text.len() - self.cur);
        self.text.replace_range(self.cur..end, "");
    }

    pub fn kill_word(&mut self) {
        let start = self.word_start(self.cur);
        self.text.replace_range(start..self.cur, "");
        self.cur = start;
    }

    /// Moves one screen row up or down; false when there is none.
    pub fn vertical(&mut self, width: usize, down: bool) -> bool {
        let rows = self.rows(width);
        let (col, row) = self.cursor(width);
        let target = if down { row + 1 } else { row.wrapping_sub(1) };
        let Some(&r) = rows.get(target) else {
            return false;
        };
        let goal = *self.goal.get_or_insert(col);
        self.cur = self.offset_at(r, goal);
        true
    }

    pub fn history_prev(&mut self) {
        let i = match self.hist {
            None if self.history.is_empty() => return,
            None => {
                self.draft = std::mem::take(&mut self.text);
                self.history.len() - 1
            }
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.hist = Some(i);
        self.text = self.history[i].clone();
        self.cur = self.text.len();
    }

    pub fn history_next(&mut self) {
        let Some(i) = self.hist else { return };
        if i + 1 < self.history.len() {
            self.hist = Some(i + 1);
            self.text = self.history[i + 1].clone();
        } else {
            self.hist = None;
            self.text = std::mem::take(&mut self.draft);
        }
        self.cur = self.text.len();
    }

    /// Places the cursor at a screen position of the laid-out text.
    pub fn click(&mut self, width: usize, col: usize, row: usize) {
        let rows = self.rows(width);
        if let Some(&r) = rows.get(row).or(rows.last()) {
            self.cur = self.offset_at(r, col);
        }
        self.goal = None;
    }

    /// The text cut into rows of at most `width` columns.
    pub fn rows(&self, width: usize) -> Vec<Row> {
        let width = width.max(1);
        let mut rows = Vec::new();
        let mut line_start = 0;
        for line in self.text.split('\n') {
            let mut start = line_start;
            let mut used = 0;
            for (i, c) in line.char_indices() {
                let w = c.width().unwrap_or(0);
                if used + w > width {
                    rows.push(Row {
                        start,
                        end: line_start + i,
                    });
                    start = line_start + i;
                    used = 0;
                }
                used += w;
            }
            let end = line_start + line.len();
            rows.push(Row { start, end });
            // A full last row leaves the cursor after it on a row of its own.
            if used == width {
                rows.push(Row { start: end, end });
            }
            line_start = end + 1;
        }
        rows
    }

    /// The cursor's column and row in the laid-out text.
    pub fn cursor(&self, width: usize) -> (usize, usize) {
        let rows = self.rows(width);
        let mut at = 0;
        for (i, r) in rows.iter().enumerate() {
            if r.start <= self.cur && self.cur <= r.end {
                at = i;
                if self.cur < r.end {
                    break;
                }
            }
        }
        let r = rows[at];
        let col = self.text[r.start..self.cur]
            .chars()
            .map(|c| c.width().unwrap_or(0))
            .sum();
        (col, at)
    }

    fn offset_at(&self, r: Row, col: usize) -> usize {
        let mut used = 0;
        for (i, c) in self.text[r.start..r.end].char_indices() {
            let w = c.width().unwrap_or(0);
            if used + w > col {
                return r.start + i;
            }
            used += w;
        }
        r.end
    }

    fn word_start(&self, from: usize) -> usize {
        let before = &self.text[..from];
        let trimmed = before.trim_end();
        trimmed.rfind(char::is_whitespace).map_or(0, |i| {
            i + trimmed[i..].chars().next().map_or(1, char::len_utf8)
        })
    }

    fn prev(&self, i: usize) -> Option<usize> {
        self.text[..i].chars().next_back().map(|c| i - c.len_utf8())
    }

    fn next(&self, i: usize) -> Option<usize> {
        self.text[i..].chars().next().map(|c| i + c.len_utf8())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(text: &str) -> Input {
        let mut i = Input::default();
        i.insert(text);
        i
    }

    #[test]
    fn rows_wrap_and_break_lines() {
        let i = with("abcdef\ngh");
        let rows: Vec<_> = i
            .rows(4)
            .iter()
            .map(|r| &i.text()[r.start..r.end])
            .collect();
        assert_eq!(rows, ["abcd", "ef", "gh"]);
    }

    #[test]
    fn cursor_after_a_full_row_goes_to_the_next() {
        let i = with("abcd");
        assert_eq!(i.cursor(4), (0, 1));
        assert_eq!(i.cursor(5), (4, 0));
    }

    #[test]
    fn moves_by_words_and_rows() {
        let mut i = with("one two  three");
        i.word_left();
        assert_eq!(&i.text()[i.cur..], "three");
        i.word_left();
        assert_eq!(&i.text()[i.cur..], "two  three");
        i.word_right();
        assert_eq!(&i.text()[i.cur..], "  three");
        i.kill_word();
        assert_eq!(i.text(), "one   three");

        let mut i = with("abc\nde\nfghij");
        assert!(i.vertical(10, false));
        assert_eq!(i.cursor(10), (2, 1));
        assert!(i.vertical(10, false));
        assert_eq!(i.cursor(10), (3, 0));
        assert!(!i.vertical(10, false));
        assert!(i.vertical(10, true) && i.vertical(10, true));
        assert_eq!(i.cursor(10), (5, 2), "the column goal is kept");
    }

    #[test]
    fn editing_multibyte_text() {
        let mut i = with("приве");
        i.backspace();
        i.insert("ет");
        assert_eq!(i.text(), "привет");
        i.line_start();
        i.delete();
        assert_eq!(i.text(), "ривет");
    }

    #[test]
    fn history_keeps_the_draft() {
        let mut i = with("first");
        i.take();
        i.insert("draft");
        i.history_prev();
        assert_eq!(i.text(), "first");
        i.history_next();
        assert_eq!(i.text(), "draft");
    }

    #[test]
    fn click_places_the_cursor() {
        let mut i = with("hello\nworld");
        i.click(20, 2, 1);
        assert_eq!(&i.text()[i.cur..], "rld");
        i.click(20, 50, 0);
        assert_eq!(&i.text()[..i.cur], "hello");
    }
}
