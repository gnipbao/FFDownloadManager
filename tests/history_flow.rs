use ffdownload::{
    service::{DownloadService, NewTask},
    test_server::{fixture_data, ServerConfig, TestServer},
};
use serde_json::{json, Value};
use std::{collections::BTreeSet, sync::Arc, time::Duration};

async fn stop(service: Arc<DownloadService>) {
    service.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while Arc::strong_count(&service) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn deleted_completed_file_disappears_live_and_after_restart_without_erasing_history() {
    let data = fixture_data(128 * 1024);
    let source = TestServer::start(ServerConfig::new(data.clone()))
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let downloads = dir.path().join("files");
    let state = dir.path().join("state");
    let service = DownloadService::open_with_ffmpeg(&downloads, &state, None).unwrap();
    let task = service
        .create(
            NewTask {
                url: source.url.clone(),
                filename: Some("history.bin".into()),
                connections: 1,
                direct: false,
                cookie: None,
                sha256: None,
            },
            false,
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let snapshot = service.snapshot();
            let task = snapshot.tasks.iter().find(|t| t.id == task.id).unwrap();
            assert_ne!(task.state, "error", "{:?}", task.error);
            if task.state == "completed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let output = downloads.join("history.bin");
    assert_eq!(std::fs::read(&output).unwrap(), data);
    std::fs::remove_file(&output).unwrap();
    assert!(service.snapshot().tasks.is_empty());

    // A restored file/disk can reappear without inventing a failed download.
    std::fs::write(&output, &data).unwrap();
    let restored = service.snapshot();
    assert_eq!(restored.tasks.len(), 1);
    assert_eq!(restored.tasks[0].state, "completed");
    std::fs::remove_file(&output).unwrap();
    stop(service).await;
    let service = DownloadService::open_with_ffmpeg(&downloads, &state, None).unwrap();
    assert!(service.snapshot().tasks.is_empty());
    let saved: Value =
        serde_json::from_slice(&std::fs::read(state.join("tasks.json")).unwrap()).unwrap();
    assert_eq!(saved["tasks"][0]["id"], task.id);
    assert_eq!(saved["tasks"][0]["state"], "completed");
    stop(service).await;
    source.stop().await.unwrap();
}

fn saved_task(id: &str, state: &str, completed: bool, error: Option<&str>) -> Value {
    json!({
        "id":id, "filename":format!("{id}.bin"), "url":"https://files.example/archive.bin",
        "connections":4, "state":state, "created_at":1,
        "completed_at":if completed { Some(2) } else { None },
        "total_bytes":1024, "completed_bytes":if completed { 1024 } else { 512 },
        "speed_bytes":0, "active_connections":0, "eta_seconds":null,
        "sha256":null, "expected_sha256":null, "error":error, "demo":false
    })
}

#[tokio::test]
async fn legacy_missing_outputs_are_hidden_but_failed_paused_and_queued_work_is_retained() {
    let dir = tempfile::tempdir().unwrap();
    let downloads = dir.path().join("files");
    let state = dir.path().join("state");
    std::fs::create_dir_all(&downloads).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(downloads.join("present.bin"), [1; 1024]).unwrap();
    std::fs::write(downloads.join("paused.bin.ffdm.part"), [2; 512]).unwrap();
    let tasks = vec![
        saved_task("present", "completed", true, None),
        saved_task("deleted", "completed", true, None),
        saved_task("legacy", "error", true, Some("下载文件已被移动或删除")),
        saved_task("failed", "error", false, Some("HTTP error: 503")),
        saved_task("paused", "paused", false, None),
        saved_task("queued", "queued", false, None),
    ];
    let saved = json!({"schema":1, "directory":downloads.canonicalize().unwrap(), "tasks":tasks});
    std::fs::write(
        state.join("tasks.json"),
        serde_json::to_vec(&saved).unwrap(),
    )
    .unwrap();
    let service = DownloadService::open_with_ffmpeg(&downloads, &state, None).unwrap();
    let snapshot = service.snapshot();
    let ids: BTreeSet<_> = snapshot.tasks.iter().map(|task| task.id.as_str()).collect();
    assert_eq!(
        ids,
        BTreeSet::from(["present", "failed", "paused", "queued"])
    );
    for (id, expected_state) in [
        ("present", "completed"),
        ("failed", "error"),
        ("paused", "paused"),
        ("queued", "paused"),
    ] {
        assert_eq!(
            snapshot
                .tasks
                .iter()
                .find(|task| task.id == id)
                .unwrap()
                .state,
            expected_state
        );
    }
    assert_eq!(
        snapshot
            .tasks
            .iter()
            .find(|task| task.id == "failed")
            .unwrap()
            .completed_bytes,
        512
    );
    assert_eq!(
        std::fs::read(downloads.join("paused.bin.ffdm.part")).unwrap(),
        [2; 512]
    );
    // Old deleted-file errors migrate back to completed metadata, and a restored
    // output returns as a usable completed file rather than a retry action.
    std::fs::write(downloads.join("legacy.bin"), [3; 1024]).unwrap();
    let restored = service
        .snapshot()
        .tasks
        .into_iter()
        .find(|task| task.id == "legacy")
        .unwrap();
    assert_eq!(restored.state, "completed");
    assert!(restored.error.is_none());
    stop(service).await;
}
