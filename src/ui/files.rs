//! The file browser of `/access`: the folders the model may use as a
//! tree, where a path is hidden from the model, or shown again, with one
//! key.

use std::path::{Path, PathBuf};

use crossterm::style::Color;

use super::screen::Style;
use super::text::{clip, width};
use super::transcript::Line;
use crate::access::{Access, Seen};
use crate::tools::Tools;

/// Lines above the tree.
pub const HEAD: usize = 3;

pub struct Files {
    rows: Vec<Row>,
    selected: usize,
    /// The first row shown.
    top: usize,
    /// What the last key did, or why it could not.
    pub note: Option<String>,
}

struct Row {
    path: PathBuf,
    depth: usize,
    dir: bool,
    open: bool,
}

impl Files {
    /// The tree with the working directory open.
    pub fn new(access: &Access) -> Self {
        let rows = access
            .roots()
            .iter()
            .map(|p| Row {
                path: p.clone(),
                depth: 0,
                dir: true,
                open: false,
            })
            .collect();
        let mut f = Self {
            rows,
            selected: 0,
            top: 0,
            note: None,
        };
        if !f.rows.is_empty() {
            f.open(0);
        }
        f
    }

    pub fn up(&mut self, n: usize) {
        self.selected = self.selected.saturating_sub(n);
    }

    pub fn down(&mut self, n: usize) {
        self.selected = self
            .selected
            .saturating_add(n)
            .min(self.rows.len().saturating_sub(1));
    }

    /// Opens the selected folder, or steps into it when it is open.
    pub fn right(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if row.open {
            self.down(1);
        } else if row.dir {
            self.open(self.selected);
        }
    }

    /// Closes the selected folder, or steps out to the one holding it.
    pub fn left(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if row.open {
            self.close(self.selected);
        } else if let Some(parent) = self.rows[..self.selected]
            .iter()
            .rposition(|r| r.depth < row.depth)
        {
            self.selected = parent;
        }
    }

    /// A click on a row of the tree: selects it, and opens or closes a
    /// folder that already was.
    pub fn click(&mut self, row: usize) {
        let i = self.top + row;
        if i >= self.rows.len() {
            return;
        }
        if i == self.selected && self.rows[i].dir {
            if self.rows[i].open {
                self.close(i);
            } else {
                self.open(i);
            }
        }
        self.selected = i;
    }

    /// Hides the selected path from the model, or shows it again.
    pub fn toggle(&mut self, tools: &mut Tools) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let read = tools.touched(&row.path).is_some();
        let access = &mut tools.access;
        let name = access.show(&row.path);
        self.note = Some(match access.seen(&row.path) {
            Seen::Shown => {
                access.hide(&row.path);
                if read {
                    format!(
                        "{name} is hidden from the model now; what it already read stays in the \
                         context, where /edit can take it out."
                    )
                } else {
                    format!("{name} is hidden from the model now.")
                }
            }
            Seen::Hidden(_) if access.unhide(&row.path) => {
                format!("{name} is shown to the model again.")
            }
            Seen::Hidden(by) => format!(
                "{name} is hidden because {} is; that one can be shown again.",
                access.show(&by)
            ),
            Seen::Outside => format!(
                "{name} leads outside the folders the model may use; /access allow PATH adds one."
            ),
        });
    }

    fn open(&mut self, i: usize) {
        let row = &self.rows[i];
        match children(&row.path) {
            Ok(kids) => {
                let depth = row.depth + 1;
                let kids = kids.into_iter().map(|(path, dir)| Row {
                    path,
                    depth,
                    dir,
                    open: false,
                });
                self.rows.splice(i + 1..i + 1, kids);
                self.rows[i].open = true;
            }
            Err(e) => self.note = Some(format!("{}: {e}", row.path.display())),
        }
    }

    fn close(&mut self, i: usize) {
        let depth = self.rows[i].depth;
        let end = i
            + 1
            + self.rows[i + 1..]
                .iter()
                .take_while(|r| r.depth > depth)
                .count();
        self.rows.drain(i + 1..end);
        self.rows[i].open = false;
    }

    /// The screen: a header, then as much of the tree as `height` holds,
    /// scrolled to keep the selected row in view.
    pub fn lines(&mut self, tools: &Tools, w: usize, height: usize) -> Vec<Line> {
        let access = &tools.access;
        let hidden: Vec<String> = access.hidden().iter().map(|h| access.show(h)).collect();
        let mut out = vec![
            Line::new(
                clip(
                    "Access · what the model may see · changes last for this session unless kept with s",
                    w,
                ),
                Style::fg(Color::Yellow),
            ),
            Line::new(
                clip(
                    &if hidden.is_empty() {
                        "Nothing is hidden.".to_string()
                    } else {
                        format!("Hidden: {}", hidden.join(", "))
                    },
                    w,
                ),
                Style::dim(),
            ),
            Line::new(String::new(), Style::default()),
        ];
        let rows = height.saturating_sub(HEAD).max(1);
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.selected >= self.top + rows {
            self.top = self.selected + 1 - rows;
        }
        self.top = self.top.min(self.rows.len().saturating_sub(rows));
        for (i, row) in self.rows.iter().enumerate().skip(self.top).take(rows) {
            let mark = match (row.dir, row.open) {
                (true, true) => "▾ ",
                (true, false) => "▸ ",
                _ => "  ",
            };
            let name = if row.depth == 0 {
                row.path.display().to_string()
            } else {
                row.path
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
            };
            let slash = if row.dir && row.depth > 0 { "/" } else { "" };
            let (rule, mut style) = match access.seen(&row.path) {
                Seen::Shown => ("", Style::default()),
                Seen::Hidden(by) if by == row.path || by == real(&row.path) => {
                    ("hidden", Style::dim())
                }
                Seen::Hidden(_) => ("", Style::dim()),
                Seen::Outside => ("outside", Style::dim()),
            };
            let tag = [tools.touched(&row.path).unwrap_or(""), rule]
                .into_iter()
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join(" · ");
            let tag = clip(&tag, w);
            if i == self.selected {
                style.reverse = true;
            }
            let left = clip(
                &format!("{}{mark}{name}{slash}", "  ".repeat(row.depth)),
                w.saturating_sub(width(&tag) + 1),
            );
            let gap = w.saturating_sub(width(&left) + width(&tag));
            out.push(Line::new(format!("{left}{}{tag}", " ".repeat(gap)), style));
        }
        out
    }
}

