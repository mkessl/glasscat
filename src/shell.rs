//! The model's shell: a command line run by bash in the working
//! directory, with Landlock limiting it to what the model may see. A
//! command another tool does better is refused: the tools leave out
//! hidden paths and noise, and their changes can be taken back.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Whether the model has a shell, and how it is limited.
#[derive(Clone, Copy, Debug, Default, serde::Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Limited by Landlock to what the model may see; no shell where the
    /// system has no Landlock.
    #[default]
    Landlock,
    /// Not limited: commands reach all the user can, hidden paths too.
    Unrestricted,
    Off,
}

/// The tool definition given to the model.
pub fn spec(mode: Mode) -> crate::context::ToolSpec {
    let limits = match mode {
        Mode::Landlock => {
            " Commands can write only in the directories you may use, the temporary directory \
             and tool caches, and cannot read what is hidden from you or the user's home \
             outside those: such access fails with a permission error."
        }
        _ => "",
    };
    crate::context::ToolSpec {
        name: "shell".into(),
        description: format!(
            "Run a command line with bash in the working directory: builds, tests, git, \
             package managers, scripts. Not for reading, listing, finding, searching, editing, \
             writing, moving or deleting files: the other tools do that, and such commands are \
             refused. stdout and stderr come back together, the middle cut when long. No input; \
             what it leaves running in the background is stopped when it ends. Stops after \
             timeout seconds, {TIMEOUT} unless given, {TIMEOUT_MAX} at most.{limits}"
        ),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "timeout": { "type": "integer", "description": "Seconds" },
            },
            "required": ["command"],
        }),
    }
}

/// How long a command may run unless the model asks for more, and at most.
pub const TIMEOUT: u64 = 120;
pub const TIMEOUT_MAX: u64 = 1800;

/// The tool for what the command line starts with, if one does it:
/// the first program of each command in it, after `cd` and variable
/// settings; what reads another program's output through a pipe is fine.
pub fn duplicate(line: &str) -> Option<&'static str> {
    let mut start = true;
    for word in words(line) {
        match word.as_str() {
            "&&" | "||" | ";" | "\n" | "&" => {
                start = true;
                continue;
            }
            "|" | "|&" => {
                start = false;
                continue;
            }
            _ => {}
        }
        if !start {
            continue;
        }
        if word.contains('=') && !word.starts_with('=') || word == "sudo" || word == "env" {
            continue;
        }
        start = false;
        let name = word.rsplit('/').next().unwrap_or(&word);
        let tool = match name {
            "cat" | "head" | "tail" | "less" | "more" | "bat" | "nl" => "read",
            "ls" | "tree" | "dir" | "exa" | "eza" => "list",
            "find" | "fd" | "fdfind" | "locate" => "find",
            "grep" | "egrep" | "fgrep" | "rg" | "ag" | "ack" => "search",
            "sed" | "perl" if line.contains(" -i") => "edit",
            "sed" => "read",
            "rm" | "rmdir" | "unlink" => "delete",
            "mv" => "move",
            "touch" => "write",
            _ => continue,
        };
        return Some(tool);
    }
    None
}

/// The words of a command line, roughly as a shell splits them; the
/// operators between commands are words of their own.
pub fn words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut chars = line.chars().peekable();
    let end = |word: &mut String, out: &mut Vec<String>| {
        if !word.is_empty() {
            out.push(std::mem::take(word));
        }
    };
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '\'' | '"') => quote = Some(c),
            (None, '\\') => word.extend(chars.next()),
            (None, ' ' | '\t') => end(&mut word, &mut out),
            (None, '\n' | ';' | '(' | ')' | '{' | '}') => {
                end(&mut word, &mut out);
                out.push(if c == '\n' { "\n" } else { ";" }.into());
            }
            (None, '&' | '|') => {
                end(&mut word, &mut out);
                let mut op = c.to_string();
                if chars.peek() == Some(&c) || (c == '|' && chars.peek() == Some(&'&')) {
                    op.extend(chars.next());
                }
                // `2>&1` and `&>` are redirections, not operators.
                if op == "&" && out.last().is_some_and(|w| w.ends_with('>')) {
                    word = out.pop().unwrap_or_default() + "&";
                    continue;
                }
                out.push(op);
            }
            (None, c) => word.push(c),
        }
    }
    end(&mut word, &mut out);
    out
}

/// Runs `line` with bash, or sh where there is no bash, in `cwd`.
pub fn run(
    line: &str,
    cwd: &Path,
    timeout: Duration,
    cancel: &AtomicBool,
    rules: Option<&crate::landlock::Ruleset>,
) -> String {
    let shell = ["/bin/bash", "/usr/bin/bash", "/bin/sh"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .unwrap_or("sh");
    let mut cmd = std::process::Command::new(shell);
    cmd.arg("-c").arg(line).current_dir(cwd);
    // Programs that would wait for a pager or an editor do not.
    cmd.env("PAGER", "cat")
        .env("GIT_PAGER", "cat")
        .env("GIT_EDITOR", "true")
        .env("EDITOR", "true")
        .env("TERM", "dumb");
    if let Some(rules) = rules {
        crate::landlock::apply(&mut cmd, rules);
    }
    match crate::custom::spawn(cmd) {
        Ok(child) => crate::custom::finish(child, timeout, cancel),
        Err(e) => format!("error: could not start {shell}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_a_tool_does_is_refused() {
        assert_eq!(duplicate("cat src/main.rs"), Some("read"));
        assert_eq!(duplicate("cd src && ls -la"), Some("list"));
        assert_eq!(duplicate("FOO=1 grep -rn x ."), Some("search"));
        assert_eq!(duplicate("sed -i 's/a/b/' f"), Some("edit"));
        assert_eq!(duplicate("cargo build; rm -rf target"), Some("delete"));
        assert_eq!(duplicate("/usr/bin/find . -name x"), Some("find"));
        assert_eq!(duplicate("cargo test 2>&1 | tail -20"), None);
        assert_eq!(duplicate("git log | head"), None);
        assert_eq!(duplicate("echo 'a; cat b'"), None);
        assert_eq!(duplicate("cargo test 2>&1 && git status"), None);
        assert_eq!(duplicate("python3 -m pytest"), None);
    }

    #[test]
    #[cfg(unix)]
    fn runs_and_stops() {
        let dir = std::env::temp_dir();
        let stop = AtomicBool::new(false);
        let t = Duration::from_secs(5);
        let out = run("echo hi; exit 3", &dir, t, &stop, None);
        assert!(out.ends_with("\nhi") && out.contains('3'), "{out}");
        // What the command leaves running in the background ends with it.
        let started = std::time::Instant::now();
        let out = run("sleep 30 & echo bg", &dir, t, &stop, None);
        assert!(out.ends_with("bg"), "{out}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
