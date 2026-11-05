//! The model's tools. Each takes JSON arguments and returns text for the
//! model; failures are text too, written so the model can act on them.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use serde_json::{Value, json};

use crate::access::{Access, not_found};
use crate::context::ToolSpec;
use crate::custom::CustomTool;
use crate::journal::Journal;

const READ_MAX_LINES: usize = 2000;
const READ_MAX_BYTES: usize = 50 * 1024;
const LINE_MAX_CHARS: usize = 2000;
const LIST_MAX: usize = 1000;
const FIND_MAX: usize = 1000;
const SEARCH_MAX: usize = 200;
const SEARCH_LINE_CHARS: usize = 300;
const SEARCH_FILE_MAX: u64 = 10 * 1024 * 1024;

pub struct Tools {
    access: Access,
    journal: Journal,
    /// This run's changes, last at the end, for /undo.
    changes: Mutex<Vec<Change>>,
    /// The user's own tools, from the settings.
    custom: std::collections::BTreeMap<String, CustomTool>,
    /// Where reports to the developers are saved.
    pub reports: PathBuf,
}

/// One change to a file and what it takes to undo it.
struct Change {
    path: PathBuf,
    /// What the change was, in a few words: "the edit at line 3".
    what: String,
    /// Hash of the file right after the change, to see later changes;
    /// `None` when the change removed it.
    after: Option<u64>,
    undo: Undo,
}

enum Undo {
    /// The file was created: remove it and the directories made for it,
    /// deepest first.
    Remove { dirs: Vec<PathBuf> },
    /// The file was overwritten: put these bytes back.
    Restore(Vec<u8>),
    /// The file was deleted: write it again with its permissions.
    Recreate(Vec<u8>, fs::Permissions),
    /// The empty directory was deleted: make it again.
    Mkdir,
    /// An edit: put `old` back at these byte offsets, where `new` stands.
    Replace {
        at: Vec<usize>,
        old: String,
        new: String,
    },
}

pub fn specs() -> Vec<ToolSpec> {
    let spec = |name: &str, description: &str, parameters: Value| ToolSpec {
        name: name.into(),
        description: description.into(),
        parameters,
    };
    vec![
        spec(
            "read",
            "Read a text file. Returns up to 2000 lines; use offset and limit for more.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "description": "First line, from 1" },
                    "limit": { "type": "integer", "description": "Number of lines" },
                },
                "required": ["path"],
            }),
        ),
        spec(
            "list",
            "List a directory. Directories end with /.",
            json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Default: ." } },
            }),
        ),
        spec(
            "find",
            "Find files by glob, e.g. *.rs or src/**/mod.rs. Skips what .gitignore ignores.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string", "description": "Directory to search, default ." },
                },
                "required": ["pattern"],
            }),
        ),
        spec(
            "search",
            "Search file contents with a regular expression. Returns path:line: text.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string", "description": "File or directory, default ." },
                    "glob": { "type": "string", "description": "Only files matching this glob" },
                },
                "required": ["pattern"],
            }),
        ),
        spec(
            "write",
            "Write a whole file, creating it and its directories if needed.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                },
                "required": ["path", "content"],
            }),
        ),
        spec(
            "edit",
            "Replace exact text in a file. old must match the file exactly and only once, unless all is true.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old": { "type": "string" },
                    "new": { "type": "string" },
                    "all": { "type": "boolean", "description": "Replace every match" },
                },
                "required": ["path", "old", "new"],
            }),
        ),
        spec(
            "delete",
            "Delete a file or an empty directory.",
            json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            }),
        ),
        crate::report::spec(),
    ]
}

/// Tools that change nothing can run at the same time.
pub fn read_only(name: &str) -> bool {
    matches!(name, "read" | "list" | "find" | "search")
}

/// The text up to the first full stop that ends a sentence, without it.
fn first_sentence(s: &str) -> &str {
    let mut from = 0;
    while let Some(i) = s[from..].find(". ").map(|i| i + from) {
        if !(s[..i].ends_with("e.g") || s[..i].ends_with("i.e")) {
            return &s[..i];
        }
        from = i + 2;
    }
    s.trim_end().trim_end_matches('.')
}

impl Tools {
    pub fn new(access: Access, journal: Journal) -> Self {
        Self {
            access,
            journal,
            changes: Mutex::new(Vec::new()),
            custom: Default::default(),
            reports: crate::journal::data_dir().join("reports"),
        }
    }

