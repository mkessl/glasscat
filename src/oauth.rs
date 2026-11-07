//! OpenRouter login by OAuth PKCE.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::json;

use crate::Result;

const AUTH_URL: &str = "https://openrouter.ai/auth";
const KEYS_URL: &str = "https://openrouter.ai/api/v1/auth/keys";
const POLL: Duration = Duration::from_millis(50);
const CLIENT_READ_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HEAD: usize = 8 << 10;

/// One sign-in attempt: the PKCE verifier, the state, and the callback listener if any.
pub struct Login {
    verifier: String,
    state: String,
    listener: Option<TcpListener>,
    /// The page to open in a browser.
    pub url: String,
}

/// Starts a sign-in. With `callback`, a localhost listener receives the code;
/// without it, the user pastes the code shown on the page.
pub fn start(callback: bool) -> Result<Login> {
    let verifier = random_token(32)?;
    let challenge = challenge(&verifier);
    let state = random_token(16)?;
    let (listener, url) = if callback {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let callback_url = format!("http://localhost:{port}/callback");
        let url = format!(
            "{AUTH_URL}?callback_url={}&code_challenge={challenge}&code_challenge_method=S256&state={}&key_label={}",
            encode(&callback_url),
            encode(&state),
            encode(crate::NAME),
        );
        (Some(listener), url)
    } else {
        let url = format!(
            "{AUTH_URL}?code_challenge={challenge}&code_challenge_method=S256&key_label={}",
            encode(crate::NAME),
        );
        (None, url)
    };
    Ok(Login {
        verifier,
        state,
        listener,
        url,
    })
}

impl Login {
    #[cfg(test)]
    /// The callback port, if a listener is running.
    pub fn port(&self) -> Option<u16> {
        self.listener
            .as_ref()
            .and_then(|l| l.local_addr().ok())
            .map(|a| a.port())
    }

    #[cfg(test)]
    /// True when the user must paste the code by hand.
    pub fn needs_paste(&self) -> bool {
        self.listener.is_none()
    }

    /// Waits for the browser to deliver the code. Fails on cancel, timeout or a sign-in error.
    pub fn wait(&self, cancel: &AtomicBool, timeout: Duration) -> Result<String> {
        let listener = self
            .listener
            .as_ref()
            .ok_or("login has no callback listener")?;
        listener.set_nonblocking(true)?;
        let start = Instant::now();
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err("login cancelled".into());
            }
            if start.elapsed() >= timeout {
                return Err("login timed out".into());
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    if let Some(code) = self.serve(stream)? {
                        return Ok(code);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(POLL);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Handles one connection. Returns the code, or an error if OpenRouter reported a failure.
    /// Broken or unrelated connections are answered and ignored.
    fn serve(&self, mut stream: TcpStream) -> Result<Option<String>> {
        let Some(head) = read_head(&mut stream) else {
            return Ok(None);
        };
        let line = head.lines().next().unwrap_or("");
        let target = line.split_whitespace().nth(1).unwrap_or("");
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        if path != "/callback" {
            respond(&mut stream, "404 Not Found", "not found");
            return Ok(None);
        }
        let params: Vec<(String, String)> = query
            .split('&')
            .filter(|kv| !kv.is_empty())
            .map(|kv| {
                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                (decode(k), decode(v))
            })
            .collect();
        if param(&params, "state") != Some(self.state.as_str()) {
            respond(&mut stream, "400 Bad Request", "state mismatch");
            return Ok(None);
        }
        if let Some(code) = param(&params, "code").filter(|c| !c.is_empty()) {
            respond(
                &mut stream,
                "200 OK",
                &done_page("Got it. The terminal finishes the sign-in; this tab can be closed."),
            );
            return Ok(Some(code.to_string()));
        }
        if let Some(error) = param(&params, "error") {
            respond(
                &mut stream,
                "200 OK",
                &done_page("Sign-in failed. You can close this tab and go back to the terminal."),
            );
            return Err(format!("OpenRouter sign-in failed: {error}").into());
        }
        respond(&mut stream, "400 Bad Request", "missing code");
        Ok(None)
    }

    /// Trades the code for an API key.
    pub fn exchange(&self, code: &str, proxy: Option<&str>) -> Result<String> {
        let code = code.trim();
        if code.is_empty() {
            return Err("empty login code".into());
        }
        let body = json!({
            "code": code,
            "code_verifier": self.verifier,
            "code_challenge_method": "S256",
        });
        let reply = crate::openai::post_json(KEYS_URL, &body, proxy)?;
        match reply["key"].as_str() {
            Some(key) if !key.is_empty() => Ok(key.to_string()),
            _ => Err("OpenRouter sent no key".into()),
        }
    }
}

/// Opens a URL in the default browser. Returns whether the opener started.
pub fn open_browser(url: &str) -> bool {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(windows) {
        ("rundll32", vec!["url.dll,FileProtocolHandler", url])
    } else {
        ("xdg-open", vec![url])
    };
    std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        // Waited for, so it leaves no finished process behind.
        .map(|mut child| std::thread::spawn(move || child.wait()))
        .is_ok()
}

/// True when no browser can be shown on this machine: over SSH, or without a display on Linux.
pub fn headless() -> bool {
    let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    if set("SSH_CONNECTION") || set("SSH_TTY") {
        return true;
    }
    !cfg!(any(target_os = "macos", windows)) && !set("DISPLAY") && !set("WAYLAND_DISPLAY")
}

/// The S256 code challenge of a verifier.
fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(ring::digest::digest(
        &ring::digest::SHA256,
        verifier.as_bytes(),
    ))
}

/// Random bytes, base64url without padding.
fn random_token(n: usize) -> Result<String> {
    let mut bytes = vec![0u8; n];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "no system randomness")?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Reads the request head (up to the blank line, at most 8 KB). None on I/O failure.
fn read_head(stream: &mut TcpStream) -> Option<String> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(CLIENT_READ_TIMEOUT)).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while buf.len() < MAX_HEAD && !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let want = (MAX_HEAD - buf.len()).min(chunk.len());
        match stream.read(&mut chunk[..want]) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Writes a complete HTTP/1.1 reply and closes the connection. Write errors are ignored.
fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// The small page shown in the browser after the callback.
fn done_page(message: &str) -> String {
    format!(
        "<!doctype html><meta charset=\"utf-8\"><title>{}</title><p>{message}</p>",
        crate::NAME
    )
}

/// The first value of a query parameter.
fn param<'a>(params: &'a [(String, String)], name: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// Percent-encodes everything except unreserved characters.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decodes percent escapes and `+` as space. Bad escapes are kept as they are.
fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                let hex = b
                    .get(i + 1..i + 3)
                    .and_then(|h| std::str::from_utf8(h).ok())
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;

    #[test]
    fn challenge_matches_rfc_7636() {
        // Checked with openssl and Python hashlib; the RFC 7636 pair given in the spec does not match.
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn start_builds_url() {
        let login = start(true).unwrap();
        let port = login.port().unwrap();
        assert!(login.url.starts_with("https://openrouter.ai/auth?"));
        assert!(login.url.contains(&format!(
            "callback_url=http%3A%2F%2Flocalhost%3A{port}%2Fcallback"
        )));
        assert!(login.url.contains("code_challenge_method=S256"));
        assert!(!login.needs_paste());

        let manual = start(false).unwrap();
        assert!(!manual.url.contains("callback_url"));
        assert!(!manual.url.contains("state="));
        assert!(manual.needs_paste());
        assert!(
            manual
                .wait(&AtomicBool::new(false), Duration::from_millis(100))
                .is_err()
        );
    }

    #[test]
    fn wait_receives_code() {
        let login = start(true).unwrap();
        let port = login.port().unwrap();
        let state = login.state.clone();
        let client = std::thread::spawn(move || {
            let send = |target: &str| -> String {
                let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
                write!(s, "GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
                let mut out = String::new();
                s.read_to_string(&mut out).unwrap();
                out
            };
            assert!(send("/favicon.ico").starts_with("HTTP/1.1 404"));
            assert!(send("/callback?code=x&state=wrong").starts_with("HTTP/1.1 400"));
            let ok = send(&format!("/callback?code=ab%2Bc&state={state}"));
            assert!(ok.starts_with("HTTP/1.1 200 OK"));
            assert!(ok.contains("Content-Type: text/html; charset=utf-8"));
        });
        let code = login
            .wait(&AtomicBool::new(false), Duration::from_secs(10))
            .unwrap();
        client.join().unwrap();
        assert_eq!(code, "ab+c");
    }

    #[test]
    fn wait_stops_when_cancelled() {
        let login = start(true).unwrap();
        let started = Instant::now();
        let result = login.wait(&AtomicBool::new(true), Duration::from_secs(5));
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn percent_round_trip() {
        let s = "a b+c/é?=&~._-";
        assert_eq!(decode(&encode(s)), s);
        assert_eq!(encode("a b"), "a%20b");
        assert_eq!(decode("a+b%2Bc"), "a b+c");
        assert_eq!(decode("100%"), "100%");
    }
}
