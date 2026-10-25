//! The typed client of the external HTTP API v0 (blueprint section 3.5,
//! ADR-0018, ADR-0034), as `schemas/api/openapi.yaml` describes it.
//!
//! One method per operation; bodies are the generated types of
//! `contracts::types`. The client builds no task and decides nothing: the
//! server does. [`Client::events`] streams a run's events and reconnects
//! with `Last-Event-ID` when the connection breaks, so its caller sees
//! every event once, in order. [`EventJournal`] keeps them on disk for a
//! caller that stops and starts again.
//!
//! Every request carries the development token (ADR-0022), which is never
//! logged, and has a time limit.

mod error;
mod journal;
mod key;
mod sse;

use std::collections::VecDeque;
use std::fmt;
use std::time::Duration;

use contracts::ids::{ApprovalId, ArtifactId, RunId};
use contracts::types::{
    ApprovalAnswerRequest, ArtifactMetadata, CancelRequest, Event, JobState, Run, RunReport,
    RunRequest,
};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue};
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The ids and bodies of API v0, for clients that may depend on this crate
/// alone (apps, blueprint section 10.3).
pub use contracts::{ids, types};
pub use error::ClientError;
pub use journal::EventJournal;
pub use key::IdempotencyKey;

/// The development token (ADR-0022). Its value is never logged or printed.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// InvalidInput if `token` is empty.
    pub fn new(token: String) -> Result<Self, ClientError> {
        if token.is_empty() {
            return Err(ClientError::InvalidInput(
                "the development token is empty".to_owned(),
            ));
        }
        Ok(Self(token))
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(..)")
    }
}

/// Time limits and reconnection of a [`Client`].
#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub connect_timeout: Duration,
    /// Every request but an event stream.
    pub request_timeout: Duration,
    /// How long an open event stream may stay silent. The server sends a
    /// keep-alive comment every 15 s by default (`config/api.yaml`).
    pub stream_idle_timeout: Duration,
    /// How many times in a row a broken stream, or a run start that could
    /// not reach the server, is tried again.
    pub retries: u32,
    /// The wait before the first retry; it doubles up to `max_backoff`.
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
            stream_idle_timeout: Duration::from_secs(60),
            retries: 5,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(8),
        }
    }
}

/// A run start's outcome: the run, and whether this call created it (201)
/// or an earlier call with the same key did (200).
#[derive(Clone, Debug)]
pub struct Started {
    pub run: Run,
    pub created: bool,
}

/// A client of one api-server.
#[derive(Clone, Debug)]
pub struct Client {
    http: reqwest::Client,
    /// Without a trailing `/`.
    base: String,
    token: Token,
    config: ClientConfig,
}

impl Client {
    /// A client of the server at `base_url`, for example
    /// `http://127.0.0.1:8080`.
    pub fn new(base_url: &str, token: Token, config: ClientConfig) -> Result<Self, ClientError> {
        let base = base_url.trim_end_matches('/');
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(ClientError::InvalidInput(format!(
                "`{base_url}` is not an http or https URL"
            )));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .build()
            .map_err(|e| ClientError::InvalidInput(format!("cannot build the HTTP client: {e}")))?;
        Ok(Self {
            http,
            base: base.to_owned(),
            token,
            config,
        })
    }