    /// Adds the user's own tools; none may take a built-in tool's name.
    pub fn with_custom(
        mut self,
        custom: std::collections::BTreeMap<String, CustomTool>,
    ) -> Result<Self, String> {
        let builtin = specs();
        if let Some(name) = custom
            .keys()
            .find(|n| builtin.iter().any(|s| &s.name == *n))
        {
            return Err(format!(
                "the tool {name} in the settings has a built-in tool's name"
            ));
        }
        self.custom = custom;
        Ok(self)
    }

    /// The definitions given to the model: built-in tools, then the user's.
    pub fn all_specs(&self) -> Vec<ToolSpec> {
        let mut all = specs();
        all.extend(self.custom.iter().map(|(n, t)| crate::custom::spec(n, t)));
        all
    }

    /// The tools the model has, one per line: whether each only reads,
    /// the first sentence it is described with, and for a tool of the
    /// user's own the program it runs.
    pub fn summary(&self) -> String {
        let specs = self.all_specs();
        let width = specs.iter().map(|t| t.name.len()).max().unwrap_or(0);
        let mut out = String::from("The model's tools:");
        for t in &specs {
            let kind = if t.name == crate::report::TOOL {
                "saves  "
            } else if self.reads_only(&t.name) {
                "reads  "
            } else {
                "changes"
            };
            let first = first_sentence(&t.description);
            out += &format!("\n  {:<width$}  {kind}  {first}", t.name);
            if let Some(c) = self.custom.get(&t.name) {
                out += &format!(" (runs {})", c.command.join(" "));
            }
        }
        out
    }

    /// Whether a call only reads, so it may run beside others.
    pub fn reads_only(&self, name: &str) -> bool {
        read_only(name) || self.custom.get(name).is_some_and(|t| t.read_only)
    }

    fn remember(&self, change: Change) {
        self.changes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(change);
    }

    /// Undoes the last change still undone, leaving a file alone if it
    /// was changed after; says what happened in a sentence.
    pub fn undo(&self) -> Result<String, String> {
        let change = self
            .changes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop()
            .ok_or("No changes to undo.")?;
        let shown = self.access.show(&change.path);
        let now = fs::read(&change.path).ok();
        let gone = fs::symlink_metadata(&change.path).is_err();
        let same = match change.after {
            Some(hash) => now.as_deref().map(fnv) == Some(hash),
            None => gone,
        };
        if !same {
            return Err(format!(
                "{shown} was changed after {}, so it was left as it is. /undo again goes one change further back.",
                change.what
            ));
        }
        let io = |e: io::Error| format!("could not undo {} in {shown}: {e}", change.what);
        match &change.undo {
            Undo::Remove { dirs } => {
                fs::remove_file(&change.path).map_err(io)?;
                for d in dirs {
                    // Only while empty: the user may have put files there.
                    let _ = fs::remove_dir(d);
                }
            }
            Undo::Restore(bytes) => write_atomic(&change.path, bytes).map_err(io)?,
            Undo::Recreate(bytes, perms) => {
                if let Some(dir) = change.path.parent() {
                    fs::create_dir_all(dir).map_err(io)?;
                }
                write_atomic(&change.path, bytes).map_err(io)?;
                fs::set_permissions(&change.path, perms.clone()).map_err(io)?;
            }
            Undo::Mkdir => fs::create_dir_all(&change.path).map_err(io)?,
            Undo::Replace { at, old, new } => {
                let mut bytes = now.expect("hash matched");
                for &i in at.iter().rev() {
                    bytes.splice(i..i + new.len(), old.bytes());
                }
                write_atomic(&change.path, &bytes).map_err(io)?;
            }
        }
        let _ = self
            .journal
            .record(json!({ "tool": "undo", "path": change.path, "what": change.what }));
        Ok(format!("Undid {} in {shown}.", change.what))
    }

    pub fn cwd(&self) -> &Path {
        self.access.cwd()
    }

    /// The text of each of these files in the working directory that
    /// exists and is not hidden, with the path as shown to the model.
    pub fn read_visible(&self, names: &[String]) -> Vec<(String, String)> {
        names
            .iter()
            .filter_map(|name| {
                let real = self.access.resolve(name).ok()?;
                let text = fs::read_to_string(&real).ok()?;
                Some((self.access.show(&real), text))
            })
            .collect()
    }

