//! Persistent task management, independent of HTTP and desktop transports.
use crate::download::{download_with_control, DownloadControl, DownloadRequest, DownloadSnapshot};
use crate::filename::{
    complete_name, is_windows_reserved_name, parse_url, FilenameInfo, FilenameResolver,
};
use crate::media::{MediaPlan, MediaPreview, MediaResolver};
use anyhow::{bail, ensure, Context, Result};
use fs2::FileExt;
use gosh_dl::DownloadId;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;

const MAX_ACTIVE: usize = 2;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub filename: String,
    pub url: String,
    pub connections: usize,
    pub state: String,
    pub created_at: u64,
    pub completed_at: Option<u64>,
    pub total_bytes: Option<u64>,
    pub completed_bytes: u64,
    pub speed_bytes: u64,
    pub active_connections: u32,
    pub eta_seconds: Option<u64>,
    pub sha256: Option<String>,
    pub expected_sha256: Option<String>,
    pub error: Option<String>,
    pub demo: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<MediaPlan>,
}

impl Task {
    fn for_display(mut self) -> Self {
        if let Some(media) = &mut self.media {
            media.for_display();
        }
        self
    }
}

#[derive(Deserialize)]
pub struct NewTask {
    pub url: String,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default = "default_connections")]
    pub connections: usize,
    #[serde(default)]
    pub sha256: Option<String>,
}

#[derive(Deserialize)]
pub struct NewMediaTask {
    pub preview_id: String,
    pub format_id: String,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default = "default_connections")]
    pub connections: usize,
    #[serde(default)]
    pub sha256: Option<String>,
}

fn default_connections() -> usize {
    4
}

#[derive(Serialize)]
pub struct Snapshot {
    pub tasks: Vec<Task>,
    pub download_directory: PathBuf,
    pub max_active: usize,
    pub can_reveal: bool,
}

struct Active {
    pause: watch::Sender<bool>,
    progress: watch::Receiver<DownloadSnapshot>,
}

struct Inner {
    tasks: Vec<Task>,
    active: HashMap<String, Active>,
    stopping: bool,
}

pub struct DownloadService {
    filenames: FilenameResolver,
    media: MediaResolver,
    directory: PathBuf,
    state_file: PathBuf,
    inner: Mutex<Inner>,
    _lock: File,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn validate_filename(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 180
            && !name.starts_with('.')
            && !name
                .chars()
                .any(|c| c.is_control() || "/\\:*?\"<>|".contains(c))
            && !is_windows_reserved_name(name)
            && !name.ends_with(['.', ' ']),
        "文件名不能以点开头，不能使用系统保留名称或字符，且须短于 180 字节"
    );
    Ok(())
}

fn job_path(directory: &Path, filename: &str) -> PathBuf {
    directory.join(format!("{filename}.ffdm-state/job.json"))
}

fn apply_progress(task: &mut Task, progress: &DownloadSnapshot) {
    if !progress.phase.is_empty() {
        if task.state != "pausing" {
            task.state.clone_from(&progress.phase);
        }
        task.total_bytes = progress.total_bytes.or(task.total_bytes);
        task.completed_bytes = progress.completed_bytes;
        task.speed_bytes = progress.speed_bytes;
        task.active_connections = progress.connections;
        task.eta_seconds = progress.eta_seconds;
    }
}

impl DownloadService {
    pub fn open(directory: &Path, state_directory: &Path) -> Result<Arc<Self>> {
        Self::open_with_ffmpeg(directory, state_directory, crate::media::find_ffmpeg())
    }

