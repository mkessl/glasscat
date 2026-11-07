//! Signing in from the command line: `login` and `logout`. OpenRouter
//! signs in through the browser (OAuth); other providers take a pasted key.

use std::io::{IsTerminal, Read, Write};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::Result;
use crate::provider::Provider;

/// How long a browser sign-in may take; OpenRouter's code lasts 10 minutes.
pub const WAIT: Duration = Duration::from_secs(600);

/// Whether `provider` signs in through the browser rather than a pasted key.
pub fn oauth(provider: &Provider) -> bool {
    provider.name == "openrouter" && provider.base_url.contains("openrouter.ai")
}

/// A pasted OpenRouter key rather than a sign-in code.
pub fn looks_like_key(s: &str) -> bool {
    s.trim().starts_with("sk-")
}

/// `NAME login [PROVIDER]` or `NAME logout [PROVIDER]`.
pub fn run(logout: bool, name: Option<String>) -> Result<()> {
    let settings = crate::settings::load()?;
    let name = name
        .or_else(|| settings.provider.clone())
        .unwrap_or_else(|| "openrouter".into());
    let provider = crate::provider::find(&name, &settings)?;
    let Some(var) = provider.key_env.clone() else {
        println!("{name} needs no key.");
        return Ok(());
    };
    let env_set = std::env::var(&var).is_ok_and(|v| !v.trim().is_empty());
    if logout {
        match crate::auth::remove(&name)? {
            0 => println!("No key for {name} was stored."),
            _ => println!("Removed the stored key for {name}."),
        }
        if env_set {
            println!("{var} is still set, and it is used.");
        }
        return Ok(());
    }
    let key = if !std::io::stdin().is_terminal() {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        s
    } else if oauth(&provider) {
        browser(provider.proxy.as_deref())?
    } else {
        if let Some(page) = crate::provider::keys_page(&name) {
            eprintln!("Get a key at {page}");
        }
        hidden("Paste the key: ")?
    };
    crate::auth::add(&name, &key, crate::NAME)?;
    println!(
        "Signed in to {name}. The key is kept in {}.",
        crate::auth::path().display()
    );
    if env_set {
        println!("{var} is set and overrides it.");
    }
    Ok(())
}

/// OpenRouter's sign-in: the browser sends the code back here, or, with
/// no browser on this machine, the user pastes the code the page shows.
fn browser(proxy: Option<&str>) -> Result<String> {
    let headless = crate::oauth::headless();
    let login = crate::oauth::start(!headless)?;
    if headless {
        eprintln!(
            "Open this page in a browser, sign in, and paste the code it shows:\n\n  {}\n",
            login.url
        );
        let code = hidden("Code: ")?;
        return login.exchange(&code, proxy);
    }
    let verb = if crate::oauth::open_browser(&login.url) {
        "Opened"
    } else {
        "Open"
    };
    eprintln!(
        "{verb} this page to sign in:\n\n  {}\n\nWaiting for the browser…",
        login.url
    );
    let code = login.wait(&AtomicBool::new(false), WAIT)?;
    login.exchange(&code, proxy)
}

/// Reads a line from the terminal without showing it.
fn hidden(prompt: &str) -> Result<String> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    crossterm::terminal::enable_raw_mode()?;
    let mut s = String::new();
    let got = loop {
        let e = match read() {
            Ok(e) => e,
            Err(e) => break Err(e.into()),
        };
        match e {
            Event::Paste(p) => s.push_str(&p),
            Event::Key(k) if k.kind != KeyEventKind::Release => match k.code {
                KeyCode::Enter => break Ok(()),
                KeyCode::Esc => break Err("cancelled".into()),
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    break Err("cancelled".into());
                }
                KeyCode::Backspace => {
                    s.pop();
                }
                KeyCode::Char(c) => s.push(c),
                _ => {}
            },
            _ => {}
        }
    };
    let _ = crossterm::terminal::disable_raw_mode();
    eprintln!();
    got.map(|()| s.trim().to_string())
}