    /// Runs a tool; returns what the model gets back, and a note for the
    /// user only: what was kept from the model, or where a report went.
    pub fn run_noting(
        &self,
        name: &str,
        args: &str,
        cancel: &AtomicBool,
    ) -> (String, Option<String>) {
        if name == crate::report::TOOL {
            let args = serde_json::from_str(args).unwrap_or(Value::Null);
            return match crate::report::save(&self.reports, &args) {
                Ok((back, note)) => (back, Some(note)),
                Err(e) => (format!("error: {e}"), None),
            };
        }
        crate::access::take_withheld();
        let result = match self.custom.get(name) {
            Some(t) => self.run_custom(name, t, args, cancel),
            None => self.run(name, args),
        };
        let kept = crate::access::take_withheld();
        (result, kept.map(|w| format!("Kept from the model: {w}.")))
    }

    fn run_custom(&self, name: &str, t: &CustomTool, args: &str, cancel: &AtomicBool) -> String {
        let args: Value = if args.trim().is_empty() {
            json!({})
        } else {
            match serde_json::from_str(args) {
                Ok(v) => v,
                Err(e) => return format!("error: arguments are not valid JSON: {e}"),
            }
        };
        let cmd = match crate::custom::command_line(t, &args) {
            Ok(c) => c,
            Err(e) => return format!("error: {e}"),
        };
        let result = crate::custom::run(t, &cmd, self.cwd(), cancel);
        let ended = result.lines().next().unwrap_or_default();
        let _ = self
            .journal
            .record(json!({ "tool": name, "command": cmd, "ended": ended }));
        result
    }

    /// Runs a tool and returns what the model gets back.
    pub fn run(&self, name: &str, args: &str) -> String {
        let args: Value = if args.trim().is_empty() {
            json!({})
        } else {
            match serde_json::from_str(args) {
                Ok(v) => v,
                Err(e) => return format!("error: arguments are not valid JSON: {e}"),
            }
        };
        let result = match name {
            "read" => self.read(&args),
            "list" => self.list(&args),
            "find" => self.find(&args),
            "search" => self.search(&args),
            "write" => self.write(&args),
            "edit" => self.edit(&args),
            "delete" => self.delete(&args),
            _ => Err(format!("unknown tool {name}")),
        };
        result.unwrap_or_else(|e| format!("error: {e}"))
    }

    fn read(&self, a: &Value) -> Result<String, String> {
        let path = req(a, "path")?;
        let real = self.access.resolve(path)?;
        let meta = fs::metadata(&real).map_err(|_| not_found(path))?;
        if meta.is_dir() {
            return Err(format!("{path} is a directory; use list"));
        }
        let bytes = fs::read(&real).map_err(|e| io_err(path, e))?;
        if is_binary(&bytes) {
            return Ok(format!("{path} is a binary file of {} bytes", bytes.len()));
        }
        let text = String::from_utf8_lossy(&bytes);
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        if text.is_empty() {
            return Ok(format!("{path} is empty"));
        }
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        let first = opt_usize(a, "offset").unwrap_or(1).max(1);
        if first > total {
            return Err(format!("{path} has {total} lines"));
        }
        let limit = opt_usize(a, "limit")
            .unwrap_or(READ_MAX_LINES)
            .clamp(1, READ_MAX_LINES);
        let mut out = String::new();
        let mut last = first - 1;
        for line in &lines[first - 1..total.min(first - 1 + limit)] {
            if out.len() + line.len() > READ_MAX_BYTES && last >= first {
                break;
            }
            out.push_str(&cut(line, LINE_MAX_CHARS));
            out.push('\n');
            last += 1;
        }
        if last < total {
            out.push_str(&format!(
                "[lines {first}-{last} of {total}; continue with offset {}]\n",
                last + 1
            ));
        }
        Ok(out)
    }

