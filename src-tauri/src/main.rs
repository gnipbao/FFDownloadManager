//! Desktop host: the same Rust download service lives inside the app process.
use ffdownload::{
    filename::FilenameInfo,
    media::{safe_error, MediaPreview},
    service::{DownloadService, NewMediaTask, NewTask, Snapshot, Task},
};
use serde::Serialize;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tauri::{
    menu::{AboutMetadata, Menu, MenuItem, PredefinedMenuItem, Submenu},
    AppHandle, Emitter, Manager, RunEvent, State, WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_dialog::DialogExt;

type Reply<T> = Result<T, String>;

struct DesktopState {
    service: Arc<DownloadService>,
    capture: Arc<ffdownload::capture::CaptureService>,
    #[cfg(debug_assertions)]
    benchmark: Arc<ffdownload::benchmark::WebBenchmark>,
    ffmpeg: PathBuf,
    quitting: AtomicBool,
    saved: AtomicBool,
}

impl DesktopState {
    async fn shutdown(&self) -> anyhow::Result<()> {
        let proxy = self.capture.stop().await;
        let tasks = self.service.shutdown().await;
        proxy.and(tasks)
    }
}

#[tauri::command]
fn desktop_capabilities() -> ffdownload::capabilities::Capabilities {
    ffdownload::capabilities::Capabilities::for_host(true)
}

fn require_capture() -> Reply<()> {
    if desktop_capabilities().browser_capture {
        Ok(())
    } else {
        Err("资源捕获与应用抓包暂未在发布版开放".into())
    }
}

#[tauri::command]
async fn desktop_capture(
    state: State<'_, DesktopState>,
) -> Reply<ffdownload::capture::CaptureSnapshot> {
    require_capture()?;
    Ok(state.capture.snapshot().await)
}

#[tauri::command]
async fn desktop_capture_browser(
    state: State<'_, DesktopState>,
    url: String,
    visible: Option<bool>,
) -> Reply<()> {
    require_capture()?;
    state
        .capture
        .open_browser_mode(&url, visible.unwrap_or(false))
        .await
        .map_err(error)
}

#[tauri::command]
async fn desktop_capture_stop(state: State<'_, DesktopState>) -> Reply<()> {
    // Cleanup must remain available when upgrading from a capture test build.
    state.capture.stop().await.map_err(error)
}

#[tauri::command]
fn desktop_capture_clear(state: State<'_, DesktopState>) -> Reply<()> {
    require_capture()?;
    state.capture.clear();
    Ok(())
}

#[tauri::command]
fn desktop_capture_download(
    state: State<'_, DesktopState>,
    id: String,
    connections: usize,
    decode_key: Option<String>,
) -> Reply<Task> {
    require_capture()?;
    let plan = state
        .capture
        .select(&id, decode_key.as_deref())
        .map_err(error)?;
    state
        .service
        .create_captured(plan, connections)
        .map_err(error)
}

#[tauri::command]
async fn desktop_capture_setup(
    state: State<'_, DesktopState>,
    service: Option<String>,
) -> Reply<serde_json::Value> {
    require_capture()?;
    state
        .capture
        .application_setup(service.as_deref())
        .await
        .map_err(error)
}

#[tauri::command]
async fn desktop_capture_application_start(
    state: State<'_, DesktopState>,
    service: String,
) -> Reply<()> {
    require_capture()?;
    state
        .capture
        .start_application(&service)
        .await
        .map_err(error)
}

#[tauri::command]
async fn desktop_capture_restore(state: State<'_, DesktopState>) -> Reply<()> {
    state.capture.recover_system_proxy().await.map_err(error)
}

#[tauri::command]
async fn desktop_capture_certificate_open(state: State<'_, DesktopState>) -> Reply<()> {
    require_capture()?;
    state.capture.open_certificate().await.map_err(error)
}

#[tauri::command]
fn desktop_benchmark(state: State<'_, DesktopState>) -> Reply<serde_json::Value> {
    #[cfg(debug_assertions)]
    {
        Ok(state.benchmark.snapshot())
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = state;
        Err("内核测速仅在开发模式可用".into())
    }
}

#[tauri::command]
fn desktop_benchmark_start(state: State<'_, DesktopState>) -> Reply<()> {
    #[cfg(debug_assertions)]
    {
        state.benchmark.start().map_err(error)
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = state;
        Err("内核测速仅在开发模式可用".into())
    }
}

fn error(e: anyhow::Error) -> String {
    safe_error(&format!("{e:#}"))
}

#[tauri::command]
fn desktop_snapshot(state: State<'_, DesktopState>) -> Snapshot {
    state.service.snapshot()
}

#[tauri::command]
async fn desktop_create(state: State<'_, DesktopState>, task: NewTask) -> Reply<Task> {
    state.service.create(task, false).await.map_err(error)
}

#[tauri::command]
async fn desktop_filename(state: State<'_, DesktopState>, url: String) -> Reply<FilenameInfo> {
    state.service.suggest_filename(&url).await.map_err(error)
}

#[tauri::command]
async fn desktop_resolve_media(
    state: State<'_, DesktopState>,
    url: String,
    cookie: Option<String>,
) -> Reply<MediaPreview> {
    state
        .service
        .resolve_media_with_cookie(&url, cookie.as_deref())
        .await
        .map_err(error)
}

#[tauri::command]
async fn desktop_create_media(state: State<'_, DesktopState>, task: NewMediaTask) -> Reply<Task> {
    state.service.create_media(task).await.map_err(error)
}

#[tauri::command]
fn desktop_pause(state: State<'_, DesktopState>, id: String) -> Reply<()> {
    state.service.pause(&id).map_err(error)
}

#[tauri::command]
async fn desktop_resume(state: State<'_, DesktopState>, id: String) -> Reply<()> {
    state.service.resume(&id).map_err(error)
}

#[tauri::command]
fn desktop_remove(state: State<'_, DesktopState>, id: String) -> Reply<()> {
    state.service.remove(&id).map_err(error)
}

#[tauri::command]
fn desktop_pause_all(state: State<'_, DesktopState>) -> Reply<()> {
    state.service.pause_all().map_err(error)
}

async fn reveal_path(path: PathBuf, select: bool) -> Reply<()> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = tokio::process::Command::new("/usr/bin/open");
        if select {
            command.arg("-R");
        }
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = tokio::process::Command::new("explorer.exe");
        if select {
            command.arg("/select,");
        }
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = {
        let _ = select;
        tokio::process::Command::new("xdg-open")
    };
    let status = command
        .arg(path)
        .status()
        .await
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err("无法打开文件管理器".into())
    }
}

