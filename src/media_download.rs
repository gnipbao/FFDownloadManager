//! Media jobs use the existing Rust downloader for each track, then publish once.
use crate::{
    download::{
        download_with_headers, sha256_file, DownloadControl, DownloadReport, DownloadRequest,
        DownloadSnapshot,
    },
    media::{safe_error, Assembly, MediaPlan, StreamTransform},
};
use anyhow::{bail, ensure, Context, Result};
use fs2::FileExt;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
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
    ensure!(
        plan.streams
            .iter()
            .all(|stream| stream.decrypt_key.is_none() || stream.transform.is_none()),
        "媒体轨道不能同时使用两种解码方式"
    );
    let connections = plan.streams.iter().fold(connections, |limit, stream| {
        ffdm_resolver::source_policy::limits(&stream.url)
            .map_or(limit, |policy| limit.min(policy.max_connections))
    });
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
    let resumed_session = identity.exists();
    if resumed_session {
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
    let engine = if plan.assembly == Assembly::Hls {
        "ffdm-resolver / Rust HLS / gosh-dl"
    } else {
        "ffdm-resolver / gosh-dl"
    };
    let report = |outcome: &str, bytes: u64, hash: Option<String>| DownloadReport {
        engine: engine.into(),
        outcome: outcome.into(),
        output: output.clone(),
        connections,
        bytes,
        starting_progress_bytes: 0,
        transfer_seconds: started.elapsed().as_secs_f64(),
        finalize_seconds: 0.0,
        total_seconds: started.elapsed().as_secs_f64(),
        average_mbps: (outcome == "completed" && !resumed_session)
            .then(|| bytes as f64 * 8.0 / started.elapsed().as_secs_f64() / 1_000_000.0),
        sha256: hash,
    };
    let mut paths = Vec::new();
    let mut completed = 0;
    let mut hls_container = None;
    // The downloader has already hashed this exact, newly completed track.
    // A direct file needs no second full-file read before publication.
    let mut verified_direct_hash = None;
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
            if plan.assembly == Assembly::Hls {
                validate_hls_track(&track, &work, index, &mut hls_container)?;
            }
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
        if plan.assembly == Assembly::Hls {
            validate_hls_track(&track, &work, index, &mut hls_container)?;
        }
        if plan.assembly == Assembly::Direct
            && stream.decrypt_key.is_none()
            && stream.transform.is_none()
        {
            verified_direct_hash = result.sha256;
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
    // Completed encrypted tracks remain untouched until the final file is
    // published. A failed decode can therefore resume without downloading again.
    let original_tracks = paths.clone();
    let mut decoded_directories = Vec::new();
    for (stream, track) in plan.streams.iter().zip(paths.iter_mut()) {
        match &stream.transform {
            Some(StreamTransform::SunoAudio { content_id }) => {
                let directory = tempfile::Builder::new()
                    .prefix("suno-decoded-")
                    .tempdir_in(&work)?;
                let decoded = directory.path().join("audio.m4a");
                ffdm_resolver::suno_audio::decode_file(content_id, track.as_path(), &decoded)
                    .await
                    .context("Suno 音频处理失败，已下载轨道已保留，可重试")?;
                if *control.pause.borrow() {
                    return Ok(report("paused", completed, None));
                }
                *track = decoded;
                decoded_directories.push(directory);
            }
            None => {}
        }
    }
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
        // FFmpeg is a console program even when the desktop host is a GUI app.
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]);
        let hls_input;
        if plan.assembly == Assembly::Hls {
            let inputs = paths.clone();
            let directory = work.clone();
            let pause = control.pause.clone();
            hls_input =
                tokio::task::spawn_blocking(move || join_hls_segments(&inputs, &directory, &pause))
                    .await??;
            let Some(input) = hls_input.as_ref() else {
                return Ok(report("paused", completed, None));
            };
            // MPEG-TS and fMP4 are both byte streams. fMP4 requires its init
            // followed by moof/mdat fragments, not independent concat inputs.
            let container = hls_input_demuxer(input.path())?;
            cmd.args(["-protocol_whitelist", "file,pipe", "-f", container, "-i"])
                .arg(input.path());
            if plan.extension == "m4a" {
                // Public audio HLS has no video stream. Keep its audio without
                // encoding, using the same completed, validated local pieces.
                cmd.args([
                    "-map",
                    "0:a:0",
                    "-vn",
                    "-c",
                    "copy",
                    "-movflags",
                    "+faststart",
                ]);
            } else {
                cmd.args([
                    "-map",
                    "0:v:0",
                    "-map",
                    "0:a:0?",
                    "-c",
                    "copy",
                    "-movflags",
                    "+faststart",
                ]);
            }
        } else if plan.assembly == Assembly::Merge {
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
    let decrypt_key = plan.streams.first().and_then(|stream| stream.decrypt_key);
    let (size, hash) = tokio::task::spawn_blocking(move || {
        if let Some(key) = decrypt_key {
            let mut decrypted = tempfile::Builder::new()
                .prefix("decrypted-")
                .suffix(".mp4")
                .tempfile_in(&work)?;
            crate::wechat_crypto::decrypt_copy(&source, decrypted.as_file_mut(), key)?;
            let path = decrypted.into_temp_path();
            publish(path.as_ref(), &dest, expected_sha256.as_deref(), None)
        } else {
            publish(
                &source,
                &dest,
                expected_sha256.as_deref(),
                verified_direct_hash,
            )
        }
    })
    .await??;
    // Remove only the completed intermediate tracks owned by this media job.
    for path in original_tracks {
        let _ = std::fs::remove_file(path);
    }
    Ok(report("completed", size, Some(hash)))
}

