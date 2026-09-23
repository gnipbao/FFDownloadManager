//! Media jobs use the existing Rust downloader for each track, then publish once.
use crate::{
    download::{
        download_with_headers, sha256_file, DownloadControl, DownloadReport, DownloadRequest,
        DownloadSnapshot,
    },
    media::{safe_error, Assembly, MediaPlan},
};
use anyhow::{bail, ensure, Context, Result};
use fs2::FileExt;
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::sync::watch;

pub async fn run(
    plan: MediaPlan,
    output: PathBuf,
    connections: usize,
    expected_sha256: Option<String>,
    mut control: DownloadControl,
    ffmpeg: Option<PathBuf>,
) -> Result<DownloadReport> {
    plan.validate()?;
    let started = Instant::now();
    ensure!(!output.try_exists()?, "目标文件已存在，不能覆盖");
    let parent = output.parent().context("缺少下载目录")?;
    let name = output.file_name().context("缺少文件名")?.to_string_lossy();
    let work = parent.join(format!("{name}.ffdm-state")).join("media");
    std::fs::create_dir_all(&work)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o700))?;
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(work.join("session.lock"))?;
    lock.try_lock_exclusive()
        .context("另一个进程正在处理此视频")?;
    let identity = work.join("plan.json");
    if identity.exists() {
        let saved: MediaPlan = serde_json::from_reader(File::open(&identity)?)?;
        ensure!(
            serde_json::to_value(saved)? == serde_json::to_value(&plan)?,
            "媒体来源已变化，请重新解析并创建任务；原进度已保留"
        );
    } else {
        let mut file = tempfile::NamedTempFile::new_in(&work)?;
        serde_json::to_writer(&mut file, &plan)?;
        file.as_file().sync_all()?;
        file.persist_noclobber(&identity).map_err(|e| e.error)?;
    }
    let report = |outcome: &str, bytes: u64, hash: Option<String>| DownloadReport {
        engine: "ytdown + bbdown-core / gosh-dl".into(),
        outcome: outcome.into(),
        output: output.clone(),
        connections,
        bytes,
        starting_progress_bytes: 0,
        transfer_seconds: started.elapsed().as_secs_f64(),
        finalize_seconds: 0.0,
        total_seconds: started.elapsed().as_secs_f64(),
        average_mbps: None,
        sha256: hash,
    };
    let mut paths = Vec::new();
    let mut completed = 0;
    for (index, stream) in plan.streams.iter().enumerate() {
        if *control.pause.borrow() {
            return Ok(report(
                "paused",
                control.progress.borrow().completed_bytes.max(completed),
                None,
            ));
        }
        let track = work.join(format!("track-{index}.bin"));
        if let Ok(metadata) = std::fs::symlink_metadata(&track) {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "媒体轨道文件已被替换"
            );
            ensure!(
                stream.size.is_none_or(|size| size == metadata.len()),
                "已下载的媒体轨道大小发生变化"
            );
            completed += metadata.len();
            paths.push(track);
            continue;
        }
        let resume = work
            .join(format!("track-{index}.bin.ffdm-state/job.json"))
            .exists();
        let (progress_tx, mut progress_rx) = watch::channel(DownloadSnapshot::default());
        let request = DownloadRequest {
            url: Some(stream.url.clone()),
            output: track.clone(),
            connections,
            resume,
            expected_sha256: None,
            pause_after: None,
            timeout: Duration::from_secs(7 * 86400),
            interactive: false,
        };
        let future = download_with_headers(
            request,
            Some(DownloadControl {
                pause: control.pause.clone(),
                progress: progress_tx,
            }),
            stream.headers.clone(),
        );
        tokio::pin!(future);
        let mut progress_open = true;
        let result = loop {
            tokio::select! {
                result = &mut future => break result,
                changed = progress_rx.changed(), if progress_open => {
                    if changed.is_ok() {
                        let mut progress = progress_rx.borrow().clone();
                        if progress.phase == "verifying" { progress.phase = "downloading".into(); }
                        progress.completed_bytes += completed;
                        progress.total_bytes = plan.total_size();
                        progress.eta_seconds = progress.total_bytes.filter(|_| progress.speed_bytes > 0)
                            .map(|total| total.saturating_sub(progress.completed_bytes) / progress.speed_bytes);
                        control.progress.send_replace(progress);
                    } else {
                        progress_open = false;
                    }
                }
            }
        };
        let result = result.map_err(|e| {
            let message = safe_error(&format!("{e:#}"));
            if message.contains("403") {
                anyhow::anyhow!("平台拒绝下载这个媒体格式（HTTP 403）。请重新解析视频并选择可用格式；重试同一地址不一定有效。已下载进度已保留。{message}")
            } else if message.contains("410") {
                anyhow::anyhow!("媒体地址已失效，请重新解析视频创建任务。已下载进度已保留。{message}")
            } else { anyhow::anyhow!(message) }
        })?;
        completed += result.bytes;
        if result.outcome == "paused" {
            return Ok(report("paused", completed, None));
        }
        paths.push(track);
    }
    if *control.pause.borrow() {
        return Ok(report("paused", completed, None));
    }
    control.progress.send_replace(DownloadSnapshot {
        phase: "merging".into(),
        total_bytes: Some(completed),
        completed_bytes: completed,
        ..Default::default()
    });
    let merged;
    let source = if plan.assembly == Assembly::Direct {
        paths[0].clone()
    } else {
        let binary =
            ffmpeg.context("需要 FFmpeg 合并音视频；安装并重启服务后可以继续，媒体轨道已保留")?;
        merged = tempfile::Builder::new()
            .prefix("assembled-")
            .suffix(&format!(".{}", plan.extension))
            .tempfile_in(&work)?;
        let mut cmd = tokio::process::Command::new(binary);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]);
        if plan.assembly == Assembly::Merge {
            for path in &paths {
                cmd.args(["-protocol_whitelist", "file,pipe", "-i"])
                    .arg(path);
            }
            cmd.args(["-map", "0:v:0", "-map", "1:a:0", "-c", "copy"]);
        } else {
            let list = work.join("concat.txt");
            let text: String = (0..paths.len())
                .map(|i| format!("file 'track-{i}.bin'\n"))
                .collect();
            std::fs::write(&list, text)?;
            cmd.args([
                "-protocol_whitelist",
                "file,pipe",
                "-f",
                "concat",
                "-safe",
                "1",
                "-i",
            ])
            .arg(&list)
            .args(["-c", "copy"]);
        }
        // Inputs are local, completed files; ffmpeg never fetches platform URLs.
        cmd.arg(merged.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(work.join("ffmpeg.log"))?)
            .kill_on_drop(true);
        let mut child = cmd.spawn().context("无法启动 FFmpeg，已下载轨道已保留")?;
        let status = tokio::select! {
            status = child.wait() => Some(status?),
            _ = wait_for_pause(&mut control.pause) => None,
            _ = tokio::time::sleep(Duration::from_secs(3600)) => {
                let _ = child.kill().await;
                bail!("音视频合并超时，已下载轨道已保留");
            }
        };
        if let Some(status) = status {
            ensure!(
                status.success(),
                "音视频合并失败（{}），已下载轨道已保留，可重试。详情：{}",
                status,
                work.join("ffmpeg.log").display()
            );
        } else {
            let _ = child.kill().await;
            return Ok(report("paused", completed, None));
        }
        merged.path().to_owned()
    };
    control.progress.send_replace(DownloadSnapshot {
        phase: "verifying".into(),
        total_bytes: Some(completed),
        completed_bytes: completed,
        ..Default::default()
    });
    let dest = output.clone();
    let (size, hash) =
        tokio::task::spawn_blocking(move || publish(&source, &dest, expected_sha256.as_deref()))
            .await??;
    // Remove only the completed intermediate tracks owned by this media job.
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
    Ok(report("completed", size, Some(hash)))
}

async fn wait_for_pause(pause: &mut watch::Receiver<bool>) {
    loop {
        if *pause.borrow_and_update() || pause.changed().await.is_err() {
            return;
        }
    }
}

fn publish(source: &Path, output: &Path, expected: Option<&str>) -> Result<(u64, String)> {
    let file = OpenOptions::new().write(true).open(source)?;
    file.sync_all()?;
    let size = file.metadata()?.len();
    ensure!(size > 0, "合并结果为空，原始轨道已保留");
    let hash = sha256_file(source)?;
    if let Some(expected) = expected {
        ensure!(
            hash.eq_ignore_ascii_case(expected),
            "SHA-256 校验不匹配，原始轨道已保留"
        );
    }
    std::fs::hard_link(source, output)
        .context("无法保存最终视频，目标文件可能已存在；原始轨道已保留")?;
    Ok((size, hash))
}