#[tauri::command]
async fn desktop_reveal(state: State<'_, DesktopState>, id: String) -> Reply<()> {
    reveal_path(state.service.output_path(&id).map_err(error)?, true).await
}

#[tauri::command]
async fn desktop_open_folder(state: State<'_, DesktopState>) -> Reply<()> {
    reveal_path(state.service.directory().to_owned(), false).await
}

#[tauri::command]
fn desktop_copy_link(app: AppHandle, state: State<'_, DesktopState>, id: String) -> Reply<()> {
    let task = state
        .service
        .snapshot()
        .tasks
        .into_iter()
        .find(|t| t.id == id)
        .ok_or("任务不存在")?;
    app.clipboard()
        .write_text(task.url)
        .map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct DesktopInfo {
    version: &'static str,
    platform: &'static str,
    ffmpeg_available: bool,
}

#[tauri::command]
fn desktop_info(state: State<'_, DesktopState>) -> DesktopInfo {
    DesktopInfo {
        version: env!("CARGO_PKG_VERSION"),
        platform: std::env::consts::OS,
        ffmpeg_available: state.ffmpeg.is_file(),
    }
}

fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let about = PredefinedMenuItem::about(
        app,
        Some("关于 FFDownload"),
        Some(AboutMetadata {
            name: Some("FFDownload".into()),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            comments: Some("本地下载管理器\n关闭窗口后继续下载；退出应用时保存进度。".into()),
            ..Default::default()
        }),
    )?;
    let application = Submenu::with_items(
        app,
        "FFDownload",
        true,
        &[
            &about,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::hide(app, Some("隐藏 FFDownload"))?,
            &PredefinedMenuItem::hide_others(app, Some("隐藏其他"))?,
            &PredefinedMenuItem::show_all(app, Some("显示全部"))?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "quit", "退出 FFDownload", true, Some("CmdOrCtrl+Q"))?,
        ],
    )?;
    let file = Submenu::with_items(
        app,
        "文件",
        true,
        &[
            &MenuItem::with_id(app, "new-download", "新建下载", true, Some("CmdOrCtrl+N"))?,
            &MenuItem::with_id(
                app,
                "open-folder",
                "打开下载文件夹",
                true,
                Some("CmdOrCtrl+Shift+O"),
            )?,
            &MenuItem::with_id(
                app,
                "pause-all",
                "暂停所有下载",
                true,
                Some("CmdOrCtrl+Shift+P"),
            )?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::close_window(
                app,
                Some(if cfg!(target_os = "windows") {
                    "最小化窗口（继续下载）"
                } else {
                    "关闭窗口（继续下载）"
                }),
            )?,
        ],
    )?;
    let edit = Submenu::with_items(
        app,
        "编辑",
        true,
        &[
            &PredefinedMenuItem::undo(app, Some("撤销"))?,
            &PredefinedMenuItem::redo(app, Some("重做"))?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::cut(app, Some("剪切"))?,
            &PredefinedMenuItem::copy(app, Some("拷贝"))?,
            &PredefinedMenuItem::paste(app, Some("粘贴"))?,
            &PredefinedMenuItem::select_all(app, Some("全选"))?,
        ],
    )?;
    let window = Submenu::with_items(
        app,
        "窗口",
        true,
        &[
            &PredefinedMenuItem::minimize(app, Some("最小化"))?,
            &PredefinedMenuItem::maximize(app, Some("缩放"))?,
            &MenuItem::with_id(app, "show-main", "显示下载管理", true, None::<&str>)?,
        ],
    )?;
    Menu::with_items(app, &[&application, &file, &edit, &window])
}

