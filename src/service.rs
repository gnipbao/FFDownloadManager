//! Persistent task management, independent of HTTP and desktop transports.
use crate::download::{
    download_with_authorization, download_with_control, DownloadControl, DownloadRequest,
    DownloadSnapshot, FileAuthorization,
};
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
    #[serde(default)]
    pub last_run_seconds: Option<f64>,
    #[serde(default)]
    pub average_mbps: Option<f64>,
    pub demo: bool,
    #[serde(default)]
    pub authorization_required: bool,
    /// Display-only: computed from the current process's private authorization map.
    #[serde(default, skip_deserializing)]
    pub authorization_needed: bool,
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
    pub cookie: Option<String>,
    /// Explicit file mode bypasses only automatic media-page classification.
    #[serde(default)]
    pub direct: bool,
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

fn source_connections(url: &str, media: Option<&MediaPlan>, requested: usize) -> usize {
    let cap = |limit: usize, source: &str| {
        ffdm_resolver::source_policy::limits(source)
            .map_or(limit, |policy| limit.min(policy.max_connections))
    };
    media.map_or_else(
        || cap(requested, url),
        |plan| {
            plan.streams
                .iter()
                .fold(requested, |limit, stream| cap(limit, &stream.url))
        },
    )
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
    authorizations: HashMap<String, FileAuthorization>,
    stopping: bool,
}

