//! The slash commands of the full-screen interface, and its help.

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;

use super::transcript::Block;
use super::{App, Msg};
use crate::context::Item;

impl App {
    /// Runs the input as a command when it names one; false otherwise.
    /// What a command only shows opens in a panel that leaves the chat
    /// where it was; what it does goes into the chat, scrolled down.
    pub(super) fn command(&mut self) -> bool {
        let text = self.input.text().trim().to_string();
        let (name, arg) = text.split_once(' ').unwrap_or((&text, ""));
        if !COMMANDS.iter().any(|(n, _)| *n == name) {
            return false;
        }
        self.input.take();
        self.panel = None;
        self.run_command(name, arg.trim());
        if self.panel.is_none() {
            self.scroll = 0;
        }
        true
    }

    fn show(&mut self, text: String) {
        self.panel = Some((text, 0));
    }

    fn run_command(&mut self, name: &str, arg: &str) {
        if name == "/help" {
            return self.show(help());
        }
        if name == "/usage" {
            return self.show(super::requests_text(&self.requests));
        }
        let Some(session) = self.session.as_mut() else {
            self.transcript
                .push(Block::Error("Still working; Esc stops it first.".into()));
            return;
        };
        match name {
            "/clear" => {
                session.clear();
                self.transcript.clear();
                self.usage = None;
                self.served = None;
                self.timing = None;
                self.cache_last = None;
                self.last_request.clear();
                self.transcript
                    .push(Block::Info("A new conversation.".into()));
            }
            "/model" if arg.is_empty() => {
                let text = format!(
                    "The model is {}. /model ID switches to another; /models WORDS finds ids.",
                    session.model
                );
                self.show(text);
            }
            "/model" => {
                session.model = arg.to_string();
                self.model = arg.to_string();
                self.served = None;
                self.look_up_window(arg);
                self.transcript
                    .push(Block::Info(format!("The model is now {arg}.")));
                self.check_model(arg);
            }
            "/edit" => self.edit_context(),
            "/settings" => self.edit_settings(),
            "/models" => {
                let (api, filter, tx) = (self.api.clone(), arg.to_string(), self.tx.clone());
                std::thread::spawn(move || {
                    let found = crate::models::list(
                        &api.base_url,
                        api.api_key.as_deref(),
                        api.proxy.as_deref(),
                    )
                    .map(|l| {
                        // Without the ZDR list, the models are still listed.
                        let zdr = crate::models::zdr(&api.base_url, api.proxy.as_deref())
                            .ok()
                            .flatten();
                        crate::models::matching(&l, &filter, 40, zdr.as_ref())
                    });
                    let block = match found {
                        Ok(t) => Block::Info(t),
                        Err(e) => Block::Error(format!("could not list the models: {e}")),
                    };
                    let _ = tx.send(Msg::Block(block));
                });
                self.show("Asking the provider for its models…".into());
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
                self.show(text);
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
                    return;
                };
                clipboard(text);
                self.transcript.push(Block::Info(format!(
                    "Copied the last reply ({} characters) to the clipboard, if the terminal \
                     allows it. In tmux that needs set-clipboard on.",
                    text.chars().count()
                )));
            }
            "/login" => self.login(),
            "/compact" => {
                self.transcript.push(Block::Info(
                    "Compacting: the model summarises the older part of the conversation…".into(),
                ));
                self.work(super::Work::Compact);
            }
            "/logout" => {
                let name = session.launch.provider.clone();
                let text = match crate::auth::remove(&name) {
                    Ok(0) => format!("No key for {name} was stored."),
                    Ok(_) => format!("Removed the stored key for {name}."),
                    Err(e) => return self.transcript.push(Block::Error(e.to_string())),
                };
                let key = session
                    .key_env
                    .as_ref()
                    .and_then(|v| std::env::var(v).ok().filter(|k| !k.trim().is_empty()));
                let text = match (&key, &session.key_env) {
                    (Some(_), Some(var)) => format!("{text} {var} is still set, and it is used."),
                    _ => text,
                };
                session.api_key = key.clone();
                self.api.api_key = key;
                self.transcript.push(Block::Info(text));
            }
            "/provider" => {
                let name = session.log.as_ref().map_or("unnamed", |l| l.1.as_str());
                let base = session.base_url.clone();
                let proxy = session.proxy.clone();
                let mut text = format!("The provider is {name}, at {base}.");
                match self.served.clone() {
                    Some(s) if crate::policy::known(&base) => {
                        let tx = self.tx.clone();
                        let first = text.clone();
                        text.push_str(" Asking OpenRouter about the last reply…");
                        std::thread::spawn(move || {
                            let b = match crate::policy::describe(&base, proxy.as_deref(), &s) {
                                Ok(t) => Block::Info(format!("{first}\n\n{t}")),
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
                self.show(text);
            }
            "/tools" => {
                let text = session.tools.summary();
                self.show(text);
            }
            "/report" => {
                let dir = &session.tools.reports;
                let text = crate::report::latest(dir).and_then(|p| std::fs::read_to_string(p).ok());
                let Some(text) = text else {
                    self.transcript.push(Block::Info(format!(
                        "No reports yet. When the model meets a fault of {} itself, it can write \
                         one up with its report tool; you can also ask it to.",
                        crate::NAME
                    )));
                    return;
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
            "/access" => {
                let (verb, path) = arg.split_once(' ').unwrap_or((arg, ""));
                let path = std::path::Path::new(path.trim());
                let access = &mut session.tools.access;
                let block = match verb {
                    "" => {
                        self.view = super::View::Files(super::files::Files::new(access));
                        return;
                    }
                    _ if path.as_os_str().is_empty() => {
                        Block::Error(format!("/access {verb} needs a path."))
                    }
                    "hide" if access.hide(path) => Block::Info(format!(
                        "{} is hidden from the model from now on, for this session. What it \
                         already read stays in the context; /edit can take it out.",
                        path.display()
                    )),
                    "hide" => Block::Info(format!("{} is already hidden.", path.display())),
                    "show" if access.unhide(path) => Block::Info(format!(
                        "{} is no longer hidden, for this session.",
                        path.display()
                    )),
                    "show" => Block::Error(format!(
                        "{} is not hidden by that path; /access lists what is.",
                        path.display()
                    )),
                    "allow" => match access.allow(path) {
                        Ok(true) => Block::Info(format!(
                            "The model may use {} too, for this session.",
                            path.display()
                        )),
                        Ok(false) => {
                            Block::Info(format!("The model may already use {}.", path.display()))
                        }
                        Err(e) => Block::Error(format!("{}: {e}", path.display())),
                    },
                    _ => Block::Error("/access takes hide, show or allow, and a path.".into()),
                };
                self.transcript.push(block);
            }
            "/retry" => {
                let Some(dropped) = session.drop_answer() else {
                    self.transcript
                        .push(Block::Error("There is no message to answer again.".into()));
                    return;
                };
                let changed = dropped.iter().any(|i| match i {
                    Item::Assistant(r) => {
                        r.calls.iter().any(|c| !session.tools.reads_only(&c.name))
                    }
                    _ => false,
                });
                let mut text = format!("Asking {} again.", session.model);
                if changed {
                    text += " What the dropped answer changed in files stays; /undo takes it back.";
                }
                let items = session.items.clone();
                self.transcript.clear();
                super::replay(&items, &mut self.transcript);
                self.transcript.push(Block::Info(text));
                self.start(None);
            }
            "/resume" => {
                let cwd = session.tools.cwd().to_path_buf();
                let now = std::time::SystemTime::now();
                let current = session.log.as_ref().map(|l| l.0.clone());
                let list: Vec<_> = crate::history::list(&cwd)
                    .into_iter()
                    .filter(|(p, _)| Some(p) != current.as_ref())
                    .take(30)
                    .collect();
                if arg.is_empty() {
                    let lines: Vec<String> = list
                        .iter()
                        .enumerate()
                        .filter_map(|(i, (p, t))| {
                            let saved = crate::history::load(p).ok()?;
                            let ago = now.duration_since(*t).unwrap_or_default();
                            let line = crate::history::describe(&saved, ago);
                            Some(format!("{:>2}. {}", i + 1, super::text::clip(&line, 200)))
                        })
                        .collect();
                    let text = if lines.is_empty() {
                        "No other conversation was held in this directory.".to_string()
                    } else {
                        format!(
                            "Earlier conversations here; /resume N picks one up:\n{}",
                            lines.join("\n")
                        )
                    };
                    return self.show(text);
                }
                let picked = arg
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| list.get(n.wrapping_sub(1)));
                let Some((path, _)) = picked else {
                    self.transcript.push(Block::Error(format!(
                        "There is no conversation {arg}; /resume lists them."
                    )));
                    return;
                };
                let saved = match crate::history::load(path) {
                    Ok(s) => s,
                    Err(e) => return self.transcript.push(Block::Error(e.to_string())),
                };
                let provider = session
                    .log
                    .as_ref()
                    .map(|l| l.1.clone())
                    .unwrap_or_default();
                let mut text =
                    String::from("Picked up the conversation; it is saved where it was.");
                if saved.provider == provider {
                    if saved.model != session.model {
                        text += &format!(
                            " It goes on with its model, {}; /model switches.",
                            saved.model
                        );
                    }
                    session.model = saved.model.clone();
                } else {
                    text += &format!(
                        " It was held with {} on {}, and goes on with {} on {provider}.",
                        saved.model, saved.provider, session.model
                    );
                }
                session.items = saved.items;
                if let Some(log) = &mut session.log {
                    log.0 = path.clone();
                }
                let (items, model) = (session.items.clone(), session.model.clone());
                self.model = model.clone();
                self.served = None;
                self.usage = None;
                self.timing = None;
                self.cache_last = None;
                self.last_request.clear();
                self.look_up_window(&model);
                self.transcript.clear();
                super::replay(&items, &mut self.transcript);
                self.transcript.push(Block::Info(text));
                self.hint_unanswered(false);
            }
            "/changes" => {
                let text = session.tools.changes();
                self.show(text);
            }
            "/undo" => {
                let n = match arg {
                    "" => 1,
                    _ => match arg.parse::<usize>() {
                        Ok(n) if n > 0 => n,
                        _ => {
                            return self.transcript.push(Block::Error(
                                "/undo takes how many changes to undo, such as /undo 3.".into(),
                            ));
                        }
                    },
                };
                let mut done = Vec::new();
                let mut failed = None;
                for _ in 0..n {
                    match session.undo() {
                        Ok(d) => done.push(d),
                        Err(e) => {
                            failed = Some(e);
                            break;
                        }
                    }
                }
                if !done.is_empty() {
                    self.transcript.push(Block::Info(done.join("\n")));
                }
                if let Some(e) = failed {
                    self.transcript.push(Block::Error(e));
                }
            }
            _ => unreachable!("listed in COMMANDS"),
        }
    }
}

/// The commands that start with what is typed, while it is a command
/// name: a `/` and no space yet.
pub(super) fn completions(typed: &str) -> Vec<(&'static str, &'static str)> {
    if !typed.starts_with('/') || typed.contains(char::is_whitespace) {
        return Vec::new();
    }
    COMMANDS
        .iter()
        .copied()
        .filter(|(n, _)| n.starts_with(typed))
        .collect()
}

/// What Tab adds to `typed`: the rest of the name its completions share,
/// and a space when only one is left.
pub(super) fn complete(typed: &str) -> String {
    let found = completions(typed);
    let Some(((first, _), rest)) = found.split_first() else {
        return String::new();
    };
    if rest.is_empty() {
        return format!("{} ", &first[typed.len()..]);
    }
    let shared = rest.iter().fold(first.len(), |n, (other, _)| {
        first
            .bytes()
            .zip(other.bytes())
            .take(n)
            .take_while(|(a, b)| a == b)
            .count()
    });
    first[typed.len()..shared].to_string()
}

/// What Tab adds to the path that ends `typed`, looked up from `cwd`: the
/// rest of the name its matches share, and a `/` or a space when only one
/// is left; and the matches when there are more, a folder with a `/`.
/// A name starting with a dot is offered only after a dot.
pub(super) fn complete_path(typed: &str, cwd: &std::path::Path) -> (String, Vec<String>) {
    let word = typed.rsplit(char::is_whitespace).next().unwrap_or("");
    if word.is_empty() {
        return Default::default();
    }
    let (dir, prefix) = match word.rfind('/') {
        Some(i) => (&word[..=i], &word[i + 1..]),
        None => ("", word),
    };
    let base = match dir.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => std::path::PathBuf::from(home).join(rest),
            None => return Default::default(),
        },
        None => cwd.join(dir),
    };
    let Ok(entries) = std::fs::read_dir(&base) else {
        return Default::default();
    };
    let mut found: Vec<(String, bool)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let dot = name.starts_with('.') && !prefix.starts_with('.');
            (name.starts_with(prefix) && !dot).then(|| (name, e.path().is_dir()))
        })
        .collect();
    found.sort();
    let Some(((first, is_dir), rest)) = found.split_first() else {
        return Default::default();
    };
    if rest.is_empty() {
        let end = if *is_dir { "/" } else { " " };
        return (format!("{}{end}", &first[prefix.len()..]), Vec::new());
    }
    let shared = rest.iter().fold(first.len(), |n, (other, _)| {
        first
            .char_indices()
            .zip(other.chars())
            .take_while(|((i, a), b)| *i < n && a == b)
            .last()
            .map_or(0, |((i, a), _)| i + a.len_utf8())
    });
    let names = found
        .iter()
        .map(|(n, d)| if *d { format!("{n}/") } else { n.clone() })
        .collect();
    (
        first[prefix.len()..shared.max(prefix.len())].to_string(),
        names,
    )
}

/// OSC 52: the terminal puts the text on the clipboard, also over SSH.
fn clipboard(text: &str) {
    let mut out = io::stdout();
    let _ = write!(
        out,
        "\x1b]52;c;{}\x07",
        crate::image::base64(text.as_bytes())
    );
    let _ = out.flush();
}

impl App {
    /// Starts a sign-in: the input then takes a key, or for OpenRouter
    /// the code its page shows, hidden as it is typed.
    pub(super) fn login(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let name = session.launch.provider.clone();
        if session.key_env.is_none() {
            return self.show(format!("{name} needs no key."));
        }
        let cancel = Arc::new(AtomicBool::new(false));
        if name != "openrouter" || !session.base_url.contains("openrouter.ai") {
            let page = crate::provider::keys_page(&name)
                .map(|p| format!(" Get one at {p}."))
                .unwrap_or_default();
            self.transcript.push(Block::Info(format!(
                "Paste a key for {name} and press Enter; it stays hidden.{page} Esc cancels."
            )));
            self.login = Some(super::LoginWait {
                provider: name,
                oauth: None,
                cancel,
            });
            return;
        }
        let headless = crate::oauth::headless();
        let login = match crate::oauth::start(!headless) {
            Ok(l) => Arc::new(l),
            Err(e) => {
                return self
                    .transcript
                    .push(Block::Error(format!("sign-in failed: {e}")));
            }
        };
        let text = if headless {
            clipboard(&login.url);
            format!(
                "Open this page in a browser (it is on the clipboard, if the terminal allows \
                 it), sign in, and paste the code it shows here:\n\n{}\n\nA key works too. \
                 Esc cancels.",
                login.url
            )
        } else {
            let verb = if crate::oauth::open_browser(&login.url) {
                "Opened"
            } else {
                "Open"
            };
            let (l, tx, stop) = (login.clone(), self.tx.clone(), cancel.clone());
            let proxy = session.proxy.clone();
            let provider = name.clone();
            std::thread::spawn(move || {
                let got = l
                    .wait(&stop, crate::login::WAIT)
                    .and_then(|code| l.exchange(&code, proxy.as_deref()));
                if !stop.load(Ordering::Relaxed) {
                    let _ = tx.send(Msg::Login(provider, got.map_err(|e| e.to_string())));
                }
            });
            format!(
                "{verb} this page to sign in:\n\n{}\n\nOr paste a key here. Esc cancels.",
                login.url
            )
        };
        self.transcript.push(Block::Info(text));
        self.login = Some(super::LoginWait {
            provider: name,
            oauth: Some(login),
            cancel,
        });
    }

