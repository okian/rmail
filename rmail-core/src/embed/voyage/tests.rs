//! Driven against a real HTTP server on loopback rather than a mocked client,
//! so the request that is asserted on is the one `reqwest` would actually send.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;
use crate::embed::MAX_INPUT_BYTES;
use crate::ErrorReason;

/// What one request looked like, as the server saw it.
#[derive(Debug, Clone)]
struct Seen {
    authorization: Option<String>,
    body: serde_json::Value,
}

/// An HTTP server that answers from a queue of canned responses and records
/// what it was asked.
///
/// A queue rather than one fixed reply, because the batching behavior worth
/// testing is what happens across *several* requests: a server that answers
/// every request identically cannot express a second chunk of a different size,
/// which is exactly the case where a vector could be attached to the wrong
/// input.
struct Server {
    endpoint: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        // A `JoinHandle` does not abort on drop, so without this every test
        // leaves an accept loop and a bound port running for the life of the
        // process.
        self.task.abort();
    }
}

impl Server {
    async fn start(status: u16, body: String) -> Self {
        Self::queued(vec![(status, body)]).await
    }

    /// Answer each request from `replies` in turn, repeating the last one once
    /// the queue is exhausted.
    async fn queued(replies: Vec<(u16, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let replies = Arc::new(Mutex::new(std::collections::VecDeque::from(replies)));
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let recorder = Arc::clone(&recorder);
                let (status, body) = {
                    let mut queue = replies.lock().unwrap_or_else(|e| e.into_inner());
                    if queue.len() > 1 {
                        queue.pop_front().unwrap_or((500, String::new()))
                    } else {
                        queue.front().cloned().unwrap_or((500, String::new()))
                    }
                };
                tokio::spawn(async move {
                    let mut raw = Vec::new();
                    let mut buf = [0u8; 4096];
                    // Read until the body is complete: headers, then exactly
                    // `Content-Length` more bytes.
                    let (head_end, length) = loop {
                        let n = stream.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        raw.extend_from_slice(&buf[..n]);
                        let text = String::from_utf8_lossy(&raw).to_string();
                        if let Some(at) = text.find("\r\n\r\n") {
                            let length = text
                                .lines()
                                .find_map(|line| {
                                    line.strip_prefix("content-length: ")
                                        .or_else(|| line.strip_prefix("Content-Length: "))
                                })
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            if raw.len() >= at + 4 + length {
                                break (at + 4, length);
                            }
                        }
                    };
                    let text = String::from_utf8_lossy(&raw).to_string();
                    let authorization = text.lines().find_map(|line| {
                        line.strip_prefix("authorization: ")
                            .or_else(|| line.strip_prefix("Authorization: "))
                            .map(|v| v.trim().to_owned())
                    });
                    let payload = String::from_utf8_lossy(&raw[head_end..head_end + length]);
                    if let Ok(mut log) = recorder.lock() {
                        log.push(Seen {
                            authorization,
                            body: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
                        });
                    }
                    let response = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });
        Self {
            endpoint: format!("http://{addr}/v1/embeddings"),
            seen,
            task,
        }
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().map(|log| log.clone()).unwrap_or_default()
    }
}

/// A response body with `count` vectors of `dim` dimensions, in `order`.
fn body(order: &[usize], dim: usize) -> String {
    let data: Vec<serde_json::Value> = order
        .iter()
        .map(|index| {
            let mut values = vec![0.0f32; dim];
            if let Some(slot) = values.get_mut(index % dim) {
                *slot = 1.0;
            }
            serde_json::json!({ "index": index, "embedding": values })
        })
        .collect();
    serde_json::json!({ "data": data }).to_string()
}

fn embedder(server: &Server, dim: u32) -> VoyageEmbedder {
    VoyageEmbedder::new(&VoyageConfig {
        model: "voyage-3".to_owned(),
        dim,
        api_key_command: "printf secret-key".to_owned(),
        rpm: 100_000,
    })
    .unwrap()
    .with_endpoint(&server.endpoint)
}

#[tokio::test]
async fn a_batch_is_embedded_and_the_key_comes_from_the_command() {
    let server = Server::start(200, body(&[0, 1], 4)).await;
    let e = embedder(&server, 4);

    let vectors = e
        .embed(&["first".to_owned(), "second".to_owned()])
        .await
        .unwrap();

    assert_eq!(vectors.len(), 2);
    assert_eq!(vectors[0].dim(), 4);
    let seen = server.requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].authorization.as_deref(),
        Some("Bearer secret-key"),
        "the key is read from the command's stdout, not from the config file"
    );
    assert_eq!(seen[0].body["model"], "voyage-3");
    assert_eq!(seen[0].body["input"][0], "first");
    assert_eq!(
        seen[0].body["input_type"], "document",
        "retrieval models embed a query and a document differently"
    );
}

