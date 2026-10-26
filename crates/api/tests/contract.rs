//! The handlers against `schemas/api/openapi.yaml` (blueprint section 16.1):
//! every documented response of every operation is produced and checked,
//! requests without the token are refused on every route, and a stream
//! resumed from its last id misses and repeats nothing. PR 14's done-when
//! tests, in memory.

#![allow(clippy::expect_used, reason = "integration tests")]

mod support;

use std::collections::BTreeSet;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use contracts::ids::RunId;
use contracts::{ArtifactStore, Clock, EventLog, JobStore};
use serde_json::json;
use support::*;
use testkit::builders::{artifact, id, marker_payload, new_event};
use testkit::leases::api_command;

/// Produces every documented (operation, status) pair and checks each
/// response, then fails if any documented pair was never produced.
#[tokio::test]
async fn every_documented_response_conforms_to_the_document() {
    let contract = Contract::load();
    let w = World::new();

    // A run that stops: approved, then completed, with an artifact.
    let done = w.start(&contract, "key-done").await;
    let claimed = w.claim().await;
    assert_eq!(claimed.job.run_id, done);
    w.record(
        &claimed,
        json!({ "type": "approval_requested", "approval_id": "appr-1", "gate": "approval",
            "subject_id": "tr-1", "summary": "Commit the final design." }),
    )
    .await;
    let answer = json!({ "answer": "approve", "comment": "Looks right." });
    let uri = format!("/v0/runs/{}/approvals/appr-1", done.as_str());
    let answered = contract
        .check("answerApproval", w.send(post(&uri, None, &answer)).await)
        .await;
    assert_eq!(answered.status, StatusCode::ACCEPTED);
    assert_eq!(answered.json["payload"]["type"], "approval_answered");
    assert_eq!(
        answered.json["actor"],
        json!({ "kind": "user", "user_id": "dev" })
    );
    let again = contract
        .check("answerApproval", w.send(post(&uri, None, &answer)).await)
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT, "answered twice");
    w.record(
        &claimed,
        json!({ "type": "run_stopped", "stop_reason": "success", "sdr_version": 1 }),
    )
    .await;
    w.complete(&claimed).await;
    let content = b"ISO-10303-21;";
    w.artifacts
        .put(
            &artifact("art-1", done.as_str(), w.clock.now(), content),
            content,
        )
        .await
        .expect("store an artifact");

    // A run that is still queued.
    let queued = w.start(&contract, "key-queued").await;

    // createRun
    let same = contract
        .check(
            "createRun",
            w.send(post("/v0/runs", Some("key-queued"), &run_request()))
                .await,
        )
        .await;
    assert_eq!(same.status, StatusCode::OK, "a repeat returns the run");
    assert_eq!(same.json["run_id"], queued.as_str());
    let mut other = run_request();
    other["goal"] = json!("Another goal.");
    let reused = contract
        .check(
            "createRun",
            w.send(post("/v0/runs", Some("key-queued"), &other)).await,
        )
        .await;
    assert_eq!(reused.status, StatusCode::CONFLICT);
    for bad in [
        post("/v0/runs", None, &run_request()),
        post("/v0/runs", Some("not a key"), &run_request()),
        post(
            "/v0/runs",
            Some("key-x"),
            &json!({ "schema_version": "0.1" }),
        ),
    ] {
        let checked = contract.check("createRun", w.send(bad).await).await;
        assert_eq!(
            checked.status,
            StatusCode::BAD_REQUEST,
            "{:?}",
            checked.json
        );
    }
    let mut unknown = run_request();
    unknown["agent_id"] = json!("no_such_agent");
    let refused = contract
        .check(
            "createRun",
            w.send(post("/v0/runs", Some("key-y"), &unknown)).await,
        )
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);

    // getRun
    for (uri, status) in [
        (format!("/v0/runs/{}", queued.as_str()), StatusCode::OK),
        ("/v0/runs/run-unknown".to_owned(), StatusCode::NOT_FOUND),
        ("/v0/runs/bad%20id".to_owned(), StatusCode::BAD_REQUEST),
    ] {
        let checked = contract.check("getRun", w.send(get(&uri)).await).await;
        assert_eq!(checked.status, status, "{uri}: {:?}", checked.json);
    }

    // streamRunEvents: a stopped run's stream ends after its last event.
    let stream = contract
        .check(
            "streamRunEvents",
            w.send(get(&format!("/v0/runs/{}/events", done.as_str())))
                .await,
        )
        .await;
    assert_eq!(stream.status, StatusCode::OK);
    let seqs: Vec<u64> = stream.frames.iter().map(|f| f.id).collect();
    let last = w.events.last_seq(&done).await.expect("last seq");
    assert_eq!(seqs, (1..=last).collect::<Vec<_>>());
    let mut bad_id = get(&format!("/v0/runs/{}/events", done.as_str()));
    bad_id
        .headers_mut()
        .insert("last-event-id", "seven".parse().expect("a header"));
    for (request, status) in [
        (bad_id, StatusCode::BAD_REQUEST),
        (get("/v0/runs/run-unknown/events"), StatusCode::NOT_FOUND),
    ] {
        let checked = contract
            .check("streamRunEvents", w.send(request).await)
            .await;
        assert_eq!(checked.status, status);
    }

    // answerApproval
    let ask =
        |run: &RunId, approval: &str| format!("/v0/runs/{}/approvals/{approval}", run.as_str());
    for (request, status) in [
        (
            post(&ask(&queued, "appr-9"), None, &answer),
            StatusCode::NOT_FOUND,
        ),
        (
            post(&ask(&id("run-unknown"), "appr-1"), None, &answer),
            StatusCode::NOT_FOUND,
        ),
        (
            post(&ask(&done, "appr-1"), None, &answer),
            StatusCode::CONFLICT,
        ),
        (
            post(&ask(&queued, "appr-1"), None, &json!({ "answer": "maybe" })),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let checked = contract
            .check("answerApproval", w.send(request).await)
            .await;
        assert_eq!(checked.status, status, "{:?}", checked.json);
    }

    // cancelRun
    let cancel = |run: &RunId| format!("/v0/runs/{}/cancel", run.as_str());
    for (request, status) in [
        (
            post(
                &cancel(&queued),
                None,
                &json!({ "reason": "Changed my mind." }),
            ),
            StatusCode::ACCEPTED,
        ),
        (post(&cancel(&done), None, &json!({})), StatusCode::CONFLICT),
        (
            post(&cancel(&id("run-unknown")), None, &json!({})),
            StatusCode::NOT_FOUND,
        ),
        (
            post(&cancel(&queued), None, &json!({ "reason": "" })),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let checked = contract.check("cancelRun", w.send(request).await).await;
        assert_eq!(checked.status, status, "{:?}", checked.json);
    }

    // getRunReport
    for (uri, status) in [
        (format!("/v0/runs/{}/report", done.as_str()), StatusCode::OK),
        (
            format!("/v0/runs/{}/report", queued.as_str()),
            StatusCode::CONFLICT,
        ),
        (
            "/v0/runs/run-unknown/report".to_owned(),
            StatusCode::NOT_FOUND,
        ),
        (
            "/v0/runs/bad%20id/report".to_owned(),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let checked = contract
            .check("getRunReport", w.send(get(&uri)).await)
            .await;
        assert_eq!(checked.status, status, "{uri}");
    }

    // getSdrVersion: the example view is version 2 of example-project.
    let versions = "/v0/projects/example-project/sdr/versions";
    for (uri, status) in [
        (format!("{versions}/2"), StatusCode::OK),
        (format!("{versions}/3"), StatusCode::NOT_FOUND),
        (
            "/v0/projects/project-unknown/sdr/versions/0".to_owned(),
            StatusCode::NOT_FOUND,
        ),
        (format!("{versions}/two"), StatusCode::BAD_REQUEST),
        (
            "/v0/projects/bad%20id/sdr/versions/2".to_owned(),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let checked = contract
            .check("getSdrVersion", w.send(get(&uri)).await)
            .await;
        assert_eq!(checked.status, status, "{uri}: {:?}", checked.json);
        if status == StatusCode::OK {
            assert_eq!(checked.json["record"]["version"], 2);
            assert_eq!(checked.json["evidence"][0]["id"], "ev-check-mass-1");
        }
    }

    // getArtifact and getArtifactContent
    for (uri, status) in [
        ("/v0/artifacts/art-1", StatusCode::OK),
        ("/v0/artifacts/art-9", StatusCode::NOT_FOUND),
        ("/v0/artifacts/bad%20id", StatusCode::BAD_REQUEST),
    ] {
        let checked = contract.check("getArtifact", w.send(get(uri)).await).await;
        assert_eq!(checked.status, status, "{uri}");
        let content_uri = format!("{uri}/content");
        let checked = contract
            .check("getArtifactContent", w.send(get(&content_uri)).await)
            .await;
        assert_eq!(checked.status, status, "{content_uri}");
        if status == StatusCode::OK {
            assert_eq!(&checked.bytes[..], content);
            assert_eq!(checked.headers[header::CONTENT_TYPE], "model/step");
            assert_eq!(checked.headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        }
    }

    // 413 and 415 on every operation with a body.
    let small = World::with_limits(api::Limits {
        max_body_bytes: 64,
        ..limits()
    });
    let small_run = small.start_unchecked().await;
    let large = json!({ "reason": "x".repeat(200) });
    for (operation, uri) in [
        ("createRun", "/v0/runs".to_owned()),
        ("cancelRun", cancel(&small_run)),
        ("answerApproval", ask(&small_run, "appr-1")),
    ] {
        let checked = contract
            .check(
                operation,
                small.send(post(&uri, Some("key-z"), &large)).await,
            )
            .await;
        assert_eq!(checked.status, StatusCode::PAYLOAD_TOO_LARGE, "{operation}");
        let text = authorized(Method::POST, &uri)
            .header(header::CONTENT_TYPE, "text/plain")
            .header("idempotency-key", "key-z")
            .body(Body::from("hello"))
            .expect("a request");
        let checked = contract.check(operation, w.send(text).await).await;
        assert_eq!(
            checked.status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{operation}"
        );
    }

    // 401 on every operation.
    for (path, method, op) in contract.operations() {
        let uri = path
            .replace("{run_id}", queued.as_str())
            .replace("{approval_id}", "appr-1")
            .replace("{artifact_id}", "art-1")
            .replace("{project_id}", "example-project")
            .replace("{version}", "2");
        let operation = op["operationId"].as_str().expect("an id").to_owned();
        for token in [None, Some("Bearer wrong-token"), Some(TOKEN)] {
            let mut builder = Request::builder()
                .method(method.to_uppercase().as_str())
                .uri(&uri)
                .header(header::CONTENT_TYPE, "application/json");
            if let Some(token) = token {
                builder = builder.header(header::AUTHORIZATION, token);
            }
            let request = builder.body(Body::from("{}")).expect("a request");
            let checked = contract.check(&operation, w.send(request).await).await;
            assert_eq!(
                checked.status,
                StatusCode::UNAUTHORIZED,
                "{operation} with {token:?}"
            );
        }
    }

    assert!(
        contract.documented().len() > 30,
        "the document lists too few responses: {:?}",
        contract.documented()
    );
    let missing: BTreeSet<_> = contract
        .documented()
        .difference(&contract.exercised())
        .cloned()
        .collect();
    assert!(missing.is_empty(), "never produced: {missing:?}");
}

/// A client that drops its stream midway and reconnects with the last id
/// it saw receives every later event exactly once, while appends go on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resumed_stream_misses_and_repeats_nothing() {
    const TOTAL: usize = 120;
    let contract = Contract::load();
    let w = World::new();
    let run = w.start(&contract, "key-stream").await;
    let claimed = w.claim().await;

    let events = w.events.clone();
    let clock = w.clock.clone();
    let writer = tokio::spawn({
        let run = run.clone();
        async move {
            for marker in 0..TOTAL {
                events
                    .append(
                        new_event(run.as_str(), clock.now(), marker_payload(marker)),
                        Some(&api_command()),
                    )
                    .await
                    .expect("append");
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    });

    let uri = format!("/v0/runs/{}/events", run.as_str());
    let response = w.send(get(&uri)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut first = FrameReader::new(response.into_body());
    let mut seen = Vec::new();
    while seen.len() < TOTAL / 2 {
        seen.push(first.next().await.expect("the stream goes on").id);
    }
    drop(first);

    let mut resumed = get(&uri);
    let last = *seen.last().expect("some events");
    resumed
        .headers_mut()
        .insert("last-event-id", last.to_string().parse().expect("a header"));
    let response = w.send(resumed).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut second = FrameReader::new(response.into_body());
    writer.await.expect("the writer finishes");
    let total = w.events.last_seq(&run).await.expect("last seq");
    while seen.last().copied() < Some(total) {
        seen.push(second.next().await.expect("the stream goes on").id);
    }
    w.complete(&claimed).await;
    assert!(
        second.next().await.is_none(),
        "the stream ends once the run has stopped"
    );
    assert_eq!(seen, (1..=total).collect::<Vec<_>>(), "gaps or repeats");
    let first_after_resume = seen.iter().position(|s| *s == last + 1);
    assert_eq!(
        first_after_resume,
        Some(TOTAL / 2),
        "resumed after its last id"
    );
}

/// A suspended run can still change, so its stream stays open; shutting the
/// server down ends it.
#[tokio::test]
async fn a_stream_stays_open_until_the_run_stops_or_the_server_shuts_down() {
    let contract = Contract::load();
    let w = World::new();
    let run = w.start(&contract, "key-open").await;
    let response = w
        .send(get(&format!("/v0/runs/{}/events", run.as_str())))
        .await;
    let mut reader = FrameReader::new(response.into_body());
    let first = reader.next().await.expect("the submit's record");
    assert_eq!(first.event, "job_state_changed");
    let waiting = tokio::time::timeout(Duration::from_millis(200), reader.next()).await;
    assert!(
        waiting.is_err(),
        "the stream ended while the run can change"
    );
    w.shutdown.cancel();
    let ended = tokio::time::timeout(Duration::from_secs(5), reader.next())
        .await
        .expect("shutdown ends the stream");
    assert!(ended.is_none());
}

/// The token is compared whole: a prefix or a longer value is refused, and
/// so is another scheme.
#[tokio::test]
async fn only_the_exact_token_is_accepted() {
    let w = World::new();
    for value in [
        format!("Bearer {}", &TOKEN[..TOKEN.len() - 1]),
        format!("Bearer {TOKEN}x"),
        format!("Basic {TOKEN}"),
        format!("bearer{TOKEN}"),
        TOKEN.to_owned(),
    ] {
        let request = Request::builder()
            .uri("/v0/runs/run-1")
            .header(header::AUTHORIZATION, value.as_str())
            .body(Body::empty())
            .expect("a request");
        assert_eq!(
            w.send(request).await.status(),
            StatusCode::UNAUTHORIZED,
            "{value}"
        );
    }
    let unknown_path = w.send(get("/v0/nothing-here")).await;
    assert_eq!(unknown_path.status(), StatusCode::NOT_FOUND);
    let unauthenticated = Request::builder()
        .uri("/v0/nothing-here")
        .body(Body::empty())
        .expect("a request");
    assert_eq!(
        w.send(unauthenticated).await.status(),
        StatusCode::UNAUTHORIZED,
        "an unknown path needs the token too"
    );
}

impl World {
    /// A queued run put straight into the job store, for a world whose body
    /// limit refuses even a run request.
    async fn start_unchecked(&self) -> RunId {
        let job = testkit::builders::job("job-small", "run-small", self.clock.now());
        let jobs: &dyn JobStore = self.jobs.as_ref();
        jobs.insert(&job, Some(&testkit::leases::job_submit()))
            .await
            .expect("insert");
        job.run_id
    }
}
