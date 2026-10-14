use axum::{
    body::Body,
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use bytes::Bytes;
use ffdownload::{
    download::{DownloadControl, DownloadSnapshot},
    hls,
    media::{find_ffmpeg, Assembly, MediaPlan},
    media_download,
    test_server::{fixture_data, fixture_router, ServerConfig},
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{oneshot, watch},
    task::JoinHandle,
};

struct Capture {
    fail: AtomicBool,
    corrupt: AtomicBool,
    failed_path: String,
    requests: HashMap<String, AtomicUsize>,
    leaked_credentials: AtomicBool,
}

struct Fixture {
    url: String,
    capture: Arc<Capture>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<std::io::Result<()>>,
}

impl Fixture {
    async fn start(
        playlist: String,
        files: Vec<(String, Bytes)>,
        pace: Option<f64>,
        failed_file: Option<&str>,
    ) -> Self {
        let headers = headers();
        let mut requests = HashMap::new();
        let mut app = Router::new()
            .route(
                "/media/list.m3u8",
                get(move || {
                    let text = playlist.clone();
                    async move { text }
                }),
            )
            .route(
                "/redirect",
                get(|| async { axum::response::Redirect::temporary("/media/list.m3u8") }),
            );
        for (name, data) in files {
            let mut config = ServerConfig::new(data);
            config.required_headers = headers.clone();
            config.per_response_mbps = pace;
            app = app.nest(&format!("/media/{name}"), fixture_router(config));
            requests.insert(format!("/media/{name}/file.bin"), AtomicUsize::new(0));
        }
        let capture = Arc::new(Capture {
            fail: AtomicBool::new(failed_file.is_some()),
            corrupt: AtomicBool::new(false),
            failed_path: failed_file
                .map(|name| format!("/media/{name}/file.bin"))
                .unwrap_or_default(),
            requests,
            leaked_credentials: AtomicBool::new(false),
        });
        let app = app.layer(middleware::from_fn_with_state(
            capture.clone(),
            capture_requests,
        ));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/redirect", listener.local_addr().unwrap());
        let (shutdown, stop) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stop.await;
                })
                .await
        });
        Self {
            url,
            capture,
            shutdown: Some(shutdown),
            task,
        }
    }

    fn requests(&self, file: &str) -> usize {
        self.capture.requests[&format!("/media/{file}/file.bin")].load(Ordering::Relaxed)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task.abort();
    }
}

async fn capture_requests(
    State(state): State<Arc<Capture>>,
    request: Request,
    next: Next,
) -> Response {
    if request.headers().contains_key("cookie") || request.headers().contains_key("authorization") {
        state.leaked_credentials.store(true, Ordering::Relaxed);
    }
    let path = request.uri().path();
    if let Some(count) = state
        .requests
        .get(path)
        .filter(|_| request.method() == axum::http::Method::GET)
    {
        count.fetch_add(1, Ordering::Relaxed);
    }
    if path == state.failed_path && state.fail.load(Ordering::Relaxed) {
        return (StatusCode::SERVICE_UNAVAILABLE, Body::empty()).into_response();
    }
    if path == state.failed_path && state.corrupt.load(Ordering::Relaxed) {
        return (StatusCode::OK, "<html>CDN verification page</html>").into_response();
    }
    next.run(request).await
}

fn headers() -> Vec<(String, String)> {
    vec![
        ("Referer".into(), "https://player.example/video".into()),
        ("User-Agent".into(), "FFDownload HLS fixture".into()),
        ("Origin".into(), "https://player.example".into()),
    ]
}

fn ts_data(length: usize) -> Bytes {
    let mut data = fixture_data(length / 188 * 188).to_vec();
    for packet in data.as_chunks_mut::<188>().0 {
        packet[0] = 0x47;
    }
    Bytes::from(data)
}

async fn plan(fixture: &Fixture) -> MediaPlan {
    MediaPlan {
        source_url: "https://player.example/video/fixture".into(),
        source_id: "hls-fixture".into(),
        platform: "HLS Fixture".into(),
        title: "HLS 本地验证".into(),
        format_id: "vod".into(),
        label: "HLS VOD".into(),
        extension: "mp4".into(),
        assembly: Assembly::Hls,
        streams: hls::expand(&fixture.url, &headers()).await.unwrap(),
        extracted_at: 1,
    }
}