    pub fn open_with_ffmpeg(
        directory: &Path,
        state_directory: &Path,
        ffmpeg: Option<PathBuf>,
    ) -> Result<Arc<Self>> {
        std::fs::create_dir_all(directory)?;
        std::fs::create_dir_all(state_directory)?;
        let directory = directory.canonicalize()?;
        let state_directory = state_directory.canonicalize()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&state_directory, std::fs::Permissions::from_mode(0o700))?;
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(state_directory.join("manager.lock"))?;
        lock.try_lock_exclusive()
            .context("另一个 FFDownload 服务正在使用这个任务目录")?;
        let state_file = state_directory.join("tasks.json");
        let mut tasks: Vec<Task> = if state_file.exists() {
            let saved: SavedTasks = serde_json::from_reader(File::open(&state_file)?)
                .context("无法读取任务记录；原文件已保留")?;
            ensure!(saved.schema == 1, "不支持的任务记录版本");
            ensure!(
                saved.directory == directory,
                "任务目录对应另一个下载位置，请使用新的 state-dir"
            );
            saved.tasks
        } else {
            Vec::new()
        };
        for task in &mut tasks {
            validate_filename(&task.filename)?;
            task.speed_bytes = 0;
            task.active_connections = 0;
            task.eta_seconds = None;
            if !matches!(task.state.as_str(), "completed" | "error" | "paused") {
                // Restart is intentionally quiet; the user chooses when to resume.
                task.state = "paused".into();
            }
            if task.state == "completed" && !directory.join(&task.filename).is_file() {
                task.state = "error".into();
                task.error = Some("下载文件已被移动或删除".into());
            }
        }
        let service = Arc::new(Self {
            filenames: FilenameResolver::new()?,
            media: MediaResolver::with_ffmpeg(ffmpeg)?,
            directory,
            state_file,
            inner: Mutex::new(Inner {
                tasks,
                active: HashMap::new(),
                stopping: false,
            }),
            _lock: lock,
        });
        service.persist(&service.inner.lock().unwrap())?;
        Ok(service)
    }

    fn persist(&self, inner: &Inner) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(self.state_file.parent().unwrap())?;
        serde_json::to_writer_pretty(
            &mut file,
            &SavedTasks {
                schema: 1,
                directory: self.directory.clone(),
                tasks: inner.tasks.clone(),
            },
        )?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        file.persist(&self.state_file).map_err(|e| e.error)?;
        Ok(())
    }

    pub fn snapshot(&self) -> Snapshot {
        let inner = self.inner.lock().unwrap();
        let mut tasks = inner.tasks.clone();
        for task in &mut tasks {
            if let Some(active) = inner.active.get(&task.id) {
                apply_progress(task, &active.progress.borrow());
            }
        }
        tasks.reverse();
        Snapshot {
            tasks: tasks.into_iter().map(Task::for_display).collect(),
            download_directory: self.directory.clone(),
            max_active: MAX_ACTIVE,
            can_reveal: cfg!(any(target_os = "macos", target_os = "windows")),
        }
    }

    pub async fn suggest_filename(&self, url: &str) -> Result<FilenameInfo> {
        self.filenames.resolve(url).await
    }

    pub async fn create(self: &Arc<Self>, request: NewTask, demo: bool) -> Result<Task> {
        let url = parse_url(&request.url)?;
        ensure!(
            crate::media::platform(url.as_str()).is_none(),
            "这是视频页面链接，请先解析视频并选择清晰度"
        );
        ensure!((1..=16).contains(&request.connections), "连接数须为 1–16");
        let expected_sha256 = request.sha256.filter(|s| !s.is_empty());
        if let Some(hash) = &expected_sha256 {
            ensure!(
                hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                "SHA-256 必须为 64 位十六进制摘要"
            );
        }
        if let Some(name) = request
            .filename
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            validate_filename(name)?;
        }
        let info = self.suggest_filename(url.as_str()).await?;
        let filename = complete_name(request.filename.as_deref(), &info);
        validate_filename(&filename)?;
        self.enqueue(
            url.into(),
            filename,
            request.connections,
            expected_sha256,
            None,
            demo,
        )
    }

    pub async fn resolve_media(&self, url: &str) -> Result<MediaPreview> {
        self.media.resolve(url).await
    }

    pub async fn resolve_media_with_cookie(
        &self,
        url: &str,
        platform_cookie: Option<&str>,
    ) -> Result<MediaPreview> {
        self.media.resolve_with_cookie(url, platform_cookie).await
    }

    pub async fn create_media(self: &Arc<Self>, request: NewMediaTask) -> Result<Task> {
        ensure!((1..=16).contains(&request.connections), "连接数须为 1–16");
        let plan = self.media.select(&request.preview_id, &request.format_id)?;
        let custom = request
            .filename
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(name) = custom {
            validate_filename(name)?;
        }
        let info = FilenameInfo {
            filename: plan.filename(),
            extension: Some(plan.extension.clone()),
            source: "media",
        };
        let filename = complete_name(custom, &info);
        validate_filename(&filename)?;
        ensure!(
            crate::filename::extension(&filename).as_deref() == Some(plan.extension.as_str()),
            "文件后缀必须与所选视频格式一致：.{}",
            plan.extension
        );
        let hash = request.sha256.filter(|s| !s.is_empty());
        if let Some(hash) = &hash {
            ensure!(
                hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                "SHA-256 必须为 64 位十六进制摘要"
            );
        }
        self.media.validate_download(&plan).await?;
        self.enqueue(
            plan.source_url.clone(),
            filename,
            request.connections,
            hash,
            Some(plan),
            false,
        )
    }

    fn enqueue(
        self: &Arc<Self>,
        url: String,
        filename: String,
        connections: usize,
        expected_sha256: Option<String>,
        media: Option<MediaPlan>,
        demo: bool,
    ) -> Result<Task> {
        let mut inner = self.inner.lock().unwrap();
        ensure!(!inner.stopping, "服务正在退出，请稍后重试");
        let filename = self.available_filename(&inner, &filename)?;
        let task = Task {
            id: DownloadId::new().to_string(),
            filename,
            url,
            connections,
            state: "queued".into(),
            created_at: now_ms(),
            completed_at: None,
            total_bytes: media.as_ref().and_then(MediaPlan::total_size),
            completed_bytes: 0,
            speed_bytes: 0,
            active_connections: 0,
            eta_seconds: None,
            sha256: None,
            expected_sha256,
            error: None,
            demo,
            media,
        };
        inner.tasks.push(task.clone());
        if let Err(e) = self.persist(&inner) {
            inner.tasks.pop();
            return Err(e);
        }
        drop(inner);
        self.schedule();
        Ok(task.for_display())
    }

    fn available_filename(&self, inner: &Inner, name: &str) -> Result<String> {
        let path = Path::new(name);
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
        let extension = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| format!(".{s}"))
            .unwrap_or_default();
        for n in 0..10000 {
            let candidate = if n == 0 {
                name.to_owned()
            } else {
                format!("{stem} ({n}){extension}")
            };
            if !inner.tasks.iter().any(|t| t.filename == candidate)
                && std::fs::symlink_metadata(self.directory.join(&candidate)).is_err()
                && !self
                    .directory
                    .join(format!("{candidate}.ffdm-state"))
                    .exists()
                && !self
                    .directory
                    .join(format!("{candidate}.ffdm.part"))
                    .exists()
            {
                validate_filename(&candidate)?;
                return Ok(candidate);
            }
        }
        bail!("同名文件过多，请更换文件名")
    }

    fn schedule(self: &Arc<Self>) {
        let mut inner = self.inner.lock().unwrap();
        while !inner.stopping && inner.active.len() < MAX_ACTIVE {
            let Some(index) = inner.tasks.iter().position(|t| t.state == "queued") else {
                break;
            };
            let task = &mut inner.tasks[index];
            task.state = "connecting".into();
            task.error = None;
            let task = task.clone();
            let (pause, pause_rx) = watch::channel(false);
            let (progress_tx, progress) = watch::channel(DownloadSnapshot::default());
            inner
                .active
                .insert(task.id.clone(), Active { pause, progress });
            let service = self.clone();
            tokio::spawn(async move {
                let output = service.directory.join(&task.filename);
                let control = DownloadControl {
                    pause: pause_rx,
                    progress: progress_tx,
                };
                let handle = if let Some(plan) = task.media.clone() {
                    tokio::spawn(crate::media_download::run(
                        plan,
                        output,
                        task.connections,
                        task.expected_sha256.clone(),
                        control,
                        service.media.ffmpeg.clone(),
                    ))
                } else {
                    let resume = job_path(&service.directory, &task.filename).exists();
                    let request = DownloadRequest {
                        url: Some(task.url.clone()),
                        output,
                        connections: task.connections,
                        resume,
                        expected_sha256: task.expected_sha256,
                        pause_after: None,
                        timeout: Duration::from_secs(7 * 86400),
                        interactive: false,
                    };
                    // Catch task panics so a slot cannot remain permanently occupied.
                    tokio::spawn(download_with_control(request, Some(control)))
                };
                let result = handle.await.map_err(anyhow::Error::from).and_then(|r| r);
                service.finish(&task.id, result);
                service.schedule();
            });
        }
    }

    fn finish(&self, id: &str, result: Result<crate::download::DownloadReport>) {
        let mut inner = self.inner.lock().unwrap();
        let progress = inner.active.remove(id).map(|a| a.progress.borrow().clone());
        if let Some(task) = inner.tasks.iter_mut().find(|t| t.id == id) {
            if let Some(progress) = progress {
                apply_progress(task, &progress);
            }
            task.speed_bytes = 0;
            task.active_connections = 0;
            task.eta_seconds = None;
            match result {
                Ok(report) => {
                    task.state = report.outcome;
                    task.completed_bytes = report.bytes;
                    if task.state == "completed" {
                        task.total_bytes = Some(report.bytes);
                        task.completed_at = Some(now_ms());
                    }
                    task.sha256 = report.sha256;
                }
                Err(error) => {
                    task.state = "error".into();
                    // Signed query strings must not leak into UI errors or logs.
                    let message = crate::media::safe_error(
                        &format!("{error:#}").replace(&task.url, "[下载地址]"),
                    );
                    task.error = Some(message);
                }
            }
        }
        if let Err(error) = self.persist(&inner) {
            eprintln!("无法保存任务列表：{error}");
            if let Some(task) = inner.tasks.iter_mut().find(|t| t.id == id) {
                task.error = Some("无法保存任务列表，请检查磁盘空间和目录权限".into());
            }
        }
    }

    pub fn pause(&self, id: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let index = inner
            .tasks
            .iter()
            .position(|t| t.id == id)
            .context("任务不存在")?;
        if let Some(active) = inner.active.get(id) {
            active.pause.send_replace(true);
            let progress = active.progress.borrow().clone();
            apply_progress(&mut inner.tasks[index], &progress);
            inner.tasks[index].state = "pausing".into();
        } else if inner.tasks[index].state == "queued" {
            inner.tasks[index].state = "paused".into();
        }
        self.persist(&inner)
    }

    pub fn pause_all(&self) -> Result<()> {
        let ids: Vec<_> = self
            .snapshot()
            .tasks
            .iter()
            .filter(|t| is_active(&t.state))
            .map(|t| t.id.clone())
            .collect();
        for id in ids {
            self.pause(&id)?;
        }
        Ok(())
    }

    pub fn resume(self: &Arc<Self>, id: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        ensure!(!inner.stopping, "服务正在退出");
        ensure!(!inner.active.contains_key(id), "任务仍在运行或保存中");
        let task = inner
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .context("任务不存在")?;
        ensure!(
            matches!(task.state.as_str(), "paused" | "error"),
            "当前状态无法继续下载"
        );
        ensure!(
            !self.directory.join(&task.filename).exists(),
            "目标文件已存在，请在 Finder 中检查"
        );
        task.state = "queued".into();
        task.error = None;
        self.persist(&inner)?;
        drop(inner);
        self.schedule();
        Ok(())
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        ensure!(!inner.active.contains_key(id), "请先暂停任务，再移除记录");
        let index = inner
            .tasks
            .iter()
            .position(|t| t.id == id)
            .context("任务不存在")?;
        ensure!(
            inner.tasks[index].state != "queued",
            "请先暂停任务，再移除记录"
        );
        let removed = inner.tasks.remove(index);
        if let Err(e) = self.persist(&inner) {
            inner.tasks.insert(index, removed);
            return Err(e);
        }
        Ok(())
    }

    pub fn output_path(&self, id: &str) -> Result<PathBuf> {
        let inner = self.inner.lock().unwrap();
        let task = inner
            .tasks
            .iter()
            .find(|t| t.id == id)
            .context("任务不存在")?;
        ensure!(task.state == "completed", "文件尚未下载完成");
        let path = self.directory.join(&task.filename);
        let metadata = std::fs::symlink_metadata(&path).context("文件已被移动或删除")?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "文件已被移动或替换"
        );
        Ok(path)
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub async fn shutdown(&self) -> Result<()> {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.stopping = true;
        }
        self.pause_all()?;
        loop {
            if self.inner.lock().unwrap().active.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        self.persist(&self.inner.lock().unwrap())
    }
}

pub fn is_active(state: &str) -> bool {
    matches!(
        state,
        "queued" | "connecting" | "downloading" | "pausing" | "verifying" | "merging"
    )
}

#[derive(Serialize, Deserialize)]
struct SavedTasks {
    schema: u32,
    directory: PathBuf,
    tasks: Vec<Task>,
}
