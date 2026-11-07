//! What a shell command changed in the directories the model may use:
//! the tree is looked at right before and right after the command, and
//! each file is kept as it was, so the change can be taken back like a
//! tool's. Kept: every file but those in directories `.gitignore` leaves
//! out as a whole, such as build output and installed packages (an
//! ignored file such as `.env` is kept), and `.git` without its objects,
//! which only grow; each file up to `FILE_MAX`. A file is copied only when it changed since
//! the last look, and on file systems that share blocks a copy costs
//! almost nothing. The copies live for the run and go when it ends.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Larger files are not kept: a command's change to one stays.
const FILE_MAX: u64 = 64 << 20;
/// What the copies may take together.
const TOTAL_MAX: u64 = 2 << 30;

#[derive(Clone, Copy, PartialEq)]
struct Stat {
    len: u64,
    mtime: i128,
    ctime: i128,
    ino: u64,
    dir: bool,
}

struct Known {
    stat: Stat,
    /// The copy, if the file was kept.
    copy: Option<PathBuf>,
}

/// One thing a command changed.
pub enum Found {
    Created(PathBuf),
    CreatedDir(PathBuf),
    /// The file as it was, if it was kept.
    Changed(PathBuf, Option<PathBuf>),
    Deleted(PathBuf, Option<PathBuf>),
    DeletedDir(PathBuf),
}

#[derive(Default)]
pub struct Store {
    dir: Option<PathBuf>,
    next: u64,
    used: u64,
    known: HashMap<PathBuf, Known>,
}

impl Store {
    /// Looks at the tree before a command, copying what changed since
    /// the last look.
    pub fn before(&mut self, roots: &[PathBuf], hidden: &[PathBuf]) {
        let now = walk(roots, hidden);
        let mut known = HashMap::with_capacity(now.len());
        for (path, stat) in now {
            let copy = match self.known.remove(&path) {
                Some(k) if k.stat == stat && (k.copy.is_some() || stat.dir) => k.copy,
                _ if stat.dir => None,
                _ => self.keep(&path, stat.len),
            };
            known.insert(path, Known { stat, copy });
        }
        self.known = known;
    }

    /// What changed since `before`, directories made first and removed
    /// last, so taking the changes back from the end goes in order.
    pub fn after(&self, roots: &[PathBuf], hidden: &[PathBuf]) -> Vec<Found> {
        let now = walk(roots, hidden);
        let mut found = Vec::new();
        let mut made: Vec<&PathBuf> = now
            .iter()
            .filter(|(p, s)| s.dir && !self.known.contains_key(*p))
            .map(|(p, _)| p)
            .collect();
        made.sort();
        found.extend(made.into_iter().cloned().map(Found::CreatedDir));
        let mut files: Vec<(&PathBuf, &Stat)> = now.iter().filter(|(_, s)| !s.dir).collect();
        files.sort_by_key(|f| f.0);
        for (path, stat) in files {
            match self.known.get(path) {
                None => found.push(Found::Created(path.clone())),
                Some(k) if k.stat.dir => found.push(Found::Created(path.clone())),
                Some(k) if k.stat != *stat => {
                    found.push(Found::Changed(path.clone(), k.copy.clone()))
                }
                Some(_) => {}
            }
        }
        let mut gone: Vec<(&PathBuf, &Known)> = self
            .known
            .iter()
            .filter(|(p, k)| now.get(*p).is_none_or(|s| s.dir != k.stat.dir))
            .collect();
        // Files first, then directories deepest first.
        gone.sort_by(|a, b| a.1.stat.dir.cmp(&b.1.stat.dir).then(b.0.cmp(a.0)));
        for (path, k) in gone {
            found.push(match k.stat.dir {
                true => Found::DeletedDir(path.clone()),
                false => Found::Deleted(path.clone(), k.copy.clone()),
            });
        }
        found
    }

    /// Copies a file into the store; None when it is too large or the
    /// store is full.
    fn keep(&mut self, path: &Path, len: u64) -> Option<PathBuf> {
        if len > FILE_MAX || self.used + len > TOTAL_MAX {
            return None;
        }
        let dir = match &self.dir {
            Some(d) => d.clone(),
            None => {
                let d = crate::journal::data_dir().join("snapshots").join(format!(
                    "{}-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs()),
                    std::process::id()
                ));
                fs::create_dir_all(&d).ok()?;
                self.dir = Some(d.clone());
                d
            }
        };
        self.next += 1;
        let copy = dir.join(self.next.to_string());
        fs::copy(path, &copy).ok()?;
        self.used += len;
        Some(copy)
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        if let Some(d) = &self.dir {
            let _ = fs::remove_dir_all(d);
        }
    }
}

