//! Loopback-only HTTP fixture. Its pacing is application-level, not TCP shaping.
use anyhow::Result;
use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, Method, Response, StatusCode},
    routing::get,
    Router,
};
use bytes::Bytes;
use futures_util::stream;
use sha2::{Digest, Sha256};
use std::{
    convert::Infallible,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle, time::Instant};

#[derive(Clone)]
pub struct ServerConfig {
    pub data: Bytes,
    pub per_response_mbps: Option<f64>,
    pub ranges: bool,
    pub incorrect_content_range: bool,
    pub reject_head: bool,
    pub required_headers: Vec<(String, String)>,
}

impl ServerConfig {
    pub fn new(data: Bytes) -> Self {
        Self {
            data,
            per_response_mbps: None,
            ranges: true,
            incorrect_content_range: false,
            reject_head: false,
            required_headers: Vec::new(),
        }
    }
}

struct ServerState {
    config: ServerConfig,
    etag: String,
    range_requests: Arc<AtomicUsize>,
}

pub struct TestServer {
    pub url: String,
    range_requests: Arc<AtomicUsize>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<std::io::Result<()>>,
}

impl TestServer {
    pub async fn start(config: ServerConfig) -> Result<Self> {
        let (app, range_requests) = fixture_router_with_counter(config);
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/file.bin", listener.local_addr()?);
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
        });
        Ok(Self {
            url,
            range_requests,
            shutdown: Some(tx),
            task,
        })
    }

    pub fn range_requests(&self) -> usize {
        self.range_requests.load(Ordering::Relaxed)
    }

    pub async fn stop(mut self) -> Result<()> {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        (&mut self.task).await??;
        Ok(())
    }
}

/// A real, ranged HTTP response for the explicitly labelled local UI sample.
pub fn fixture_router(config: ServerConfig) -> Router {
    fixture_router_with_counter(config).0
}

fn fixture_router_with_counter(config: ServerConfig) -> (Router, Arc<AtomicUsize>) {
    let etag = format!("\"{:x}\"", Sha256::digest(&config.data));
    let range_requests = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/file.bin", get(serve_file))
        .with_state(Arc::new(ServerState {
            config,
            etag,
            range_requests: range_requests.clone(),
        }));
    (app, range_requests)
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        self.task.abort();
    }
}

pub fn fixture_data(bytes: usize) -> Bytes {
    let mut state = 0x4d59_5df4_d0f3_3173_u64;
    let mut data = vec![0; bytes];
    for chunk in data.chunks_mut(8) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
    }
    Bytes::from(data)
}

async fn serve_file(
    State(state): State<Arc<ServerState>>,
    method: Method,
    headers: HeaderMap,
) -> Response<Body> {
    if state.config.required_headers.iter().any(|(name, value)| {
        headers
            .get(name)
            .is_none_or(|actual| actual != value.as_str())
    }) {
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Body::empty())
            .unwrap();
    }
    if method == Method::HEAD && state.config.reject_head {
        return Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(Body::empty())
            .unwrap();
    }
    let len = state.config.data.len();
    let mut start = 0;
    let mut end = len.saturating_sub(1);
    let mut partial = false;
    let if_range_matches = headers
        .get(header::IF_RANGE)
        .is_none_or(|v| v == state.etag.as_str());
    if method != Method::HEAD && state.config.ranges && if_range_matches {
        if let Some(range) = headers.get(header::RANGE) {
            let parsed = range
                .to_str()
                .ok()
                .and_then(|v| v.strip_prefix("bytes="))
                .and_then(|v| v.split_once('-'));
            let bounds = parsed.and_then(|(a, b)| {
                Some((
                    a.parse::<usize>().ok()?,
                    if b.is_empty() {
                        len.checked_sub(1)?
                    } else {
                        b.parse::<usize>().ok()?
                    },
                ))
            });
            match bounds {
                Some((a, b)) if a <= b && a < len && b < len => {
                    start = a;
                    end = b;
                    partial = true;
                }
                _ => {
                    return Response::builder()
                        .status(StatusCode::RANGE_NOT_SATISFIABLE)
                        .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                        .body(Body::empty())
                        .unwrap()
                }
            }
        }
    }
    let size = if len == 0 { 0 } else { end - start + 1 };
    let mut response = Response::builder()
        .status(if partial {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, size)
        .header(header::ETAG, &state.etag)
        .header(
            header::ACCEPT_RANGES,
            if state.config.ranges { "bytes" } else { "none" },
        );
    if partial {
        state.range_requests.fetch_add(1, Ordering::Relaxed);
        let reported_start = start + usize::from(state.config.incorrect_content_range);
        response = response.header(
            header::CONTENT_RANGE,
            format!("bytes {reported_start}-{end}/{len}"),
        );
    }
    if method == Method::HEAD || size == 0 {
        return response.body(Body::empty()).unwrap();
    }
    let mut data = state.config.data.slice(start..=end);
    if partial && state.config.incorrect_content_range {
        let mut corrupted = data.to_vec();
        corrupted[0] ^= 0xff;
        data = Bytes::from(corrupted);
    }
    let rate = state
        .config
        .per_response_mbps
        .map(|v| v * 1_000_000.0 / 8.0);
    let body = stream::unfold(
        (data, 0_usize, Instant::now()),
        move |(data, pos, started)| async move {
            if pos == data.len() {
                return None;
            }
            let next = (pos + 256 * 1024).min(data.len());
            if let Some(rate) = rate {
                tokio::time::sleep_until(started + Duration::from_secs_f64(next as f64 / rate))
                    .await;
            }
            let chunk = data.slice(pos..next);
            Some((Ok::<_, Infallible>(chunk), (data, next, started)))
        },
    );
    response.body(Body::from_stream(body)).unwrap()
}
