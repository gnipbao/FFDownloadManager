FFDownload third-party components

FFmpeg 8.0.3 (https://ffmpeg.org/)
Used as a separate local-file-only process for stream-copy remuxing.
License: GNU LGPL 2.1 or later. See FFmpeg-LICENSE.txt.
Complete unmodified corresponding source is included in ffmpeg-8.0.3.tar.xz.
The platform build scripts and configurations are included in
build-ffmpeg-macos.sh and build-ffmpeg-windows.sh.
No GPL or nonfree components, external codec libraries, encoders, or network
protocols are enabled. The FFmpeg executable may be replaced by the user with
a compatible build. On macOS, re-sign the app after replacing executable resources.

The desktop window uses Tauri (MIT / Apache-2.0), system WKWebView on macOS,
and system WebView2 on Windows.
The Rust download engine uses gosh-dl, Tokio, reqwest, and rustls;
video extraction uses ytdown and bbdown-core.
Dependency versions are recorded in the project's Cargo.lock.
