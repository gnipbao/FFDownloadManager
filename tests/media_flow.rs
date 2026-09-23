use ffdownload::{
    download::{DownloadControl, DownloadSnapshot},
    media::{find_ffmpeg, Assembly, MediaPlan, MediaStream},
    media_download,
    test_server::{fixture_data, ServerConfig, TestServer},
};
use std::{path::Path, process::Command, time::Duration};
use tokio::sync::watch;

fn plan(streams: Vec<MediaStream>, assembly: Assembly) -> MediaPlan {
    MediaPlan {
        source_url: "https://www.bilibili.com/video/av170001".into(),
        source_id: "fixture".into(),
        platform: "Bilibili".into(),
        title: "媒体集成测试".into(),
        format_id: "fixture-format".into(),
        label: "测试 MP4".into(),
        extension: "mp4".into(),
        assembly,
        streams,
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
async fn media_headers_and_partial_data_survive_pause_and_resume() {
    let data = fixture_data(6 * 1024 * 1024);
    let headers = vec![("Referer".into(), "https://www.bilibili.com/".into())];
    let mut config = ServerConfig::new(data.clone());
    config.per_response_mbps = Some(8.0);
    config.required_headers = headers.clone();
    let server = TestServer::start(config).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("视频.mp4");
    let plan = plan(
        vec![MediaStream {
            url: server.url.clone(),
            headers,
            size: Some(data.len() as u64),
        }],
        Assembly::Direct,
    );
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
    assert!(paused.bytes > 0 && paused.bytes < data.len() as u64);
    assert!(!output.exists());
    let requests_before = server.range_requests();
    let (_pause, _progress, control) = controls();
    let done = media_download::run(plan, output.clone(), 2, None, control, None)
        .await
        .unwrap();
    assert_eq!(done.outcome, "completed");
    assert_eq!(std::fs::read(&output).unwrap(), data);
    assert!(server.range_requests() > requests_before);
    assert!(done.sha256.is_some());
    server.stop().await.unwrap();
}

fn make_track(ffmpeg: &Path, output: &Path, audio: bool) {
    let mut command = Command::new(ffmpeg);
    command.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-f",
        "lavfi",
        "-i",
    ]);
    if audio {
        command.args([
            "sine=frequency=440:sample_rate=44100",
            "-t",
            "2",
            "-c:a",
            "aac",
        ]);
    } else {
        command.args([
            "color=c=blue:s=320x180:r=24",
            "-t",
            "2",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
        ]);
    }
    let result = command.arg(output).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[tokio::test]
#[ignore = "requires FFmpeg and ffprobe installed"]
async fn completed_tracks_survive_failed_assembly_and_merge_into_playable_video() {
    let ffmpeg = find_ffmpeg().expect("FFmpeg is required for the real mux test");
    let directory = tempfile::tempdir().unwrap();
    let video = directory.path().join("video.mp4");
    let audio = directory.path().join("audio.m4a");
    make_track(&ffmpeg, &video, false);
    make_track(&ffmpeg, &audio, true);
    let mut servers = Vec::new();
    let mut streams = Vec::new();
    for path in [&video, &audio] {
        let data = std::fs::read(path).unwrap();
        let size = data.len() as u64;
        let server = TestServer::start(ServerConfig::new(data.into()))
            .await
            .unwrap();
        streams.push(MediaStream {
            url: server.url.clone(),
            headers: vec![],
            size: Some(size),
        });
        servers.push(server);
    }
    let plan = plan(streams, Assembly::Merge);
    let output = directory.path().join("完整视频.mp4");
    let (_pause, _progress, control) = controls();
    // Simulate a dependency disappearing after a job was queued.
    let failed = media_download::run(plan.clone(), output.clone(), 2, None, control, None)
        .await
        .unwrap_err();
    assert!(failed.to_string().contains("FFmpeg"));
    assert!(!output.exists());
    let work = directory.path().join("完整视频.mp4.ffdm-state/media");
    assert!(work.join("track-0.bin").exists() && work.join("track-1.bin").exists());
    let before: usize = servers.iter().map(TestServer::range_requests).sum();
    let (_pause, _progress, control) = controls();
    let done = media_download::run(
        plan.clone(),
        output.clone(),
        2,
        None,
        control,
        Some(ffmpeg.clone()),
    )
    .await
    .unwrap();
    assert_eq!(done.outcome, "completed");
    assert_eq!(
        before,
        servers
            .iter()
            .map(TestServer::range_requests)
            .sum::<usize>(),
        "completed tracks must be reused"
    );
    let probe = Command::new(ffmpeg.with_file_name("ffprobe"))
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "json",
        ])
        .arg(&output)
        .output()
        .unwrap();
    assert!(probe.status.success());
    let metadata: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
    let kinds: Vec<_> = metadata["streams"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["codec_type"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"audio") && kinds.contains(&"video"));
    assert!(!work.join("track-0.bin").exists());
    let before = std::fs::read(&output).unwrap();
    let (_pause, _progress, control) = controls();
    assert!(
        media_download::run(plan, output.clone(), 2, None, control, Some(ffmpeg))
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(output).unwrap(),
        before,
        "existing output is never replaced"
    );
    for server in servers {
        server.stop().await.unwrap();
    }
}
