//! Landlock, Linux's own way for a process to give up file access: the
//! model's shell commands get what the model may see and little else. No
//! sandbox, container or other user; the kernel checks every open, also
//! by programs the command starts.
//!
//! Granted: reading and running the system (all of `/` but the homes),
//! the toolchains and caches in the home directory, the directories the
//! model may use, and the temporary directory; writing only to those
//! directories, the caches and the temporary directory.
//!
//! A hidden path, the agent's own files with its keys among them, reads
//! as empty: the command gets a mount namespace of its own, where an empty
//! file or directory is mounted over it. Where the system does not let a
//! user make one, Landlock alone keeps it out: a directory with a hidden
//! path inside is then granted entry by entry, and cannot itself be listed
//! or have files made in it. The same holds for `/` and the homes.

use std::path::{Path, PathBuf};

/// Directories under the home directory that programs need to build and
/// run: read and written.
const HOME_TOOLS: &[&str] = &[
    ".cargo",
    ".rustup",
    ".npm",
    ".nvm",
    ".pnpm-store",
    ".yarn",
    ".bun",
    ".deno",
    ".cache",
    ".gradle",
    ".m2",
    ".pyenv",
    ".rbenv",
    ".gem",
    ".sdkman",
    "go",
    ".local/bin",
    ".local/lib",
    ".local/share/pnpm",
    ".gitconfig",
    ".config/git",
];

/// Paths that get access, with what they get; hidden paths inside are
/// left out by `grant`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct Plan {
    pub read: Vec<PathBuf>,
    pub write: Vec<PathBuf>,
    pub hidden: Vec<PathBuf>,
    /// Left out of reading the system, but not of what is granted inside.
    pub homes: Vec<PathBuf>,
}

/// What a command may touch, for the model's access rules.
pub fn plan(roots: &[PathBuf], hidden: &[PathBuf]) -> Plan {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let mut hidden: Vec<PathBuf> = hidden.iter().map(|p| real(p)).collect();
    // The agent's settings and data: keys, conversations, journals.
    hidden.push(real(&crate::settings::dir()));
    hidden.push(real(&crate::journal::data_dir()));
    let mut write: Vec<PathBuf> = roots.iter().map(|p| real(p)).collect();
    write.push(real(&std::env::temp_dir()));
    write.extend(["/tmp", "/var/tmp", "/dev"].map(PathBuf::from));
    let read = vec![PathBuf::from("/")];
    let mut homes = vec![PathBuf::from("/root")];
    if let Some(home) = &home {
        let home = real(home);
        for t in HOME_TOOLS {
            let p = home.join(t);
            if p.exists() {
                write.push(real(&p));
            }
        }
        // Other people's homes and this one are not part of the system.
        if let Some(parent) = home.parent().filter(|p| *p != Path::new("/")) {
            homes.push(parent.to_path_buf());
        }
        homes.push(home);
    }
    Plan {
        read,
        write,
        hidden,
        homes,
    }
}

