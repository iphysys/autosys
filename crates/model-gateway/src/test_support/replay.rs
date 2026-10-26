//! A loopback HTTP/1.1 server that answers each request with the next
//! recorded reply and captures what it received, so adapter tests exercise
//! the real HTTP client, timeouts and cancellation without a live model.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// One reply as a recording stores it.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reply {
    pub(crate) status: u16,
    #[serde(default)]
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Value,
    /// Held back this long before replying.
    #[serde(skip)]
    pub(crate) delay: Duration,
}

impl Reply {
    pub(crate) fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body,
            delay: Duration::ZERO,
        }
    }

    pub(crate) fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Recording {
    #[allow(dead_code, reason = "documents the recording; not read")]
    description: String,
    replies: Vec<Reply>,
}

/// The replies of the recording at `path`, relative to the crate directory,
/// for example `src/providers/openai_compatible/recordings/text.json`.
pub(crate) fn recording(path: &str) -> Vec<Reply> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: cannot read: {e}", path.display()));
    let recording: Recording = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{}: not a recording: {e}", path.display()));
    recording.replies
}

/// A request the server received.
#[derive(Clone, Debug)]
pub(crate) struct Captured {
    pub(crate) method: String,
    pub(crate) path: String,
    /// Header names in lowercase.
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Value,
}

impl Captured {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Default)]
struct State {
    replies: VecDeque<Reply>,
    captured: Vec<Captured>,
}

pub(crate) struct Replay {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
    accepting: JoinHandle<()>,
}

impl Replay {
    /// Listens on a free loopback port and answers with `replies` in order.
    /// A request past the last reply gets a 500.
    pub(crate) async fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind replay server");
        let addr = listener.local_addr().expect("replay address");
        let state = Arc::new(Mutex::new(State {
            replies: replies.into(),
            captured: Vec::new(),
        }));
        let accepting = {
            let state = state.clone();
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(serve(stream, state.clone()));
                }
            })
        };
        Self {
            addr,
            state,
            accepting,
        }
    }

    pub(crate) fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Requests received so far, in order.
    pub(crate) fn captured(&self) -> Vec<Captured> {
        self.state.lock().expect("replay state").captured.clone()
    }

    /// Replies not yet sent.
    pub(crate) fn remaining(&self) -> usize {
        self.state.lock().expect("replay state").replies.len()
    }
}

impl Drop for Replay {
    fn drop(&mut self) {
        self.accepting.abort();
    }
}

/// Answers the requests of one connection until the client closes it.
async fn serve(mut stream: TcpStream, state: Arc<Mutex<State>>) {
    let mut buffer = Vec::new();
    loop {
        let Some(request) = read_request(&mut stream, &mut buffer).await else {
            return;
        };
        let reply = {
            let mut state = state.lock().expect("replay state");
            state.captured.push(request);
            state.replies.pop_front()
        }
        .unwrap_or_else(|| {
            Reply::json(500, serde_json::json!({ "error": { "type": "replay_exhausted", "message": "no reply left" } }))
        });
        if !reply.delay.is_zero() {
            tokio::time::sleep(reply.delay).await;
        }
        let body = reply.body.to_string();
        let mut head = format!(
            "HTTP/1.1 {} Replay\r\ncontent-type: application/json\r\ncontent-length: {}\r\n",
            reply.status,
            body.len()
        );
        for (name, value) in &reply.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        if stream.write_all(head.as_bytes()).await.is_err()
            || stream.write_all(body.as_bytes()).await.is_err()
        {
            return;
        }
    }
}

/// Reads one request with a `content-length` body. `None` when the client
/// closes the connection.
async fn read_request(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Option<Captured> {
    let head_end = loop {
        if let Some(end) = find(buffer, b"\r\n\r\n") {
            break end;
        }
        let mut chunk = [0u8; 8192];
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_owned();
    let path = request_line.next()?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let body_start = head_end + 4;
    while buffer.len() < body_start + length {
        let mut chunk = [0u8; 8192];
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
        }
    }
    let body =
        serde_json::from_slice(&buffer[body_start..body_start + length]).unwrap_or(Value::Null);
    buffer.drain(..body_start + length);
    Some(Captured {
        method,
        path,
        headers,
        body,
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
