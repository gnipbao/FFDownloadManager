# FFDownload macOS 客户端

桌面版复用现有 Rust `DownloadService`、gosh-dl、ytdown 和 bbdown-core。Tauri 2 使用系统 WKWebView 显示同一套界面，通过有权限限制的 IPC 直接调用 Rust，应用不启动 Web 服务器、不占用 17890 端口。

## 使用

本地构建产物为 `dist/FFDownload.app`，另提供带 Applications 快捷方式的 DMG 和 ZIP。首版目标为 Apple Silicon，最低系统版本设为 macOS 13；实际运行验证环境见下方验证记录。双击应用即可启动，安装到应用程序文件夹后同样可以运行。

- 下载文件：`~/Downloads/FFDownload`。
- 任务记录：`~/Library/Application Support/com.ffdownload.manager`。
- 关闭窗口会隐藏窗口并继续下载，点击 Dock 图标或“窗口 → 显示下载管理”恢复。
- 按 ⌘Q / 退出应用时，Rust 服务暂停任务并等待保存完成，再退出进程。再次启动后主动选择继续。菜单退出走异步保存；macOS Dock / 系统退出还在最终退出回调中同步等待保存，覆盖 AppKit 绕过常规退出请求的路径。
- 新建下载可使用 ⌘N、⌘K；菜单提供暂停全部、打开下载文件夹和标准编辑快捷键。
- 重复打开应用会聚焦已有窗口，防止多个实例同时操作任务。

Web 版的工作目录和任务记录保持独立。桌面版不自动导入旧记录，避免两个宿主同时写入同一下载会话。

## 合并组件

应用自带从 FFmpeg 8.0.3 官方源码构建的约 1.7 MB 合并工具，仅启用本地 file / pipe 协议以及必要的封装、解封装、解析功能。Rust 负责网络下载，FFmpeg 只做 stream copy，不重新编码。它只依赖系统库，不依赖 `/opt/homebrew`。LGPL 许可证、完整对应源码和重建脚本随应用放在 `Contents/Resources/third-party`。

FFmpeg 在桌面宿主启动时通过 `DownloadService::open_with_ffmpeg` 注入，不修改进程环境变量、不依赖用户 PATH。CLI / Web 版原有的外部 FFmpeg 查找方式仍可用。

## 构建

需要 macOS、Xcode Command Line Tools、Node/npm，以及项目 Rust 工具链。

```sh
sh scripts/build-macos.sh
```

构建工具与 Cargo 缓存保存在项目 `.tools`，FFmpeg 源码版本与 SHA-256 固定。`Cargo.lock` 和 `package-lock.json` 固定桌面依赖。`scripts/build-ffmpeg-macos.sh` 编译合并工具，Tauri 嵌入前端资源并生成 `.app`，随后做 ad-hoc 签名并生成 DMG / ZIP。

当前使用临时本地签名，尚无 Developer ID 签名或 Apple 公证。可用于本机试用；向其他 Mac 分发时可能出现 Gatekeeper 提示，公开发布前需要完成正式签名与公证。Intel / Universal 构建尚未验证。

隔离测试可在启动可执行文件前指定绝对路径 `FFDM_DESKTOP_DOWNLOAD_DIR` 与 `FFDM_DESKTOP_DATA_DIR`。默认启动不使用这些覆盖值。无需访问用户已有下载和任务记录即可测试退出恢复。

## 验证

原生 IPC 回归覆盖添加真实下载、任务快照、路径限制、退出保存、重启续传及最终字节一致，并验证非 main 窗口不能调用任务接口。应用只给本地 main 窗口开放列出的命令；导航限制在内置界面。

2026-09-21，在 Apple Silicon / macOS 26.6.2 上实际打开打包后的 `.app`：原生 IPC 连接成功，⌘N 可新建任务；YouTube 测试视频解析出 25 种格式，1440p 下载与内置 FFmpeg 合并完成，产物为 2560×1440 AV1 + AAC、68.057687 秒、31,894,494 字节，并通过完整解码检查。

运行记录与检查结果保存在 `.bench-work/desktop/`。根项目 21 项测试、原生 IPC 测试及工作区 Clippy 检查通过；应用签名校验、DMG 校验通过。关闭窗口后下载继续运行，重新打开可见持续增长的进度。⌘Q 退出后任务保存为 paused，保留 29,917,184 / 33,554,432 字节（89.2%）；重新打开后界面仍显示该进度，继续后完整下载 32 MiB，SHA-256 与测试源 ETag 一致。Dock 退出的自动化工具访问超时，该入口的保存兜底已实现但未单独完成 GUI 验证。

参考：[Tauri IPC](https://v2.tauri.app/develop/calling-rust/)、[macOS bundle](https://v2.tauri.app/distribute/macos-application-bundle/)、[单实例插件](https://v2.tauri.app/plugin/single-instance/)、[macOS 退出回调问题](https://github.com/tauri-apps/tauri/issues/13778)、[FFmpeg 官方源码](https://ffmpeg.org/releases/ffmpeg-8.0.3.tar.xz)。