#[cfg(target_os = "linux")]
mod sys {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    const EXECUTE: u64 = 1 << 0;
    const WRITE_FILE: u64 = 1 << 1;
    const READ_FILE: u64 = 1 << 2;
    const READ_DIR: u64 = 1 << 3;
    const TRUNCATE: u64 = 1 << 14;
    const IOCTL_DEV: u64 = 1 << 15;
    /// The rights a rule on a file, not a directory, may hold.
    const FILE: u64 = EXECUTE | WRITE_FILE | READ_FILE | TRUNCATE | IOCTL_DEV;
    const READ: u64 = EXECUTE | READ_FILE | READ_DIR;

    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
    }

    #[repr(C, packed)]
    struct PathBeneath {
        allowed_access: u64,
        parent_fd: i32,
    }

    /// The rights this kernel's Landlock knows, by its version; None when
    /// it has no Landlock or has it turned off.
    fn handled() -> Option<u64> {
        // SAFETY: the version query takes no attribute.
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<RulesetAttr>(),
                0usize,
                1u32,
            )
        };
        let bits = match abi {
            ..=0 => return None,
            1 => 13,
            2 => 14,
            3 | 4 => 15,
            _ => 16,
        };
        Some((1u64 << bits) - 1)
    }

    /// Whether this system can limit commands with Landlock.
    pub fn available() -> bool {
        handled().is_some()
    }

    /// The limits for one command: a Landlock ruleset, and the hidden
    /// paths to mount over when the system lets it.
    pub struct Ruleset(OwnedFd, Option<Mask>);

    impl Ruleset {
        pub fn new(plan: &super::Plan) -> Option<Ruleset> {
            let all = handled()?;
            let mask = can_mask().then(|| Mask::new(&plan.hidden));
            let attr = RulesetAttr {
                handled_access_fs: all,
            };
            // SAFETY: a valid attribute of the size given.
            let fd = unsafe {
                libc::syscall(
                    libc::SYS_landlock_create_ruleset,
                    &attr as *const RulesetAttr,
                    std::mem::size_of::<RulesetAttr>(),
                    0u32,
                )
            };
            if fd < 0 {
                return None;
            }
            // SAFETY: the kernel just returned this descriptor.
            let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
            // What is mounted over needs no leaving out.
            let hidden: &[PathBuf] = if mask.is_some() { &[] } else { &plan.hidden };
            let set = Ruleset(fd, mask);
            let outside: Vec<PathBuf> = plan.homes.iter().chain(hidden).cloned().collect();
            for p in &plan.read {
                set.grant(p, READ & all, &outside);
            }
            for p in &plan.write {
                set.grant(p, all, hidden);
            }
            Some(set)
        }

        /// Grants `rights` on `path` and all under it, but for the hidden
        /// paths: a directory holding one gets its entries granted, each
        /// the same way, and cannot itself be listed or changed.
        fn grant(&self, path: &Path, rights: u64, hidden: &[PathBuf]) {
            if hidden.iter().any(|h| path.starts_with(h)) {
                return;
            }
            if !hidden.iter().any(|h| h.starts_with(path)) {
                return self.add(path, rights);
            }
            // Nothing on the directory itself: a right on it would hold
            // for all under it, the hidden path too.
            let Ok(entries) = std::fs::read_dir(path) else {
                return;
            };
            for e in entries.flatten() {
                let p = e.path();
                // A link is granted where it leads; no further for one
                // that leads into a hidden place.
                let real = p.canonicalize().unwrap_or(p);
                self.grant(&real, rights, hidden);
            }
        }

        fn add(&self, path: &Path, rights: u64) {
            let mut c = path.as_os_str().as_bytes().to_vec();
            c.push(0);
            // SAFETY: a NUL-terminated path.
            let fd = unsafe { libc::open(c.as_ptr().cast(), libc::O_PATH | libc::O_CLOEXEC) };
            if fd < 0 {
                return;
            }
            // SAFETY: the descriptor was just opened.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            let dir = std::fs::metadata(path).is_ok_and(|m| m.is_dir());
            let attr = PathBeneath {
                allowed_access: if dir { rights } else { rights & FILE },
                parent_fd: fd.as_raw_fd(),
            };
            // SAFETY: a valid rule attribute; a failed rule only grants less.
            unsafe {
                libc::syscall(
                    libc::SYS_landlock_add_rule,
                    self.0.as_raw_fd(),
                    1u32,
                    &attr as *const PathBeneath,
                    0u32,
                );
            }
        }

        /// What the child does before it runs the command.
        pub fn in_child(&self) -> impl FnMut() -> std::io::Result<()> + Send + Sync + 'static {
            let fd = self.0.as_raw_fd();
            let mask = self.1.clone();
            move || {
                if let Some(m) = &mask {
                    m.apply()?;
                }
                restrict(fd)
            }
        }
    }

    /// Hidden paths, with what the child needs to mount over them, made
    /// before the fork: the child only makes system calls.
    #[derive(Clone)]
    struct Mask {
        files: Vec<CString>,
        dirs: Vec<CString>,
        uid_map: Vec<u8>,
        gid_map: Vec<u8>,
    }

    fn cstr(p: &Path) -> CString {
        CString::new(p.as_os_str().as_bytes()).unwrap_or_default()
    }

    impl Mask {
        fn new(hidden: &[PathBuf]) -> Mask {
            let (mut files, mut dirs) = (Vec::new(), Vec::new());
            for h in hidden {
                match std::fs::metadata(h) {
                    Ok(m) if m.is_dir() => dirs.push(cstr(h)),
                    Ok(_) => files.push(cstr(h)),
                    Err(_) => {}
                }
            }
            // SAFETY: plain getters.
            let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
            Mask {
                files,
                dirs,
                uid_map: format!("{uid} {uid} 1\n").into_bytes(),
                gid_map: format!("{gid} {gid} 1\n").into_bytes(),
            }
        }

        /// A user and mount namespace of the child's own, with nothing of
        /// it seen outside; then the hidden paths mounted over.
        fn apply(&self) -> std::io::Result<()> {
            enter(&self.uid_map, &self.gid_map)?;
            let null = c"/dev/null";
            let tmpfs = c"tmpfs";
            for f in &self.files {
                // SAFETY: NUL-terminated paths.
                check(unsafe {
                    libc::mount(
                        null.as_ptr(),
                        f.as_ptr(),
                        std::ptr::null(),
                        libc::MS_BIND,
                        std::ptr::null(),
                    )
                })?;
            }
            for d in &self.dirs {
                let flags = libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
                // SAFETY: NUL-terminated strings.
                check(unsafe {
                    libc::mount(
                        tmpfs.as_ptr(),
                        d.as_ptr(),
                        tmpfs.as_ptr(),
                        flags,
                        c"size=4k,mode=000".as_ptr().cast(),
                    )
                })?;
            }
            Ok(())
        }
    }

    fn check(r: i32) -> std::io::Result<()> {
        if r == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// Unshares the user and mount namespaces, keeping the same user, and
    /// makes every mount private so none made here reaches the system.
    fn enter(uid_map: &[u8], gid_map: &[u8]) -> std::io::Result<()> {
        // SAFETY: plain system calls on prepared, NUL-terminated data.
        unsafe {
            check(libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS))?;
            for (path, data) in [
                (c"/proc/self/setgroups", &b"deny"[..]),
                (c"/proc/self/uid_map", uid_map),
                (c"/proc/self/gid_map", gid_map),
            ] {
                let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                if fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let n = libc::write(fd, data.as_ptr().cast(), data.len());
                libc::close(fd);
                if n != data.len() as isize {
                    return Err(std::io::Error::last_os_error());
                }
            }
            check(libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            ))
        }
    }

    /// Whether this system lets a user make the namespaces `Mask` needs;
    /// tried once, on a child that does nothing else.
    pub fn can_mask() -> bool {
        static CAN: OnceLock<bool> = OnceLock::new();
        *CAN.get_or_init(|| {
            use std::os::unix::process::CommandExt;
            let m = Mask::new(&[]);
            let mut cmd = std::process::Command::new("/bin/sh");
            cmd.arg("-c").arg("exit 0");
            // SAFETY: only system calls in the child.
            unsafe {
                cmd.pre_exec(move || enter(&m.uid_map, &m.gid_map));
            }
            cmd.stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
    }

    /// In the child, after fork and before exec: only system calls.
    fn restrict(fd: i32) -> std::io::Result<()> {
        // SAFETY: plain system calls on integers.
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::syscall(libc::SYS_landlock_restrict_self, fd, 0u32) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
pub use sys::{Ruleset, available};

/// Limits the command to the ruleset when it starts.
#[cfg(target_os = "linux")]
pub fn apply(cmd: &mut std::process::Command, set: &Ruleset) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the child makes only system calls, as a forked child may.
    unsafe {
        cmd.pre_exec(set.in_child());
    }
}

/// Other systems have no Landlock: no ruleset is ever made.
#[cfg(not(target_os = "linux"))]
pub struct Ruleset;

#[cfg(not(target_os = "linux"))]
impl Ruleset {
    pub fn new(_: &Plan) -> Option<Ruleset> {
        None
    }
}

#[cfg(not(target_os = "linux"))]
pub fn apply(_: &mut std::process::Command, _: &Ruleset) {}

#[cfg(not(target_os = "linux"))]
pub fn available() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plan_leaves_out_the_homes_and_the_agents_files() {
        let cwd = std::env::temp_dir();
        let p = plan(std::slice::from_ref(&cwd), &[cwd.join("secret")]);
        assert_eq!(p.read, [PathBuf::from("/")]);
        assert!(p.hidden.iter().any(|h| h.ends_with("secret")));
        assert!(
            p.hidden.contains(
                &crate::journal::data_dir()
                    .canonicalize()
                    .unwrap_or(crate::journal::data_dir())
            )
        );
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home).canonicalize().unwrap();
            assert!(p.homes.contains(&home));
        }
    }

    /// Runs `sh -c script` under the rules for `dir` with `hidden` hidden.
    #[cfg(target_os = "linux")]
    fn run(dir: &Path, hidden: &[PathBuf], script: &str) -> Option<String> {
        let set = Ruleset::new(&plan(&[dir.to_path_buf()], hidden))?;
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(script).current_dir(dir);
        apply(&mut cmd, &set);
        let out = cmd.output().unwrap();
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn commands_reach_only_what_the_model_may() {
        let dir = std::env::temp_dir().join(format!("landlock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.txt"), "a").unwrap();
        std::fs::write(dir.join(".env"), "KEY").unwrap();
        let dir = dir.canonicalize().unwrap();
        let script = "cat src/a.txt; echo; [ -s .env ] && cat .env || echo hidden; \
                      echo b > src/b.txt && cat src/b.txt; echo c > c.txt && cat c.txt; \
                      ls /usr >/dev/null && echo system; cat \"$HOME/.bashrc\" >/dev/null 2>&1 || echo home; \
                      ls \"$HOME/.local/share\" >/dev/null 2>&1 || echo listing";
        let on = std::fs::read_to_string("/sys/kernel/security/lsm")
            .is_ok_and(|l| l.split(',').any(|m| m.trim() == "landlock"));
        let Some(out) = run(&dir, &[dir.join(".env")], script) else {
            assert!(!on, "Landlock is on, yet no ruleset was made");
            return;
        };
        // Without namespaces no file can be made next to a hidden one.
        let c = if sys::can_mask() { "c\n" } else { "" };
        assert_eq!(out, format!("a\nhidden\nb\n{c}system\nhome\nlisting"));
        assert_eq!(std::fs::read_to_string(dir.join(".env")).unwrap(), "KEY");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
