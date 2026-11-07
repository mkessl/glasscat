//! What the model keeps between conversations: a few short notes of its
//! own, about the user and about each project, in the system prompt of
//! every new conversation; and every past conversation, searched when a
//! detail is needed. Notes carry the date they were written, so an old one
//! can be checked against the conversations of that day rather than
//! trusted. The notes are plain files the user may read and edit.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::context::{Item, ToolSpec};

/// What the notes of each kind may take, in characters: they go into
/// every request, so they stay few and dense.
const USER_MAX: usize = 1_400;
const PROJECT_MAX: usize = 2_200;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Scope {
    User,
    Project,
}

impl Scope {
    fn parse(s: Option<&str>) -> Result<Self, String> {
        match s {
            Some("user") => Ok(Scope::User),
            Some("project") | None => Ok(Scope::Project),
            Some(o) => Err(format!("scope is user or project, not {o}")),
        }
    }

    fn max(self) -> usize {
        match self {
            Scope::User => USER_MAX,
            Scope::Project => PROJECT_MAX,
        }
    }

    fn path(self, cwd: &Path) -> PathBuf {
        match self {
            Scope::User => crate::settings::dir().join("memory.md"),
            Scope::Project => {
                let name = crate::history::dir_for(cwd);
                let name = name.file_name().unwrap_or_default();
                crate::journal::data_dir()
                    .join("memory")
                    .join(format!("{}.md", name.to_string_lossy()))
            }
        }
    }
}

/// The notes in a file: each starts with `- `; lines after it that do not
/// belong to it.
fn entries(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(e) = line.strip_prefix("- ") {
            out.push(e.to_string());
        } else if let Some(last) = out.last_mut().filter(|_| !line.trim().is_empty()) {
            last.push('\n');
            last.push_str(line.trim_start());
        }
    }
    out
}

fn render(entries: &[String]) -> String {
    entries
        .iter()
        .map(|e| format!("- {}\n", e.replace('\n', "\n  ")))
        .collect()
}

fn size(entries: &[String]) -> usize {
    entries.iter().map(|e| e.chars().count()).sum()
}

/// The notes for the system prompt of a new conversation in `cwd`.
pub fn snapshot(cwd: &Path) -> String {
    snapshot_from(&Scope::User.path(cwd), &Scope::Project.path(cwd))
}

fn snapshot_from(user: &Path, project: &Path) -> String {
    let mut out = String::from(
        "# Memory\nYour notes from earlier conversations, as of their start; each has the date \
         it was written, and an old one may be out of date. The memory tool changes them; the \
         recall tool searches past conversations.",
    );
    for (scope, path, title) in [
        (Scope::User, user, "About the user"),
        (Scope::Project, project, "About this project"),
    ] {
        let e = entries(&fs::read_to_string(path).unwrap_or_default());
        let pct = size(&e) * 100 / scope.max();
        out += &format!("\n\n## {title} ({pct}% full)\n");
        out += if e.is_empty() { "(none yet)\n" } else { "" };
        out += &render(&e);
    }
    out.trim_end().to_string()
}

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "memory".into(),
            description: "Keep a note for later conversations, change one or remove one. \
                Notes about the user go to scope user (in every project), the rest to project. \
                Keep, without being asked: the user's preferences and corrections of how you \
                work, facts of the environment, the project's conventions, lessons that cost \
                effort, and what the user asks you to remember. Not: what the code, its history \
                or instruction files already say, trivia, or what matters only now. One dense \
                fact per note. Notes are short, so when one would not fit, remove or shorten \
                old ones first. Changes show in the next conversation; tell the user in a few \
                words what you kept."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["add", "replace", "remove"] },
                    "scope": { "type": "string", "enum": ["user", "project"] },
                    "text": { "type": "string", "description": "The note, for add and replace" },
                    "old": { "type": "string", "description": "Part of the note to replace or remove, found in only one" },
                },
                "required": ["action", "scope"],
            }),
        },
        ToolSpec {
            name: "recall".into(),
            description: "Search the past conversations held in this project, or in all with \
                everywhere, for words; finds what the user and you wrote, newest first. With \
                conversation, gives that conversation's messages instead."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Words that all appear, in any case" },
                    "everywhere": { "type": "boolean" },
                    "conversation": { "type": "string", "description": "An id from a search" },
                },
            }),
        },
    ]
}

