use ffdownload::download::{
    download_with_headers, sha256_file, DownloadControl, DownloadReport, DownloadRequest,
    DownloadSnapshot,
};
use gosh_dl::{DownloadState, Segment, SegmentState, SqliteStorage, Storage};
use serde_json::{json, Value};
use std::{io::Write, path::Path, time::Duration};
use tokio::sync::watch;

const SOURCE: &str =
    "https://upload.wikimedia.org/wikipedia/commons/a/a9/FFDownload_policy_fixture.bin";
const MIB: u64 = 1024 * 1024;

fn request(output: &Path, resume: bool) -> DownloadRequest {
    DownloadRequest {
        url: Some(SOURCE.into()),
        output: output.into(),
        connections: 4,
        resume,
        expected_sha256: None,
        pause_after: None,
        timeout: Duration::from_secs(3),
        interactive: false,
    }
}

async fn checkpoint_without_network(
    request: DownloadRequest,
    headers: Vec<(String, String)>,
) -> DownloadReport {
    // The already-paused control makes download_inner return before source
    // preflight/acquisition or engine.resume. No HTTP fixture or public I/O is used.
    let (_pause, pause) = watch::channel(true);
    let (progress, _progress) = watch::channel(DownloadSnapshot::default());
    download_with_headers(request, Some(DownloadControl { pause, progress }), headers)
        .await
        .unwrap()
}

#[tokio::test]
async fn legacy_wikimedia_checkpoint_adopts_limits_without_replacing_identity_or_ranges() {
    let limits = ffdm_resolver::source_policy::limits(SOURCE).unwrap();
    assert_eq!(limits.max_connections, 1);
    assert_eq!(limits.bytes_per_second, 1_400_000);
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("wiki-fixture.bin");
    let state = directory.path().join("wiki-fixture.bin.ffdm-state");
    let job_path = state.join("job.json");
    let database = state.join("engine.sqlite");
    let engine_part = directory.path().join("wiki-fixture.bin.ffdm.part.part");
    let headers = vec![("X-Checkpoint-Fixture".into(), "preserve-me".into())];

    let initial = checkpoint_without_network(request(&output, false), headers.clone()).await;
    assert_eq!(initial.outcome, "paused");
    assert_eq!(initial.connections, 1);
    assert_eq!(initial.bytes, 0);
    assert!(!output.exists());
    let mut legacy_job: Value = serde_json::from_slice(&std::fs::read(&job_path).unwrap()).unwrap();
    let id = serde_json::from_value(legacy_job["id"].clone()).unwrap();

    // Reconstruct a real older four-connection checkpoint: one complete range,
    // one partially downloaded failed range, and two ranges still pending.
    let mut segments = (0..4)
        .map(|index| Segment::new(index, index as u64 * MIB, (index as u64 + 1) * MIB - 1))
        .collect::<Vec<_>>();
    segments[0].downloaded = MIB;
    segments[0].state = SegmentState::Completed;
    segments[1].downloaded = MIB / 4;
    segments[1].state = SegmentState::Failed {
        error: "fixture connection interrupted".into(),
        retries: 2,
    };
    let transferred = MIB + MIB / 4;
    let mut partial = std::fs::File::create(&engine_part).unwrap();
    partial.set_len(4 * MIB).unwrap();
    partial
        .write_all(&vec![0x5a; transferred as usize])
        .unwrap();
    partial.sync_all().unwrap();
    drop(partial);
    let partial_hash = sha256_file(&engine_part).unwrap();

    let storage = SqliteStorage::new(&database).await.unwrap();
    let mut status = storage.load_download(id).await.unwrap().unwrap();
    status.state = DownloadState::Error {
        kind: "Network".into(),
        message: "fixture connection interrupted".into(),
        retryable: true,
    };
    status.progress.total_size = Some(4 * MIB);
    status.progress.completed_size = transferred;
    status.metadata.etag = Some("\"unchanged-wiki-fixture\"".into());
    status.metadata.last_modified = Some("Mon, 05 Oct 2026 00:00:00 GMT".into());
    storage.save_download(&status).await.unwrap();
    storage.save_segments(id, &segments).await.unwrap();
    let original_metadata = serde_json::to_value(&status.metadata).unwrap();
    let original_segments = serde_json::to_value(&segments).unwrap();

    let mut legacy_runtime: Value =
        serde_json::from_str(&storage.load_runtime_metadata(id).await.unwrap().unwrap()).unwrap();
    legacy_runtime["options"]["max_connections"] = json!(4);
    storage
        .save_runtime_metadata(id, &legacy_runtime.to_string())
        .await
        .unwrap();
    legacy_job["connections"] = json!(4);
    std::fs::write(&job_path, serde_json::to_vec_pretty(&legacy_job).unwrap()).unwrap();
    drop(storage);

    // No new headers: the old job must retain its own request metadata.
    let resumed = checkpoint_without_network(request(&output, true), Vec::new()).await;
    assert_eq!(resumed.outcome, "paused");
    assert_eq!(resumed.connections, 1);
    assert_eq!(resumed.starting_progress_bytes, transferred);
    assert_eq!(resumed.bytes, transferred);
    assert!(!output.exists());
    assert_eq!(sha256_file(&engine_part).unwrap(), partial_hash);

    let retained_job: Value = serde_json::from_slice(&std::fs::read(&job_path).unwrap()).unwrap();
    legacy_job["connections"] = json!(1);
    assert_eq!(
        retained_job, legacy_job,
        "only the connection cap may change"
    );
    let storage = SqliteStorage::new(&database).await.unwrap();
    let retained_status = storage.load_download(id).await.unwrap().unwrap();
    assert_eq!(retained_status.id, id);
    assert_eq!(retained_status.state, DownloadState::Paused);
    assert_eq!(retained_status.progress.completed_size, transferred);
    assert_eq!(
        serde_json::to_value(retained_status.metadata).unwrap(),
        original_metadata,
    );
    assert_eq!(
        serde_json::to_value(storage.load_segments(id).await.unwrap()).unwrap(),
        original_segments,
    );
    let retained_runtime: Value =
        serde_json::from_str(&storage.load_runtime_metadata(id).await.unwrap().unwrap()).unwrap();
    legacy_runtime["options"]["max_connections"] = json!(1);
    assert_eq!(
        retained_runtime, legacy_runtime,
        "gosh-dl resumes from persisted options, so its cap must migrate too",
    );
}
