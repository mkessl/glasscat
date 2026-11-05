//! The slash commands of the full-screen interface, and its help.

use std::io::{self, Write};

use serde_json::Value;

use super::transcript::Block;
use super::{App, Msg};
use crate::context::Item;

impl App {
    /// Runs the input as a command when it names one; false otherwise.
    pub(super) fn command(&mut self) -> bool {
        let text = self.input.text().trim().to_string();
        let (name, arg) = text.split_once(' ').unwrap_or((&text, ""));
        let arg = arg.trim();
        if !COMMANDS.iter().any(|(n, _)| *n == name) {
            return false;
        }
        self.input.take();
        self.scroll = 0;
        if name == "/help" {
            self.transcript.push(Block::Info(help()));
            return true;
        }
        let Some(session) = self.session.as_mut() else {
            self.transcript
                .push(Block::Error("Still working; Esc stops it first.".into()));
            return true;
        };
        match name {
            "/clear" => {
                session.clear();
                self.transcript.clear();
                self.usage = None;
                self.served = None;
                self.timing = None;
                self.last_request.clear();
                self.transcript
                    .push(Block::Info("A new conversation.".into()));
            }
            "/model" if arg.is_empty() => {
                self.transcript
                    .push(Block::Info(format!("The model is {}.", session.model)));
            }
            "/model" => {
                session.model = arg.to_string();
                self.model = arg.to_string();
                self.served = None;
                self.look_up_window(arg);
                self.transcript
                    .push(Block::Info(format!("The model is now {arg}.")));
            }
            "/edit" => self.edit_context(),
            "/models" => {
                let (api, filter, tx) = (self.api.clone(), arg.to_string(), self.tx.clone());
                std::thread::spawn(move || {
                    let found = crate::models::list(
                        &api.base_url,
                        api.api_key.as_deref(),
                        api.proxy.as_deref(),
                    )
                    .map(|l| crate::models::matching(&l, &filter, 40));
                    let block = match found {
                        Ok(t) => Block::Info(t),
                        Err(e) => Block::Error(format!("could not list the models: {e}")),
                    };
                    let _ = tx.send(Msg::Block(block));
                });
            }
            "/set" if arg.is_empty() => {
                let sources = session.fields.sources(&session.model);
                let text = if sources.is_empty() {
                    "No request fields are set. /set NAME VALUE sets one for this session; \
                     the settings file sets them for good."
                        .to_string()
                } else {
                    let fields: Vec<String> = sources
                        .iter()
                        .map(|(k, v, from)| match v {
                            Value::Null => format!("  {k}: left out, by {from}"),
                            v => format!("  {k}: {v}, by {from}"),
                        })
                        .collect();
                    format!("Set in every request:\n{}", fields.join("\n"))
                };
                self.transcript.push(Block::Info(text));
            }
            "/set" => {
                let (key, value) = arg.split_once(' ').unwrap_or((arg, ""));
                let value = value.trim();
                let text = if value.is_empty() {
                    session.fields.session.remove(key);
                    let now = session.fields.sources(&session.model);
                    match now.iter().find(|f| f.0 == key) {
                        Some((_, v, from)) => format!("{key} is {v} again, by {from}."),
                        None => format!("{key} is back to how {} sends it.", crate::NAME),
                    }
                } else {
                    // JSON when it reads as JSON, otherwise a string.
                    let value: Value =
                        serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.into()));
                    let text = if value.is_null() {
                        format!("{key} is now left out of every request.")
                    } else {
                        format!("Every request now has {key}: {value}.")
                    };
                    session.fields.session.insert(key.to_string(), value);
                    text
                };
                self.transcript.push(Block::Info(text));
            }
            "/copy" => {
                let last = session.items.iter().rev().find_map(|i| match i {
                    Item::Assistant(r) if !r.text.trim().is_empty() => Some(r.text.trim()),
                    _ => None,
                });
                let Some(text) = last else {
                    self.transcript
                        .push(Block::Error("There is no reply to copy yet.".into()));
                    return true;
                };
                // OSC 52: the terminal puts the text on the clipboard, also over SSH.
                let mut out = io::stdout();
                let _ = write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()));
                let _ = out.flush();
                self.transcript.push(Block::Info(format!(
                    "Copied the last reply ({} characters) to the clipboard, if the terminal \
                     allows it. In tmux that needs set-clipboard on.",
                    text.chars().count()
                )));
            }
            "/provider" => {
                let name = session.log.as_ref().map_or("unnamed", |l| l.1.as_str());
                let base = session.base_url.clone();
                let proxy = session.proxy.clone();
                let mut text = format!("The provider is {name}, at {base}.");
                match self.served.clone() {
                    Some(s) if crate::policy::known(&base) => {
                        text.push_str(" Asking OpenRouter about the last reply…");
                        let tx = self.tx.clone();
                        std::thread::spawn(move || {
                            let b = match crate::policy::describe(&base, proxy.as_deref(), &s) {
                                Ok(t) => Block::Info(t),
                                Err(e) => Block::Error(format!("policy lookup failed: {e}")),
                            };
                            let _ = tx.send(Msg::Block(b));
                        });
                    }
                    _ if crate::policy::known(&base) => text.push_str(
                        " OpenRouter names the provider that ran a model with each reply; \
                         ask again after one.",
                    ),
                    Some(s) => text.push_str(&format!(
                        " The last reply came from {}. {} knows no policy API for this provider.",
                        s.model,
                        crate::NAME
                    )),
                    None => text.push_str(&format!(
                        " {} knows no policy API for this provider.",
                        crate::NAME
                    )),
                }
                self.transcript.push(Block::Info(text));
            }
            "/tools" => self.transcript.push(Block::Info(session.tools.summary())),
            "/report" => {
                let dir = &session.tools.reports;
                let text = crate::report::latest(dir).and_then(|p| std::fs::read_to_string(p).ok());
                let Some(text) = text else {
                    self.transcript.push(Block::Info(format!(
                        "No reports yet. When the model meets a fault of {} itself, it can write \
                         one up with its report tool; you can also ask it to.",
                        crate::NAME
                    )));
                    return true;
                };
                let url = crate::report::issue_url(&text);
                let block = match crate::report::open(&url) {
                    Ok(()) => Block::Info(
                        "Opened the last report as a new issue in the browser; it is sent only \
                         if you send it there."
                            .into(),
                    ),
                    Err(e) => {
                        Block::Error(format!("could not open the browser ({e}); the link: {url}"))
                    }
                };
                self.transcript.push(block);
            }
            "/undo" => match session.undo() {
                Ok(done) => self.transcript.push(Block::Info(done)),
                Err(e) => self.transcript.push(Block::Error(e)),
            },
            _ => unreachable!("listed in COMMANDS"),
        }
        true
    }
}

