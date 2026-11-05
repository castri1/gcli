//! A scripted HTTP server on 127.0.0.1 for the tests: it answers each request with the next
//! reply in its queue (or, for requests sent at the same time, by what their path contains:
//! [`MockServer::route`]) and keeps what it was sent. Point a client at it with
//! `GCLI_API_ROOT`'s mapping ([`super::api_url`]).

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct Reply {
    status: u16,
    body: String,
}

impl Reply {
    pub fn json(status: u16, body: &str) -> Reply {
        Reply { status, body: body.to_string() }
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// Path and query string.
    pub path: String,
    /// Header names lower-cased.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

pub struct MockServer {
    port: u16,
    replies: Arc<Mutex<VecDeque<Reply>>>,
    requests: Arc<Mutex<Vec<Request>>>,
    routes: Arc<Mutex<Vec<(String, Reply)>>>,
}

impl MockServer {
    pub fn start(replies: Vec<Reply>) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
        let port = listener.local_addr().unwrap().port();
        let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let routes: Arc<Mutex<Vec<(String, Reply)>>> = Arc::new(Mutex::new(Vec::new()));
        let (queue, seen, routed) = (replies.clone(), requests.clone(), routes.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Some(request) = read_request(&mut stream) else { continue };
                let route = routed.lock().unwrap().iter().find(|(p, _)| request.path.contains(p)).map(|r| r.1.clone());
                seen.lock().unwrap().push(request);
                let reply = route
                    .or_else(|| queue.lock().unwrap().pop_front())
                    .unwrap_or(Reply::json(500, r#"{"error":"no reply scripted"}"#));
                let _ = write!(
                    stream,
                    "HTTP/1.1 {} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    reply.status,
                    reply.body.len(),
                    reply.body
                );
                let _ = stream.flush();
            }
        });
        MockServer { port, replies, requests, routes }
    }

    /// The base URL, as `GCLI_API_ROOT` would be set.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Answers every request whose path (with its query) contains `pattern` with `reply`,
    /// before the queue: for requests sent at the same time, in an order no one knows.
    pub fn route(&self, pattern: &str, reply: Reply) {
        self.routes.lock().unwrap().push((pattern.to_string(), reply));
    }

    pub fn push(&self, reply: Reply) {
        self.replies.lock().unwrap().push_back(reply);
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next()?.to_string(), parts.next()?.to_string());
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        reader.read_line(&mut h).ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let length = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(Request { method, path, headers, body: String::from_utf8_lossy(&body).into_owned() })
}
