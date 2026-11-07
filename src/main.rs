mod access;
mod agent;
mod auth;
mod cache;
mod compact;
mod context;
mod custom;
mod danger;
mod history;
mod image;
mod journal;
mod landlock;
mod login;
mod mcp;
mod memory;
mod models;
mod oauth;
mod openai;
mod policy;
mod provider;
mod report;
mod settings;
mod shell;
mod snapshot;
mod sse;
#[cfg(test)]
mod test_server;
mod tools;
mod ui;

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

pub type Result<T, E = Box<dyn std::error::Error + Send + Sync>> = std::result::Result<T, E>;

/// The program's name, taken from the binary name in `Cargo.toml`.
pub const NAME: &str = env!("CARGO_BIN_NAME");

const USAGE: &str = "usage: NAME [OPTIONS] [PROMPT...]
       NAME login [PROVIDER]
       NAME logout [PROVIDER]

Without PROMPT, opens the full-screen interface. With PROMPT, gives it to
the model, lets it work with its tools in the current directory, and
streams the answer to stdout; tool calls go to stderr. login signs in to
a provider (OpenRouter through the browser, others with a pasted key or
one piped in); logout removes the stored key.

  -c, --continue    pick up the last conversation held in this directory
  --provider NAME   one of: PROVIDERS (default: openrouter)
  -m, --model ID    model id (default: the provider's default)
  --allow PATH      let the model see PATH too (the current directory always)
  --hide PATH       PATH does not exist for the model; repeatable
  --proxy URL       send requests through URL (socks5://, http://)
  --show-request    print each exact request body to stderr before sending
  -V, --version     print the version";

struct Args {
    resume: bool,
    provider: Option<String>,
    model: Option<String>,
    allow: Vec<PathBuf>,
    hide: Vec<PathBuf>,
    proxy: Option<String>,
    show_request: bool,
    prompt: Option<String>,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        resume: false,
        provider: None,
        model: None,
        allow: Vec::new(),
        hide: Vec::new(),
        proxy: None,
        show_request: false,
        prompt: None,
    };
    let mut words = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "-h" | "--help" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("{NAME} {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-c" | "--continue" => args.resume = true,
            "--provider" => args.provider = Some(value()?),
            "-m" | "--model" => args.model = Some(value()?),
            "--allow" => args.allow.push(value()?.into()),
            "--hide" => args.hide.push(value()?.into()),
            "--proxy" => args.proxy = Some(value()?),
            "--show-request" => args.show_request = true,
            "--" => words.extend(it.by_ref()),
            // Not taken as the prompt: a mistyped option would be sent.
            s if s.starts_with('-') && s.len() > 1 => {
                return Err(format!(
                    "unknown option {s}; a prompt that starts with - goes after --\n\n{}",
                    usage()
                )
                .into());
            }
            _ => words.push(a),
        }
    }
    if !words.is_empty() {
        args.prompt = Some(words.join(" "));
    }
    Ok(args)
}

fn usage() -> String {
    let names: Vec<&str> = provider::PRESETS.iter().map(|p| p.name).collect();
    USAGE
        .replace("usage: NAME", &format!("usage: {NAME}"))
        .replace("       NAME", &format!("       {NAME}"))
        .replace("PROVIDERS", &names.join(", "))
}

