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
    /// Text and cursor before each group of edits, for undo.
    undo: Vec<(String, usize)>,
    /// The kind of the last edit: a run of typing or of deleting is one
    /// step to undo.
    last: Edit,
    /// The text cut out last with a kill.
    killed: String,
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Edit {
    #[default]
    None,
    Typing,
    Deleting,
    /// A kill, a paste or a clear: always a step of its own.
    Other,
}

/// Steps kept to undo.
const UNDO_MAX: usize = 200;

/// One screen row of the text: a byte range without the newline.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Row {
    pub start: usize,
    pub end: usize,
}

impl Input {
    pub fn at_end(&self) -> bool {
        self.cur == self.text.len()
    }

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
        self.undo.clear();
        self.last = Edit::None;
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
        self.save(Edit::Other);
        self.text.clear();
        self.cur = 0;
        self.goal = None;
        self.hist = None;
    }

    pub fn insert(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        // A word typed is a step; a paste is one too.
        match s.chars().count() {
            1 if s == " " || s == "\n" => self.save(Edit::Other),
            1 => self.save(Edit::Typing),
            _ => self.save(Edit::Other),
        }
        if s == " " || s == "\n" {
            self.last = Edit::Typing;
        }
        self.text.insert_str(self.cur, &s);
        self.cur += s.len();
        self.goal = None;
    }

    pub fn backspace(&mut self) {
        self.save(Edit::Deleting);
        if let Some(p) = self.prev(self.cur) {
            self.text.replace_range(p..self.cur, "");
            self.cur = p;
        }
        self.goal = None;
    }

    pub fn delete(&mut self) {
        self.save(Edit::Deleting);
        if let Some(n) = self.next(self.cur) {
            self.text.replace_range(self.cur..n, "");
        }
        self.goal = None;
    }

    pub fn left(&mut self) {
        self.cur = self.prev(self.cur).unwrap_or(0);
        self.moved();
    }

    pub fn right(&mut self) {
        self.cur = self.next(self.cur).unwrap_or(self.cur);
        self.moved();
    }

    pub fn word_left(&mut self) {
        self.cur = self.word_start(self.cur);
        self.moved();
    }

    pub fn word_right(&mut self) {
        self.cur = self.word_end();
        self.moved();
    }

    /// Where the word at or after the cursor ends.
    fn word_end(&self) -> usize {
        let rest = &self.text[self.cur..];
        let skip_space = rest.len() - rest.trim_start().len();
        let word = rest[skip_space..]
            .find(char::is_whitespace)
            .unwrap_or(rest.len() - skip_space);
        self.cur + skip_space + word
    }

    pub fn line_start(&mut self) {
        self.cur = self.text[..self.cur].rfind('\n').map_or(0, |i| i + 1);
        self.moved();
    }

    pub fn line_end(&mut self) {
        self.cur += self.text[self.cur..]
            .find('\n')
            .unwrap_or(self.text.len() - self.cur);
        self.moved();
    }

    /// Deletes from the start of the line to the cursor.
    pub fn kill_before(&mut self) {
        let start = self.text[..self.cur].rfind('\n').map_or(0, |i| i + 1);
        self.kill(start..self.cur);
    }

    /// Deletes from the cursor to the end of the line.
    pub fn kill_after(&mut self) {
        let end = self.cur
            + self.text[self.cur..]
                .find('\n')
                .unwrap_or(self.text.len() - self.cur);
        self.kill(self.cur..end);
    }

    pub fn kill_word(&mut self) {
        let start = self.word_start(self.cur);
        self.kill(start..self.cur);
    }

    /// Cuts out the word after the cursor.
    pub fn kill_word_after(&mut self) {
        self.kill(self.cur..self.word_end());
    }

    /// Cuts out `range`, keeping it for `yank`; the cursor goes to its
    /// start.
    fn kill(&mut self, range: std::ops::Range<usize>) {
        if range.is_empty() {
            return;
        }
        self.save(Edit::Other);
        self.killed = self.text[range.clone()].to_string();
        self.cur = range.start;
        self.text.replace_range(range, "");
    }

    /// Puts back the text cut out last, at the cursor.
    pub fn yank(&mut self) {
        let killed = std::mem::take(&mut self.killed);
        self.insert(&killed);
        self.killed = killed;
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
        self.last = Edit::None;
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
        self.undo.clear();
        self.last = Edit::None;
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
        self.undo.clear();
        self.last = Edit::None;
    }

    /// Puts back the text as it was before the last group of edits;
    /// false when there is nothing to undo.
    pub fn undo(&mut self) -> bool {
        while let Some((text, cur)) = self.undo.pop() {
            if text != self.text {
                self.text = text;
                self.cur = cur;
                self.goal = None;
                self.last = Edit::None;
                return true;
            }
        }
        false
    }

    /// Remembers the text before an edit of this kind, unless it goes on
    /// a run of the same kind.
    fn save(&mut self, kind: Edit) {
        let new_step = kind == Edit::Other || kind != self.last;
        if new_step && self.undo.last().is_none_or(|(t, _)| *t != self.text) {
            if self.undo.len() == UNDO_MAX {
                self.undo.remove(0);
            }
            self.undo.push((self.text.clone(), self.cur));
        }
        self.last = kind;
    }

    fn moved(&mut self) {
        self.goal = None;
        self.last = Edit::None;
    }

    /// Places the cursor at a screen position of the laid-out text.
    pub fn click(&mut self, width: usize, col: usize, row: usize) {
        let rows = self.rows(width);
        if let Some(&r) = rows.get(row).or(rows.last()) {
            self.cur = self.offset_at(r, col);
        }
        self.moved();
    }

    /// The text cut into rows of at most `width` columns, after a space
    /// where there is one, else inside a word too long for a row.
    pub fn rows(&self, width: usize) -> Vec<Row> {
        let width = width.max(1);
        let mut rows = Vec::new();
        let mut line_start = 0;
        for line in self.text.split('\n') {
            let mut start = line_start;
            let mut used = 0;
            // Just after the row's last space, and the columns since.
            let mut space: Option<(usize, usize)> = None;
            for (i, c) in line.char_indices() {
                let w = c.width().unwrap_or(0);
                if used + w > width {
                    let at = line_start + i;
                    let (end, since) = match space {
                        Some((after, since)) if c != ' ' => (after, since),
                        _ => (at, 0),
                    };
                    rows.push(Row { start, end });
                    start = end;
                    used = since;
                    space = None;
                }
                used += w;
                match (c, &mut space) {
                    (' ', _) => space = Some((line_start + i + 1, 0)),
                    (_, Some((_, since))) => *since += w,
                    _ => {}
                }
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
        let rows_of = |text: &str, w: usize| -> Vec<String> {
            let i = with(text);
            i.rows(w)
                .iter()
                .map(|r| i.text()[r.start..r.end].to_string())
                .collect()
        };
        assert_eq!(rows_of("aaa bbb ccc", 5), ["aaa ", "bbb ", "ccc"]);
        assert_eq!(rows_of("aaaa bb", 4), ["aaaa", " bb"]);
        assert_eq!(rows_of("a bcdefgh", 4), ["a ", "bcde", "fgh"]);
        assert_eq!(rows_of("界 界界", 4), ["界 ", "界界", ""]);
        let i = with("aaa bbb");
        assert_eq!(i.cursor(5), (3, 1));
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
    fn undo_goes_back_a_word_or_a_kill_at_a_time() {
        let mut i = Input::default();
        for c in "one two".chars() {
            i.insert(&c.to_string());
        }
        i.kill_before();
        assert_eq!(i.text(), "");
        assert!(i.undo());
        assert_eq!((i.text(), i.cur), ("one two", 7));
        i.backspace();
        i.backspace();
        assert!(i.undo());
        assert_eq!(i.text(), "one two");
        assert!(i.undo());
        assert_eq!(i.text(), "one");
        assert!(i.undo());
        assert_eq!(i.text(), "");
        assert!(!i.undo());
        i.insert("pasted text");
        i.clear();
        assert!(i.undo());
        assert_eq!(i.text(), "pasted text");
        i.take();
        assert!(!i.undo(), "a sent message starts anew");

        let mut i = with("cut this");
        i.word_left();
        i.kill_before();
        i.line_end();
        i.insert(" ");
        i.yank();
        assert_eq!(i.text(), "this cut ");
        let mut i = with("one two three");
        i.line_start();
        i.word_right();
        i.kill_word_after();
        assert_eq!(i.text(), "one three");
        i.line_end();
        i.yank();
        assert_eq!(i.text(), "one three two");
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