fn begin_shutdown(app: &AppHandle) {
    let state = app.state::<DesktopState>();
    if state.quitting.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    let _ = app.emit("desktop-status", "正在保存下载进度，完成后退出…");
    tauri::async_runtime::spawn(async move {
        match app.state::<DesktopState>().shutdown().await {
            Ok(()) => {
                app.state::<DesktopState>()
                    .saved
                    .store(true, Ordering::SeqCst);
                app.exit(0);
            }
            Err(e) => {
                app.state::<DesktopState>()
                    .quitting
                    .store(false, Ordering::SeqCst);
                show_main(&app);
                app.dialog()
                    .message(format!(
                        "恢复代理或保存进度失败，应用暂未退出。请处理以下问题后再次退出。\n{}",
                        error(e)
                    ))
                    .title("FFDownload")
                    .kind(tauri_plugin_dialog::MessageDialogKind::Error)
                    .show(|_| {});
            }
        }
    });
}

fn save_before_system_exit(app: &AppHandle) {
    let state = app.state::<DesktopState>();
    if state.saved.load(Ordering::SeqCst) {
        return;
    }
    // AppKit's Dock Quit / system termination skips ExitRequested in Tao.
    // Exit still runs before Tauri tears down the app. Wait here for the Rust
    // workers (which do not need the UI thread) instead of spawning work that
    // would be killed as soon as this callback returns.
    match tauri::async_runtime::block_on(state.shutdown()) {
        Ok(()) => state.saved.store(true, Ordering::SeqCst),
        Err(e) => eprintln!("退出时恢复代理或保存进度失败：{}", error(e)),
    }
}

fn configured_path(key: &str, default: PathBuf) -> anyhow::Result<PathBuf> {
    let path = std::env::var_os(key).map(PathBuf::from).unwrap_or(default);
    anyhow::ensure!(path.is_absolute(), "{key} 必须是绝对路径");
    Ok(path)
}

