# FFDownload Windows 客户端

Windows 版复用 macOS 版的 Rust 下载服务、视频解析器和 Tauri 界面。首版目标是 Windows 10/11 x64，通过 GitHub Actions 的 Windows 构建器生成 NSIS 安装包。

## 使用

- 默认下载到 `%USERPROFILE%\Downloads\FFDownload`，任务记录位于系统应用数据目录。
- 关闭主窗口时最小化到任务栏，下载继续运行；通过“窗口 → 显示下载管理”恢复。
- Ctrl+N 新建下载，Ctrl+K 聚焦链接输入框，Ctrl+Q 保存进度并退出。
- 文件名会过滤 Windows 不允许的字符与保留设备名。
- 安装包包含本地 FFmpeg 合并组件，用于高清视频的音视频无转码合并。

当前安装包尚未使用发布者证书签名，Windows 可能提示未知发布者。

## 构建

安装 [Tauri 2 的 Windows 前置条件](https://v2.tauri.app/start/prerequisites/)、Rust 1.98.1、Node.js 22、npm 与 MSYS2 UCRT64。先在 MSYS2 UCRT64 shell 运行：

```sh
sh scripts/build-ffmpeg-windows.sh
```

再在 PowerShell 中运行：

```powershell
powershell -ExecutionPolicy Bypass -File scripts/build-windows.ps1
```

安装包在 `target/release/bundle/nsis/`。构建脚本固定 FFmpeg 8.0.3 源码与 SHA-256，随安装包提供对应源码、LGPL 许可证和构建脚本。Windows 原生构建与打包结果以 [GitHub Actions](https://github.com/gnipbao/FFDownloadManager/actions) 为准。