    fn list(&self, a: &Value) -> Result<String, String> {
        let path = opt_str(a, "path").unwrap_or(".");
        let real = self.access.resolve(path)?;
        let entries = fs::read_dir(&real).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => not_found(path),
            _ if real.is_file() => format!("{path} is a file; use read"),
            _ => io_err(path, e),
        })?;
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|e| self.access.listed(&e.path()))
            .map(|e| {
                let mut name = e.file_name().to_string_lossy().into_owned();
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    name.push('/');
                }
                name
            })
            .collect();
        if names.is_empty() {
            return Ok(format!("{path} is empty"));
        }
        names.sort();
        let more = names.len().saturating_sub(LIST_MAX);
        names.truncate(LIST_MAX);
        let mut out = names.join("\n");
        if more > 0 {
            out.push_str(&format!("\n[{more} more entries not shown]"));
        }
        Ok(out)
    }

    fn find(&self, a: &Value) -> Result<String, String> {
        let pattern = req(a, "pattern")?;
        let base = opt_str(a, "path").unwrap_or(".");
        let root = self.access.resolve(base)?;
        let glob = glob_matcher(pattern)?;
        let mut found = Vec::new();
        for file in self.walk(&root, base)? {
            let rel = file.strip_prefix(&root).unwrap_or(&file);
            if glob.is_match(rel) {
                found.push(self.access.show(&file));
            }
        }
        if found.is_empty() {
            return Ok(format!("no files match {pattern}"));
        }
        found.sort();
        let more = found.len().saturating_sub(FIND_MAX);
        found.truncate(FIND_MAX);
        let mut out = found.join("\n");
        if more > 0 {
            out.push_str(&format!("\n[{more} more files not shown]"));
        }
        Ok(out)
    }

    fn search(&self, a: &Value) -> Result<String, String> {
        let pattern = req(a, "pattern")?;
        let re = regex::Regex::new(pattern).map_err(|e| format!("bad pattern: {e}"))?;
        let base = opt_str(a, "path").unwrap_or(".");
        let root = self.access.resolve(base)?;
        let glob = opt_str(a, "glob").map(glob_matcher).transpose()?;
        let mut files = if root.is_file() {
            vec![root.clone()]
        } else {
            self.walk(&root, base)?
        };
        files.sort();
        let mut out = String::new();
        let mut count = 0;
        'files: for file in files {
            let rel = file.strip_prefix(&root).unwrap_or(&file);
            if glob.as_ref().is_some_and(|g| !g.is_match(rel)) {
                continue;
            }
            if fs::metadata(&file).map_or(true, |m| m.len() > SEARCH_FILE_MAX) {
                continue;
            }
            let Ok(bytes) = fs::read(&file) else { continue };
            if is_binary(&bytes) {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);
            let shown = self.access.show(&file);
            for (n, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    if count == SEARCH_MAX {
                        out.push_str(&format!("[stopped at {SEARCH_MAX} matches]\n"));
                        break 'files;
                    }
                    count += 1;
                    let line = cut(line.trim(), SEARCH_LINE_CHARS);
                    out.push_str(&format!("{shown}:{}: {line}\n", n + 1));
                }
            }
        }
        if count == 0 {
            return Ok(format!("no matches for {pattern}"));
        }
        Ok(out)
    }

    fn write(&self, a: &Value) -> Result<String, String> {
        let path = req(a, "path")?;
        let content = req(a, "content")?;
        let real = self.access.resolve(path)?;
        if real.is_dir() {
            return Err(format!("{path} is a directory"));
        }
        let old = fs::read(&real).ok();
        let mut dirs = Vec::new();
        if let Some(dir) = real.parent() {
            dirs = dir
                .ancestors()
                .take_while(|d| !d.exists())
                .map(Path::to_path_buf)
                .collect();
            fs::create_dir_all(dir).map_err(|e| io_err(path, e))?;
        }
        write_atomic(&real, content.as_bytes()).map_err(|e| io_err(path, e))?;
        self.remember(Change {
            path: real.clone(),
            what: if old.is_some() {
                "the overwrite"
            } else {
                "the creation"
            }
            .into(),
            after: Some(fnv(content.as_bytes())),
            undo: match &old {
                Some(bytes) => Undo::Restore(bytes.clone()),
                None => Undo::Remove { dirs },
            },
        });
        let old_text = old.as_deref().map(String::from_utf8_lossy);
        self.journal
            .record(json!({ "tool": "write", "path": real, "old": old_text, "new": content }))
            .map_err(|e| format!("wrote {path}, but could not record the change: {e}"))?;
        let lines = content.lines().count();
        Ok(match old {
            Some(_) => format!("wrote {path} ({lines} lines)"),
            None => format!("created {path} ({lines} lines)"),
        })
    }

    fn delete(&self, a: &Value) -> Result<String, String> {
        let path = req(a, "path")?;
        let real = self.access.resolve(path)?;
        // The path resolves through links; deleting would hit the target.
        let given = self.access.cwd().join(path);
        if fs::symlink_metadata(&given).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!(
                "{path} is a symbolic link; delete cannot remove links yet"
            ));
        }
        if real == self.access.cwd() {
            return Err("that is the working directory; it cannot be deleted".into());
        }
        let meta = fs::symlink_metadata(&real).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => not_found(path),
            _ => io_err(path, e),
        })?;
        let (undo, what) = if meta.is_dir() {
            fs::remove_dir(&real).map_err(|e| match e.kind() {
                io::ErrorKind::DirectoryNotEmpty => {
                    format!("{path} is not empty; delete what is in it first")
                }
                _ => io_err(path, e),
            })?;
            (Undo::Mkdir, "the deletion of the directory")
        } else {
            let bytes = fs::read(&real).map_err(|e| io_err(path, e))?;
            fs::remove_file(&real).map_err(|e| io_err(path, e))?;
            let old = String::from_utf8_lossy(&bytes).into_owned();
            self.journal
                .record(json!({ "tool": "delete", "path": real, "old": old }))
                .map_err(|e| format!("deleted {path}, but could not record the change: {e}"))?;
            (Undo::Recreate(bytes, meta.permissions()), "the deletion")
        };
        if meta.is_dir() {
            self.journal
                .record(json!({ "tool": "delete", "path": real, "dir": true }))
                .map_err(|e| format!("deleted {path}, but could not record the change: {e}"))?;
        }
        self.remember(Change {
            path: real,
            what: what.into(),
            after: None,
            undo,
        });
        Ok(format!("deleted {path}"))
    }

    fn edit(&self, a: &Value) -> Result<String, String> {
        let path = req(a, "path")?;
        let old = req(a, "old")?;
        let new = req(a, "new")?;
        let all = a.get("all").and_then(Value::as_bool).unwrap_or(false);
        if old.is_empty() {
            return Err("old is empty; use write to create a file".into());
        }
        if old == new {
            return Err("old and new are the same".into());
        }
        let real = self.access.resolve(path)?;
        let bytes = fs::read(&real).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => not_found(path),
            _ => io_err(path, e),
        })?;
        let raw = String::from_utf8(bytes)
            .map_err(|_| format!("{path} is not UTF-8 text; edit cannot change it"))?;
        let (bom, text) = match raw.strip_prefix('\u{feff}') {
            Some(t) => ("\u{feff}", t),
            None => ("", raw.as_str()),
        };
        // The model writes `\n`; a CRLF file keeps its line endings.
        let crlf = text.contains("\r\n") && !old.contains('\r');
        let (old, new) = if crlf {
            (old.replace('\n', "\r\n"), new.replace('\n', "\r\n"))
        } else {
            (old.to_string(), new.to_string())
        };
        let at: Vec<usize> = text.match_indices(&old).map(|(i, _)| i).collect();
        let line_of = |i: usize| text[..i].matches('\n').count() + 1;
        match at.len() {
            0 => return Err(miss_hint(path, text, &old, &new)),
            1 => {}
            n if !all => {
                let lines: Vec<String> = at.iter().map(|&i| line_of(i).to_string()).collect();
                return Err(format!(
                    "old matches {n} places in {path} (lines {}); add surrounding lines to pick one, or set all",
                    lines.join(", ")
                ));
            }
            _ => {}
        }
        let first_line = line_of(at[0]);
        let changed = format!("{bom}{}", text.replace(&old, &new));
        write_atomic(&real, changed.as_bytes()).map_err(|e| io_err(path, e))?;
        // Where each replacement now stands in the changed file.
        let now_at = at
            .iter()
            .enumerate()
            .map(|(n, &i)| bom.len() + i - n * old.len() + n * new.len())
            .collect();
        self.remember(Change {
            path: real.clone(),
            what: format!("the edit at line {}", line_of(at[0])),
            after: Some(fnv(changed.as_bytes())),
            undo: Undo::Replace {
                at: now_at,
                old: old.clone(),
                new: new.clone(),
            },
        });
        self.journal
            .record(json!({
                "tool": "edit", "path": real, "line": first_line,
                "count": at.len(), "old": old, "new": new,
            }))
            .map_err(|e| format!("edited {path}, but could not record the change: {e}"))?;
        Ok(match at.len() {
            1 => format!("edited {path} at line {first_line}"),
            n => format!("edited {path}: {n} places, first at line {first_line}"),
        })
    }

    /// Files under `root` the model may see, skipping `.git`, what
    /// `.gitignore` ignores inside a repository, and hidden paths.
    fn walk(&self, root: &Path, shown_as: &str) -> Result<Vec<PathBuf>, String> {
        if !root.is_dir() {
            return Err(format!("{shown_as} is not a directory"));
        }
        let walker = ignore::WalkBuilder::new(root)
            .hidden(false)
            .require_git(true)
            .filter_entry(|e| e.file_name() != ".git")
            .build();
        Ok(walker
            .flatten()
            .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
            .map(|e| e.into_path())
            .filter(|p| self.access.listed(p))
            .collect())
    }
}

