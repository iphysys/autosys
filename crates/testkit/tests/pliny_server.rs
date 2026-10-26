//! The fake Pliny (chat-completions) server a worker process under test
//! talks to.

#![allow(clippy::unwrap_used, reason = "integration tests")]

use std::time::Duration;

use serde_json::{Value, json};
use testkit::fakes::{FakePlinyServer, completion, tool_call};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const WAIT: Duration = Duration::from_secs(5);

/// Posts `body` to the server and returns the response's body, or `None`
/// if no response arrives within a short wait.
async fn post(server: &FakePlinyServer, body: &Value) -> Option<Value> {
    let addr = server.base_url().trim_start_matches("http://").to_owned();
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let body = body.to_string();
    let request = format!(
        "POST /chat/completions HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = tokio::time::timeout(Duration::from_millis(300), stream.read(&mut chunk)).await;
        match read {
            Ok(Ok(n)) if n > 0 => response.extend_from_slice(&chunk[..n]),
            _ => break,
        }
        let text = String::from_utf8_lossy(&response);
        if let Some((_, body)) = text.split_once("\r\n\r\n")
            && serde_json::from_str::<Value>(body).is_ok()
        {
            break;
        }
    }
    let text = String::from_utf8_lossy(&response).into_owned();
    let (_, body) = text.split_once("\r\n\r\n")?;
    serde_json::from_str(body).ok()
}

#[tokio::test]
async fn answers_each_request_from_its_body_and_holds_the_chosen_one() {
    let server = FakePlinyServer::start(
        |request| completion(Some(&request["n"].to_string()), vec![]),
        Some(1),
    )
    .await;
    let first = post(&server, &json!({ "n": 0 })).await.expect("an answer");
    assert_eq!(first["choices"][0]["message"]["content"], "0");
    assert_eq!(first["choices"][0]["finish_reason"], "stop");

    assert_eq!(
        post(&server, &json!({ "n": 1 })).await,
        None,
        "the held call was answered"
    );
    server.held(WAIT).await;

    let third = post(&server, &json!({ "n": 2 })).await.expect("an answer");
    assert_eq!(third["choices"][0]["message"]["content"], "2");
    assert_eq!(server.requests().len(), 3);
}

#[test]
fn a_reply_with_a_tool_call_finishes_for_tool_calls() {
    let reply = completion(
        None,
        vec![tool_call("t1", "measure", &json!({ "part": "p" }))],
    );
    let choice = &reply["choices"][0];
    assert_eq!(choice["finish_reason"], "tool_calls");
    let call = &choice["message"]["tool_calls"][0];
    assert_eq!(call["function"]["name"], "measure");
    assert_eq!(call["function"]["arguments"], r#"{"part":"p"}"#);
}