#[tokio::test]
async fn vectors_are_returned_in_input_order_whatever_order_they_arrived_in() {
    // The contract is that vector `i` belongs to input `i`. Trusting a remote
    // service's ordering means that when it is wrong, every vector is attached
    // to the wrong message and nothing downstream can tell.
    let server = Server::start(200, body(&[1, 0], 4)).await;
    let e = embedder(&server, 4);

    let vectors = e
        .embed(&["first".to_owned(), "second".to_owned()])
        .await
        .unwrap();

    assert_eq!(vectors[0].as_slice()[0], 1.0, "index 0 first");
    assert_eq!(vectors[1].as_slice()[1], 1.0, "index 1 second");
}

#[tokio::test]
async fn a_short_response_is_an_error_not_a_silent_misalignment() {
    let server = Server::start(200, body(&[0], 4)).await;
    let e = embedder(&server, 4);

    let err = e
        .embed(&["first".to_owned(), "second".to_owned()])
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::Internal);
}

#[tokio::test]
async fn a_dimension_mismatch_is_caught_before_it_reaches_the_index() {
    // Two dimensionalities in one index make every comparison between them
    // meaningless, and nothing later can tell which rows are which.
    let server = Server::start(200, body(&[0], 8)).await;
    let e = embedder(&server, 4);

    let err = e.embed(&["first".to_owned()]).await.unwrap_err();
    assert_eq!(err.reason(), ErrorReason::Internal);
}

#[tokio::test]
async fn a_rejected_key_is_told_apart_from_an_outage() {
    // "Fix your key" and "try again later" call for opposite responses; one
    // error for both sends the operator to the wrong place half the time.
    let unauthorized = Server::start(401, r#"{"detail":"bad key"}"#.to_owned()).await;
    let err = embedder(&unauthorized, 4)
        .embed(&["x".to_owned()])
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::Unauthenticated);

    let limited = Server::start(429, r#"{"detail":"slow down"}"#.to_owned()).await;
    let err = embedder(&limited, 4)
        .embed(&["x".to_owned()])
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::Unavailable);

    let broken = Server::start(500, r#"{"detail":"oops"}"#.to_owned()).await;
    let err = embedder(&broken, 4)
        .embed(&["x".to_owned()])
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::Unavailable);
}

#[tokio::test]
async fn a_failing_key_command_is_an_authentication_error() {
    let server = Server::start(200, body(&[0], 4)).await;
    let e = VoyageEmbedder::new(&VoyageConfig {
        model: "voyage-3".to_owned(),
        dim: 4,
        api_key_command: "exit 1".to_owned(),
        rpm: 100_000,
    })
    .unwrap()
    .with_endpoint(&server.endpoint);

    let err = e.embed(&["x".to_owned()]).await.unwrap_err();
    assert_eq!(err.reason(), ErrorReason::Unauthenticated);
    assert!(
        server.requests().is_empty(),
        "no request should go out without a key"
    );
}