/// Why an edit's old text was not found, as far as can be told.
fn miss_hint(path: &str, text: &str, old: &str, new: &str) -> String {
    if !new.is_empty() && text.contains(new) {
        return format!("old is not in {path}, but new already is; the edit may be done");
    }
    let first = old
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if !first.is_empty()
        && let Some(n) = text.lines().position(|l| l.trim() == first)
    {
        return format!(
            "old is not in {path} exactly; its first line matches line {} but the text differs after it or in whitespace. Read the file again and copy the text exactly",
            n + 1
        );
    }
    format!("old is not in {path}; read the file again and copy the text exactly")
}

fn glob_matcher(pattern: &str) -> Result<globset::GlobMatcher, String> {
    // A pattern without a slash matches a file name at any depth.
    let full = if pattern.contains('/') {
        pattern.to_string()
    } else {
        format!("**/{pattern}")
    };
    globset::GlobBuilder::new(&full)
        .literal_separator(true)
        .build()
        .map(|g| g.compile_matcher())
        .map_err(|e| format!("bad glob: {e}"))
}

/// Writes through a temporary file and a rename, so a reader never sees
/// half a file; keeps the old file's permissions.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    fs::write(&tmp, bytes)?;
    if let Ok(meta) = fs::metadata(path) {
        let _ = fs::set_permissions(&tmp, meta.permissions());
    }
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// FNV-1a: a hash that stays the same across builds.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8192)].contains(&0)
}

