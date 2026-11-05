//! Reports to the developers: the model writes up a fault of the agent it
//! met, the report is saved for the user, and nothing is sent. The user may
//! open it as a new issue on the project's page and send it from there.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::context::ToolSpec;

pub const TOOL: &str = "report";

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: TOOL.into(),
        description: format!(
            "Report a fault of {} itself, the agent you run in, to its developers. A fault is a \
             tool that misbehaves, an error that should not happen, or something it lacks for the \
             work; not a fault in the user's work. The report is saved for the user, who decides \
             whether to send it. Give only what the fault needs: no file contents, paths or \
             personal details unless the fault cannot be understood without them.",
            crate::NAME
        ),
        parameters: json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "One line" },
                "details": {
                    "type": "string",
                    "description": "What happened, what was expected, how to make it happen again"
                },
            },
            "required": ["title", "details"],
        }),
    }
}

/// Saves the report in `dir`; returns what the model gets back and a note
/// for the user.
pub fn save(dir: &Path, args: &Value) -> Result<(String, String), String> {
    let field = |k: &str| {
        args[k]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or(format!("{k} is missing"))
    };
    let (title, details) = (field("title")?, field("details")?);
    let title = title.lines().next().unwrap_or_default();
    std::fs::create_dir_all(dir).map_err(|e| format!("could not save it: {e}"))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let path = dir.join(format!("{stamp}.md"));
    let text = format!(
        "# {title}\n\n{details}\n\n---\n{} {}, {} {}\n",
        crate::NAME,
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    std::fs::write(&path, text).map_err(|e| format!("could not save it: {e}"))?;
    Ok((
        "Saved for the user; nothing was sent.".into(),
        format!("Report saved in {}; nothing was sent.", path.display()),
    ))
}

/// The newest report in `dir`.
pub fn latest(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .max()
}

/// A link that opens a new issue on the project's page with the report
/// filled in; only the title when the whole report would make the link
/// too long for the site.
pub fn issue_url(report: &str) -> String {
    let (title, body) = match report.split_once("\n\n") {
        Some((t, b)) => (t.trim_start_matches("# "), b),
        None => (report.trim_start_matches("# "), ""),
    };
    let base = format!(
        "{}/issues/new?title={}",
        env!("CARGO_PKG_REPOSITORY"),
        encode(title)
    );
    let full = format!("{base}&body={}", encode(body.trim_end()));
    if full.len() <= 8000 { full } else { base }
}

fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Opens `url` in the user's browser.
pub fn open(url: &str) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    // Not `cmd /C start`: cmd would read the `&` in the link as its own.
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else if cfg!(target_os = "macos") {
        Command::new("open")
    } else {
        Command::new("xdg-open")
    };
    let mut child = cmd
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_and_links() {
        let dir = std::env::temp_dir().join(format!("reports-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(save(&dir, &json!({"title": "x"})).is_err());
        let (back, note) = save(
            &dir,
            &json!({"title": "edit fails\nmore", "details": "It said a & b."}),
        )
        .unwrap();
        assert_eq!(back, "Saved for the user; nothing was sent.");
        let path = latest(&dir).unwrap();
        assert!(note.contains(&path.display().to_string()));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("# edit fails\n\nIt said a & b.\n\n---\n"),
            "{text}"
        );
        let url = issue_url(&text);
        assert!(
            url.contains(
                "/issues/new?title=edit%20fails&body=It%20said%20a%20%26%20b.%0A%0A---%0A"
            ),
            "{url}"
        );
        assert!(!issue_url(&format!("# t\n\n{}", "é".repeat(5000))).contains("body="));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