fn join_hls_segments(
    inputs: &[PathBuf],
    directory: &Path,
    pause: &watch::Receiver<bool>,
) -> Result<Option<tempfile::NamedTempFile>> {
    let mut joined = tempfile::Builder::new()
        .prefix("hls-input-")
        .suffix(".media")
        .tempfile_in(directory)?;
    let mut buffer = vec![0_u8; 256 * 1024];
    for path in inputs {
        let metadata = std::fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "HLS 分段文件已被替换"
        );
        let mut source = File::open(path)?;
        loop {
            if *pause.borrow() {
                return Ok(None);
            }
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            joined.write_all(&buffer[..read])?;
        }
    }
    joined.as_file().sync_all()?;
    Ok(Some(joined))
}

fn hls_input_demuxer(path: &Path) -> Result<&'static str> {
    match hls_piece_format(path)? {
        HlsPieceFormat::MpegTs => Ok("mpegts"),
        HlsPieceFormat::Mp4Init => Ok("mov"),
        HlsPieceFormat::Aac => Ok("aac"),
        HlsPieceFormat::Mp4Fragment => bail!("HLS fMP4 缺少初始化段"),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HlsPieceFormat {
    MpegTs,
    Mp4Init,
    Mp4Fragment,
    Aac,
}

fn hls_piece_format(path: &Path) -> Result<HlsPieceFormat> {
    let mut header = [0_u8; 384];
    let mut file = File::open(path)?;
    let file_size = file.metadata()?.len();
    let length = file.read(&mut header)?;
    // Force the expected binary demuxer instead of allowing arbitrary file
    // autodetection (for example a concat script in an untrusted segment).
    if length >= 189 && file_size.is_multiple_of(188) && header[0] == 0x47 && header[188] == 0x47 {
        return Ok(HlsPieceFormat::MpegTs);
    }
    if length >= 8
        && matches!(
            &header[4..8],
            b"ftyp" | b"moov" | b"styp" | b"sidx" | b"moof"
        )
    {
        let first_box_size = u32::from_be_bytes(header[..4].try_into().unwrap()) as u64;
        ensure!(
            first_box_size == 0 || (first_box_size >= 8 && first_box_size <= file_size),
            "HLS fMP4 分段首 box 不完整"
        );
        if matches!(&header[4..8], b"ftyp" | b"moov") {
            return Ok(HlsPieceFormat::Mp4Init);
        }
        if matches!(&header[4..8], b"styp" | b"sidx" | b"moof") {
            return Ok(HlsPieceFormat::Mp4Fragment);
        }
    }
    if length >= 7 && header[0] == 0xff && header[1] & 0xf6 == 0xf0 {
        return Ok(HlsPieceFormat::Aac);
    }
    bail!("HLS 分段格式不是 MPEG-TS、fMP4 或 AAC；原始分段已保留")
}

fn validate_hls_track(
    path: &Path,
    directory: &Path,
    index: usize,
    container: &mut Option<HlsPieceFormat>,
) -> Result<()> {
    let validated: Result<()> = (|| {
        let kind = hls_piece_format(path)?;
        if let Some(previous) = container {
            ensure!(
                if *previous == HlsPieceFormat::Mp4Init {
                    kind == HlsPieceFormat::Mp4Fragment
                } else {
                    kind == *previous
                },
                "HLS 分段容器改变或出现重复初始化段"
            );
        } else {
            ensure!(kind != HlsPieceFormat::Mp4Fragment, "HLS fMP4 缺少初始化段");
            *container = Some(kind);
        }
        Ok(())
    })();
    if let Err(error) = validated {
        let name = path
            .file_name()
            .context("缺少 HLS 分段文件名")?
            .to_string_lossy();
        let rejected = tempfile::Builder::new()
            .prefix("rejected-hls-")
            .tempdir_in(directory)?
            .keep();
        let state = directory.join(format!("{name}.ffdm-state"));
        // Move the checkpoint first. If interrupted, the invalid canonical
        // track still exists and will be checked again before reuse; it cannot
        // leave a completed checkpoint with a missing canonical output.
        if state.try_exists()? {
            std::fs::rename(&state, rejected.join("state"))
                .context("无法隔离无效 HLS 分段的下载记录，原始记录已保留")?;
        }
        std::fs::rename(path, rejected.join("segment.bin"))
            .context("无法隔离无效 HLS 分段，原始分段已保留")?;
        bail!(
            "HLS 第 {} 个分段无效：{}。有效分段进度已保留，无效响应已隔离，重试将重新下载此段",
            index + 1,
            safe_error(&format!("{error:#}"))
        );
    }
    Ok(())
}

async fn wait_for_pause(pause: &mut watch::Receiver<bool>) {
    loop {
        if *pause.borrow_and_update() || pause.changed().await.is_err() {
            return;
        }
    }
}

fn publish(
    source: &Path,
    output: &Path,
    expected: Option<&str>,
    verified_hash: Option<String>,
) -> Result<(u64, String)> {
    let file = OpenOptions::new().write(true).open(source)?;
    file.sync_all()?;
    let size = file.metadata()?.len();
    ensure!(size > 0, "合并结果为空，原始轨道已保留");
    let hash = match verified_hash {
        Some(hash) => hash,
        None => sha256_file(source)?,
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hls_binary_inputs_cannot_be_autodetected_as_local_file_scripts() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.media");
        std::fs::write(&input, "ffconcat version 1.0\nfile '/private/data'\n").unwrap();
        assert!(hls_input_demuxer(&input).is_err());
        let mut ts = vec![0_u8; 376];
        ts[0] = 0x47;
        ts[188] = 0x47;
        std::fs::write(&input, ts).unwrap();
        assert_eq!(hls_input_demuxer(&input).unwrap(), "mpegts");
        std::fs::write(&input, b"\0\0\0\x0cftypiso6").unwrap();
        assert_eq!(hls_input_demuxer(&input).unwrap(), "mov");
        std::fs::write(&input, [0xff, 0xf1, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!(hls_input_demuxer(&input).unwrap(), "aac");
    }
}