/// The memory tool.
pub fn tool(args: &Value, cwd: &Path) -> Result<String, String> {
    let scope = Scope::parse(args["scope"].as_str())?;
    change(&scope.path(cwd), scope.max(), args, &crate::agent::today())
}

fn change(path: &Path, max: usize, args: &Value, today: &str) -> Result<String, String> {
    let mut e = entries(&fs::read_to_string(path).unwrap_or_default());
    let text = args["text"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let note = || {
        text.map(|t| format!("({today}) {}", t.trim_start_matches("- ")))
            .ok_or("give the note in text")
    };
    let find = |e: &[String]| -> Result<usize, String> {
        let old = args["old"]
            .as_str()
            .filter(|o| !o.is_empty())
            .ok_or("give part of the note in old")?;
        let found: Vec<usize> = (0..e.len()).filter(|&i| e[i].contains(old)).collect();
        match found.as_slice() {
            [i] => Ok(*i),
            [] => Err(format!("no note holds {old:?}")),
            _ => Err(format!(
                "{} notes hold {old:?}; give more of the one meant",
                found.len()
            )),
        }
    };
    let done = match args["action"].as_str() {
        Some("add") => {
            let n = note()?;
            if e.iter().any(|x| {
                x.split_once(") ").map_or(x.as_str(), |s| s.1)
                    == n.split_once(") ").map_or("", |s| s.1)
            }) {
                return Ok("That note is kept already.".into());
            }
            e.push(n);
            "Kept"
        }
        Some("replace") => {
            let i = find(&e)?;
            e[i] = note()?;
            "Replaced"
        }
        Some("remove") => {
            let i = find(&e)?;
            e.remove(i);
            "Removed"
        }
        _ => return Err("action is add, replace or remove".into()),
    };
    let used = size(&e);
    if used > max {
        return Err(format!(
            "the notes would take {used} of {max} characters; remove or shorten some first. \
             They are now:\n{}",
            render(&entries(&fs::read_to_string(path).unwrap_or_default()))
        ));
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    fs::write(path, render(&e)).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!(
        "{done}; the notes take {used} of {max} characters."
    ))
}

/// The recall tool.
pub fn recall(args: &Value, cwd: &Path) -> Result<String, String> {
    let root = crate::journal::data_dir().join("conversations");
    let here = crate::history::dir_for(cwd);
    if let Some(id) = args["conversation"].as_str() {
        return read(&root, id);
    }
    let query = args["query"].as_str().unwrap_or_default();
    let dirs: Vec<PathBuf> = if args["everywhere"].as_bool() == Some(true) {
        fs::read_dir(&root)
            .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default()
    } else {
        vec![here]
    };
    search(&root, &dirs, query)
}

/// What the user and the model wrote in a saved conversation.
fn messages(path: &Path) -> Vec<(&'static str, String)> {
    let Ok(saved) = crate::history::load(path) else {
        return Vec::new();
    };
    saved
        .items
        .into_iter()
        .filter_map(|i| match i {
            Item::User(t) => Some(("user", t)),
            Item::Assistant(r) if !r.text.trim().is_empty() => Some(("you", r.text)),
            _ => None,
        })
        .collect()
}

const HITS_MAX: usize = 20;

fn search(root: &Path, dirs: &[PathBuf], query: &str) -> Result<String, String> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return Err("give words to find in query".into());
    }
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = dirs
        .iter()
        .flat_map(|d| fs::read_dir(d).into_iter().flatten())
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| Some((p.clone(), fs::metadata(&p).and_then(|m| m.modified()).ok()?)))
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.1));
    let mut out = Vec::new();
    for (path, _) in &files {
        let id = path.strip_prefix(root).unwrap_or(path).with_extension("");
        for (who, text) in messages(path) {
            let low = text.to_lowercase();
            if !words.iter().all(|w| low.contains(w.as_str())) {
                continue;
            }
            let at = low.find(words[0].as_str()).unwrap_or(0);
            out.push(format!("{} {who}: {}", id.display(), around(&text, at)));
            if out.len() == HITS_MAX {
                out.push(format!("(more; the first {HITS_MAX} shown)"));
                return Ok(out.join("\n"));
            }
        }
    }
    Ok(match out.is_empty() {
        true => format!("Nothing found in {} conversations.", files.len()),
        false => out.join("\n"),
    })
}