/// A folder's entries, folders first, each by name.
fn children(dir: &Path) -> std::io::Result<Vec<(PathBuf, bool)>> {
    let mut kids: Vec<(PathBuf, bool)> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| {
            let p = e.path();
            let dir = p.is_dir();
            (p, dir)
        })
        .collect();
    kids.sort_by_key(|(p, dir)| (!dir, p.file_name().map(|n| n.to_ascii_lowercase())));
    Ok(kids)
}

fn real(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_and_shows_from_the_tree() {
        let base = std::env::temp_dir().join(format!("files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src/main.rs"), "").unwrap();
        std::fs::write(base.join(".env"), "").unwrap();
        let base = base.canonicalize().unwrap();
        let access = Access::new(&base, std::slice::from_ref(&base), &[]).unwrap();
        let journal = crate::journal::Journal::new(base.join("journal.jsonl"));
        let mut access = Tools::new(access, journal);
        let mut f = Files::new(&access.access);
        let text = |f: &mut Files, a: &Tools| -> Vec<String> {
            f.lines(a, 40, 10)
                .iter()
                .map(|l| l.text().trim_end().to_string())
                .collect()
        };
        let t = text(&mut f, &access);
        assert_eq!(t[1], "Nothing is hidden.");
        assert!(t[3].starts_with("▾ /"), "{t:?}");
        assert_eq!(&t[4..], ["  ▸ src/", "    .env"]);
        f.down(1);
        f.right();
        assert_eq!(
            &text(&mut f, &access)[4..],
            ["  ▾ src/", "      main.rs", "    .env"]
        );
        f.toggle(&mut access);
        assert_eq!(f.note.as_deref(), Some("src is hidden from the model now."));
        let t = text(&mut f, &access);
        assert_eq!(t[1], "Hidden: src");
        assert!(t[4].ends_with("hidden"), "{t:?}");
        f.down(1);
        f.toggle(&mut access);
        assert!(f.note.as_deref().unwrap().contains("because src is"));
        f.left();
        f.toggle(&mut access);
        assert_eq!(access.access.seen(Path::new("src/main.rs")), Seen::Shown);
        f.left();
        assert_eq!(&text(&mut f, &access)[4..], ["  ▸ src/", "    .env"]);
        // What the model read is marked, and hiding it says so.
        access.run("read", r#"{"path": ".env"}"#);
        f.down(usize::MAX);
        assert!(text(&mut f, &access)[5].ends_with("read"));
        f.toggle(&mut access);
        assert!(f.note.as_deref().unwrap().contains("/edit"));
        assert!(text(&mut f, &access)[5].ends_with("read · hidden"));
        assert_eq!(text(&mut f, &access).len(), 6);
        assert!(
            f.lines(&access, 5, 10)
                .iter()
                .all(|l| width(&l.text()) <= 5)
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
