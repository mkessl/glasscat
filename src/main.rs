mod access;
mod agent;
mod context;
mod journal;
mod openai;
mod provider;
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

Without PROMPT, opens the full-screen interface. With PROMPT, gives it to
the model, lets it work with its tools in the current directory, and
streams the answer to stdout; tool calls go to stderr.

  --provider NAME   one of: PROVIDERS (default: openrouter)
  --model ID        model id (default: the provider's default)
  --allow PATH      let the model see PATH too (the current directory always)
  --hide PATH       PATH does not exist for the model; repeatable
  --show-request    print each exact request body to stderr before sending";

struct Args {
    provider: String,
    model: Option<String>,
    allow: Vec<PathBuf>,
    hide: Vec<PathBuf>,
    show_request: bool,
    prompt: Option<String>,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        provider: "openrouter".into(),
        model: None,
        allow: Vec::new(),
        hide: Vec::new(),
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
            "--provider" => args.provider = value()?,
            "--model" => args.model = Some(value()?),
            "--allow" => args.allow.push(value()?.into()),
            "--hide" => args.hide.push(value()?.into()),
            "--show-request" => args.show_request = true,
            "--" => words.extend(it.by_ref()),
            s if s.starts_with("--") => {
                return Err(format!("unknown option {s}\n\n{}", usage()).into());
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
        .replace("NAME [", &format!("{NAME} ["))
        .replace("PROVIDERS", &names.join(", "))
}

fn run() -> Result<()> {
    let args = parse_args()?;
    let preset = provider::preset(&args.provider)?;
    let key = preset.api_key()?;
    let model = args.model.unwrap_or_else(|| preset.default_model.into());

    let cwd = std::env::current_dir()?;
    let mut allowed = vec![cwd.clone()];
    allowed.extend(args.allow);
    let access = access::Access::new(&cwd, &allowed, &args.hide)?;
    let tools = tools::Tools::new(access, journal::Journal::for_session());
    let mut session =
        agent::Session::new(preset.base_url.into(), model, key, preset.prefill, tools);
    let Some(prompt) = args.prompt else {
        use std::io::IsTerminal;
        if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
            return Err(format!("no prompt given and not a terminal\n\n{}", usage()).into());
        }
        return ui::run(session);
    };

    let show_request = args.show_request;
    let mut out = std::io::stdout().lock();
    let mut write_err = None;
    let mut at_line_start = true;
    let turn = session.turn(prompt, &AtomicBool::new(false), |shown| {
        let r = match shown {
            agent::Shown::Request(body) if show_request => {
                let mut err = std::io::stderr().lock();
                err.write_all(body).and_then(|_| err.write_all(b"\n"))
            }
            agent::Shown::Stream(openai::Event::Text(t)) => {
                at_line_start = t.ends_with('\n');
                out.write_all(t.as_bytes()).and_then(|_| out.flush())
            }
            agent::Shown::ToolDone { name, args, result } => {
                if !at_line_start {
                    at_line_start = true;
                    let _ = writeln!(out);
                }
                let first = result.lines().next().unwrap_or("");
                let more = result.lines().count().saturating_sub(1);
                let more = if more > 0 {
                    format!(" (+{more} lines)")
                } else {
                    String::new()
                };
                writeln!(std::io::stderr(), "› {name} {args}\n  {first}{more}")
            }
            _ => Ok(()),
        };
        if let Err(e) = r {
            write_err.get_or_insert(e);
        }
    });
    if !at_line_start {
        writeln!(out)?;
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