    /// `POST /v0/runs`. A start that cannot reach the server is tried again
    /// with the same key, so it never starts two runs.
    pub async fn start_run(
        &self,
        request: &RunRequest,
        key: &IdempotencyKey,
    ) -> Result<Started, ClientError> {
        let body = body(request)?;
        let mut backoff = Backoff::new(&self.config);
        loop {
            let call = self
                .request(Method::POST, "/v0/runs")
                .header("Idempotency-Key", key.as_str())
                .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
                .body(body.clone());
            match self.send(call, self.config.request_timeout).await {
                Ok(response) => {
                    let created = response.status() == StatusCode::CREATED;
                    let run = json(response, self.config.request_timeout).await?;
                    return Ok(Started { run, created });
                }
                Err(error) if error.is_retryable() && backoff.wait().await => {
                    tracing::warn!(%error, "starting the run again with the same key");
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// `GET /v0/runs/{run_id}`.
    pub async fn run(&self, run_id: &RunId) -> Result<Run, ClientError> {
        let path = format!("/v0/runs/{}", run_id.as_str());
        self.get_json(&path).await
    }

    /// `POST /v0/runs/{run_id}/approvals/{approval_id}`: the stored
    /// `approval_answered` event.
    pub async fn answer_approval(
        &self,
        run_id: &RunId,
        approval_id: &ApprovalId,
        answer: &ApprovalAnswerRequest,
    ) -> Result<Event, ClientError> {
        let path = format!(
            "/v0/runs/{}/approvals/{}",
            run_id.as_str(),
            approval_id.as_str()
        );
        self.post_json(&path, answer).await
    }

    /// `POST /v0/runs/{run_id}/cancel`: the stored `cancel_requested`
    /// event.
    pub async fn cancel(
        &self,
        run_id: &RunId,
        request: &CancelRequest,
    ) -> Result<Event, ClientError> {
        let path = format!("/v0/runs/{}/cancel", run_id.as_str());
        self.post_json(&path, request).await
    }

    /// `GET /v0/runs/{run_id}/report`.
    pub async fn report(&self, run_id: &RunId) -> Result<RunReport, ClientError> {
        let path = format!("/v0/runs/{}/report", run_id.as_str());
        self.get_json(&path).await
    }

    /// `GET /v0/artifacts/{artifact_id}`.
    pub async fn artifact(
        &self,
        artifact_id: &ArtifactId,
    ) -> Result<ArtifactMetadata, ClientError> {
        let path = format!("/v0/artifacts/{}", artifact_id.as_str());
        self.get_json(&path).await
    }

    /// `GET /v0/artifacts/{artifact_id}/content`: the file's bytes.
    pub async fn artifact_content(&self, artifact_id: &ArtifactId) -> Result<Vec<u8>, ClientError> {
        let path = format!("/v0/artifacts/{}/content", artifact_id.as_str());
        let timeout = self.config.request_timeout;
        let response = self.send(self.request(Method::GET, &path), timeout).await?;
        let bytes = limited(timeout, response.bytes()).await??;
        Ok(bytes.to_vec())
    }

    /// The run's events after `after` (0 for all), as `GET
    /// /v0/runs/{run_id}/events` streams them.
    pub fn events(&self, run_id: &RunId, after: u64) -> EventStream {
        EventStream {
            client: self.clone(),
            run: run_id.clone(),
            last_seq: after,
            response: None,
            parser: sse::Parser::default(),
            pending: VecDeque::new(),
            backoff: Backoff::new(&self.config),
            ended: false,
        }
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token.0)
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let timeout = self.config.request_timeout;
        let call = self
            .request(Method::GET, path)
            .header(ACCEPT, HeaderValue::from_static("application/json"));
        json(self.send(call, timeout).await?, timeout).await
    }

    async fn post_json<T: DeserializeOwned>(
        &self,
        path: &str,
        request: &impl Serialize,
    ) -> Result<T, ClientError> {
        let timeout = self.config.request_timeout;
        let call = self
            .request(Method::POST, path)
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .body(body(request)?);
        json(self.send(call, timeout).await?, timeout).await
    }

    /// Sends `call` and returns its response if the status is a success;
    /// otherwise the server's `ApiError`.
    async fn send(&self, call: RequestBuilder, timeout: Duration) -> Result<Response, ClientError> {
        let response = limited(timeout, call.send()).await??;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let bytes = limited(timeout, response.bytes()).await??;
        Err(match serde_json::from_slice(&bytes) {
            Ok(body) => ClientError::Api {
                status: status.as_u16(),
                body,
            },
            Err(_) => ClientError::Status {
                status: status.as_u16(),
            },
        })
    }
}

/// A run's events, in order and each once. [`EventStream::next`] reconnects
/// after the last event received when the connection breaks, a bounded
/// number of times in a row.
#[derive(Debug)]
pub struct EventStream {
    client: Client,
    run: RunId,
    last_seq: u64,
    response: Option<Response>,
    parser: sse::Parser,
    pending: VecDeque<Event>,
    backoff: Backoff,
    ended: bool,
}

impl EventStream {
    /// The sequence number of the last event returned, or the `after` the
    /// stream started from.
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// The next event; `None` once the run has stopped for good and every
    /// event has been returned. A suspended run's stream waits.
    pub async fn next(&mut self) -> Result<Option<Event>, ClientError> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            if self.ended {
                return Ok(None);
            }
            let Some(response) = self.response.as_mut() else {
                match self.connect().await {
                    Ok(response) => self.response = Some(response),
                    Err(error) => self.retry(error).await?,
                }
                continue;
            };
            let idle = self.client.config.stream_idle_timeout;
            match tokio::time::timeout(idle, response.chunk()).await {
                Ok(Ok(Some(chunk))) => {
                    for frame in self.parser.feed(&chunk) {
                        let event = self.event_of(&frame)?;
                        self.last_seq = event.seq.get();
                        self.pending.push_back(event);
                        self.backoff.reset();
                    }
                }
                Ok(Ok(None)) => {
                    // The server ends a stream when the run has stopped for
                    // good, but also when it shuts down or a read fails.
                    self.response = None;
                    let run = self.client.run(&self.run).await;
                    match run {
                        Ok(run) if is_final(&run) && run.last_seq <= self.last_seq => {
                            self.ended = true;
                        }
                        Ok(_) => {
                            self.retry(ClientError::Transport(
                                "the event stream ended before the run stopped".to_owned(),
                            ))
                            .await?;
                        }
                        Err(error) => self.retry(error).await?,
                    }
                }
                Ok(Err(error)) => {
                    self.response = None;
                    self.retry(error.into()).await?;
                }
                Err(_) => {
                    self.response = None;
                    self.retry(ClientError::Timeout(idle)).await?;
                }
            }
        }
    }

    async fn connect(&mut self) -> Result<Response, ClientError> {
        self.parser = sse::Parser::default();
        let path = format!("/v0/runs/{}/events", self.run.as_str());
        let mut call = self
            .client
            .request(Method::GET, &path)
            .header(ACCEPT, HeaderValue::from_static("text/event-stream"));
        if self.last_seq > 0 {
            call = call.header("Last-Event-ID", self.last_seq.to_string());
        }
        tracing::debug!(
            run = self.run.as_str(),
            after = self.last_seq,
            "opening the event stream"
        );
        // The connection is limited; the stream itself by its idle timeout.
        self.client
            .send(call, self.client.config.request_timeout)
            .await
    }

    /// Waits before the next attempt, or fails with `error` once the
    /// retries are used up or it cannot succeed by trying again.
    async fn retry(&mut self, error: ClientError) -> Result<(), ClientError> {
        if error.is_retryable() && self.backoff.wait().await {
            tracing::warn!(run = self.run.as_str(), %error, "reconnecting the event stream");
            return Ok(());
        }
        Err(error)
    }

    /// The frame's event, which must be the one after the last.
    fn event_of(&self, frame: &sse::Frame) -> Result<Event, ClientError> {
        let event: Event = serde_json::from_str(&frame.data)
            .map_err(|e| ClientError::Protocol(format!("a frame is not an event: {e}")))?;
        let seq = event.seq.get();
        if event.run_id != self.run {
            return Err(ClientError::Protocol(format!(
                "event {seq} belongs to run {}",
                event.run_id.as_str()
            )));
        }
        if seq != self.last_seq + 1 {
            return Err(ClientError::Protocol(format!(
                "event {seq} after event {}",
                self.last_seq
            )));
        }
        if frame.id.as_deref().is_some_and(|id| id != seq.to_string()) {
            return Err(ClientError::Protocol(format!(
                "event {seq} came in a frame with another id"
            )));
        }
        Ok(event)
    }
}

/// Whether the run has stopped for good.
pub fn is_final(run: &Run) -> bool {
    matches!(
        contracts::job_state(&run.status),
        JobState::Completed | JobState::Cancelled | JobState::Failed
    )
}

/// Bounded, doubling waits between attempts.
#[derive(Debug)]
struct Backoff {
    failures: u32,
    retries: u32,
    initial: Duration,
    max: Duration,
}

impl Backoff {
    fn new(config: &ClientConfig) -> Self {
        Self {
            failures: 0,
            retries: config.retries,
            initial: config.initial_backoff,
            max: config.max_backoff,
        }
    }

