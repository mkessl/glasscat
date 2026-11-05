mod context;
mod openai;
mod provider;
mod sse;

use std::io::Write;
use std::sync::atomic::AtomicBool;

use context::Item;

pub type Result<T, E = Box<dyn std::error::Error + Send + Sync>> = std::result::Result<T, E>;

/// The program's name, taken from the binary name in `Cargo.toml`.
pub const NAME: &str = env!("CARGO_BIN_NAME");

const USAGE: &str = "usage: NAME [--provider NAME] [--model ID] [--show-request] PROMPT...

Sends PROMPT to the model and streams the answer to stdout.

  --provider NAME   one of: PROVIDERS (default: openrouter)
  --model ID        model id (default: the provider's default)
  --show-request    print the exact request body to stderr before sending";

struct Args {
    provider: String,
    model: Option<String>,
    show_request: bool,
    prompt: String,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        provider: "openrouter".into(),
        model: None,
        show_request: false,
        prompt: String::new(),
    };
    let mut words = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            "--provider" => args.provider = it.next().ok_or("--provider needs a value")?,
            "--model" => args.model = Some(it.next().ok_or("--model needs a value")?),
            "--show-request" => args.show_request = true,
            "--" => words.extend(it.by_ref()),
            s if s.starts_with("--") => {
                return Err(format!("unknown option {s}\n\n{}", usage()).into());
            }
            _ => words.push(a),
        }
    }
    if words.is_empty() {
        return Err(usage().into());
    }
    args.prompt = words.join(" ");
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
    let model = args.model.as_deref().unwrap_or(preset.default_model);

    let items = [Item::User(args.prompt)];
    let body = openai::request_body(model, &items, &[]);
    if args.show_request {
        let mut err = std::io::stderr().lock();
        err.write_all(&body)?;
        err.write_all(b"\n")?;
    }

    let stream = openai::send(preset.base_url, key.as_deref(), &body)?;
    let mut out = std::io::stdout().lock();
    let mut write_err = None;
    let outcome = openai::read_stream(stream, &AtomicBool::new(false), |e| {
        if let openai::Event::Text(t) = e
            && let Err(e) = out.write_all(t.as_bytes()).and_then(|_| out.flush())
        {
            write_err.get_or_insert(e);
        }
    })?;
    if let Some(e) = write_err {
        return Err(e.into());
    }
    if !outcome.reply.text.ends_with('\n') {
        writeln!(out)?;
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("{NAME}: {e}");
        std::process::exit(1);
    }
}