#[tokio::test]
async fn a_large_batch_is_split_and_rejoined_in_order() {
    // A backfill over a hundred-thousand-message mailbox must not become one
    // request, one allocation, or one thing to retry — and the seam between two
    // chunks is the one place where a vector can be attached to the wrong
    // input with nothing downstream able to tell.
    //
    // Sending exactly `MAX_BATCH` inputs, as an earlier version of this test
    // did, is exactly one chunk: it asserted `requests().len() == 1` and
    // covered the split path not at all.
    let dim = 8;
    let first: Vec<usize> = (0..MAX_BATCH).collect();
    let server = Server::queued(vec![(200, body(&first, dim)), (200, body(&[0, 1, 2], dim))]).await;
    let e = embedder(&server, dim as u32);

    let texts: Vec<String> = (0..MAX_BATCH + 3).map(|n| format!("text {n}")).collect();
    let vectors = e.embed(&texts).await.unwrap();

    assert_eq!(vectors.len(), MAX_BATCH + 3);
    let seen = server.requests();
    assert_eq!(seen.len(), 2, "one request per chunk");
    assert_eq!(
        seen[0].body["input"].as_array().map(Vec::len),
        Some(MAX_BATCH)
    );
    assert_eq!(seen[1].body["input"].as_array().map(Vec::len), Some(3));
    assert_eq!(seen[1].body["input"][0], "text 64", "the seam is at 64");
    // The second chunk's vectors are its own, in its own order, appended after
    // the first chunk's rather than interleaved with or overwriting them.
    for (n, offset) in (MAX_BATCH..MAX_BATCH + 3).zip(0..) {
        assert_eq!(
            vectors[n].as_slice()[offset],
            1.0,
            "vector {n} should be the second chunk\'s index {offset}"
        );
    }
}

#[tokio::test]
async fn a_batch_of_long_messages_splits_on_bytes_as_well_as_count() {
    // Sixty-four inputs of eight kibibytes is half a megabyte of text in one
    // request, past the per-request token cap of every hosted embedding API —
    // which comes back as a 400 for the whole batch rather than a shortfall on
    // one input.
    let dim = 4;
    let server = Server::queued(vec![
        (200, body(&(0..12).collect::<Vec<_>>(), dim)),
        (200, body(&(0..8).collect::<Vec<_>>(), dim)),
    ])
    .await;
    let e = embedder(&server, dim as u32);

    let texts: Vec<String> = (0..20).map(|_| "x".repeat(MAX_INPUT_BYTES)).collect();
    let vectors = e.embed(&texts).await.unwrap();

    assert_eq!(vectors.len(), 20);
    let seen = server.requests();
    assert!(
        seen.len() > 1,
        "twenty 8 KiB inputs is 160 KiB and must not be one request"
    );
    for request in &seen {
        let bytes: usize = request.body["input"]
            .as_array()
            .map(|inputs| inputs.iter().filter_map(|i| i.as_str()).map(str::len).sum())
            .unwrap_or(0);
        assert!(bytes <= 96 * 1024, "one request carried {bytes} bytes");
    }
}

#[tokio::test]
async fn a_body_that_will_not_parse_is_reported_as_an_outage() {
    // Retry policies treat `Internal` as non-retryable. A truncated upstream
    // body is very likely to succeed on the next attempt, so classifying it as
    // internal turns a transient fault into a permanent one.
    let server = Server::start(200, "not json at all".to_owned()).await;
    let err = embedder(&server, 4)
        .embed(&["x".to_owned()])
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::Unavailable);
}

#[tokio::test]
async fn duplicate_or_missing_indices_are_refused() {
    // The remote decides which vector carries which index. Two vectors claiming
    // index 0 means one input has no vector at all, and accepting the pair
    // attaches both to the wrong messages.
    let dim = 4;
    for (label, order) in [("duplicate", vec![0, 0]), ("out of range", vec![0, 7])] {
        let server = Server::start(200, body(&order, dim)).await;
        let err = embedder(&server, dim as u32)
            .embed(&["a".to_owned(), "b".to_owned()])
            .await
            .unwrap_err();
        assert_eq!(err.reason(), ErrorReason::Internal, "{label}");
    }
}

