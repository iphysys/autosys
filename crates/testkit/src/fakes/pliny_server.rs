//! A loopback HTTP server that answers OpenAI-compatible chat completions
//! (`POST {base}/chat/completions`), as Pliny does, for a worker process
//! under test, which reaches it through a plain-http base URL, allowed only
//! in the Test environment (ADR-0031). A responder computes each reply from the request body; one
//! chosen request can be held unanswered, so the test can kill the worker
//! while that call is in flight.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Computes a chat-completions response body from a request body.
pub type Responder = Arc<dyn Fn(&Value) -> Value + Send + Sync>;

struct State {
    responder: Responder,
    /// Request bodies received, in order.
    requests: Vec<Value>,
    /// The request number (from 0) to hold unanswered.
    hold: Option<usize>,
}

pub struct FakePlinyServer {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
    held: watch::Sender<bool>,
    accepting: JoinHandle<()>,
}

impl FakePlinyServer {
    /// Listens on a free loopback port and answers every request with
    /// `responder`, except request number `hold` (from 0), if given, which
    /// it never answers.
    pub async fn start(
        responder: impl Fn(&Value) -> Value + Send + Sync + 'static,
        hold: Option<usize>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the fake Pliny server");
        let addr = listener.local_addr().expect("its address");
        let state = Arc::new(Mutex::new(State {
            responder: Arc::new(responder),
            requests: Vec::new(),
            hold,
        }));
        let held = watch::channel(false).0;
        let accepting = {
            let (state, held) = (Arc::clone(&state), held.clone());
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(serve(stream, Arc::clone(&state), held.clone()));
                }
            })
        };
        Self {
            addr,
            state,
            held,
            accepting,
        }
    }

    /// The base URL a model profile's `base_url_env` names.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Request bodies received so far, in order.
    pub fn requests(&self) -> Vec<Value> {
        self.state.lock().unwrap().requests.clone()
    }

    /// Resolves once the held request has arrived. Panics if it does not
    /// within `within`.
    pub async fn held(&self, within: Duration) {
        let mut held = self.held.subscribe();
        let waited = tokio::time::timeout(within, held.wait_for(|h| *h)).await;
        assert!(
            matches!(waited, Ok(Ok(_))),
            "the held model call did not arrive within {within:?}"
        );
    }

    /// Answers every request from now on, the held one excepted.
    pub fn stop_holding(&self) {
        self.state.lock().unwrap().hold = None;
    }
}

impl Drop for FakePlinyServer {
    fn drop(&mut self) {
        self.accepting.abort();
    }
}

/// The model the fake answers as: the committed `pliny` profile's.
pub const FAKE_PLINY_MODEL: &str = "snps-provider/nemotron-3-ultra-550b-a55";

/// A chat-completions reply: the assistant's text, if any, and its tool
/// calls, with a finish reason that follows from them.
pub fn completion(text: Option<&str>, tool_calls: Vec<Value>) -> Value {
    let finish_reason = if tool_calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    let mut message = json!({ "role": "assistant", "content": text });
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    json!({
        "id": "chatcmpl-fake",
        "object": "chat.completion",
        "model": FAKE_PLINY_MODEL,
        "choices": [{ "index": 0, "message": message, "finish_reason": finish_reason }],
        "usage": { "prompt_tokens": 1000, "completion_tokens": 100, "total_tokens": 1100 }
    })
}

/// One entry of a reply's `tool_calls`; the arguments are sent as a JSON
/// string, as the chat-completions format has them.
pub fn tool_call(id: &str, name: &str, arguments: &Value) -> Value {
    json!({
        "id": id,
        "type": "function",
        "function": { "name": name, "arguments": arguments.to_string() }
    })
}

/// Answers the requests of one connection until the client closes it.
async fn serve(mut stream: TcpStream, state: Arc<Mutex<State>>, held: watch::Sender<bool>) {
    let mut buffer = Vec::new();
    while let Some(body) = read_request(&mut stream, &mut buffer).await {
        let reply = {
            let mut state = state.lock().unwrap();
            let number = state.requests.len();
            state.requests.push(body.clone());
            if state.hold == Some(number) {
                None
            } else {
                Some((state.responder)(&body))
            }
        };
        let Some(reply) = reply else {
            held.send_replace(true);
            // Never answered: the client gives up or is killed.
            std::future::pending::<()>().await;
            return;
        };
        let body = reply.to_string();
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nx-request-id: req_fake\r\n\r\n",
            body.len()
        );
        if stream.write_all(head.as_bytes()).await.is_err()
            || stream.write_all(body.as_bytes()).await.is_err()
        {
            return;
        }
    }
}

/// Reads one request's JSON body, by its `content-length`. `None` when the
/// client closes the connection.
async fn read_request(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Option<Value> {
    let head_end = loop {
        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break end;
        }
        read_more(stream, buffer).await?;
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).to_ascii_lowercase();
    let length: usize = head
        .split("\r\n")
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim() == "content-length")
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0);
    let start = head_end + 4;
    while buffer.len() < start + length {
        read_more(stream, buffer).await?;
    }
    let body = serde_json::from_slice(&buffer[start..start + length]).unwrap_or(Value::Null);
    buffer.drain(..start + length);
    Some(body)
}

async fn read_more(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Option<()> {
    let mut chunk = [0u8; 8192];
    match stream.read(&mut chunk).await {
        Ok(0) | Err(_) => None,
        Ok(n) => {
            buffer.extend_from_slice(&chunk[..n]);
            Some(())
        }
    }
}