fn controls() -> (
    watch::Sender<bool>,
    watch::Receiver<DownloadSnapshot>,
    DownloadControl,
) {
    let (pause, pause_rx) = watch::channel(false);
    let (progress, progress_rx) = watch::channel(DownloadSnapshot::default());
    (
        pause,
        progress_rx,
        DownloadControl {
            pause: pause_rx,
            progress,
        },
    )
}

#[tokio::test]
async fn hls_vod_pauses_and_resumes_partial_segment_without_refetching_completed_segments() {
    let first = ts_data(6 * 1024 * 1024);
    let second = ts_data(256 * 1024);
    let fixture = Fixture::start(
        "#EXTM3U\n#EXTINF:1,\nfirst/file.bin\n#EXTINF:1,\nsecond/file.bin\n#EXT-X-ENDLIST\n".into(),
        vec![
            ("first".into(), first.clone()),
            ("second".into(), second.clone()),
        ],
        Some(8.0),
        None,
    )
    .await;
    let plan = plan(&fixture).await;
    assert!(
        plan.streams[0].url.ends_with("/media/first/file.bin"),
        "redirected playlist base is used"
    );
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("paused.mp4");
    let (pause, mut progress, control) = controls();
    let task = tokio::spawn(media_download::run(
        plan.clone(),
        output.clone(),
        2,
        None,
        control,
        None,
    ));
    tokio::time::timeout(Duration::from_secs(10), async {
        while progress.borrow().completed_bytes == 0 {
            progress.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    pause.send_replace(true);
    let paused = task.await.unwrap().unwrap();
    assert_eq!(paused.outcome, "paused");
    assert!(paused.bytes > 0 && paused.bytes < first.len() as u64);
    assert!(!output.exists());
    assert_eq!(fixture.requests("second"), 0);
    let (_pause, _progress, control) = controls();
    let failure = media_download::run(plan.clone(), output.clone(), 2, None, control, None)
        .await
        .unwrap_err();
    assert!(failure.to_string().contains("FFmpeg"));
    let work = directory.path().join("paused.mp4.ffdm-state/media");
    assert_eq!(std::fs::read(work.join("track-0.bin")).unwrap(), first);
    assert_eq!(std::fs::read(work.join("track-1.bin")).unwrap(), second);
    let first_requests = fixture.requests("first");
    let second_requests = fixture.requests("second");
    let (_pause, _progress, control) = controls();
    assert!(
        media_download::run(plan, output.clone(), 2, None, control, None)
            .await
            .is_err()
    );
    assert_eq!(fixture.requests("first"), first_requests);
    assert_eq!(fixture.requests("second"), second_requests);
    assert!(!output.exists());
    assert!(!fixture.capture.leaked_credentials.load(Ordering::Relaxed));
}

#[tokio::test]
async fn failed_hls_segment_retry_retains_completed_segments_and_rejects_encryption_before_transfer(
) {
    let first = ts_data(128 * 1024);
    let second = ts_data(256 * 1024);
    let playlist =
        "#EXTM3U\n#EXTINF:1,\nfirst/file.bin\n#EXTINF:1,\nsecond/file.bin\n#EXT-X-ENDLIST\n";
    let fixture = Fixture::start(
        playlist.into(),
        vec![
            ("first".into(), first.clone()),
            ("second".into(), second.clone()),
        ],
        None,
        Some("second"),
    )
    .await;
    let plan = plan(&fixture).await;
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("retry.mp4");
    let (_pause, _progress, control) = controls();
    let failure = media_download::run(plan.clone(), output.clone(), 2, None, control, None)
        .await
        .unwrap_err();
    assert!(failure.to_string().contains("503"), "{failure:#}");
    let first_requests = fixture.requests("first");
    let work = directory.path().join("retry.mp4.ffdm-state/media");
    assert_eq!(std::fs::read(work.join("track-0.bin")).unwrap(), first);
    fixture.capture.fail.store(false, Ordering::Relaxed);
    let (_pause, _progress, control) = controls();
    let failure = media_download::run(plan, output.clone(), 2, None, control, None)
        .await
        .unwrap_err();
    assert!(failure.to_string().contains("FFmpeg"));
    assert_eq!(fixture.requests("first"), first_requests);
    assert_eq!(std::fs::read(work.join("track-1.bin")).unwrap(), second);
    assert!(!output.exists());
    let encrypted = Fixture::start(
        playlist.replacen(
            "#EXTM3U\n",
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"private.key\"\n",
            1,
        ),
        vec![("first".into(), first)],
        None,
        None,
    )
    .await;
    assert!(hls::expand(&encrypted.url, &headers())
        .await
        .unwrap_err()
        .to_string()
        .contains("加密"));
    assert_eq!(encrypted.requests("first"), 0);
    assert!(!fixture.capture.leaked_credentials.load(Ordering::Relaxed));
}

#[tokio::test]
async fn rejects_oversized_playlist_and_sensitive_headers_before_any_segment_fetch() {
    let fixture = Fixture::start(
        format!("#EXTM3U\n#{}", "x".repeat(2 * 1024 * 1024)),
        vec![("first".into(), fixture_data(32))],
        None,
        None,
    )
    .await;
    assert!(hls::expand(&fixture.url, &headers())
        .await
        .unwrap_err()
        .to_string()
        .contains("2 MiB"));
    assert!(
        hls::expand(&fixture.url, &[("Cookie".into(), "private=secret".into())])
            .await
            .is_err()
    );
    assert_eq!(fixture.requests("first"), 0);
    assert!(!fixture.capture.leaked_credentials.load(Ordering::Relaxed));
}

#[tokio::test]
async fn successful_html_response_in_middle_segment_is_quarantined_and_redownloaded_on_retry() {
    let first = ts_data(128 * 1024);
    let second = ts_data(256 * 1024);
    let fixture = Fixture::start(
        "#EXTM3U\n#EXTINF:1,\nfirst/file.bin\n#EXTINF:1,\nsecond/file.bin\n#EXT-X-ENDLIST\n".into(),
        vec![
            ("first".into(), first.clone()),
            ("second".into(), second.clone()),
        ],
        None,
        Some("second"),
    )
    .await;
    fixture.capture.fail.store(false, Ordering::Relaxed);
    fixture.capture.corrupt.store(true, Ordering::Relaxed);
    let plan = plan(&fixture).await;
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("html-retry.mp4");
    let (_pause, _progress, control) = controls();
    let failure = media_download::run(
        plan.clone(),
        output.clone(),
        2,
        None,
        control,
        Some(PathBuf::from("must-not-run")),
    )
    .await
    .unwrap_err();
    assert!(
        failure.to_string().contains("第 2 个分段无效"),
        "{failure:#}"
    );
    let work = directory.path().join("html-retry.mp4.ffdm-state/media");
    assert_eq!(std::fs::read(work.join("track-0.bin")).unwrap(), first);
    assert!(!work.join("track-1.bin").exists());
    assert!(!work.join("track-1.bin.ffdm-state").exists());
    let rejected = std::fs::read_dir(&work)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("rejected-hls-")
        })
        .unwrap();
    assert!(std::fs::read_to_string(rejected.join("segment.bin"))
        .unwrap()
        .contains("CDN verification page"));
    let first_requests = fixture.requests("first");
    let second_requests = fixture.requests("second");
    fixture.capture.corrupt.store(false, Ordering::Relaxed);
    let (_pause, _progress, control) = controls();
    let failure = media_download::run(plan.clone(), output.clone(), 2, None, control, None)
        .await
        .unwrap_err();
    assert!(failure.to_string().contains("FFmpeg"));
    assert_eq!(fixture.requests("first"), first_requests);
    assert!(fixture.requests("second") > second_requests);
    assert_eq!(std::fs::read(work.join("track-1.bin")).unwrap(), second);
    // Old completed intermediates must be revalidated, too.
    std::fs::write(work.join("track-1.bin"), "{\"error\":\"expired\"}").unwrap();
    let (_pause, _progress, control) = controls();
    let failure = media_download::run(plan, output.clone(), 2, None, control, None)
        .await
        .unwrap_err();
    assert!(failure.to_string().contains("第 2 个分段无效"));
    assert!(!output.exists());
}

fn make_vod(ffmpeg: &Path, directory: &Path, fmp4: bool) -> String {
    let extension = if fmp4 { "m4s" } else { "ts" };
    let result = Command::new(ffmpeg)
        .current_dir(directory)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-stream_loop",
            "3",
            "-i",
        ])
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample-av.mp4"))
        .args([
            "-map",
            "0:v:0",
            "-map",
            "0:a:0",
            "-c",
            "copy",
            "-hls_time",
            "1",
            "-hls_list_size",
            "0",
            "-hls_segment_type",
        ])
        .arg(if fmp4 { "fmp4" } else { "mpegts" })
        .arg("-hls_segment_filename")
        .arg(format!("part-%03d.{extension}"))
        .arg("list.m3u8")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let mut list = std::fs::read_to_string(directory.join("list.m3u8")).unwrap();
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap();
        if name != "list.m3u8" {
            list = list.replace(name, &format!("{name}/file.bin"));
        }
    }
    list
}