fn main() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| show_main(app)))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .setup(|app| {
            let directory = configured_path("FFDM_DESKTOP_DOWNLOAD_DIR", app.path().download_dir()?.join("FFDownload"))?;
            let state_directory = configured_path("FFDM_DESKTOP_DATA_DIR", app.path().app_data_dir()?)?;
            let ffmpeg_name = if cfg!(target_os = "windows") {
                "ffmpeg.exe"
            } else {
                "ffmpeg"
            };
            let bundled = app.path().resource_dir()?.join(ffmpeg_name);
            // Debug builds can use the project copy; shipped apps must be self-contained.
            let ffmpeg = if bundled.is_file() { bundled } else if cfg!(debug_assertions) {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources").join(ffmpeg_name)
            } else { bundled };
            if !ffmpeg.is_file() {
                return Err("应用缺少音视频合并组件，请重新安装完整的 FFDownload".into());
            }
            let service = tauri::async_runtime::block_on(async {
                DownloadService::open_with_ffmpeg(&directory, &state_directory, Some(ffmpeg.clone()))
            })?;
            let capture = ffdownload::capture::CaptureService::new(state_directory.join("capture-browser"), "tauri://localhost".into());
            if let Err(error) = tauri::async_runtime::block_on(capture.recover_system_proxy()) {
                eprintln!("上次代理配置仍需恢复，请在 macOS 网络设置中检查：{error}");
            }
            app.manage(DesktopState {
                service, capture, ffmpeg,
                #[cfg(debug_assertions)]
                benchmark: ffdownload::benchmark::WebBenchmark::new(state_directory.join("speed-test.json")),
                quitting: AtomicBool::new(false), saved: AtomicBool::new(false),
            });
            let config = &app.config().app.windows[0];
            WebviewWindowBuilder::from_config(app, config)?
                .on_navigation(|url| {
                    (url.scheme() == "tauri" && url.host_str() == Some("localhost"))
                        || (url.scheme() == "http" && url.host_str() == Some("tauri.localhost"))
                })
                .build()?;
            app.set_menu(menu(app.handle())?)?;
            #[cfg(unix)]
            {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    if let Ok(mut signal) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                        tokio::select! { _ = signal.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
                        handle.exit(0);
                    }
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            desktop_snapshot, desktop_create, desktop_filename, desktop_resolve_media,
            desktop_create_media, desktop_pause, desktop_resume, desktop_remove,
            desktop_reveal, desktop_pause_all, desktop_open_folder, desktop_copy_link, desktop_info,
            desktop_capabilities, desktop_capture, desktop_capture_browser, desktop_capture_stop, desktop_capture_clear, desktop_capture_download, desktop_capture_setup, desktop_capture_application_start, desktop_capture_restore, desktop_capture_certificate_open, desktop_benchmark, desktop_benchmark_start,
        ])
        .on_menu_event(|app, event| match event.id().as_ref() {
            "quit" => begin_shutdown(app),
            "show-main" => show_main(app),
            "new-download" => { show_main(app); let _ = app.emit("desktop-new-download", ()); }
            "pause-all" => {
                if let Err(e) = app.state::<DesktopState>().service.pause_all() {
                    let _ = app.emit("desktop-status", error(e));
                }
            }
            "open-folder" => {
                let path = app.state::<DesktopState>().service.directory().to_owned();
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = reveal_path(path, false).await { let _ = app.emit("desktop-status", e); }
                });
            }
            _ => {}
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                #[cfg(target_os = "macos")]
                let _ = window.hide();
                #[cfg(target_os = "windows")]
                let _ = window.minimize();
            }
        })
        .build(tauri::generate_context!())
        .expect("无法启动 FFDownload");
    app.run(|app, event| match event {
        RunEvent::Exit => save_before_system_exit(app),
        RunEvent::ExitRequested { api, .. } => {
            if !app.state::<DesktopState>().saved.load(Ordering::SeqCst) {
                api.prevent_exit();
                begin_shutdown(app);
            }
        }
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => show_main(app),
        _ => {}
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffdownload::test_server::{fixture_data, ServerConfig, TestServer};
    use serde_json::{json, Value};
    use std::time::Duration;
    use tauri::test::{get_ipc_response, mock_builder, MockRuntime, INVOKE_KEY};

    fn invoke(
        window: &tauri::WebviewWindow<MockRuntime>,
        command: &str,
        args: Value,
    ) -> Result<Value, Value> {
        get_ipc_response(
            window,
            tauri::webview::InvokeRequest {
                cmd: command.into(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                url: if cfg!(target_os = "windows") {
                    "http://tauri.localhost"
                } else {
                    "tauri://localhost"
                }
                .parse()
                .unwrap(),
                body: tauri::ipc::InvokeBody::Json(args),
                headers: Default::default(),
                invoke_key: INVOKE_KEY.into(),
            },
        )
        .map(|body| body.deserialize::<Value>().unwrap())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_ipc_download_checkpoints_on_exit_and_rejects_untrusted_windows() {
        let directory = tempfile::tempdir().unwrap();
        let data = fixture_data(3 * 1024 * 1024);
        let mut config = ServerConfig::new(data.clone());
        config.per_response_mbps = Some(8.0);
        let server = TestServer::start(config).await.unwrap();
        let download = directory.path().join("downloads");
        let store = directory.path().join("state");
        let service = DownloadService::open_with_ffmpeg(&download, &store, None).unwrap();
        let app = mock_builder()
            .manage(DesktopState {
                service: service.clone(),
                capture: ffdownload::capture::CaptureService::new(
                    store.join("capture-browser"),
                    "tauri://localhost".into(),
                ),
                #[cfg(debug_assertions)]
                benchmark: ffdownload::benchmark::WebBenchmark::new(store.join("speed-test.json")),
                ffmpeg: "missing".into(),
                quitting: AtomicBool::new(false),
                saved: AtomicBool::new(false),
            })
            .invoke_handler(tauri::generate_handler![
                desktop_snapshot,
                desktop_create,
                desktop_resume,
                desktop_remove,
                desktop_capabilities,
                desktop_capture,
                desktop_capture_clear,
                desktop_capture_stop,
                desktop_capture_browser,
                desktop_capture_download,
                desktop_capture_setup,
                desktop_capture_application_start,
                desktop_capture_certificate_open,
                desktop_benchmark
            ])
            .build(tauri::generate_context!())
            .unwrap();
        let window = WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();
        let untrusted = WebviewWindowBuilder::new(&app, "untrusted", Default::default())
            .build()
            .unwrap();
        assert!(
            invoke(&untrusted, "desktop_snapshot", json!({})).is_err(),
            "only the bundled main window may access tasks"
        );
        let capabilities = invoke(&window, "desktop_capabilities", json!({})).unwrap();
        assert_eq!(capabilities["dev_mode"], cfg!(debug_assertions));
        assert_eq!(capabilities["browser_capture"], cfg!(debug_assertions));
        assert_eq!(
            capabilities["application_capture"],
            cfg!(all(debug_assertions, target_os = "macos"))
        );
        assert!(invoke(&untrusted, "desktop_capture", json!({})).is_err());
        if cfg!(debug_assertions) {
            assert!(
                !invoke(&window, "desktop_capture", json!({})).unwrap()["running"]
                    .as_bool()
                    .unwrap()
            );
            app.state::<DesktopState>().capture.start().await.unwrap();
            assert!(
                invoke(&window, "desktop_capture", json!({})).unwrap()["running"]
                    .as_bool()
                    .unwrap()
            );
            invoke(&window, "desktop_capture_clear", json!({})).unwrap();
            invoke(&window, "desktop_capture_stop", json!({})).unwrap();
            assert!(
                !invoke(&window, "desktop_capture", json!({})).unwrap()["running"]
                    .as_bool()
                    .unwrap()
            );
        } else {
            for (command, args) in [
                ("desktop_capture", json!({})),
                ("desktop_capture_clear", json!({})),
                (
                    "desktop_capture_browser",
                    json!({"url":"http://127.0.0.1/"}),
                ),
                (
                    "desktop_capture_download",
                    json!({"id":"test","connections":4}),
                ),
                ("desktop_capture_setup", json!({})),
                (
                    "desktop_capture_application_start",
                    json!({"service":"Wi-Fi"}),
                ),
                ("desktop_capture_certificate_open", json!({})),
            ] {
                let result = invoke(&window, command, args).unwrap_err();
                assert!(
                    result.as_str().unwrap().contains("暂未在发布版开放"),
                    "{command}: {result}"
                );
            }
            assert!(!app.state::<DesktopState>().capture.snapshot().await.running);
            assert!(
                !store.join("capture-browser").exists(),
                "disabled commands must not prepare a CA or browser profile"
            );
            invoke(&window, "desktop_capture_stop", json!({})).unwrap();
        }
        assert_eq!(
            invoke(&window, "desktop_benchmark", json!({})).is_ok(),
            cfg!(debug_assertions)
        );
        assert!(invoke(
            &window,
            "desktop_create",
            json!({"task":{"url":server.url,"filename":"../escape.bin"}})
        )
        .is_err());
        let task = invoke(
            &window,
            "desktop_create",
            json!({"task":{"url":server.url,"filename":"native.bin","connections":1}}),
        )
        .unwrap();
        let id = task["id"].as_str().unwrap().to_owned();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let snapshot = invoke(&window, "desktop_snapshot", json!({})).unwrap();
                if snapshot["tasks"][0]["completed_bytes"].as_u64().unwrap() > 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        service.shutdown().await.unwrap();
        let paused = invoke(&window, "desktop_snapshot", json!({})).unwrap();
        assert_eq!(paused["tasks"][0]["state"], "paused");
        assert!(!download.join("native.bin").exists());
        drop(window);
        drop(untrusted);
        drop(app);
        drop(service);
        // MockRuntime retains managed state until the test process exits. Load
        // its persisted snapshot in a fresh store, without bypassing file locks
        // or unsafely removing borrowed Tauri state. Real process exit is also
        // exercised in the packaged-app smoke test.
        let restarted_store = directory.path().join("restarted-state");
        std::fs::create_dir(&restarted_store).unwrap();
        std::fs::copy(store.join("tasks.json"), restarted_store.join("tasks.json")).unwrap();
        let restored =
            DownloadService::open_with_ffmpeg(&download, &restarted_store, None).unwrap();
        assert_eq!(restored.snapshot().tasks[0].state, "paused");
        assert!(restored.snapshot().tasks[0].completed_bytes > 0);
        restored.resume(&id).unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while restored.snapshot().tasks[0].state != "completed" {
                assert_ne!(restored.snapshot().tasks[0].state, "error");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(std::fs::read(download.join("native.bin")).unwrap(), data);
        restored.shutdown().await.unwrap();
        server.stop().await.unwrap();
    }
}
