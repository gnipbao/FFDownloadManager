//! Contract tests: standalone extraction -> the application's Rust executor.
//! The tiny, silent media below were generated from a black frame / silence
//! with FFmpeg once. Tests need no FFmpeg, browser, credentials or public CDN.
use axum::{
    extract::{Request, State},
    http::header,
    middleware::{self, Next},
    response::{Html, Response},
    routing::get,
    Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use bytes::Bytes;
use ffdownload::{
    download::{DownloadControl, DownloadSnapshot},
    media_download,
    resolver::{Assembly, MediaPlan, ResourceResolver},
    test_server::{fixture_router, ServerConfig},
};
use sha2::{Digest, Sha256};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{net::TcpListener, sync::watch, task::JoinHandle};

const MP4: &str = "AAAAIGZ0eXBpc29tAAACAGlzb21pc28yYXZjMW1wNDEAAAMobW9vdgAAAGxtdmhkAAAAAAAAAAAAAAAAAAAD6AAAAMgAAQAAAQAAAAAAAAAAAAAAAAEAAAAAAAAAAAAAAAAAAAABAAAAAAAAAAAAAAAAAABAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAgAAAlN0cmFrAAAAXHRraGQAAAADAAAAAAAAAAAAAAABAAAAAAAAAMgAAAAAAAAAAAAAAAAAAAAAAAEAAAAAAAAAAAAAAAAAAAABAAAAAAAAAAAAAAAAAABAAAAAABAAAAAQAAAAAAAkZWR0cwAAABxlbHN0AAAAAAAAAAEAAADIAAAAAAABAAAAAAHLbWRpYQAAACBtZGhkAAAAAAAAAAAAAAAAAAAoAAAACABVxAAAAAAALWhkbHIAAAAAAAAAAHZpZGUAAAAAAAAAAAAAAABWaWRlb0hhbmRsZXIAAAABdm1pbmYAAAAUdm1oZAAAAAEAAAAAAAAAAAAAACRkaW5mAAAAHGRyZWYAAAAAAAAAAQAAAAx1cmwgAAAAAQAAATZzdGJsAAAAtnN0c2QAAAAAAAAAAQAAAKZhdmMxAAAAAAAAAAEAAAAAAAAAAAAAAAAAAAAAABAAEABIAAAASAAAAAAAAAABFExhdmM2MS4zLjEwMCBsaWJ4MjY0AAAAAAAAAAAAAAAAGP//AAAALGF2Y0MBQsAK/+EAFWdCwAraewEQAAADABAAAAMBQPEiagEABGjOD8gAAAAQcGFzcAAAAAEAAAABAAAAFGJ0cnQAAAAAAABhWAAAYVgAAAAYc3R0cwAAAAAAAAABAAAAAgAABAAAAAAUc3RzcwAAAAAAAAABAAAAAQAAABxzdHNjAAAAAAAAAAEAAAABAAAAAgAAAAEAAAAcc3RzegAAAAAAAAAAAAAAAgAAAmYAAAAJAAAAFHN0Y28AAAAAAAAAAQAAA1gAAABhdWR0YQAAAFltZXRhAAAAAAAAACFoZGxyAAAAAAAAAABtZGlyYXBwbAAAAAAAAAAAAAAAACxpbHN0AAAAJKl0b28AAAAcZGF0YQAAAAEAAAAATGF2ZjYxLjEuMTAwAAAACGZyZWUAAAJ3bWRhdAAAAlQGBf//UNxF6b3m2Ui3lizYINkj7u94MjY0IC0gY29yZSAxNjQgcjMxMDggMzFlMTlmOSAtIEguMjY0L01QRUctNCBBVkMgY29kZWMgLSBDb3B5bGVmdCAyMDAzLTIwMjMgLSBodHRwOi8vd3d3LnZpZGVvbGFuLm9yZy94MjY0Lmh0bWwgLSBvcHRpb25zOiBjYWJhYz0wIHJlZj0xIGRlYmxvY2s9MDowOjAgYW5hbHlzZT0wOjAgbWU9ZGlhIHN1Ym1lPTAgcHN5PTEgcHN5X3JkPTEuMDA6MC4wMCBtaXhlZF9yZWY9MCBtZV9yYW5nZT0xNiBjaHJvbWFfbWU9MSB0cmVsbGlzPTAgOHg4ZGN0PTAgY3FtPTAgZGVhZHpvbmU9MjEsMTEgZmFzdF9wc2tpcD0xIGNocm9tYV9xcF9vZmZzZXQ9MCB0aHJlYWRzPTEgbG9va2FoZWFkX3RocmVhZHM9MSBzbGljZWRfdGhyZWFkcz0wIG5yPTAgZGVjaW1hdGU9MSBpbnRlcmxhY2VkPTAgYmx1cmF5X2NvbXBhdD0wIGNvbnN0cmFpbmVkX2ludHJhPTAgYmZyYW1lcz0wIHdlaWdodHA9MCBrZXlpbnQ9MjUwIGtleWludF9taW49MTAgc2NlbmVjdXQ9MCBpbnRyYV9yZWZyZXNoPTAgcmM9Y3JmIG1idHJlZT0wIGNyZj0yMy4wIHFjb21wPTAuNjAgcXBtaW49MCBxcG1heD02OSBxcHN0ZXA9NCBpcF9yYXRpbz0xLjQwIGFxPTAAgAAAAApliIQ6JigACQLgAAAABUGaIDKU";
const WEBM: &str = "GkXfo59ChoEBQveBAULygQRC84EIQoKEd2VibUKHgQJChYECGFOAZwEAAAAAAAH1EU2bdLpNu4tTq4QVSalmU6yBoU27i1OrhBZUrmtTrIHWTbuMU6uEElTDZ1OsggEcTbuMU6uEHFO7a1OsggHf7AEAAAAAAABZAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAVSalmsCrXsYMPQkBNgIxMYXZmNjEuMS4xMDBXQYxMYXZmNjEuMS4xMDBEiYhAaQAAAAAAABZUrmvBrgEAAAAAAAA414EBc8WI1r6/nkc2XQycgQAitZyDdW5kiIEAhoVWX1ZQOYOBASPjg4QF9eEA4ImwgRC6gRCagQISVMNn/nNzn2PAgGfImUWjh0VOQ09ERVJEh4xMYXZmNjEuMS4xMDBzc9ljwItjxYjWvr+eRzZdDGfIpEWjh0VOQ09ERVJEh5dMYXZjNjEuMy4xMDAgbGlidnB4LXZwOWfIoUWjiERVUkFUSU9ORIeTMDA6MDA6MDAuMjAwMDAwMDAwAB9DtnW754EAo6GBAACAgkmDQgAA8AD2ADgkHBhCAAAwYAAAEL///YsqAACjk4EAZACGAECSnABJQAADIAAAQkAcU7trkbuPs4EAt4r3gQHxggGf8IED";
const MP3: &str = "SUQzBAAAAAAAIlRTU0UAAAAOAAADTGF2ZjYxLjEuMTAwAAAAAAAAAAAAAAD/+0DAAAAAAAAAAAAAAAAAAAAAAABJbmZvAAAADwAAAAkAAARiAEFBQUFBQUFBQUFBWVlZWVlZWVlZWVlwcHBwcHBwcHBwcIiIiIiIiIiIiIiIoKCgoKCgoKCgoKC4uLi4uLi4uLi4uNDQ0NDQ0NDQ0NDQ6Ojo6Ojo6Ojo6Oj//////////////wAAAABMYXZjNjEuMy4AAAAAAAAAAAAAAAAkA8wAAAAAAAAEYtn1KHYAAAAAAP/7EMQAA8AAAaQAAAAgAAA0gAAABExBTUUzLjEwMFVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVMQU1FMy4xMDBV//sSxCmDwAABpAAAACAAADSAAAAEVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVMQU1FMy4xMDBV//sQxFODwAABpAAAACAAADSAAAAEVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVUxBTUUzLjEwMFX/+xLEfQPAAAGkAAAAIAAANIAAAARVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVX/+xDEpwPAAAGkAAAAIAAANIAAAARVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVf/7EsTQg8AAAaQAAAAgAAA0gAAABFVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVf/7EMTWA8AAAaQAAAAgAAA0gAAABFVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVV//sSxNWDwAABpAAAACAAADSAAAAEVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVV//sQxNYDwAABpAAAACAAADSAAAAEVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVU=";

fn media_fixture(extension: &str) -> Bytes {
    Bytes::from(
        STANDARD
            .decode(match extension {
                "mp4" => MP4,
                "webm" => WEBM,
                "mp3" => MP3,
                _ => unreachable!(),
            })
            .unwrap(),
    )
}

fn file_fixture(extension: &str) -> Bytes {
    match extension {
        // Valid empty ZIP: end-of-central-directory record with zero entries.
        "zip" => Bytes::from_static(b"PK\x05\x06\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0"),
        "pdf" => Bytes::from_static(b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Count 0 /Kids [] >>\nendobj\ntrailer\n<< /Root 1 0 R >>\n%%EOF\n"),
        _ => unreachable!(),
    }
}

#[derive(Default)]
struct Requests {
    leaked_credentials: AtomicBool,
    duplicate_user_agent: AtomicBool,
    slow: AtomicUsize,
}

struct Fixture {
    base: String,
    requests: Arc<Requests>,
    task: JoinHandle<std::io::Result<()>>,
}

impl Fixture {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Requests::default());
        let mut opaque_audio = ServerConfig::new(media_fixture("mp3"));
        opaque_audio.required_headers =
            vec![("Referer".into(), format!("{base}/html/opaque-audio"))];
        let app = Router::new()
            .route("/html/video", get(|| async { Html(r#"<html><title>Black frame sample</title><video controls><source src="/assets/mp4/file.bin?signature=private%2Btoken&amp;expires=999" type="video/mp4" label="720p"><source src="/assets/webm/file.bin?signature=second-private-token" type="video/webm" label="1080p"></video></html>"#) }))
            .route("/html/audio", get(|| async { Html(r#"<html><title>Silence</title><audio controls><source src="/assets/mp3/file.bin?signature=audio-private-token" type="audio/mpeg"></audio></html>"#) }))
            .route("/html/slow", get(|| async { Html(r#"<html><title>Usable primary format</title><video><source src="/assets/mp4/file.bin?signature=healthy" type="video/mp4" label="720p"><source src="/opaque?signature=slow" label="1080p"></video></html>"#) }))
            .route("/html/opaque-audio", get(|| async { Html(r#"<html><title>Opaque audio source</title><audio><source src="/opaque/audio/file.bin?signature=opaque-private-token"></audio></html>"#) }))
            .route("/opaque", get(|| async {
                tokio::time::sleep(Duration::from_secs(15)).await;
                Html("<html><title>Login</title><form>Sign in</form></html>")
            }))
            .route("/login.mp4", get(|| async { Html("<!doctype html><html><form>Sign in</form></html>") }))
            .route("/html/login-media", get(|| async { Html(r#"<html><video src="/login.mp4" type="video/mp4"></video></html>"#) }))
            .nest("/assets/mp4", fixture_router(ServerConfig::new(media_fixture("mp4"))))
            .nest("/assets/webm", fixture_router(ServerConfig::new(media_fixture("webm"))))
            .nest("/assets/mp3", fixture_router(ServerConfig::new(media_fixture("mp3"))))
            .nest("/opaque/audio", fixture_router(opaque_audio))
            .nest("/downloads/zip", fixture_router(ServerConfig::new(file_fixture("zip"))))
            .nest("/downloads/pdf", fixture_router(ServerConfig::new(file_fixture("pdf"))))
            .layer(middleware::from_fn_with_state(requests.clone(), media_headers));
        let task = tokio::spawn(async move { axum::serve(listener, app).await });
        Self {
            base,
            requests,
            task,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn clean_requests(&self) {
        assert!(!self.requests.leaked_credentials.load(Ordering::Relaxed));
        assert!(!self.requests.duplicate_user_agent.load(Ordering::Relaxed));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn media_headers(
    State(requests): State<Arc<Requests>>,
    request: Request,
    next: Next,
) -> Response {
    if request.headers().contains_key(header::COOKIE)
        || request.headers().contains_key(header::AUTHORIZATION)
    {
        requests.leaked_credentials.store(true, Ordering::Relaxed);
    }
    if request.headers().get_all(header::USER_AGENT).iter().count() > 1 {
        requests.duplicate_user_agent.store(true, Ordering::Relaxed);
    }
    let path = request.uri().path().to_owned();
    if path == "/opaque" {
        requests.slow.fetch_add(1, Ordering::Relaxed);
    }
    let mut response = next.run(request).await;
    let (mime, filename) = match path.as_str() {
        "/assets/mp4/file.bin" => ("video/mp4", None),
        "/assets/webm/file.bin" => ("video/webm", None),
        "/assets/mp3/file.bin" => ("audio/mpeg", None),
        "/opaque/audio/file.bin" => ("audio/mpeg", None),
        "/downloads/zip/file.bin" => (
            "application/zip",
            Some("attachment; filename=\"research.zip\""),
        ),
        "/downloads/pdf/file.bin" => (
            "application/pdf",
            Some("attachment; filename=\"paper.pdf\""),
        ),
        _ => return response,
    };
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, mime.parse().unwrap());
    if let Some(filename) = filename {
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, filename.parse().unwrap());
    }
    response
}

async fn roundtrip(plan: MediaPlan, expected: &Bytes) {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join(plan.filename());
    let hash = format!("{:x}", Sha256::digest(expected));
    let (_keep_host_alive, pause) = watch::channel(false);
    let (progress, _events) = watch::channel(DownloadSnapshot::default());
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        media_download::run(
            plan,
            output.clone(),
            4,
            Some(hash.clone()),
            DownloadControl { pause, progress },
            None,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.outcome, "completed");
    assert_eq!(result.bytes, expected.len() as u64);
    assert_eq!(result.sha256.as_deref(), Some(hash.as_str()));
    assert_eq!(tokio::fs::read(&output).await.unwrap(), expected.as_ref());
    assert!(!output.with_extension("ffdm.part").exists());
}

#[tokio::test]
async fn standalone_html5_formats_keep_containers_and_roundtrip_through_rust_downloader() {
    let fixture = Fixture::start().await;
    let resolver = ResourceResolver::with_ffmpeg(None).unwrap();
    let url = fixture.url("/html/video");
    let extracted = resolver.extract(&url).await.unwrap();
    assert_eq!(extracted.platform, "Generic");
    assert_eq!(extracted.formats.len(), 2);
    assert!(extracted
        .formats
        .iter()
        .any(|format| format.height == 720 && format.plan.extension == "mp4"));
    assert!(extracted
        .formats
        .iter()
        .any(|format| format.height == 1080 && format.plan.extension == "webm"));
    let preview = resolver.resolve(&url).await.unwrap();
    assert!(!preview.ffmpeg_available);
    assert_eq!(preview.formats[0].extension, "webm");
    let public = serde_json::to_string(&preview).unwrap();
    for private in [
        "signature=",
        "private-token",
        "/assets/",
        "User-Agent",
        "Referer",
        "\"headers\"",
        "\"streams\"",
    ] {
        assert!(
            !public.contains(private),
            "preview contains private download information"
        );
    }
    for format in preview.formats {
        let plan = resolver.select(&preview.id, &format.id).unwrap();
        assert_eq!(plan.assembly, Assembly::Direct);
        assert!(!format.needs_merge && !format.audio_only);
        assert!(plan.streams[0].url.contains("signature="));
        assert!(plan.streams[0]
            .headers
            .iter()
            .any(|(name, _)| name == "Referer"));
        roundtrip(plan, &media_fixture(&format.extension)).await;
    }
    fixture.clean_requests();
}

#[tokio::test]
async fn generic_audio_keeps_audio_only_and_mp3_without_ffmpeg() {
    let fixture = Fixture::start().await;
    let resolver = ResourceResolver::with_ffmpeg(None).unwrap();
    let preview = resolver.resolve(&fixture.url("/html/audio")).await.unwrap();
    assert_eq!(preview.formats.len(), 1);
    let format = &preview.formats[0];
    assert!(format.audio_only);
    assert_eq!(format.extension, "mp3");
    assert!(!format.needs_merge);
    let plan = resolver.select(&preview.id, &format.id).unwrap();
    roundtrip(plan, &media_fixture("mp3")).await;
    fixture.clean_requests();
}

#[tokio::test]
async fn opaque_audio_inspection_preserves_required_page_referer() {
    let fixture = Fixture::start().await;
    let resolver = ResourceResolver::with_ffmpeg(None).unwrap();
    let preview = resolver
        .resolve(&fixture.url("/html/opaque-audio"))
        .await
        .unwrap();
    assert_eq!(preview.formats.len(), 1);
    assert!(preview.formats[0].audio_only);
    assert_eq!(preview.formats[0].extension, "mp3");
    let plan = resolver
        .select(&preview.id, &preview.formats[0].id)
        .unwrap();
    assert_eq!(
        plan.streams[0].size,
        Some(media_fixture("mp3").len() as u64)
    );
    assert!(plan.streams[0].headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("referer") && value == &fixture.url("/html/opaque-audio")
    }));
    roundtrip(plan, &media_fixture("mp3")).await;
    fixture.clean_requests();
}

#[tokio::test]
async fn explicit_arbitrary_zip_and_pdf_are_direct_files_without_media_tools() {
    let fixture = Fixture::start().await;
    let resolver = ResourceResolver::with_ffmpeg(None).unwrap();
    for (extension, title) in [("zip", "research"), ("pdf", "paper")] {
        let url = fixture.url(&format!(
            "/downloads/{extension}/file.bin?download=private-file-token"
        ));
        let extracted = resolver.extract(&url).await.unwrap();
        assert_eq!(extracted.platform, "Direct File");
        assert_eq!(extracted.formats.len(), 1);
        let plan = extracted.formats.into_iter().next().unwrap().plan;
        assert_eq!(plan.assembly, Assembly::Direct);
        assert_eq!(plan.extension, extension);
        assert_eq!(plan.title, title);
        assert_eq!(plan.filename(), format!("{title}.{extension}"));
        roundtrip(plan, &file_fixture(extension)).await;
    }
    fixture.clean_requests();
}

#[tokio::test]
async fn http_200_login_html_is_never_offered_as_a_media_file() {
    let fixture = Fixture::start().await;
    let resolver = ResourceResolver::with_ffmpeg(None).unwrap();
    assert!(resolver.extract(&fixture.url("/login.mp4")).await.is_err());
    assert!(resolver
        .resolve(&fixture.url("/html/login-media"))
        .await
        .is_err());
    fixture.clean_requests();
}

#[tokio::test]
async fn healthy_generic_format_survives_a_slow_opaque_source() {
    let fixture = Fixture::start().await;
    let resolver = ResourceResolver::with_ffmpeg(None).unwrap();
    let started = Instant::now();
    let extracted = resolver.extract(&fixture.url("/html/slow")).await.unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(16),
        "one slow source must not consume the entire extraction budget: {elapsed:?}"
    );
    assert!(
        elapsed >= Duration::from_secs(10),
        "the slow source must exercise its deadline"
    );
    assert_eq!(fixture.requests.slow.load(Ordering::Relaxed), 1);
    assert_eq!(extracted.formats.len(), 1);
    assert_eq!(extracted.formats[0].height, 720);
    assert_eq!(extracted.formats[0].plan.extension, "mp4");
    roundtrip(
        extracted.formats.into_iter().next().unwrap().plan,
        &media_fixture("mp4"),
    )
    .await;
    fixture.clean_requests();
}