    /// The input sent while signing in: a key, or OpenRouter's code.
    pub(super) fn login_input(&mut self) {
        let text = self.input.text().trim().to_string();
        self.input.forget();
        let Some(wait) = self.login.take() else {
            return;
        };
        if text.is_empty() {
            self.login = Some(wait);
            return;
        }
        wait.cancel.store(true, Ordering::Relaxed);
        match wait.oauth {
            Some(login) if !crate::login::looks_like_key(&text) => {
                let tx = self.tx.clone();
                let proxy = self.api.proxy.clone();
                self.transcript.push(Block::Info("Signing in…".into()));
                std::thread::spawn(move || {
                    let got = login.exchange(&text, proxy.as_deref());
                    let _ = tx.send(Msg::Login(wait.provider, got.map_err(|e| e.to_string())));
                });
            }
            _ => self.logged_in(&wait.provider, Ok(text)),
        }
    }

    /// Keeps the key a sign-in gave, and uses it from the next request.
    pub(super) fn logged_in(&mut self, provider: &str, got: Result<String, String>) {
        let key = match got.and_then(|k| {
            crate::auth::add(provider, &k, crate::NAME)
                .map(|()| k.trim().to_string())
                .map_err(|e| e.to_string())
        }) {
            Ok(k) => k,
            Err(e) => {
                self.transcript
                    .push(Block::Error(format!("sign-in failed: {e}")));
                return self.restore_message();
            }
        };
        let mut text = format!(
            "Signed in to {provider}. The key is kept in {}.",
            crate::auth::path().display()
        );
        let var = self.session.as_ref().and_then(|s| s.key_env.clone());
        if let Some(var) = var.filter(|v| std::env::var(v).is_ok_and(|k| !k.trim().is_empty())) {
            text += &format!(" {var} is set and overrides it.");
        } else {
            self.api.api_key = Some(key.clone());
            if let Some(s) = self.session.as_mut() {
                s.api_key = Some(key);
            }
        }
        self.transcript.push(Block::Info(text));
        self.send_after_login();
    }
}

const COMMANDS: &[(&str, &str)] = &[
    ("/help", "this list"),
    ("/clear", "start a new conversation"),
    (
        "/resume",
        "earlier conversations held here; /resume N picks one up",
    ),
    (
        "/retry",
        "drop the last answer and ask again, also after /model",
    ),
    (
        "/undo",
        "undo the agent's last change to a file; /undo N, the last N",
    ),
    (
        "/changes",
        "the agent's changes to files in this run, which /undo takes back",
    ),
    (
        "/edit",
        "edit the whole context in $EDITOR before the next request",
    ),
    ("/copy", "copy the last reply to the clipboard"),
    ("/model", "show the model; /model <id> switches to another"),
    (
        "/models",
        "the provider's models; /models WORDS finds ids with them; the word zdr, those run with no data kept",
    ),
    (
        "/set",
        "request fields; /set NAME VALUE sets one (null leaves it out), /set NAME unsets",
    ),
    (
        "/provider",
        "who ran the last reply, and what they publish about your data",
    ),
    (
        "/login",
        "sign in to the provider: OpenRouter in the browser, others with a key",
    ),
    ("/logout", "remove the stored key for the provider"),
    (
        "/compact",
        "make the context shorter now: the older part becomes a summary",
    ),
    (
        "/usage",
        "each request of this run: tokens, cache, cost, time, and who ran it",
    ),
    ("/tools", "the tools the model has, and what each one runs"),
    (
        "/access",
        "a tree of what the model may see, Space hides or shows; or hide|show|allow PATH",
    ),
    (
        "/settings",
        "edit the settings file; what can change applies at once",
    ),
    (
        "/report",
        "open the model's last report about a fault of the agent as a new issue",
    ),
];

const KEYS: &[(&str, &str)] = &[
    (
        "Enter",
        "send; on an empty line, pick up a reply that did not finish",
    ),
    ("Shift+Enter, Alt+Enter, Ctrl+J", "new line"),
    (
        "Tab",
        "complete a command name or a file path; / lists the commands",
    ),
    ("Esc", "stop the reply"),
    ("Ctrl+C", "stop, clear the input, or quit"),
    ("Ctrl+D", "quit on an empty line"),
    ("Ctrl+T", "the exact context sent to the model"),
    (
        "Ctrl+O",
        "show or fold the model's reasoning and long messages",
    ),
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
        "Alt+B, Alt+F",
        "a word back, forward; so do Ctrl+Left, Ctrl+Right",
    ),
    (
        "Ctrl+U, Ctrl+K, Ctrl+W, Alt+D",
        "delete to line start, to line end, the word before, the word after",
    ),
    ("Ctrl+Y", "put back the text deleted last"),
    ("Ctrl+Z or Ctrl+_", "undo in the input"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_command_names() {
        assert_eq!(complete("/he"), "lp ");
        assert_eq!(complete("/mo"), "del");
        assert_eq!(complete("/model"), "");
        assert_eq!(completions("/model").len(), 2);
        assert_eq!(complete("/x"), "");
        assert!(completions("/model x").is_empty());
        assert!(completions("hi").is_empty());
    }

    #[test]
    fn completes_paths() {
        let dir = std::env::temp_dir().join(format!("complete-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/ui")).unwrap();
        for f in [
            "src/main.rs",
            "src/mod.rs",
            "src/model.rs",
            ".env",
            "README.md",
        ] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        let add = |typed: &str| complete_path(typed, &dir).0;
        assert_eq!(add("look at REA"), "DME.md ");
        assert_eq!(add("sr"), "c/");
        assert_eq!(add("read src/mo"), "d");
        assert_eq!(add("read src/u"), "i/");
        assert_eq!(add("src/m"), "");
        assert_eq!(add(""), "");
        assert_eq!(add("x "), "");
        assert_eq!(add("e"), "");
        assert_eq!(add(".e"), "nv ");
        assert_eq!(
            complete_path("src/", &dir).1,
            ["main.rs", "mod.rs", "model.rs", "ui/"]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