#[tokio::test]
#[ignore = "requires a full FFmpeg with HLS fixture muxer and ffprobe installed"]
async fn ts_and_fmp4_vod_retry_preserves_segments_and_produces_audio_video_without_overwrite() {
    let ffmpeg = find_ffmpeg().expect("FFmpeg required");
    let remux = std::env::var_os("FFDM_HLS_REMUX_FFMPEG")
        .map(PathBuf::from)
        .unwrap_or_else(|| ffmpeg.clone());
    for fmp4 in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let assets = directory.path().join("assets");
        std::fs::create_dir(&assets).unwrap();
        let playlist = make_vod(&ffmpeg, &assets, fmp4);
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&assets).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_str().unwrap().to_owned();
            if name != "list.m3u8" {
                files.push((name, Bytes::from(std::fs::read(path).unwrap())));
            }
        }
        let fixture = Fixture::start(playlist, files, None, None).await;
        let plan = plan(&fixture).await;
        assert!(plan.streams.len() >= 4);
        if fmp4 {
            assert!(plan.streams[0].url.contains("init.mp4"));
        }
        let output = directory.path().join("playable.mp4");
        let (_pause, _progress, control) = controls();
        let failure = media_download::run(plan.clone(), output.clone(), 2, None, control, None)
            .await
            .unwrap_err();
        assert!(failure.to_string().contains("FFmpeg"));
        let counts: Vec<_> = fixture
            .capture
            .requests
            .values()
            .map(|c| c.load(Ordering::Relaxed))
            .collect();
        let (_pause, _progress, control) = controls();
        let done = media_download::run(
            plan.clone(),
            output.clone(),
            2,
            None,
            control,
            Some(remux.clone()),
        )
        .await
        .unwrap();
        assert_eq!(done.outcome, "completed");
        assert_eq!(
            counts,
            fixture
                .capture
                .requests
                .values()
                .map(|c| c.load(Ordering::Relaxed))
                .collect::<Vec<_>>()
        );
        let probe = Command::new(ffmpeg.with_file_name(if cfg!(windows) {
            "ffprobe.exe"
        } else {
            "ffprobe"
        }))
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type",
            "-show_entries",
            "format=duration",
            "-of",
            "json",
        ])
        .arg(&output)
        .output()
        .unwrap();
        assert!(
            probe.status.success(),
            "{}",
            String::from_utf8_lossy(&probe.stderr)
        );
        let metadata: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
        let streams = metadata["streams"].as_array().unwrap();
        assert!(streams.iter().any(|stream| stream["codec_type"] == "video"));
        assert!(streams.iter().any(|stream| stream["codec_type"] == "audio"));
        assert!(
            metadata["format"]["duration"]
                .as_str()
                .unwrap()
                .parse::<f64>()
                .unwrap()
                >= 3.9
        );
        let before = std::fs::read(&output).unwrap();
        let (_pause, _progress, control) = controls();
        assert!(
            media_download::run(plan, output.clone(), 2, None, control, Some(remux.clone()))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(output).unwrap(), before);
        assert!(!fixture.capture.leaked_credentials.load(Ordering::Relaxed));
    }
}