/// Every file and directory under `roots` a snapshot covers.
fn walk(roots: &[PathBuf], hidden: &[PathBuf]) -> HashMap<PathBuf, Stat> {
    let mut out = HashMap::new();
    let Some((first, rest)) = roots.split_first() else {
        return out;
    };
    let hidden: Arc<[PathBuf]> = hidden.into();
    // The top `.gitignore` of each root, and git's own exclusions.
    let ignores: Arc<[ignore::gitignore::Gitignore]> = roots
        .iter()
        .map(|r| {
            let mut g = ignore::gitignore::GitignoreBuilder::new(r);
            g.add(r.join(".gitignore"));
            g.add(r.join(".git/info/exclude"));
            g.build()
                .unwrap_or_else(|_| ignore::gitignore::Gitignore::empty())
        })
        .collect();
    let mut b = ignore::WalkBuilder::new(first);
    for r in rest {
        b.add(r);
    }
    b.standard_filters(false)
        .follow_links(false)
        .filter_entry(move |e| {
            let p = e.path();
            if p.ends_with(".git/objects") || hidden.iter().any(|h| p.starts_with(h)) {
                return false;
            }
            if !e.file_type().is_some_and(|t| t.is_dir()) || e.depth() == 0 {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            if matches!(
                &*name,
                "node_modules" | "__pycache__" | ".venv" | ".tox" | ".mypy_cache" | ".pytest_cache"
            ) {
                return false;
            }
            !ignores.iter().any(|g| {
                p.starts_with(g.path()) && g.matched_path_or_any_parents(p, true).is_ignore()
            })
        });
    for e in b.build().flatten() {
        if e.depth() == 0 {
            continue;
        }
        let Ok(m) = e.path().symlink_metadata() else {
            continue;
        };
        out.insert(e.into_path(), stat(&m));
    }
    out
}

#[cfg(unix)]
fn stat(m: &fs::Metadata) -> Stat {
    use std::os::unix::fs::MetadataExt;
    Stat {
        len: m.len(),
        mtime: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
        ctime: i128::from(m.ctime()) * 1_000_000_000 + i128::from(m.ctime_nsec()),
        ino: m.ino(),
        dir: m.is_dir(),
    }
}

#[cfg(not(unix))]
fn stat(m: &fs::Metadata) -> Stat {
    let t = |t: std::io::Result<std::time::SystemTime>| {
        t.ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as i128)
    };
    Stat {
        len: m.len(),
        mtime: t(m.modified()),
        ctime: t(m.created()),
        ino: 0,
        dir: m.is_dir(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_what_changed_and_keeps_what_was() {
        let dir = std::env::temp_dir().join(format!("snapshot-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("old")).unwrap();
        fs::create_dir_all(dir.join("target")).unwrap();
        fs::write(dir.join(".gitignore"), "target/\n.env\n").unwrap();
        fs::write(dir.join("a"), "a").unwrap();
        fs::write(dir.join("b"), "b").unwrap();
        fs::write(dir.join("old/c"), "c").unwrap();
        fs::write(dir.join("secret"), "s").unwrap();
        let roots = [dir.clone()];
        let hidden = [dir.join("secret")];
        let mut s = Store::default();
        s.before(&roots, &hidden);
        fs::write(dir.join("a"), "A!").unwrap();
        fs::remove_file(dir.join("b")).unwrap();
        fs::remove_dir_all(dir.join("old")).unwrap();
        fs::create_dir_all(dir.join("new")).unwrap();
        fs::write(dir.join("new/d"), "d").unwrap();
        fs::write(dir.join("target/out"), "o").unwrap();
        fs::write(dir.join(".env"), "E").unwrap();
        fs::write(dir.join("secret"), "S").unwrap();
        let found: Vec<String> = s
            .after(&roots, &hidden)
            .iter()
            .map(|f| match f {
                Found::Created(p) => format!("+{}", p.strip_prefix(&dir).unwrap().display()),
                Found::CreatedDir(p) => format!("+{}/", p.strip_prefix(&dir).unwrap().display()),
                Found::Changed(p, c) => {
                    let was = fs::read_to_string(c.as_ref().unwrap()).unwrap();
                    format!("~{}={was}", p.strip_prefix(&dir).unwrap().display())
                }
                Found::Deleted(p, c) => {
                    let was = fs::read_to_string(c.as_ref().unwrap()).unwrap();
                    format!("-{}={was}", p.strip_prefix(&dir).unwrap().display())
                }
                Found::DeletedDir(p) => format!("-{}/", p.strip_prefix(&dir).unwrap().display()),
            })
            .collect();
        assert_eq!(
            found,
            [
                "+new/", "+.env", "~a=a", "+new/d", "-old/c=c", "-b=b", "-old/"
            ]
        );
        let store = s.dir.clone().unwrap();
        drop(s);
        assert!(!store.exists(), "the copies go with the run");
        fs::remove_dir_all(dir).unwrap();
    }
}
