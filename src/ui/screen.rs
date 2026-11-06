//! A grid of cells drawn each frame and sent to the terminal as the
//! difference from the last frame, inside one synchronized update, so the
//! terminal never shows a half-drawn frame and unchanged cells cost nothing.

use std::io::{self, Write};

use crossterm::style::{Attribute, Color, SetAttribute, SetBackgroundColor, SetForegroundColor};
use crossterm::{cursor, queue, terminal};
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
    pub strike: bool,
}

impl Style {
    pub const fn fg(c: Color) -> Self {
        Self {
            fg: Some(c),
            bg: None,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            reverse: false,
            strike: false,
        }
    }
    pub const fn dim() -> Self {
        Self {
            fg: None,
            bg: None,
            bold: false,
            dim: true,
            italic: false,
            underline: false,
            reverse: false,
            strike: false,
        }
    }
    pub const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }
    pub const fn italic(mut self) -> Self {
        self.italic = true;
        self
    }
    pub const fn underline(mut self) -> Self {
        self.underline = true;
        self
    }
    pub const fn strike(mut self) -> Self {
        self.strike = true;
        self
    }
}

/// One cell's text: a character and any zero-width marks after it, inline.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Sym {
    len: u8,
    buf: [u8; 15],
}

impl Sym {
    const BLANK: Sym = Sym {
        len: 1,
        buf: [b' ', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    };
    /// The right half of a wide character: nothing is drawn there.
    const CONT: Sym = Sym {
        len: 0,
        buf: [0; 15],
    };

    fn new(c: char) -> Sym {
        let mut s = Sym {
            len: 0,
            buf: [0; 15],
        };
        s.len = c.encode_utf8(&mut s.buf).len() as u8;
        s
    }

    fn push(&mut self, c: char) {
        let mut tmp = [0; 4];
        let b = c.encode_utf8(&mut tmp).as_bytes();
        let at = self.len as usize;
        if at + b.len() <= self.buf.len() {
            self.buf[at..at + b.len()].copy_from_slice(b);
            self.len += b.len() as u8;
        }
    }

    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or(" ")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Cell {
    sym: Sym,
    style: Style,
}

const BLANK: Cell = Cell {
    sym: Sym::BLANK,
    style: Style {
        fg: None,
        bg: None,
        bold: false,
        dim: false,
        italic: false,
        underline: false,
        reverse: false,
        strike: false,
    },
};

pub struct Screen {
    w: u16,
    h: u16,
    cells: Vec<Cell>,
    shown: Vec<Cell>,
    /// Whether the terminal content is unknown and must be redrawn whole.
    stale: bool,
    cursor: Option<(u16, u16)>,
}

impl Screen {
    pub fn new(w: u16, h: u16) -> Self {
        let n = w as usize * h as usize;
        Self {
            w,
            h,
            cells: vec![BLANK; n],
            shown: vec![BLANK; n],
            stale: true,
            cursor: None,
        }
    }

    pub fn width(&self) -> u16 {
        self.w
    }

    pub fn height(&self) -> u16 {
        self.h
    }

    pub fn resize(&mut self, w: u16, h: u16) {
        *self = Screen::new(w, h);
    }

    /// Starts a new frame: all cells blank, no cursor.
    pub fn clear(&mut self) {
        self.cells.fill(BLANK);
        self.cursor = None;
    }

    pub fn set_cursor(&mut self, at: Option<(u16, u16)>) {
        self.cursor = at;
    }

    /// Writes text from column `x` on row `y`, clipped at column `end`.
    /// Returns the column after the last cell written.
    pub fn put(&mut self, x: u16, y: u16, end: u16, text: &str, style: Style) -> u16 {
        let end = end.min(self.w);
        if y >= self.h {
            return x;
        }
        let row = y as usize * self.w as usize;
        let mut x = x;
        for c in text.chars() {
            let c = if c == '\t' { ' ' } else { c };
            let w = c.width().unwrap_or(0) as u16;
            if w == 0 {
                // A zero-width mark joins the character before it.
                if x > 0 && !c.is_control() {
                    let mut at = row + x as usize - 1;
                    if self.cells[at].sym == Sym::CONT && x > 1 {
                        at -= 1;
                    }
                    self.cells[at].sym.push(c);
                }
                continue;
            }
            if x + w > end {
                break;
            }
            self.cells[row + x as usize] = Cell {
                sym: Sym::new(c),
                style,
            };
            if w == 2 {
                self.cells[row + x as usize + 1] = Cell {
                    sym: Sym::CONT,
                    style,
                };
            }
            x += w;
        }
        x
    }

    /// Sends the frame to the terminal.
    pub fn flush(&mut self, out: &mut impl Write) -> io::Result<()> {
        queue!(out, terminal::BeginSynchronizedUpdate, cursor::Hide)?;
        if self.stale {
            queue!(
                out,
                SetAttribute(Attribute::Reset),
                terminal::Clear(terminal::ClearType::All)
            )?;
            self.shown.fill(BLANK);
            self.stale = false;
        }
        let mut style: Option<Style> = None;
        let mut at: Option<(u16, u16)> = None;
        for y in 0..self.h {
            for x in 0..self.w {
                let i = y as usize * self.w as usize + x as usize;
                let cell = self.cells[i];
                if cell == self.shown[i] || cell.sym == Sym::CONT {
                    continue;
                }
                if at != Some((x, y)) {
                    queue!(out, cursor::MoveTo(x, y))?;
                }
                if style != Some(cell.style) {
                    set_style(out, cell.style)?;
                    style = Some(cell.style);
                }
                out.write_all(cell.sym.as_str().as_bytes())?;
                let wide = x + 1 < self.w && self.cells[i + 1].sym == Sym::CONT;
                at = Some((x + 1 + u16::from(wide), y));
            }
        }
        self.shown.copy_from_slice(&self.cells);
        queue!(out, SetAttribute(Attribute::Reset))?;
        if let Some((x, y)) = self.cursor {
            queue!(out, cursor::MoveTo(x, y), cursor::Show)?;
        }
        queue!(out, terminal::EndSynchronizedUpdate)?;
        out.flush()
    }
}

fn set_style(out: &mut impl Write, s: Style) -> io::Result<()> {
    queue!(out, SetAttribute(Attribute::Reset))?;
    if s.bold {
        queue!(out, SetAttribute(Attribute::Bold))?;
    }
    if s.dim {
        queue!(out, SetAttribute(Attribute::Dim))?;
    }
    if s.italic {
        queue!(out, SetAttribute(Attribute::Italic))?;
    }
    if s.underline {
        queue!(out, SetAttribute(Attribute::Underlined))?;
    }
    if s.reverse {
        queue!(out, SetAttribute(Attribute::Reverse))?;
    }
    if s.strike {
        queue!(out, SetAttribute(Attribute::CrossedOut))?;
    }
    if let Some(c) = s.fg {
        queue!(out, SetForegroundColor(c))?;
    }
    if let Some(c) = s.bg {
        queue!(out, SetBackgroundColor(c))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(s: &Screen, y: u16) -> String {
        let r = y as usize * s.w as usize;
        s.cells[r..r + s.w as usize]
            .iter()
            .map(|c| c.sym.as_str())
            .collect()
    }

    #[test]
    fn wide_and_combining_characters() {
        let mut s = Screen::new(6, 1);
        let end = s.put(0, 0, 6, "a界e\u{301}xyz", Style::default());
        assert_eq!(end, 6);
        assert_eq!(row(&s, 0), "a界e\u{301}xy");
    }

    #[test]
    fn wide_character_does_not_split_at_the_edge() {
        let mut s = Screen::new(3, 1);
        assert_eq!(s.put(0, 0, 3, "ab界", Style::default()), 2);
    }

    #[test]
    fn second_frame_sends_only_changes() {
        let mut s = Screen::new(10, 2);
        let mut first = Vec::new();
        s.put(0, 0, 10, "hello", Style::default());
        s.flush(&mut first).unwrap();
        s.clear();
        s.put(0, 0, 10, "hellO", Style::default());
        let mut second = Vec::new();
        s.flush(&mut second).unwrap();
        let text = String::from_utf8_lossy(&second);
        assert!(text.contains('O') && !text.contains("hell"), "{text:?}");
    }
}
