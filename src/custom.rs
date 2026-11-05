//! Tools the user defines in the settings: a program run with the model's
//! arguments put into its command line. No shell is involved, so an
//! argument is never parsed as shell syntax.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::context::ToolSpec;

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CustomTool {
    /// What the model is told the tool does.
    pub description: String,
    /// The program and its arguments. `{name}` in an argument becomes the
    /// model's argument `name`; an argument that is only `{name}` is left
    /// out when the model gives none, and becomes several when it gives a
    /// list.
    pub command: Vec<String>,
    /// JSON Schema of the arguments; none means the tool takes none.
    #[serde(default)]
    pub parameters: Option<Value>,
    /// Whether it only reads, so it may run beside other reading calls.
    #[serde(default)]
    pub read_only: bool,
    /// Seconds before the program is stopped.
    #[serde(default = "default_timeout")]
    pub timeout: u64,
}

fn default_timeout() -> u64 {
    120
}

/// Output beyond this many bytes is cut from the middle: the start says
/// what ran, the end usually holds the errors.
const MAX_OUTPUT: usize = 30_000;

pub fn spec(name: &str, t: &CustomTool) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: t.description.clone(),
        parameters: t
            .parameters
            .clone()
            .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
    }
}

/// The command line with the model's arguments put in.
pub fn command_line(t: &CustomTool, args: &Value) -> Result<Vec<String>, String> {
    let text = |v: &Value| match v {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    };
    let mut out = Vec::new();
    for part in &t.command {
        if let Some(name) = part.strip_prefix('{').and_then(|p| p.strip_suffix('}'))
            && is_name(name)
        {
            match &args[name] {
                Value::Null => {}
                Value::Array(items) => out.extend(items.iter().map(text)),
                v => out.push(text(v)),
            }
            continue;
        }
        let mut s = String::new();
        let mut rest = part.as_str();
        while let Some(open) = rest.find('{') {
            s.push_str(&rest[..open]);
            let after = &rest[open + 1..];
            match after.find('}') {
                Some(close) if is_name(&after[..close]) => {
                    let v = &args[&after[..close]];
                    if !v.is_null() {
                        s.push_str(&text(v));
                    }
                    rest = &after[close + 1..];
                }
                _ => {
                    s.push('{');
                    rest = after;
                }
            }
        }
        s.push_str(rest);
        out.push(s);
    }
    if out.is_empty() {
        return Err("the tool's command is empty".into());
    }
    Ok(out)
}

fn is_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// Runs the tool in `cwd` and returns the exit status and output for the
/// model. Stops the program when `cancel` is set or the timeout passes.
pub fn run(t: &CustomTool, cmd: &[String], cwd: &Path, cancel: &AtomicBool) -> String {
    let mut child = match Command::new(&cmd[0])
        .args(&cmd[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return format!("error: could not start {}: {e}", cmd[0]),
    };
    let read = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = r.read_to_end(&mut buf);
            buf
        })
    };
    let out = read(Box::new(child.stdout.take().expect("piped")));
    let err = read(Box::new(child.stderr.take().expect("piped")));
    let deadline = Instant::now() + Duration::from_secs(t.timeout);
    let ended = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.to_string(),
            Ok(None) if cancel.load(Ordering::Relaxed) => {
                let _ = child.kill();
                let _ = child.wait();
                break "stopped by the user".into();
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break format!("stopped after {} seconds", t.timeout);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => break format!("could not wait for it: {e}"),
        }
    };
    let out = out.join().unwrap_or_default();
    let err = err.join().unwrap_or_default();
    let mut text = String::from_utf8_lossy(&out).into_owned();
    if !err.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&String::from_utf8_lossy(&err));
    }
    let text = clip_middle(text.trim_end());
    if text.is_empty() {
        format!("{ended}, no output")
    } else {
        format!("{ended}\n{text}")
    }
}

fn clip_middle(s: &str) -> String {
    if s.len() <= MAX_OUTPUT {
        return s.to_string();
    }
    let floor = |mut i: usize| {
        while !s.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    let head = floor(MAX_OUTPUT / 3);
    let tail = floor(s.len() - MAX_OUTPUT * 2 / 3);
    format!(
        "{}\n[{} bytes left out]\n{}",
        &s[..head],
        tail - head,
        &s[tail..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(command: &[&str]) -> CustomTool {
        CustomTool {
            description: "d".into(),
            command: command.iter().map(|s| s.to_string()).collect(),
            parameters: None,
            read_only: false,
            timeout: 5,
        }
    }

    #[test]
    fn puts_arguments_in() {
        let t = tool(&[
            "grep",
            "-n",
            "{pattern}",
            "{paths}",
            "{flag}",
            "--max={n}",
            "{x",
        ]);
        let args = json!({"pattern": "a b; rm -rf /", "paths": ["x", "y"], "n": 3});
        assert_eq!(
            command_line(&t, &args).unwrap(),
            ["grep", "-n", "a b; rm -rf /", "x", "y", "--max=3", "{x"]
        );
        assert!(command_line(&tool(&["{only}"]), &json!({})).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn runs_and_reports() {
        let cwd = std::env::temp_dir();
        let no = AtomicBool::new(false);
        let t = tool(&["sh", "-c", "echo out; echo err >&2; exit 3"]);
        let cmd = command_line(&t, &json!({})).unwrap();
        assert_eq!(run(&t, &cmd, &cwd, &no), "exit status: 3\nout\nerr");
        let quiet = tool(&["true"]);
        assert_eq!(
            run(&quiet, &["true".into()], &cwd, &no),
            "exit status: 0, no output"
        );
        let missing = run(&quiet, &["no-such-program-x".into()], &cwd, &no);
        assert!(missing.starts_with("error: could not start"), "{missing}");
    }

    #[cfg(unix)]
    #[test]
    fn stops_on_cancel_and_timeout() {
        let cwd = std::env::temp_dir();
        let mut t = tool(&["sleep", "10"]);
        t.timeout = 1;
        let started = Instant::now();
        let r = run(&t, &t.command.clone(), &cwd, &AtomicBool::new(false));
        assert_eq!(r, "stopped after 1 seconds, no output");
        assert!(started.elapsed() < Duration::from_secs(3));
        t.timeout = 60;
        let r = run(&t, &t.command.clone(), &cwd, &AtomicBool::new(true));
        assert_eq!(r, "stopped by the user, no output");
    }

    #[test]
    fn long_output_is_cut_in_the_middle() {
        let s = "a".repeat(20_000) + &"b".repeat(20_000);
        let c = clip_middle(&s);
        assert!(c.starts_with('a') && c.ends_with('b'));
        assert!(c.contains("bytes left out"));
        assert!(c.len() < MAX_OUTPUT + 100);
    }
}
