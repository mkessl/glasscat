//! Colours worked out from the terminal's own background: soft panels
//! for the user's messages, the input, code and changed lines. Asked of
//! the terminal once at start (OSC 11); a terminal that does not say
//! gets no panels, and text keeps the terminal's palette either way.

use std::sync::OnceLock;

use crossterm::style::Color;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Theme {
    /// Behind the user's messages and the input.
    pub panel: Option<Color>,
    /// Behind code blocks.
    pub code: Option<Color>,
    /// Behind lines a change put in, and took out.
    pub added: Option<Color>,
    pub removed: Option<Color>,
}

static THEME: OnceLock<Theme> = OnceLock::new();

/// The theme; no panels until `detect` found the background.
pub fn get() -> Theme {
    THEME.get().copied().unwrap_or_default()
}

/// Asks the terminal for its background, in raw mode and before anything
/// else reads input. Takes no longer than the terminal's answer to a
/// device-attributes query, which every terminal gives, or 200 ms.
pub fn detect() {
    let theme = background().map(from_background).unwrap_or_default();
    let _ = THEME.set(theme);
}

/// The panels for a background of `bg`.
fn from_background(bg: (u8, u8, u8)) -> Theme {
    let light = luma(bg) > 128.0;
    let toward = if light { (0, 0, 0) } else { (255, 255, 255) };
    let truecolor = truecolor();
    let c = |rgb| Some(color(rgb, truecolor));
    Theme {
        panel: c(blend(bg, toward, if light { 0.06 } else { 0.08 })),
        code: c(blend(bg, toward, if light { 0.035 } else { 0.045 })),
        added: c(blend(bg, (40, 167, 69), if light { 0.18 } else { 0.22 })),
        removed: c(blend(bg, (215, 58, 73), if light { 0.16 } else { 0.22 })),
    }
}

fn luma((r, g, b): (u8, u8, u8)) -> f32 {
    0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32
}

fn blend(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

/// Whether the terminal takes 24-bit colour, by what it tells programs.
fn truecolor() -> bool {
    let var = |k| std::env::var(k).unwrap_or_default();
    let ct = var("COLORTERM");
    ct == "truecolor"
        || ct == "24bit"
        || var("TERM").contains("direct")
        || var("TERM") == "xterm-kitty"
        || !var("WT_SESSION").is_empty()
        || ["iTerm.app", "WezTerm", "vscode", "ghostty"].contains(&var("TERM_PROGRAM").as_str())
}

/// The colour itself, or the nearest of the 256 that every terminal has.
fn color((r, g, b): (u8, u8, u8), truecolor: bool) -> Color {
    if truecolor {
        return Color::Rgb { r, g, b };
    }
    // The 6x6x6 cube or the grey ramp, whichever is nearer.
    let level = |v: u8| -> u8 {
        if v < 48 {
            0
        } else if v < 115 {
            1
        } else {
            (v - 35) / 40
        }
    };
    let value = |l: u8| if l == 0 { 0 } else { 55 + 40 * l as i32 };
    let (lr, lg, lb) = (level(r), level(g), level(b));
    let cube = (value(lr), value(lg), value(lb));
    let avg = (r as i32 + g as i32 + b as i32) / 3;
    let grey = ((avg - 3).clamp(0, 230) / 10) as u8;
    let gv = 8 + 10 * grey as i32;
    let dist = |(x, y, z): (i32, i32, i32)| {
        (x - r as i32).pow(2) + (y - g as i32).pow(2) + (z - b as i32).pow(2)
    };
    if dist((gv, gv, gv)) < dist(cube) {
        Color::AnsiValue(232 + grey.min(23))
    } else {
        Color::AnsiValue(16 + 36 * lr + 6 * lg + lb)
    }
}

#[cfg_attr(not(unix), allow(dead_code))]
/// Parses an OSC 11 answer: `rgb:RRRR/GGGG/BBBB`, 1 to 4 hex digits each.
fn parse(reply: &str) -> Option<(u8, u8, u8)> {
    let rgb = reply.split("rgb:").nth(1)?;
    let mut parts = rgb.split('/').map(|p| {
        let hex: String = p.chars().take_while(char::is_ascii_hexdigit).collect();
        let v = u32::from_str_radix(&hex, 16).ok()?;
        let max = (1u32 << (4 * hex.len() as u32)) - 1;
        (!hex.is_empty()).then(|| (v * 255 / max.max(1)) as u8)
    });
    Some((parts.next()??, parts.next()??, parts.next()??))
}

#[cfg(unix)]
fn background() -> Option<(u8, u8, u8)> {
    use std::io::{Read, Write};
    use std::os::unix::fs::OpenOptionsExt;
    use std::time::{Duration, Instant};
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const NONBLOCK: i32 = 0o4000;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    const NONBLOCK: i32 = 0x0004;
    // tmux answers for itself: its own background, which is what shows.
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(NONBLOCK)
        .open("/dev/tty")
        .ok()?;
    // The background, then device attributes: when the second answer
    // comes first, the terminal does not tell its background.
    tty.write_all(b"\x1b]11;?\x1b\\\x1b[c").ok()?;
    tty.flush().ok()?;
    let start = Instant::now();
    let mut got = Vec::new();
    let mut buf = [0u8; 256];
    while start.elapsed() < Duration::from_millis(200) {
        match tty.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => break,
        }
        let text = String::from_utf8_lossy(&got);
        // The device attributes answer ends the wait: `ESC [ ? ... c`.
        if let Some(at) = text.find("\x1b[?")
            && text[at..].contains('c')
        {
            return parse(&text[..at]);
        }
    }
    parse(&String::from_utf8_lossy(&got))
}

#[cfg(not(unix))]
fn background() -> Option<(u8, u8, u8)> {
    // Windows Terminal and conhost: dark by default.
    std::env::var_os("WT_SESSION").map(|_| (12, 12, 12))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_background_answer() {
        assert_eq!(
            parse("\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\"),
            Some((30, 30, 46))
        );
        assert_eq!(parse("\x1b]11;rgb:ff/ff/ff\x07"), Some((255, 255, 255)));
        assert_eq!(parse("\x1b[?62;c"), None);
    }

    #[test]
    fn panels_lean_away_from_the_background() {
        let dark = from_background((20, 20, 20));
        let light = from_background((250, 250, 250));
        let rgb = |c: Option<Color>| match c {
            Some(Color::Rgb { r, .. }) => r,
            Some(Color::AnsiValue(v)) => v,
            _ => 0,
        };
        if truecolor() {
            assert!(rgb(dark.panel) > 20);
            assert!(rgb(light.panel) < 250);
        }
        assert_ne!(dark.added, dark.removed);
    }

    #[test]
    fn nearest_of_256() {
        assert_eq!(color((0, 0, 0), false), Color::AnsiValue(16));
        assert_eq!(color((128, 128, 128), false), Color::AnsiValue(244));
        assert_eq!(color((255, 0, 0), false), Color::AnsiValue(196));
    }
}
