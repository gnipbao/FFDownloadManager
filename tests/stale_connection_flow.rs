//! Exercise real TLS response-header disconnects through the patched SDK.
use gosh_dl::{
    DownloadEngine, DownloadEvent, DownloadOptions, DownloadState, EngineConfig, EngineError,
    HttpConfig, MemoryStorage,
};
use hudsucker::rustls::{self, pki_types::PrivatePkcs8KeyDer};
use std::{
    collections::HashMap,
    error::Error as _,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

const TOTAL_BYTES: usize = 4 * 1024 * 1024 + 37;
const RETRIES: usize = 2;

#[derive(Clone, Copy)]
enum Failure {
    DisconnectOnce,
    DisconnectAlways,
    Status(u16),
}

#[derive(Default)]
struct Requests {
    heads: Vec<bool>,
    ranges: Vec<(usize, usize)>,
    disconnected: Vec<(usize, usize)>,
    complete: Vec<(usize, usize)>,
    unexpected_full_gets: usize,
}

struct TlsSource {
    url: String,
    data: Arc<Vec<u8>>,
    requests: Arc<Mutex<Requests>>,
    server: JoinHandle<()>,
}

impl Drop for TlsSource {
    fn drop(&mut self) {
        // Dropping the server's JoinSet also cancels its accepted connections.
        self.server.abort();
    }
}

impl TlsSource {
    async fn start(failure: Failure) -> Self {
        let data = Arc::new(
            (0..TOTAL_BYTES)
                .map(|offset| ((offset * 31 + offset / 257) % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let requests = Arc::new(Mutex::new(Requests::default()));
        let key = rcgen::KeyPair::generate().unwrap();
        let certificate = rcgen::CertificateParams::new(vec!["127.0.0.1".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![certificate.der().clone()],
            PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
        )
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("https://{}/fixture.bin", listener.local_addr().unwrap());
        let (served_data, observed) = (data.clone(), requests.clone());
        let server = tokio::spawn(async move {
            let mut clients = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let (acceptor, data, requests) =
                            (acceptor.clone(), served_data.clone(), observed.clone());
                        clients.spawn(async move {
                            let Ok(mut stream) = acceptor.accept(stream).await else { return };
                            // Read one bounded HTTP/1 request. A scripted server
                            // lets the failure happen after TLS and the complete
                            // request, before any HTTP response headers exist.
                            let mut input = Vec::new();
                            let mut buffer = [0_u8; 1024];
                            while !input.windows(4).any(|w| w == b"\r\n\r\n") {
                                let Ok(count) = stream.read(&mut buffer).await else { return };
                                if count == 0 || input.len() + count > 16 * 1024 { return }
                                input.extend_from_slice(&buffer[..count]);
                            }
                            let text = String::from_utf8(input).unwrap();
                            let mut lines = text.split("\r\n");
                            let method = lines.next().unwrap().split_whitespace().next().unwrap();
                            let headers = lines.filter_map(|line| line.split_once(':'))
                                .map(|(key, value)| (key.to_ascii_lowercase(), value.trim()))
                                .collect::<HashMap<_, _>>();
                            let head = method == "HEAD";
                            let range = headers.get("range")
                                .and_then(|value| value.strip_prefix("bytes="))
                                .and_then(|value| value.split_once('-'))
                                .and_then(|(start, end)| Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?)));
                            let mut status = 200;
                            let (start, end) = range.unwrap_or((0, data.len() - 1));
                            let disconnect = {
                                let mut recorded = requests.lock().unwrap();
                                if head {
                                    recorded.heads.push(headers.get("connection") == Some(&"close"));
                                    false
                                } else if let Some(range) = range {
                                    recorded.ranges.push(range);
                                    let fail = match failure {
                                        Failure::DisconnectOnce => recorded.disconnected.is_empty(),
                                        Failure::DisconnectAlways => true,
                                        Failure::Status(code) => { status = code; false }
                                    };
                                    if fail { recorded.disconnected.push(range); }
                                    fail
                                } else {
                                    recorded.unexpected_full_gets += 1;
                                    status = 400;
                                    false
                                }
                            };
                            if disconnect {
                                // Intentionally omit TLS shutdown/close_notify.
                                // This produces reqwest::is_request() with a
                                // typed source io::ErrorKind::UnexpectedEof,
                                // rather than a connect or response-body error.
                                return;
                            }
                            if range.is_some() && status == 200 { status = 206; }
                            let payload = if status == 200 || status == 206 {
                                &data[start..=end]
                            } else { &[] };
                            let mut response = format!(
                                "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"stable-local-fixture\"\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n",
                                payload.len(),
                            );
                            if status == 206 {
                                response.push_str(&format!("Content-Range: bytes {start}-{end}/{}\r\n", data.len()));
                            }
                            response.push_str("\r\n");
                            if stream.write_all(response.as_bytes()).await.is_err() { return }
                            if !head && !payload.is_empty() {
                                if stream.write_all(payload).await.is_err() { return }
                                requests.lock().unwrap().complete.push((start, end));
                            }
                            let _ = stream.shutdown().await;
                        });
                    }
                    _ = clients.join_next(), if !clients.is_empty() => {}
                }
            }
        });
        Self {
            url,
            data,
            requests,
            server,
        }
    }
}