const COMMANDS: &[(&str, &str)] = &[
    ("/help", "this list"),
    ("/clear", "start a new conversation"),
    ("/model", "show the model; /model <id> switches to another"),
    (
        "/models",
        "the provider's models; /models WORDS finds ids with them",
    ),
    ("/tools", "the tools the model has, and what each one runs"),
    ("/undo", "undo the agent's last change to a file"),
    (
        "/report",
        "open the model's last report about a fault of the agent as a new issue",
    ),
    (
        "/edit",
        "edit the whole context in $EDITOR before the next request",
    ),
    ("/copy", "copy the last reply to the clipboard"),
    (
        "/set",
        "request fields; /set NAME VALUE sets one (null leaves it out), /set NAME unsets",
    ),
    (
        "/provider",
        "who ran the last reply, and what they publish about your data",
    ),
];

const KEYS: &[(&str, &str)] = &[
    (
        "Enter",
        "send; on an empty line, pick up a reply that did not finish",
    ),
    ("Shift+Enter, Alt+Enter, Ctrl+J", "new line"),
    ("Esc", "stop the reply"),
    ("Ctrl+C", "stop, clear the input, or quit"),
    ("Ctrl+D", "quit on an empty line"),
    ("Ctrl+T", "the exact context sent to the model"),
    ("Ctrl+O", "show or fold the model's reasoning"),
    (
        "Up, Down",
        "move between lines and through earlier messages",
    ),
    ("PageUp, PageDown, wheel", "scroll"),
    (
        "Shift+drag",
        "select text in most terminals; /copy copies a whole reply",
    ),
    ("Ctrl+A, Ctrl+E", "start, end of line"),
    (
        "Ctrl+U, Ctrl+K, Ctrl+W",
        "delete to line start, to line end, a word",
    ),
];

fn help() -> String {
    let width = |t: &[(&str, &str)]| t.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let (cw, kw) = (width(COMMANDS), width(KEYS));
    let mut out = String::from("Commands\n");
    for (name, what) in COMMANDS {
        out += &format!("  {name:<cw$}  {what}\n");
    }
    out += "\nKeys\n";
    for (key, what) in KEYS {
        out += &format!("  {key:<kw$}  {what}\n");
    }
    out.trim_end().to_string()
}

/// Standard base64 with padding.
fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64("héllo wörld".as_bytes()), "aMOpbGxvIHfDtnJsZA==");
    }
}