enum TaskSource {
    File(Option<FileAuthorization>),
    Media(Box<MediaPlan>),
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
            // Older builds relabeled completed files removed outside the app as
            // failed downloads. Restore their historical status; snapshot hides
            // missing outputs without discarding genuine unfinished tasks.
            if task.state == "error"
                && task.completed_at.is_some()
                && task.error.as_deref() == Some("下载文件已被移动或删除")
            {
                task.state = "completed".into();
                task.error = None;
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
                authorizations: HashMap::new(),
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
        let mut tasks: Vec<_> = inner
            .tasks
            .iter()
            .filter(|task| {
                if task.state != "completed" {
                    return true;
                }
                // Keep historical metadata so a temporarily unavailable disk or
                // a restored file can reappear. Only the displayed list changes.
                match std::fs::symlink_metadata(self.directory.join(&task.filename)) {
                    Ok(metadata) => metadata.is_file() && !metadata.file_type().is_symlink(),
                    Err(error) => error.kind() != std::io::ErrorKind::NotFound,
                }
            })
            .cloned()
            .collect();
        for task in &mut tasks {
            task.authorization_needed = task.authorization_required
                && task.state != "completed"
                && !inner.authorizations.contains_key(&task.id);
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
        let authorization = FileAuthorization::new(url.as_str(), request.cookie.as_deref())?;
        ensure!(
            request.direct || crate::media::platform(url.as_str()).is_none(),
            "这是音视频页面链接，请先解析并选择下载格式"
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
        let info = match &authorization {
            Some(auth) => {
                self.filenames
                    .resolve_authorized(url.as_str(), auth)
                    .await?
            }
            None => self.suggest_filename(url.as_str()).await?,
        };
        let filename = complete_name(request.filename.as_deref(), &info);
        validate_filename(&filename)?;
        self.enqueue(
            url.into(),
            filename,
            source_connections(&request.url, None, request.connections),
            expected_sha256,
            TaskSource::File(authorization),
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
            "文件后缀必须与所选下载格式一致：.{}",
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
        let connections = source_connections(&plan.source_url, Some(&plan), request.connections);
        self.enqueue(
            plan.source_url.clone(),
            filename,
            connections,
            hash,
            TaskSource::Media(Box::new(plan)),
            false,
        )
    }

    /// Captured plans are created by the Rust proxy, never accepted from the browser.
    pub fn create_captured(self: &Arc<Self>, plan: MediaPlan, connections: usize) -> Result<Task> {
        ensure!((1..=16).contains(&connections), "连接数须为 1–16");
        plan.validate()?;
        let filename = plan.filename();
        validate_filename(&filename)?;
        self.enqueue(
            plan.source_url.clone(),
            filename,
            connections,
            None,
            TaskSource::Media(Box::new(plan)),
            false,
        )
    }

    fn enqueue(
        self: &Arc<Self>,
        url: String,
        filename: String,
        connections: usize,
        expected_sha256: Option<String>,
        source: TaskSource,
        demo: bool,
    ) -> Result<Task> {
        let mut inner = self.inner.lock().unwrap();
        ensure!(!inner.stopping, "服务正在退出，请稍后重试");
        let filename = self.available_filename(&inner, &filename)?;
        let (media, authorization) = match source {
            TaskSource::File(authorization) => (None, authorization),
            TaskSource::Media(plan) => (Some(*plan), None),
        };
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
            last_run_seconds: None,
            average_mbps: None,
            demo,
            authorization_required: authorization.is_some(),
            authorization_needed: false,
            media,
        };
        inner.tasks.push(task.clone());
        if let Err(e) = self.persist(&inner) {
            inner.tasks.pop();
            return Err(e);
        }
        if let Some(authorization) = authorization {
            inner.authorizations.insert(task.id.clone(), authorization);
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
            let authorization = inner.authorizations.get(&task.id).cloned();
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
                    tokio::spawn(async move {
                        match authorization {
                            Some(auth) => {
                                download_with_authorization(request, Some(control), auth).await
                            }
                            None => download_with_control(request, Some(control)).await,
                        }
                    })
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
                    task.last_run_seconds = Some(report.total_seconds);
                    task.average_mbps = report.average_mbps;
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
                    task.error = Some(download_error(&error, &task.url));
                }
            }
        }
        if inner
            .tasks
            .iter()
            .any(|task| task.id == id && task.state == "completed")
        {
            inner.authorizations.remove(id);
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
        self.resume_with_cookie(id, None)
    }

    pub fn resume_with_cookie(self: &Arc<Self>, id: &str, cookie: Option<&str>) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        ensure!(!inner.stopping, "服务正在退出");
        ensure!(!inner.active.contains_key(id), "任务仍在运行或保存中");
        let index = inner
            .tasks
            .iter()
            .position(|t| t.id == id)
            .context("任务不存在")?;
        let task = &inner.tasks[index];
        ensure!(
            matches!(task.state.as_str(), "paused" | "error"),
            "当前状态无法继续下载"
        );
        ensure!(
            !self.directory.join(&task.filename).exists(),
            "目标文件已存在，请在 Finder 中检查"
        );
        let authorization = FileAuthorization::new(&task.url, cookie)?;
        ensure!(
            authorization.is_none() || task.media.is_none(),
            "音视频任务请重新解析，不能使用文件下载授权"
        );
        ensure!(
            !task.authorization_required
                || authorization.is_some()
                || inner.authorizations.contains_key(id),
            "此任务需要网站登录授权，请重新填写 Cookie 后继续下载"
        );
        let previous = task.clone();
        let task = &mut inner.tasks[index];
        task.connections = source_connections(&task.url, task.media.as_ref(), task.connections);
        task.authorization_required |= authorization.is_some();
        task.state = "queued".into();
        task.error = None;
        if let Err(error) = self.persist(&inner) {
            inner.tasks[index] = previous;
            return Err(error);
        }
        if let Some(authorization) = authorization {
            inner.authorizations.insert(id.to_owned(), authorization);
        }
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
        inner.authorizations.remove(id);
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
        let mut inner = self.inner.lock().unwrap();
        inner.authorizations.clear();
        self.persist(&inner)
    }
}