fn run() -> Result<()> {
    let mut raw = std::env::args().skip(1);
    if let Some(cmd @ ("login" | "logout")) = raw.next().as_deref() {
        return login::run(cmd == "logout", raw.next());
    }
    let args = parse_args()?;
    let settings = settings::load()?;
    let instruction_files = settings.instruction_files();
    let cwd = std::env::current_dir()?;
    let (saved, log) = if args.resume {
        let path =
            history::latest(&cwd).ok_or("no saved conversation was held in this directory")?;
        (Some(history::load(&path)?), path)
    } else {
        (None, history::new_path(&cwd))
    };
    let name = args
        .provider
        .or_else(|| saved.as_ref().map(|s| s.provider.clone()))
        .or_else(|| settings.provider.clone())
        .unwrap_or_else(|| "openrouter".into());
    let provider = provider::find(&name, &settings)?;
    let key = provider.api_key();
    let model = args
        .model
        .or_else(|| {
            saved
                .as_ref()
                .filter(|s| s.provider == name)
                .map(|s| s.model.clone())
        })
        .or_else(|| {
            settings
                .model
                .clone()
                .filter(|_| settings.provider.as_ref() == Some(&name))
        })
        .unwrap_or_else(|| provider.default_model.clone());

    // Only the working directory until the settings are applied.
    let access = access::Access::new(&cwd, std::slice::from_ref(&cwd), &[])?;
    let tools = tools::Tools::new(access, journal::Journal::for_session());
    let mut session = agent::Session::new(provider.base_url, model, key, provider.prefill, tools);
    session.key_env = provider.key_env.clone();
    session.launch = settings::Launch {
        provider: name.clone(),
        allow: args.allow,
        hide: args.hide,
        proxy: args.proxy,
    };
    settings::apply(&mut session, &settings)?;
    session.log = Some((log, name));
    if let Some(saved) = saved {
        session.items = saved.items;
    } else {
        let path = settings::dir().join("AGENTS.md");
        let user = std::fs::read_to_string(&path)
            .ok()
            .map(|t| (path.display().to_string(), t));
        let (names, first) = &instruction_files;
        let mut project = session.tools.read_visible(names);
        if *first {
            project.retain(|(_, t)| !t.trim().is_empty());
            project.truncate(1);
        }
        session.add_instructions(user, project);
        session.add_memory(&memory::snapshot(session.tools.cwd()));
    }
    let Some(prompt) = args.prompt else {
        use std::io::IsTerminal;
        if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
            return Err(format!("no prompt given and not a terminal\n\n{}", usage()).into());
        }
        return ui::run(session);
    };
    if let (None, Some(var)) = (&session.api_key, &session.key_env) {
        return Err(provider::no_key(&session.launch.provider, var).into());
    }

    let show_request = args.show_request;
    let cwd = session.tools.cwd().display().to_string();
    let mut out = std::io::stdout().lock();
    let mut write_err = None;
    let mut at_line_start = true;
    // Blank lines a reply starts with are left out.
    let mut fresh = true;
    // What the requests cost together, and how many reported a cost.
    let mut spent = (0.0, 0);
    // What the agents the model starts spend, and what they do.
    let sub_spent = std::sync::Arc::new(std::sync::Mutex::new((0.0, 0)));
    let (counted, sub_cwd) = (sub_spent.clone(), cwd.clone());
    session.tools.sub = Some(std::sync::Arc::new(move |sub| match sub {
        agent::Sub::Usage(u) => {
            if let (Some(c), Ok(mut s)) = (
                u.get("cost").and_then(serde_json::Value::as_f64),
                counted.lock(),
            ) {
                *s = (s.0 + c, s.1 + 1);
            }
        }
        agent::Sub::Tool { agent, name, args } => {
            eprintln!("  agent {agent} › {name} {}", ui::summary(&args, &sub_cwd));
        }
        _ => {}
    }));
    let turn = session.turn(prompt, &AtomicBool::new(false), |shown| {
        let r = match shown {
            agent::Shown::Request(body) if show_request => {
                let mut err = std::io::stderr().lock();
                err.write_all(body).and_then(|_| err.write_all(b"\n"))
            }
            agent::Shown::Stream(openai::Event::Text(t)) => {
                let t = if fresh { t.trim_start_matches('\n') } else { t };
                if t.is_empty() {
                    return;
                }
                fresh = false;
                at_line_start = t.ends_with('\n');
                out.write_all(t.as_bytes()).and_then(|_| out.flush())
            }
            agent::Shown::ToolDone {
                name,
                args,
                result,
                note,
                before,
            } => {
                fresh = true;
                if !at_line_start {
                    at_line_start = true;
                    let _ = writeln!(out);
                }
                let failed = result.starts_with("error:");
                let outcome = ui::outcome(name, result, failed);
                let note = note.map(|n| format!("\n  {n}")).unwrap_or_default();
                let mut shown = format!("› {name} {}\n  {outcome}", ui::summary(args, &cwd));
                if !failed {
                    for (line, _) in ui::change(name, args, result, before) {
                        shown += &format!("\n{line}");
                    }
                }
                writeln!(std::io::stderr(), "{shown}{note}")
            }
            agent::Shown::Usage(u) => {
                if let Some(c) = u.get("cost").and_then(serde_json::Value::as_f64) {
                    spent = (spent.0 + c, spent.1 + 1);
                }
                Ok(())
            }
            agent::Shown::ImagesDropped { why, refused: true } => {
                writeln!(std::io::stderr(), "{why}\n{}", ui::IMAGES_DROPPED)
            }
            agent::Shown::ImagesDropped { why, .. } => writeln!(
                std::io::stderr(),
                "{why} The images were taken out of the context."
            ),
            agent::Shown::ToolsOff { why, refused: true } => {
                writeln!(std::io::stderr(), "{why}\n{}", ui::TOOLS_OFF)
            }
            agent::Shown::ToolsOff { why, .. } => {
                writeln!(std::io::stderr(), "{why}, {}", ui::NO_TOOLS)
            }
            agent::Shown::Retrying { error, wait, .. } => writeln!(
                std::io::stderr(),
                "{error}\ntrying again in {}s",
                wait.as_secs()
            ),
            agent::Shown::Cleared { results, saved } => writeln!(
                std::io::stderr(),
                "cleared {results} old tool results, about {} tokens, to make room",
                models::short(saved)
            ),
            agent::Shown::Notice(n) => writeln!(std::io::stderr(), "{n}"),
            agent::Shown::Compacted { before, after, .. } => writeln!(
                std::io::stderr(),
                "compacted the context from about {} to {} tokens",
                models::short(before),
                models::short(after)
            ),
            _ => Ok(()),
        };
        if let Err(e) = r {
            write_err.get_or_insert(e);
        }
    });
    if !at_line_start {
        writeln!(out)?;
    }
    if let Ok(s) = sub_spent.lock() {
        spent = (spent.0 + s.0, spent.1 + s.1);
    }
    if spent.1 > 0 {
        let s = if spent.1 == 1 { "" } else { "s" };
        eprintln!("cost {} for {} request{s}", ui::money(spent.0), spent.1);
    }
    turn?;
    if let Some(e) = write_err {
        return Err(e.into());
    }
    if session.cut_reply().is_some() {
        return Err("the reply was cut off: the model ran out of output tokens".into());
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("{NAME}: {e}");
        std::process::exit(1);
    }
}