/// About 300 characters of `text` around byte `at`, on one line.
fn around(text: &str, at: usize) -> String {
    let start = text[..at.min(text.len())]
        .char_indices()
        .rev()
        .nth(100)
        .map_or(0, |(i, _)| i);
    let s: String = text[start..].chars().take(300).collect();
    let mut s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if start > 0 {
        s.insert(0, '…');
    }
    if text.len() - start > 300 {
        s.push('…');
    }
    s
}

const READ_MAX: usize = 30_000;

fn read(root: &Path, id: &str) -> Result<String, String> {
    if id.contains("..") {
        return Err("not a conversation id".into());
    }
    let path = root.join(format!("{id}.json"));
    let msgs = messages(&path);
    if msgs.is_empty() {
        return Err(format!("no conversation {id}"));
    }
    let mut out = String::new();
    for (who, text) in msgs {
        out += &format!("{who}: {}\n\n", text.trim());
    }
    if out.len() > READ_MAX {
        let cut = (0..=READ_MAX)
            .rev()
            .find(|&i| out.is_char_boundary(i))
            .unwrap_or(0);
        out.truncate(cut);
        out += "\n(cut here)";
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_are_kept_dated_and_within_their_room() {
        let dir = std::env::temp_dir().join(format!("memory-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("m.md");
        let day = "2026-11-08";
        let add = |t: &str| change(&path, 70, &json!({"action":"add","text":t}), day);
        assert!(add("uses tabs").unwrap().starts_with("Kept"));
        assert_eq!(add("uses tabs").unwrap(), "That note is kept already.");
        add("tests with cargo nextest").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "- (2026-11-08) uses tabs\n- (2026-11-08) tests with cargo nextest\n"
        );
        let full = add("a note far too long to fit in what is left").unwrap_err();
        assert!(
            full.contains("remove or shorten") && full.contains("uses tabs"),
            "{full}"
        );
        let r = json!({"action":"replace","old":"tabs","text":"uses spaces"});
        change(&path, 70, &r, day).unwrap();
        let gone = json!({"action":"remove","old":"nextest"});
        change(&path, 70, &gone, day).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "- (2026-11-08) uses spaces\n"
        );
        let shown = snapshot_from(&dir.join("none.md"), &path);
        assert!(shown.contains("(none yet)") && shown.contains("- (2026-11-08) uses spaces"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn past_conversations_are_searched_and_read() {
        let root = std::env::temp_dir().join(format!("recall-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let here = root.join("proj");
        fs::create_dir_all(&here).unwrap();
        let saved = crate::history::Saved {
            cwd: here.clone(),
            provider: "p".into(),
            model: "m".into(),
            items: vec![
                Item::System("s".into()),
                Item::User("Why does the Build fail on arm?".into()),
                Item::Assistant(crate::context::Reply {
                    text: "The linker flag was wrong.".into(),
                    ..Default::default()
                }),
            ],
        };
        crate::history::save(&here.join("1.json"), &saved).unwrap();
        let found = search(&root, std::slice::from_ref(&here), "build ARM").unwrap();
        assert_eq!(found, "proj/1 user: Why does the Build fail on arm?");
        assert!(
            search(&root, &[here], "nowhere")
                .unwrap()
                .starts_with("Nothing found")
        );
        let all = read(&root, "proj/1").unwrap();
        assert!(all.contains("you: The linker flag was wrong."));
        assert!(read(&root, "../x").is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