fn download_error(error: &anyhow::Error, url: &str) -> String {
    let message = crate::media::safe_error(&format!("{error:#}").replace(url, "[下载地址]"));
    let status =
        error
            .chain()
            .find_map(|cause| match cause.downcast_ref::<gosh_dl::EngineError>() {
                Some(gosh_dl::EngineError::Network {
                    kind: gosh_dl::NetworkErrorKind::HttpStatus(code),
                    ..
                }) => Some(*code),
                _ => cause
                    .downcast_ref::<reqwest::Error>()
                    .and_then(|error| error.status())
                    .map(|status| status.as_u16()),
            });
    // Worker events and restored task errors contain the engine's serialized
    // message rather than its typed error. Match only HTTP status descriptions,
    // never arbitrary numbers from a URL, filename or byte count.
    static HTTP_STATUS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)(?:HTTP (?:error|status):?\s*|HEAD request returned:\s*)(\d{3})\b")
            .unwrap()
    });
    let status = status.or_else(|| {
        HTTP_STATUS
            .captures(&message)?
            .get(1)?
            .as_str()
            .parse::<u16>()
            .ok()
    });
    match status {
        Some(401) => "下载源要求登录授权（HTTP 401）。请点击“授权后重试”，填写已登录浏览器中该下载请求的 Cookie。".into(),
        Some(403) => "下载源拒绝访问（HTTP 403）。请确认访问权限，或在原网站重新获取有效下载链接。".into(),
        Some(code @ (404 | 410)) => format!("文件不存在或链接已失效（HTTP {code}），请重新获取下载链接。"),
        _ => message,
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

#[cfg(test)]
mod authorization_tests {
    use super::*;

    fn paused_task() -> Task {
        serde_json::from_value(serde_json::json!({
            "id":"authorized-file", "filename":"private.zip", "url":"https://files.example/private.zip",
            "connections":4, "state":"error", "created_at":1, "completed_at":null,
            "total_bytes":null, "completed_bytes":0, "speed_bytes":0, "active_connections":0,
            "eta_seconds":null, "sha256":null, "expected_sha256":null,
            "error":"HTTP 401", "demo":false
        })).unwrap()
    }

    fn keep_queue_idle(service: &DownloadService) {
        // Occupy worker slots without spawning network requests. This isolates
        // task persistence and authorization lifetime from source availability.
        let mut inner = service.inner.lock().unwrap();
        for index in 0..MAX_ACTIVE {
            let (pause, _) = watch::channel(false);
            let (_, progress) = watch::channel(DownloadSnapshot::default());
            inner
                .active
                .insert(format!("reserved-{index}"), Active { pause, progress });
        }
    }

    #[test]
    fn authorization_stays_private_reuses_in_process_and_is_required_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let downloads = dir.path().join("downloads");
        let service = DownloadService::open_with_ffmpeg(&downloads, &state, None).unwrap();
        service.inner.lock().unwrap().tasks.push(paused_task());
        keep_queue_idle(&service);
        let cookie = "auth_token=service-secret-sentinel";
        service
            .resume_with_cookie("authorized-file", Some(cookie))
            .unwrap();
        let snapshot = service.snapshot();
        assert!(snapshot.tasks[0].authorization_required);
        assert!(!snapshot.tasks[0].authorization_needed);
        assert!(!serde_json::to_string(&snapshot)
            .unwrap()
            .contains("service-secret-sentinel"));
        assert!(!std::fs::read_to_string(state.join("tasks.json"))
            .unwrap()
            .contains("service-secret-sentinel"));

        service.pause("authorized-file").unwrap();
        service.resume("authorized-file").unwrap();
        assert!(!service.snapshot().tasks[0].authorization_needed);
        service.pause("authorized-file").unwrap();
        drop(service);

        let restored = DownloadService::open_with_ffmpeg(&downloads, &state, None).unwrap();
        assert!(restored.snapshot().tasks[0].authorization_needed);
        assert!(restored
            .resume("authorized-file")
            .unwrap_err()
            .to_string()
            .contains("Cookie"));
        assert_eq!(restored.snapshot().tasks[0].state, "paused");
        keep_queue_idle(&restored);
        restored
            .resume_with_cookie("authorized-file", Some("auth_token=replacement-secret"))
            .unwrap();
        assert!(!restored.snapshot().tasks[0].authorization_needed);
        restored.pause("authorized-file").unwrap();
        restored.remove("authorized-file").unwrap();
        assert!(restored.inner.lock().unwrap().authorizations.is_empty());
        assert!(!std::fs::read_to_string(state.join("tasks.json"))
            .unwrap()
            .contains("replacement-secret"));
    }
}
