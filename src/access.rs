//! Which paths the model may see. A path outside the allowed roots, or
//! inside a hidden one, does not exist for the model: every tool answers
//! for it exactly as for a missing file.

use std::path::{Component, Path, PathBuf};

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
            .map(|p| cwd.join(p).canonicalize())
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

    /// The real path a model-given path stands for, if the model may use it.
    /// The error text is the same for hidden and missing paths.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        let real = real(&self.cwd.join(path));
        if self.visible(&real) {
            Ok(real)
        } else {
            Err(not_found(path))
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
            Ok(rel) => rel.display().to_string(),
            Err(_) => real.display().to_string(),
        }
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
    fn shows_paths_relative_to_cwd() {
        let (base, a) = tree("show");
        let p = a.resolve("a.txt").unwrap();
        assert_eq!(a.show(&p), "a.txt");
        assert_eq!(a.show(a.cwd()), ".");
        fs::remove_dir_all(base).unwrap();
    }
}