fn cut(line: &str, max: usize) -> String {
    match line.char_indices().nth(max) {
        Some((i, _)) => format!("{}… [line cut, {} chars]", &line[..i], line.chars().count()),
        None => line.to_string(),
    }
}

fn req<'a>(a: &'a Value, key: &str) -> Result<&'a str, String> {
    a.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string argument {key}"))
}

fn opt_str<'a>(a: &'a Value, key: &str) -> Option<&'a str> {
    a.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn opt_usize(a: &Value, key: &str) -> Option<usize> {
    let v = a.get(key)?;
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        .map(|n| n as usize)
}

fn io_err(path: &str, e: io::Error) -> String {
    format!("{path}: {e}")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        base: PathBuf,
        tools: Tools,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn fixture(name: &str) -> Fixture {
        let base = std::env::temp_dir().join(format!("tools-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let proj = base.join("proj");
        fs::create_dir_all(proj.join("src")).unwrap();
        fs::create_dir_all(proj.join("secret")).unwrap();
        fs::write(
            proj.join("src/main.rs"),
            "fn main() {\n    println!(\"hi\");\n}\n",
        )
        .unwrap();
        fs::write(proj.join("src/lib.rs"), "pub fn a() {}\npub fn b() {}\n").unwrap();
        fs::write(proj.join("secret/key.rs"), "fn main() {}\n").unwrap();
        fs::write(proj.join("bin.dat"), [0u8, 1, 2]).unwrap();
        let access = Access::new(
            &proj,
            std::slice::from_ref(&proj),
            &[PathBuf::from("secret")],
        )
        .unwrap();
        let journal = Journal::new(base.join("journal.jsonl"));
        Fixture {
            base,
            tools: Tools::new(access, journal),
        }
    }

    fn run(f: &Fixture, name: &str, args: Value) -> String {
        f.tools.run(name, &args.to_string())
    }

    #[test]
    fn read_with_range_and_errors() {
        let f = fixture("read");
        assert_eq!(
            run(&f, "read", json!({"path": "src/lib.rs"})),
            "pub fn a() {}\npub fn b() {}\n"
        );
        assert_eq!(
            run(
                &f,
                "read",
                json!({"path": "src/main.rs", "offset": 2, "limit": 1})
            ),
            "    println!(\"hi\");\n[lines 2-2 of 3; continue with offset 3]\n"
        );
        assert_eq!(
            run(&f, "read", json!({"path": "secret/key.rs"})),
            "error: secret/key.rs: no such file or directory"
        );
        assert_eq!(
            run(&f, "read", json!({"path": "nope"})),
            "error: nope: no such file or directory"
        );
        assert!(run(&f, "read", json!({"path": "bin.dat"})).contains("binary"));
        assert!(run(&f, "read", json!({"path": "src"})).contains("directory"));
        assert!(run(&f, "read", json!({})).contains("missing string argument path"));
        assert!(
            f.tools
                .run("read", "{bad")
                .starts_with("error: arguments are not valid JSON")
        );
    }

    #[test]
    fn list_hides_hidden_paths() {
        let f = fixture("list");
        assert_eq!(run(&f, "list", json!({})), "bin.dat\nsrc/");
        assert!(run(&f, "list", json!({"path": "secret"})).contains("no such file"));
    }

    #[test]
    fn find_and_search_skip_hidden_paths() {
        let f = fixture("find");
        assert_eq!(
            run(&f, "find", json!({"pattern": "*.rs"})),
            "src/lib.rs\nsrc/main.rs"
        );
        assert_eq!(
            run(&f, "find", json!({"pattern": "src/m*.rs"})),
            "src/main.rs"
        );
        assert_eq!(
            run(&f, "search", json!({"pattern": "fn main"})),
            "src/main.rs:1: fn main() {\n"
        );
        assert_eq!(
            run(&f, "search", json!({"pattern": "pub fn", "glob": "lib.rs"})),
            "src/lib.rs:1: pub fn a() {}\nsrc/lib.rs:2: pub fn b() {}\n"
        );
        assert!(run(&f, "search", json!({"pattern": "("})).contains("bad pattern"));
    }

    #[test]
    fn undo_goes_back_one_change_at_a_time() {
        let f = fixture("undo");
        let p = f.base.join("proj");
        let lib = fs::read(p.join("src/lib.rs")).unwrap();
        // An edit with several matches whose length changes, an
        // overwrite, and a new file in a new directory.
        let edit =
            json!({"path": "src/lib.rs", "old": "pub fn", "new": "pub(crate) fn", "all": true});
        assert!(run(&f, "edit", edit).starts_with("edited"));
        let main = fs::read(p.join("src/main.rs")).unwrap();
        run(&f, "write", json!({"path": "src/main.rs", "content": "x"}));
        run(
            &f,
            "write",
            json!({"path": "new/deep/a.txt", "content": "a"}),
        );

        assert_eq!(
            f.tools.undo().unwrap(),
            "Undid the creation in new/deep/a.txt."
        );
        assert!(!p.join("new").exists(), "directories made for it go too");
        assert_eq!(
            f.tools.undo().unwrap(),
            "Undid the overwrite in src/main.rs."
        );
        assert_eq!(fs::read(p.join("src/main.rs")).unwrap(), main);
        assert_eq!(
            f.tools.undo().unwrap(),
            "Undid the edit at line 1 in src/lib.rs."
        );
        assert_eq!(fs::read(p.join("src/lib.rs")).unwrap(), lib);
        assert_eq!(f.tools.undo().unwrap_err(), "No changes to undo.");
    }

    #[test]
    fn undo_restores_bom_and_crlf_files_exactly() {
        let f = fixture("undo-bom");
        let path = f.base.join("proj/w.txt");
        let orig = "\u{feff}one\r\ntwo\r\none\r\n";
        fs::write(&path, orig).unwrap();
        let edit = json!({"path": "w.txt", "old": "one\n", "new": "1\n", "all": true});
        assert!(run(&f, "edit", edit).starts_with("edited"));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "\u{feff}1\r\ntwo\r\n1\r\n"
        );
        f.tools.undo().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), orig);
    }

    #[test]
    fn delete_and_undo() {
        let f = fixture("delete");
        let p = f.base.join("proj");
        let lib = fs::read(p.join("src/lib.rs")).unwrap();
        assert_eq!(
            run(&f, "delete", json!({"path": "src/lib.rs"})),
            "deleted src/lib.rs"
        );
        assert!(!p.join("src/lib.rs").exists());
        assert!(run(&f, "delete", json!({"path": "src"})).contains("not empty"));
        fs::create_dir(p.join("empty")).unwrap();
        assert_eq!(run(&f, "delete", json!({"path": "empty"})), "deleted empty");
        assert!(run(&f, "delete", json!({"path": "secret/key.rs"})).contains("no such file"));
        assert!(run(&f, "delete", json!({"path": "."})).contains("working directory"));

        assert_eq!(
            f.tools.undo().unwrap(),
            "Undid the deletion of the directory in empty."
        );
        assert!(p.join("empty").is_dir());
        assert_eq!(f.tools.undo().unwrap(), "Undid the deletion in src/lib.rs.");
        assert_eq!(fs::read(p.join("src/lib.rs")).unwrap(), lib);
    }

    #[cfg(unix)]
    #[test]
    fn delete_refuses_links() {
        let f = fixture("delete-link");
        let p = f.base.join("proj");
        std::os::unix::fs::symlink(p.join("src/lib.rs"), p.join("link.rs")).unwrap();
        assert!(run(&f, "delete", json!({"path": "link.rs"})).contains("symbolic link"));
        assert!(p.join("src/lib.rs").exists());
    }

    #[test]
    fn undo_leaves_a_file_changed_since() {
        let f = fixture("undo-changed");
        let path = f.base.join("proj/src/lib.rs");
        run(
            &f,
            "edit",
            json!({"path": "src/lib.rs", "old": "a()", "new": "c()"}),
        );
        fs::write(&path, "the user's own text").unwrap();
        assert!(
            f.tools
                .undo()
                .unwrap_err()
                .contains("was changed after the edit at line 1")
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "the user's own text");
    }

    #[test]
    fn edit_exact_once_and_journal() {
        let f = fixture("edit");
        assert_eq!(
            run(
                &f,
                "edit",
                json!({"path": "src/main.rs", "old": "\"hi\"", "new": "\"hello\""})
            ),
            "edited src/main.rs at line 2"
        );
        let text = fs::read_to_string(f.base.join("proj/src/main.rs")).unwrap();
        assert!(text.contains("\"hello\""));
        let journal = fs::read_to_string(f.base.join("journal.jsonl")).unwrap();
        let entry: Value = serde_json::from_str(journal.lines().next().unwrap()).unwrap();
        assert_eq!(
            (entry["old"].as_str(), entry["new"].as_str()),
            (Some("\"hi\""), Some("\"hello\""))
        );

        let twice = run(
            &f,
            "edit",
            json!({"path": "src/lib.rs", "old": "pub fn", "new": "fn"}),
        );
        assert!(
            twice.contains("matches 2 places") && twice.contains("lines 1, 2"),
            "{twice}"
        );
        assert_eq!(
            run(
                &f,
                "edit",
                json!({"path": "src/lib.rs", "old": "pub fn", "new": "fn", "all": true})
            ),
            "edited src/lib.rs: 2 places, first at line 1"
        );
        assert!(
            run(
                &f,
                "edit",
                json!({"path": "src/lib.rs", "old": "pub fn", "new": "fn"})
            )
            .contains("new already is")
        );
        assert!(
            run(
                &f,
                "edit",
                json!({"path": "src/main.rs", "old": "fn main() {\n  x", "new": "y"})
            )
            .contains("first line matches line 1")
        );
    }

    #[test]
    fn edit_keeps_crlf_and_bom() {
        let f = fixture("crlf");
        let p = f.base.join("proj/win.txt");
        fs::write(&p, "\u{feff}one\r\ntwo\r\n").unwrap();
        assert_eq!(
            run(
                &f,
                "edit",
                json!({"path": "win.txt", "old": "one\ntwo", "new": "1\n2"})
            ),
            "edited win.txt at line 1"
        );
        assert_eq!(fs::read_to_string(&p).unwrap(), "\u{feff}1\r\n2\r\n");
    }

    #[test]
    fn write_creates_and_overwrites() {
        let f = fixture("write");
        assert_eq!(
            run(
                &f,
                "write",
                json!({"path": "new/dir/a.txt", "content": "x\n"})
            ),
            "created new/dir/a.txt (1 lines)"
        );
        assert_eq!(
            run(
                &f,
                "write",
                json!({"path": "new/dir/a.txt", "content": "y\nz\n"})
            ),
            "wrote new/dir/a.txt (2 lines)"
        );
        assert_eq!(
            fs::read_to_string(f.base.join("proj/new/dir/a.txt")).unwrap(),
            "y\nz\n"
        );
        assert!(
            run(&f, "write", json!({"path": "secret/x", "content": ""})).contains("no such file")
        );
        assert!(
            run(&f, "write", json!({"path": "../escape", "content": ""})).contains("no such file")
        );
        let journal = fs::read_to_string(f.base.join("journal.jsonl")).unwrap();
        assert_eq!(journal.lines().count(), 2);
    }

    #[test]
    fn summary_names_each_tool() {
        let mut f = fixture("summary");
        let check: CustomTool = serde_json::from_value(json!({
            "description": "Builds the project. Takes no arguments.",
            "command": ["cargo", "check"]
        }))
        .unwrap();
        f.tools.custom.insert("check".into(), check);
        let s = f.tools.summary();
        let line = |name: &str| {
            s.lines()
                .find(|l| l.trim_start().starts_with(&format!("{name} ")))
                .unwrap_or_else(|| panic!("{name} missing: {s}"))
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert!(line("read").starts_with("read reads "), "{s}");
        assert!(line("find").contains("e.g. "), "{s}");
        assert!(line("write").starts_with("write changes "), "{s}");
        assert_eq!(
            line("check"),
            "check changes Builds the project (runs cargo check)"
        );
    }
}
