//! A local HTTP server for tests: answers each connection with the next
//! canned response and hands back the requests it got.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::thread::JoinHandle;

pub fn serve(responses: Vec<String>) -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for response in responses {
            let (mut sock, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(sock.try_clone().unwrap());
            let mut head = String::new();
            let mut len = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                head.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0; len];
            reader.read_exact(&mut body).unwrap();
            sock.write_all(response.as_bytes()).unwrap();
            requests.push(head + &String::from_utf8(body).unwrap());
        }
        requests
    });
    (url, handle)
}

pub fn serve_once(response: String) -> (String, JoinHandle<String>) {
    let (url, handle) = serve(vec![response]);
    let handle = std::thread::spawn(move || handle.join().unwrap().remove(0));
    (url, handle)
}

/// An HTTP 200 response streaming the given chunks as server-sent events.
pub fn sse(chunks: &[&str]) -> String {
    let mut body = String::new();
    for c in chunks {
        body.push_str("data: ");
        body.push_str(c);
        body.push_str("\n\n");
    }
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}