async fn run_download(source: &TlsSource, connections: usize) -> (DownloadState, Vec<u8>) {
    let directory = tempfile::tempdir().unwrap();
    let engine = DownloadEngine::with_storage(
        EngineConfig {
            download_dir: directory.path().to_owned(),
            max_connections_per_download: connections,
            min_segment_size: 1024 * 1024,
            database_path: None,
            http: HttpConfig {
                // Only this localhost fixture accepts its self-signed certificate.
                accept_invalid_certs: true,
                max_retries: RETRIES,
                retry_delay_ms: 10,
                max_retry_delay_ms: 30,
                connect_timeout: 3,
                read_timeout: 3,
                ..Default::default()
            },
            ..Default::default()
        },
        Arc::new(MemoryStorage::new()),
    )
    .await
    .unwrap();
    let mut events = engine.subscribe();
    let id = engine
        .add_http(
            &source.url,
            DownloadOptions {
                filename: Some("fixture.bin".into()),
                max_connections: Some(connections),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match events.recv().await.unwrap() {
                DownloadEvent::Completed { id: event_id }
                | DownloadEvent::Failed { id: event_id, .. }
                    if event_id == id =>
                {
                    break
                }
                _ => {}
            }
        }
    })
    .await
    .expect("retry budget must terminate the transfer");
    let status = engine.status(id).unwrap();
    engine.shutdown().await.unwrap();
    let output = std::fs::read(directory.path().join("fixture.bin")).unwrap_or_default();
    (status.state, output)
}

#[tokio::test]
async fn response_header_tls_eof_retries_only_the_interrupted_segment() {
    let source = TlsSource::start(Failure::DisconnectOnce).await;
    let (state, output) = run_download(&source, 4).await;
    assert_eq!(state, DownloadState::Completed);
    assert_eq!(
        output, *source.data,
        "all segmented bytes must be preserved"
    );
    let requests = source.requests.lock().unwrap();
    assert_eq!(requests.disconnected.len(), 1);
    let failed_range = requests.disconnected[0];
    assert_eq!(
        requests
            .ranges
            .iter()
            .filter(|r| **r == failed_range)
            .count(),
        2,
        "only the interrupted segment should be requested again",
    );
    assert_eq!(requests.ranges.len(), requests.complete.len() + 1);
    assert!(
        requests.complete.len() > 1,
        "fixture must use multiple segments"
    );
    assert_eq!(requests.unexpected_full_gets, 0);
    assert!(!requests.heads.is_empty());
    assert!(requests.heads.iter().all(|close| *close));
    let mut complete = requests.complete.clone();
    complete.sort_unstable();
    let mut position = 0;
    for (start, end) in complete {
        assert_eq!(
            start, position,
            "successful ranges must not overlap or skip bytes"
        );
        position = end + 1;
    }
    assert_eq!(position, source.data.len());
}

#[tokio::test]
async fn recurring_response_header_disconnects_exhaust_the_existing_retry_budget() {
    let source = TlsSource::start(Failure::DisconnectAlways).await;
    let (state, output) = run_download(&source, 1).await;
    assert!(matches!(
        state,
        DownloadState::Error {
            retryable: true,
            ..
        }
    ));
    assert!(
        output.is_empty(),
        "a failed transfer must not publish a file"
    );
    let requests = source.requests.lock().unwrap();
    assert_eq!(requests.ranges.len(), RETRIES + 1);
    assert_eq!(requests.disconnected.len(), RETRIES + 1);
    assert!(requests.complete.is_empty());
    assert_eq!(requests.unexpected_full_gets, 0);
}

#[tokio::test]
async fn authorization_statuses_are_still_permanent_without_retry() {
    for code in [401, 403] {
        let source = TlsSource::start(Failure::Status(code)).await;
        let (state, output) = run_download(&source, 1).await;
        assert!(
            matches!(
                state,
                DownloadState::Error {
                    retryable: false,
                    ..
                }
            ),
            "HTTP {code} must not become a transient network failure: {state:?}",
        );
        assert!(output.is_empty());
        let requests = source.requests.lock().unwrap();
        assert_eq!(requests.ranges.len(), 1, "HTTP {code} must not be retried");
        assert!(requests.disconnected.is_empty());
        assert!(requests.complete.is_empty());
        assert_eq!(requests.unexpected_full_gets, 0);
    }
}

#[tokio::test]
async fn disconnect_fixture_is_a_request_io_error_after_successful_tls_connection() {
    let source = TlsSource::start(Failure::DisconnectOnce).await;
    let error = reqwest::Client::builder()
        .no_proxy()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap()
        .get(&source.url)
        .header("Range", "bytes=0-0")
        .send()
        .await
        .unwrap_err();
    assert!(error.is_request());
    assert!(!error.is_connect());
    assert!(!error.is_body());
    let mut cause = error.source();
    let mut eof = false;
    while let Some(source) = cause {
        eof |= source
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::UnexpectedEof);
        cause = source.source();
    }
    assert!(eof, "fixture must reach the exact response-header EOF path");
    assert!(EngineError::from(error).is_retryable());
}
