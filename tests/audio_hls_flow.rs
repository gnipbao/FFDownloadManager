//! Real audio-only HLS contract: standalone Rust HTML extraction, private
//! executor plans, Rust ranged transfers/retry, and local lossless M4A remux.
//! Fixture generation/ffprobe require a full FFmpeg; no public service is used.
use axum::{
    extract::{Request, State},
    http::{header, Method, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use bytes::Bytes;
use ffdownload::{
    download::{DownloadControl, DownloadSnapshot},
    media::find_ffmpeg,
    media_download,
    resolver::{Assembly, ResourceResolver},
    test_server::{fixture_router, ServerConfig},
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{net::TcpListener, sync::watch, task::JoinHandle};

#[derive(Clone, Copy, Debug)]
enum Container {
    MpegTs,
    Fmp4,
    Adts,
}

struct Assets {
    playlist: String,
    files: Vec<(String, Bytes)>,
    duration: f64,
}

fn make_audio_vod(ffmpeg: &Path, directory: &Path, container: Container) -> Assets {
    let mut command = Command::new(ffmpeg);
    command.current_dir(directory).args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-stream_loop",
        "3",
        "-i",
    ]);
    command
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample-av.mp4"))
        .args(["-map", "0:a:0", "-vn", "-c", "copy"]);
    match container {
        Container::MpegTs | Container::Fmp4 => {
            command.args([
                "-f",
                "hls",
                "-hls_time",
                "1",
                "-hls_list_size",
                "0",
                "-hls_segment_type",
            ]);
            command.arg(if matches!(container, Container::Fmp4) {
                "fmp4"
            } else {
                "mpegts"
            });
            command
                .arg("-hls_segment_filename")
                .arg(if matches!(container, Container::Fmp4) {
                    "part-%03d.m4s"
                } else {
                    "part-%03d.ts"
                });
            command.arg("list.m3u8");
        }
        Container::Adts => {
            command.args([
                "-f",
                "segment",
                "-segment_time",
                "1",
                "-segment_format",
                "adts",
                "-segment_list_type",
                "m3u8",
                "-segment_list",
                "list.m3u8",
                "part-%03d.aac",
            ]);
        }
    }
    let result = command.output().expect("fixture FFmpeg must be executable");
    assert!(
        result.status.success(),
        "{container:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let mut playlist = std::fs::read_to_string(directory.join("list.m3u8")).unwrap();
    assert!(playlist.contains("#EXT-X-ENDLIST"));
    let duration = playlist
        .lines()
        .filter_map(|line| {
            line.strip_prefix("#EXTINF:")?
                .split(',')
                .next()?
                .parse::<f64>()
                .ok()
        })
        .sum();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        if name == "list.m3u8" {
            continue;
        }
        playlist = playlist.replace(&name, &format!("{name}/file.bin"));
        files.push((name, Bytes::from(std::fs::read(path).unwrap())));
    }
    Assets {
        playlist,
        files,
        duration,
    }
}

#[derive(Default)]
struct Capture {
    fail: AtomicBool,
    failed_path: Mutex<String>,
    requests: HashMap<String, AtomicUsize>,
    leaked_credentials: AtomicBool,
    duplicate_user_agent: AtomicBool,
}

struct Fixture {
    page: String,
    capture: Arc<Capture>,
    task: JoinHandle<std::io::Result<()>>,
}

impl Fixture {
    async fn start(assets: &Assets) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let page = format!("{base}/html/audio");
        let mut requests = HashMap::new();
        let playlist = assets.playlist.clone();
        let mut app = Router::new()
            .route("/html/audio", get(|| async {
                Html("<html><title>Public audio HLS fixture</title><audio controls src='/assets/list.m3u8?signature=fixture-private%2Btoken' type='application/vnd.apple.mpegurl'></audio></html>")
            }))
            .route("/assets/list.m3u8", get(move || {
                let playlist = playlist.clone();
                async move { ([(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")], playlist) }
            }));
        for (name, data) in &assets.files {
            let mut config = ServerConfig::new(data.clone());
            config.required_headers = vec![("Referer".to_owned(), page.clone())];
            app = app.nest(&format!("/assets/{name}"), fixture_router(config));
            requests.insert(format!("/assets/{name}/file.bin"), AtomicUsize::new(0));
        }
        let capture = Arc::new(Capture {
            requests,
            ..Capture::default()
        });
        let app = app.layer(middleware::from_fn_with_state(
            capture.clone(),
            capture_requests,
        ));
        let task = tokio::spawn(async move { axum::serve(listener, app).await });
        Self {
            page,
            capture,
            task,
        }
    }

    fn count(&self, path: &str) -> usize {
        self.capture.requests[path].load(Ordering::Relaxed)
    }

    fn counts(&self) -> HashMap<String, usize> {
        self.capture
            .requests
            .keys()
            .map(|path| (path.clone(), self.count(path)))
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn capture_requests(
    State(state): State<Arc<Capture>>,
    request: Request,
    next: Next,
) -> Response {
    if request.headers().contains_key(header::COOKIE)
        || request.headers().contains_key(header::AUTHORIZATION)
    {
        state.leaked_credentials.store(true, Ordering::Relaxed);
    }
    if request.headers().get_all(header::USER_AGENT).iter().count() > 1 {
        state.duplicate_user_agent.store(true, Ordering::Relaxed);
    }
    let path = request.uri().path();
    if request.method() == Method::GET {
        if let Some(count) = state.requests.get(path) {
            count.fetch_add(1, Ordering::Relaxed);
        }
    }
    let failed_path = state.failed_path.lock().unwrap().clone();
    if path == failed_path && state.fail.load(Ordering::Relaxed) {
        return (StatusCode::SERVICE_UNAVAILABLE, "temporary fixture failure").into_response();
    }
    next.run(request).await
}

fn controls() -> (watch::Sender<bool>, DownloadControl) {
    let (pause, pause_rx) = watch::channel(false);
    let (progress, _events) = watch::channel(DownloadSnapshot::default());
    (
        pause,
        DownloadControl {
            pause: pause_rx,
            progress,
        },
    )
}

fn assert_audio_only_m4a(ffprobe: &Path, output: &Path, duration: f64) {
    let result = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type,codec_name",
            "-show_entries",
            "format=duration,format_name",
            "-of",
            "json",
        ])
        .arg(output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let metadata: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    let streams = metadata["streams"].as_array().unwrap();
    assert_eq!(
        streams.len(),
        1,
        "M4A must contain exactly the selected audio track"
    );
    assert_eq!(streams[0]["codec_type"], "audio");
    assert_eq!(streams[0]["codec_name"], "aac");
    assert!(metadata["format"]["format_name"]
        .as_str()
        .unwrap()
        .split(',')
        .any(|name| name == "m4a"));
    let actual = metadata["format"]["duration"]
        .as_str()
        .unwrap()
        .parse::<f64>()
        .unwrap();
    assert!(
        (actual - duration).abs() < 0.3,
        "truncated audio: expected {duration:.3}s, got {actual:.3}s"
    );
}

#[tokio::test]
#[ignore = "requires a full FFmpeg/ffprobe for fixture generation; set FFDM_HLS_REMUX_FFMPEG to test bundled remuxer"]
async fn audio_hls_ts_fmp4_adts_retry_remuxes_aac_to_m4a_without_redownload_or_overwrite() {
    let ffmpeg = find_ffmpeg().expect("full FFmpeg required for fixture generation");
    let remux = std::env::var_os("FFDM_HLS_REMUX_FFMPEG")
        .map(PathBuf::from)
        .unwrap_or_else(|| ffmpeg.clone());
    let ffprobe = ffmpeg.with_file_name(if cfg!(windows) {
        "ffprobe.exe"
    } else {
        "ffprobe"
    });
    for container in [Container::MpegTs, Container::Fmp4, Container::Adts] {
        let directory = tempfile::tempdir().unwrap();
        let asset_directory = directory.path().join("assets");
        std::fs::create_dir(&asset_directory).unwrap();
        let assets = make_audio_vod(&ffmpeg, &asset_directory, container);
        let fixture = Fixture::start(&assets).await;
        let resolver = ResourceResolver::with_ffmpeg(Some(remux.clone())).unwrap();
        let extracted = resolver.extract(&fixture.page).await.unwrap();
        assert_eq!(extracted.platform, "Generic");
        assert_eq!(extracted.formats.len(), 1);
        assert!(extracted.formats[0].audio_only);
        assert_eq!(extracted.formats[0].plan.extension, "m4a");
        assert_eq!(extracted.formats[0].plan.assembly, Assembly::Hls);
        let preview = resolver.resolve(&fixture.page).await.unwrap();
        assert!(preview.ffmpeg_available);
        assert_eq!(preview.formats.len(), 1);
        assert!(preview.formats[0].audio_only && preview.formats[0].needs_merge);
        assert_eq!(preview.formats[0].extension, "m4a");
        assert!(preview.formats[0].filename.ends_with(".m4a"));
        let public = serde_json::to_string(&preview).unwrap();
        for private in [
            "signature=",
            "fixture-private",
            "/assets/",
            "\"streams\"",
            "\"headers\"",
        ] {
            assert!(
                !public.contains(private),
                "preview exposes private executor metadata"
            );
        }
        let plan = resolver
            .select(&preview.id, &preview.formats[0].id)
            .unwrap();
        assert_eq!(plan.extension, "m4a");
        assert_eq!(plan.assembly, Assembly::Hls);
        assert!(
            plan.streams.len() >= 4,
            "fixture needs a middle segment to fail"
        );
        let fail_index = 2;
        let failed_path = reqwest::Url::parse(&plan.streams[fail_index].url)
            .unwrap()
            .path()
            .to_owned();
        *fixture.capture.failed_path.lock().unwrap() = failed_path.clone();
        fixture.capture.fail.store(true, Ordering::Relaxed);
        let output = directory.path().join(format!("{container:?}.m4a"));
        let (_pause, control) = controls();
        let failure = tokio::time::timeout(
            Duration::from_secs(30),
            media_download::run(
                plan.clone(),
                output.clone(),
                2,
                None,
                control,
                Some(remux.clone()),
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(
            failure.to_string().contains("503"),
            "{container:?}: {failure:#}"
        );
        assert!(
            !output.exists(),
            "failed transfer must not publish a partial M4A"
        );
        let work = directory
            .path()
            .join(format!("{container:?}.m4a.ffdm-state/media"));
        let completed_counts: Vec<_> = plan.streams[..fail_index]
            .iter()
            .enumerate()
            .map(|(index, stream)| {
                let url = reqwest::Url::parse(&stream.url).unwrap();
                let completed = work.join(format!("track-{index}.bin"));
                assert!(completed.is_file());
                assert!(std::fs::metadata(completed).unwrap().len() > 0);
                (url.path().to_owned(), fixture.count(url.path()))
            })
            .collect();
        let failed_requests = fixture.count(&failed_path);
        fixture.capture.fail.store(false, Ordering::Relaxed);
        let (_pause, control) = controls();
        let done = tokio::time::timeout(
            Duration::from_secs(30),
            media_download::run(
                plan.clone(),
                output.clone(),
                2,
                None,
                control,
                Some(remux.clone()),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(done.outcome, "completed");
        for (path, count) in completed_counts {
            assert_eq!(
                fixture.count(&path),
                count,
                "completed piece was refetched: {container:?}"
            );
        }
        assert!(fixture.count(&failed_path) > failed_requests);
        assert_audio_only_m4a(&ffprobe, &output, assets.duration);
        let before = std::fs::read(&output).unwrap();
        assert_eq!(done.bytes, before.len() as u64);
        assert_eq!(
            done.sha256.as_deref(),
            Some(format!("{:x}", Sha256::digest(&before)).as_str())
        );
        let counts = fixture.counts();
        let (_pause, control) = controls();
        let failure =
            media_download::run(plan, output.clone(), 2, None, control, Some(remux.clone()))
                .await
                .unwrap_err();
        assert!(failure.to_string().contains("目标文件已存在"));
        assert_eq!(std::fs::read(output).unwrap(), before);
        assert_eq!(fixture.counts(), counts);
        assert!(!fixture.capture.leaked_credentials.load(Ordering::Relaxed));
        assert!(!fixture.capture.duplicate_user_agent.load(Ordering::Relaxed));
        println!(
            "{container:?}: Rust audio HLS -> AAC M4A, retry checkpoint and no-overwrite verified ({:.3}s)",
            assets.duration
        );
    }
}