#[tokio::test]
async fn an_upstream_error_body_is_clipped_before_it_reaches_a_client() {
    // Everything but `Internal` is emitted verbatim as the `tonic::Status`
    // message. An unbounded third-party body would become an unbounded
    // `grpc-message` trailer, which exceeds HTTP/2 header limits and turns a
    // clean error into a transport failure.
    let huge = "z".repeat(64 * 1024);
    let server = Server::start(400, huge).await;
    let err = embedder(&server, 4)
        .embed(&["x".to_owned()])
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
    assert!(
        err.to_string().len() < 512,
        "the message was {} bytes",
        err.to_string().len()
    );
}

#[tokio::test]
async fn warming_costs_a_key_lookup_and_not_a_billable_request() {
    // The default `warm` embeds a document, which for a hosted backend is a
    // third-party API call on every daemon start.
    let server = Server::start(200, body(&[0], 4)).await;
    let e = embedder(&server, 4);
    e.warm().await.unwrap();
    assert!(server.requests().is_empty());

    let broken = VoyageEmbedder::new(&VoyageConfig {
        model: "voyage-3".to_owned(),
        dim: 4,
        api_key_command: "exit 1".to_owned(),
        rpm: 100_000,
    })
    .unwrap();
    assert!(
        broken.warm().await.is_err(),
        "warming must still prove the key command works — that is the part \
         worth finding out at start-up"
    );
}

#[tokio::test]
async fn a_caller_that_would_wait_minutes_is_refused_and_keeps_its_slot_free() {
    // `rpm` can be configured down to one, a sixty-second gap. Waiting past the
    // point where anyone still wants the answer is worse than failing, and a
    // caller that gives up must not leave a reservation behind — that would
    // throttle everyone after it harder than the configured rate.
    let server = Server::start(200, body(&[0], 4)).await;
    let e = VoyageEmbedder::new(&VoyageConfig {
        model: "voyage-3".to_owned(),
        dim: 4,
        api_key_command: "printf k".to_owned(),
        rpm: 1,
    })
    .unwrap()
    .with_endpoint(&server.endpoint);

    // The first goes out immediately and reserves the next slot 60s away.
    e.embed(&["a".to_owned()]).await.unwrap();
    let err = e.embed(&["b".to_owned()]).await.unwrap_err();
    assert_eq!(err.reason(), ErrorReason::Unavailable);
    // The refusal must not have consumed the slot: a third caller sees the same
    // 60s wait, not 120s.
    let err = e.embed(&["c".to_owned()]).await.unwrap_err();
    assert_eq!(err.reason(), ErrorReason::Unavailable);
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn an_empty_batch_costs_no_request_and_no_key() {
    let server = Server::start(200, body(&[], 4)).await;
    let e = embedder(&server, 4);
    assert!(e.embed(&[]).await.unwrap().is_empty());
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn requests_are_paced_to_the_configured_rate() {
    // Discovering the limit from 429s means a backfill spends its time being
    // rejected at full speed. Pacing costs nothing when the caller is slower
    // than the limit anyway.
    let server = Server::start(200, body(&[0], 4)).await;
    let e = VoyageEmbedder::new(&VoyageConfig {
        model: "voyage-3".to_owned(),
        dim: 4,
        api_key_command: "printf k".to_owned(),
        // One every 50ms.
        rpm: 1200,
    })
    .unwrap()
    .with_endpoint(&server.endpoint);

    let started = std::time::Instant::now();
    for _ in 0..3 {
        e.embed(&["x".to_owned()]).await.unwrap();
    }
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(100),
        "three requests at 1200/min span at least two 50ms gaps, took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_model_name_travels_with_the_request() {
    let server = Server::start(200, body(&[0], 4)).await;
    let e = embedder(&server, 4);
    assert_eq!(e.model(), "voyage-3");
    assert_eq!(e.dim(), 4);
    e.embed(&["x".to_owned()]).await.unwrap();
    assert_eq!(server.requests()[0].body["model"], "voyage-3");
}