    /// Waits and returns true, or returns false once the retries are used.
    async fn wait(&mut self) -> bool {
        if self.failures >= self.retries {
            return false;
        }
        let factor = 2u32.saturating_pow(self.failures);
        self.failures += 1;
        tokio::time::sleep(self.initial.saturating_mul(factor).min(self.max)).await;
        true
    }

    fn reset(&mut self) {
        self.failures = 0;
    }
}

fn body(value: &impl Serialize) -> Result<Vec<u8>, ClientError> {
    serde_json::to_vec(value)
        .map_err(|e| ClientError::InvalidInput(format!("cannot serialize the request: {e}")))
}

/// The response body as `T`.
async fn json<T: DeserializeOwned>(
    response: Response,
    timeout: Duration,
) -> Result<T, ClientError> {
    let bytes = limited(timeout, response.bytes()).await??;
    serde_json::from_slice(&bytes)
        .map_err(|e| ClientError::Protocol(format!("the body is not the expected type: {e}")))
}

/// `future`, or Timeout after `limit`.
async fn limited<T>(
    limit: Duration,
    future: impl Future<Output = Result<T, reqwest::Error>>,
) -> Result<Result<T, ClientError>, ClientError> {
    tokio::time::timeout(limit, future)
        .await
        .map(|result| result.map_err(ClientError::from))
        .map_err(|_| ClientError::Timeout(limit))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "tests")]

    use super::*;

    fn token() -> Token {
        Token::new("secret-token-value".to_owned()).expect("a token")
    }

    #[test]
    fn the_token_is_never_printed() {
        let client =
            Client::new("http://127.0.0.1:8080/", token(), ClientConfig::default()).expect("ok");
        assert!(!format!("{client:?}").contains("secret-token-value"));
        assert!(Token::new(String::new()).is_err());
    }

    #[test]
    fn the_base_url_must_be_http() {
        for good in [
            "http://127.0.0.1:8080",
            "http://localhost:1/",
            "https://api.example",
        ] {
            assert!(
                Client::new(good, token(), ClientConfig::default()).is_ok(),
                "{good}"
            );
        }
        for bad in ["127.0.0.1:8080", "ftp://x", ""] {
            assert!(
                matches!(
                    Client::new(bad, token(), ClientConfig::default()),
                    Err(ClientError::InvalidInput(_))
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn requests_go_under_the_base_with_the_token() {
        let client =
            Client::new("http://127.0.0.1:8080/", token(), ClientConfig::default()).expect("ok");
        let request = client
            .request(Method::GET, "/v0/runs/run-1")
            .build()
            .expect("a request");
        assert_eq!(
            request.url().as_str(),
            "http://127.0.0.1:8080/v0/runs/run-1"
        );
        let auth = request
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .expect("authorization");
        assert!(auth.is_sensitive());
        assert_eq!(auth, "Bearer secret-token-value");
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_doubles_up_to_its_limit_and_stops() {
        let config = ClientConfig {
            retries: 4,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(250),
            ..ClientConfig::default()
        };
        let mut backoff = Backoff::new(&config);
        let start = tokio::time::Instant::now();
        let mut waits = Vec::new();
        while backoff.wait().await {
            waits.push(start.elapsed());
        }
        let ms: Vec<u128> = waits.iter().map(Duration::as_millis).collect();
        assert_eq!(ms, [100, 300, 550, 800]);
        backoff.reset();
        assert!(backoff.wait().await);
    }

    #[test]
    fn retryable_errors() {
        assert!(ClientError::Transport("x".to_owned()).is_retryable());
        assert!(ClientError::Timeout(Duration::from_secs(1)).is_retryable());
        assert!(ClientError::Status { status: 502 }.is_retryable());
        assert!(!ClientError::Status { status: 404 }.is_retryable());
        assert!(!ClientError::Protocol("x".to_owned()).is_retryable());
    }
}
