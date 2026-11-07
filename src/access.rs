//! Which paths the model may see. A path outside the allowed roots, or
//! inside a hidden one, does not exist for the model: every tool answers
//! for it exactly as for a missing file.

use std::cell::RefCell;
use std::path::{Component, Path, PathBuf};

thread_local! {
    /// What was kept from the model on this thread, to tell the user.
    static WITHHELD: RefCell<(Vec<String>, usize)> = const { RefCell::new((Vec::new(), 0)) };
}

/// What was kept from the model on this thread since the last call: the
/// paths it asked for and was refused, and how many entries were left out
/// of listings. For the user; the model never sees this.
pub fn take_withheld() -> Option<String> {
    let (paths, entries) = WITHHELD.with(|w| std::mem::take(&mut *w.borrow_mut()));
    let mut parts = paths;
    match entries {
        0 => {}
        1 => parts.push("1 entry left out of a listing".into()),
        n => parts.push(format!("{n} entries left out of listings")),
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// Notes for the user that something was kept from the model.
pub fn withhold(what: String) {
    WITHHELD.with(|w| w.borrow_mut().0.push(what));
}

#[derive(Debug, PartialEq)]
pub enum Seen {
    Shown,
    /// Hidden by this rule.
    Hidden(PathBuf),
    /// Outside the folders the model may use.
    Outside,
}

#[derive(Clone)]
pub struct Access {
    cwd: PathBuf,
    allowed: Vec<PathBuf>,
    hidden: Vec<PathBuf>,
}

impl Access {
    /// `allowed` roots must exist; `hidden` paths need not.
    pub fn new(cwd: &Path, allowed: &[PathBuf], hidden: &[PathBuf]) -> std::io::Result<Self> {
        let cwd = cwd.canonicalize()?;
        let allowed = allowed
            .iter()
            .map(|p| {
                cwd.join(p)
                    .canonicalize()
                    .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", p.display())))
            })
            .collect::<std::io::Result<_>>()?;
        let hidden = hidden.iter().map(|p| real(&cwd.join(p))).collect();
        Ok(Self {
            cwd,
            allowed,
            hidden,
        })
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The folders the model may use.
    pub fn roots(&self) -> &[PathBuf] {
        &self.allowed
    }

    /// The hidden paths, as given.
    pub fn hidden(&self) -> &[PathBuf] {
        &self.hidden
    }

    /// The real path a rule for `path` stands for.
    pub fn rule(&self, path: &Path) -> PathBuf {
        real(&self.cwd.join(path))
    }

    /// Whether the model sees `path`, and if not, why.
    pub fn seen(&self, path: &Path) -> Seen {
        let real = real(&self.cwd.join(path));
        if !self.allowed.iter().any(|a| real.starts_with(a)) {
            return Seen::Outside;
        }
        match self.hidden.iter().find(|h| real.starts_with(h)) {
            Some(h) => Seen::Hidden(h.clone()),
            None => Seen::Shown,
        }
    }

    /// Hides `path` from now on; false if it already was.
    pub fn hide(&mut self, path: &Path) -> bool {
        let real = real(&self.cwd.join(path));
        if self.hidden.contains(&real) {
            return false;
        }
        self.hidden.push(real);
        true
    }

    /// Stops hiding `path`; false if it was not hidden by that path.
    pub fn unhide(&mut self, path: &Path) -> bool {
        let real = real(&self.cwd.join(path));
        let before = self.hidden.len();
        self.hidden.retain(|h| *h != real);
        self.hidden.len() < before
    }

    /// Lets the model use the folder `path` too.
    pub fn allow(&mut self, path: &Path) -> std::io::Result<bool> {
        let real = self.cwd.join(path).canonicalize()?;
        if self.allowed.contains(&real) {
            return Ok(false);
        }
        self.allowed.push(real);
        Ok(true)
    }

    /// The real path a model-given path stands for, if the model may use it.
    /// The error text is the same for hidden and missing paths.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        let real = real(&self.cwd.join(path));
        if self.visible(&real) {
            return Ok(real);
        }
        let why = if self.allowed.iter().any(|a| real.starts_with(a)) {
            "hidden"
        } else {
            "outside the folders the agent may use"
        };
        WITHHELD.with(|w| w.borrow_mut().0.push(format!("{path} ({why})")));
        Err(not_found(path))
    }

    /// Like `visible`, for entries of a listing: those left out are counted
    /// for the user.
    pub fn listed(&self, real: &Path) -> bool {
        let shown = self.visible(real);
        if !shown {
            WITHHELD.with(|w| w.borrow_mut().1 += 1);
        }
        shown
    }

    /// Counts `n` entries left out of a listing made on other threads.
    pub fn left_out(&self, n: usize) {
        if n > 0 {
            WITHHELD.with(|w| w.borrow_mut().1 += n);
        }
    }

    /// Whether a real path may be shown to the model.
    pub fn visible(&self, real: &Path) -> bool {
        self.allowed.iter().any(|a| real.starts_with(a))
            && !self.hidden.iter().any(|h| real.starts_with(h))
    }

    /// How a real path is shown to the model: relative to the working
    /// directory when it is inside it.
    pub fn show(&self, real: &Path) -> String {
        match real.strip_prefix(&self.cwd) {
            Ok(rel) if rel.as_os_str().is_empty() => ".".into(),
            Ok(rel) => shown(rel),
            Err(_) => shown(real),
        }
    }
}

/// A path as text for the model and the user, always with `/` separators.
pub fn shown(p: &Path) -> String {
    let text = p.display().to_string();
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text
    }
}

pub fn not_found(path: &str) -> String {
    format!("{path}: no such file or directory")
}

/// Resolves symlinks in the longest existing prefix of an absolute path
/// and appends the rest, with `.` and `..` removed first. The result is
/// the path the agent will really open, so no link can lead elsewhere.
fn real(path: &Path) -> PathBuf {
    let mut lexical = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                lexical.pop();
            }
            c => lexical.push(c),
        }
    }
    let mut existing = lexical.as_path();
    let mut rest = Vec::new();
    loop {
        if let Ok(r) = existing.canonicalize() {
            let mut r = r;
            r.extend(rest.iter().rev());
            return r;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name);
                existing = parent;
            }
            _ => return lexical,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tree(name: &str) -> (PathBuf, Access) {
        let base = std::env::temp_dir().join(format!("access-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("proj/secret")).unwrap();
        fs::create_dir_all(base.join("outside")).unwrap();
        fs::write(base.join("proj/a.txt"), "a").unwrap();
        fs::write(base.join("proj/secret/key"), "k").unwrap();
        fs::write(base.join("outside/x"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(base.join("outside"), base.join("proj/link")).unwrap();
        let proj = base.join("proj");
        let access = Access::new(
            &proj,
            std::slice::from_ref(&proj),
            &[PathBuf::from("secret")],
        )
        .unwrap();
        (base, access)
    }

    #[test]
    fn resolves_and_hides() {
        let (base, a) = tree("hide");
        assert!(a.resolve("a.txt").is_ok());
        assert!(a.resolve("new/dir/file").is_ok());
        assert_eq!(a.resolve("secret/key"), Err(not_found("secret/key")));
        assert!(a.resolve("secret").is_err());
        assert!(a.resolve("../outside/x").is_err());
        assert!(a.resolve("sub/../../outside/x").is_err());
        assert!(a.resolve(base.join("outside/x").to_str().unwrap()).is_err());
        #[cfg(unix)]
        assert!(a.resolve("link/x").is_err());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn tells_the_user_what_was_withheld() {
        let (base, a) = tree("withheld");
        take_withheld();
        assert!(a.resolve("a.txt").is_ok());
        assert_eq!(take_withheld(), None);
        assert!(a.resolve("secret/key").is_err());
        assert!(a.resolve("../outside/x").is_err());
        assert!(!a.listed(&base.join("proj/secret")));
        assert_eq!(
            take_withheld().as_deref(),
            Some(
                "secret/key (hidden); ../outside/x (outside the folders the agent may use); \
                 1 entry left out of a listing"
            )
        );
        assert_eq!(take_withheld(), None);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn shows_paths_relative_to_cwd() {
        let (base, a) = tree("show");
        let p = a.resolve("a.txt").unwrap();
        assert_eq!(a.show(&p), "a.txt");
        assert_eq!(a.show(a.cwd()), ".");
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn rules_change_while_running() {
        let (base, mut a) = tree("change");
        assert_eq!(a.seen(Path::new("a.txt")), Seen::Shown);
        assert!(
            matches!(a.seen(Path::new("secret/key")), Seen::Hidden(h) if h.ends_with("secret"))
        );
        assert_eq!(a.seen(Path::new("../outside")), Seen::Outside);
        assert!(a.hide(Path::new("a.txt")));
        assert!(!a.hide(Path::new("./a.txt")));
        assert!(a.resolve("a.txt").is_err());
        assert!(a.unhide(Path::new("a.txt")));
        assert!(a.resolve("a.txt").is_ok());
        assert!(a.unhide(Path::new("secret")));
        assert!(a.resolve("secret/key").is_ok());
        assert!(!a.unhide(Path::new("secret")));
        assert!(a.resolve("../outside/x").is_err());
        assert!(a.allow(Path::new("../outside")).unwrap());
        assert!(!a.allow(Path::new("../outside")).unwrap());
        assert!(a.resolve("../outside/x").is_ok());
        assert!(a.allow(Path::new("no-such-dir")).is_err());
        assert!(a.roots().iter().any(|r| r.ends_with("outside")));
        fs::remove_dir_all(base).unwrap();
    }
}
