//! Server-sent events, as providers stream model output.

use std::io::{self, BufRead};

/// Reads events from a byte stream and returns the `data` of each one.
pub struct Events<R> {
    reader: R,
    line: Vec<u8>,
}

impl<R: BufRead> Events<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            line: Vec::new(),
        }
    }

    /// The next event's data, `None` at the end of the stream.
    /// Several `data` lines of one event are joined with `\n`; comments and
    /// other fields are skipped.
    pub fn next_data(&mut self) -> io::Result<Option<String>> {
        let mut data: Option<Vec<u8>> = None;
        loop {
            self.line.clear();
            if self.reader.read_until(b'\n', &mut self.line)? == 0 {
                return Ok(data.map(lossy));
            }
            let line = trim_eol(&self.line);
            if line.is_empty() {
                if data.is_some() {
                    return Ok(data.map(lossy));
                }
                continue;
            }
            let (field, value) = match line.iter().position(|&b| b == b':') {
                Some(0) => continue,
                Some(i) => {
                    let v = &line[i + 1..];
                    (&line[..i], v.strip_prefix(b" ").unwrap_or(v))
                }
                None => (line, &[][..]),
            };
            if field == b"data" {
                let d = data.get_or_insert_with(Vec::new);
                if !d.is_empty() {
                    d.push(b'\n');
                }
                d.extend_from_slice(value);
            }
        }
    }
}

fn trim_eol(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn lossy(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(input: &str) -> Vec<String> {
        let mut ev = Events::new(input.as_bytes());
        let mut out = Vec::new();
        while let Some(d) = ev.next_data().unwrap() {
            out.push(d);
        }
        out
    }

    #[test]
    fn splits_events_and_skips_comments() {
        let s =
            ": OPENROUTER PROCESSING\n\ndata: {\"a\":1}\n\ndata:{\"b\":2}\r\n\r\ndata: [DONE]\n\n";
        assert_eq!(all(s), ["{\"a\":1}", "{\"b\":2}", "[DONE]"]);
    }

    #[test]
    fn joins_multiline_data_and_ignores_other_fields() {
        let s = "event: message\nid: 7\ndata: one\ndata: two\n\n";
        assert_eq!(all(s), ["one\ntwo"]);
    }

    #[test]
    fn keeps_last_event_without_blank_line() {
        assert_eq!(all("data: tail"), ["tail"]);
    }

    #[test]
    fn empty_data_is_an_event() {
        assert_eq!(all("data:\n\n"), [""]);
    }
}
