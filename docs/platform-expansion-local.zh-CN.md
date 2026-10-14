# Rust 平台扩展：本地验证版本

本轮优先增加快手、微博、AcFun；代码和本地工作台已更新；本次仓库更新包含这些改动，正式安装包以 Release 为准。新增解析器采用 Rust HTTP 请求、JSON/页面数据解析与平台分流，运行时不调用 yt-dlp 或 Python。下载继续复用 Rust 内核；FFmpeg 只处理下载完成的本地文件，以 `copy` 封装，不请求网络、不转码。

## 本轮范围

| 平台 | 输入与格式 | 本轮本机验证 |
| --- | --- | --- |
| 快手 | 单作品、短分享链接；移动分享页的 AVC / HEVC MP4 | 720p AVC 完整下载，通过音视频与时长校验 |
| 微博 | 单条状态、移动详情、TV 视频页；公开 MP4 | 720p 完整下载，通过音视频与时长校验 |
| AcFun | `/v/ac数字`，支持 `_分P`；H.264 / HEVC HLS | 360p H.264 完整下载；失败合并任务复用 35 个已有片段后成功 |
| Streamable | 公开单视频、嵌入/分享页；MP4 | 480p 完整下载，H.264 + AAC |
| Imgur | 单个视频与 gifv 播放页；MP4 | 完整下载，通过视频校验；该样本本来没有音轨 |
| Vimeo | 公开单视频、嵌入页、可访问的非 DRM MP4 / HLS | 实验适配；样本遇到访问限制、失效或 DRM，尚未完成下载验证 |
| Dailymotion | 公开单视频与短链；可访问的 MP4 / HLS | 实验适配；本机 Rust 请求被 CDN 拒绝，尚未完成下载验证 |

新增路由会精确识别作品页面，文件、CDN 直链及相册保持其原有处理方式。不会用页面中无关的推荐作品代替目标视频。上述下载样本证明流程可用，不能代表所有作品、网络或清晰度均可用。

清晰度可读性检查最多并行 4 档，单档有独立超时和总检查预算；慢档位不会丢弃已验证的可用结果。展示的选项以本次实际验证结果为准。

微博使用网站普通的匿名访客初始化。访客凭据仅参与本次微博元数据请求，不读取用户浏览器登录信息，不写入任务，不传给媒体 CDN。快手没有对应的 yt-dlp 专用解析器，本轮通过正式公开分享页的作品数据独立实现。

## HLS 视频下载

支持未加密、已经结束的 VOD 媒体清单：解析相对地址和 `EXT-X-MAP` 初始化段，逐段通过 Rust 下载器保存，暂停与失败后继续已有进度。每个新片段和复用片段都检查媒体头和容器一致性，错误网页会被隔离而不会成为成品；全部完成后在本地拼接 TS / fMP4，再封装 MP4。

本轮明确拒绝直播、加密/DRM、BYTERANGE、时间轴或编码切换、缺失分段与分离音频。主清单由平台适配器选成独立音视频清单。清单不超过 2 MiB，每档最多 4096 个输入片段；分片按顺序下载，每片沿用所选连接数。封装期间可能需要约媒体大小三倍的磁盘空间。

内置 FFmpeg 增加 MPEG-TS 读取和 AAC 参数探测。两平台构建脚本都已更新，仍只启用 `file,pipe` 协议；本轮只在 macOS 验证了实际二进制。

## 人工验证

启动本地开发工作台：

```sh
sh scripts/build-ffmpeg-macos.sh
sh scripts/cargo-local.sh build --offline --locked
NO_PROXY=127.0.0.1,localhost,::1 \
FFDM_FFMPEG="$PWD/src-tauri/resources/ffmpeg" \
./target/debug/ffdm serve --port 17890 \
  --download-dir .bench-work/platform-expansion/downloads \
  --state-dir .bench-work/platform-expansion/web-state
```

打开 <http://127.0.0.1:17890/>，新建下载、粘贴公开作品链接、选择清晰度，再开始下载。`NO_PROXY` 只用于本次进程的本机连接，公网仍使用正常网络/代理，不修改系统设置。

本轮已验证的公开页面可用于复查：

- 快手：<https://www.kuaishou.com/short-video/3x25msbsnncn8cs>
- 微博：<https://weibo.com/tv/show/1034:4797699866951785>
- AcFun：<https://www.acfun.cn/v/ac35457073>
- Streamable：<https://streamable.com/moo>
- Imgur：<https://imgur.com/A61SaA1>

实际文件检查记录位于本地 `.bench-work/platform-expansion/domestic-verification.json` 与 `international-verification.json`，包含文件大小、SHA-256、ffprobe 结果与成功/失败情况。签名媒体地址和访客凭据不会出现在这些报告中。

## 开发回归

```sh
sh scripts/cargo-local.sh fmt --all -- --check
NO_PROXY=127.0.0.1,localhost,::1 sh scripts/cargo-local.sh test --workspace --offline --locked
sh scripts/cargo-local.sh clippy --workspace --all-targets --offline --locked -- -D warnings
npm run test:web
NO_PROXY=127.0.0.1,localhost,::1 \
FFDM_HLS_REMUX_FFMPEG="$PWD/src-tauri/resources/ffmpeg" \
sh scripts/cargo-local.sh test --offline --locked --test hls_flow -- --ignored
```

最后一项需要完整的开发用 FFmpeg 生成测试片段、ffprobe 检查成品；实际封装由随包精简工具完成。新增回归覆盖目标作品隔离、分 P、访客信息处理、HLS 暂停/续传、失败片段重试、中间错误网页隔离、TS/fMP4 成品及不覆盖已有文件。

## 解析原理参考

参考 yt-dlp 官方解析器中的 URL 匹配、公开元数据请求、播放器配置和格式归一化思路，在本项目中用 Rust 实现：

- [微博](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/weibo.py)
- [AcFun](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/acfun.py)
- [Vimeo](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/vimeo.py)
- [Dailymotion](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/dailymotion.py)
- [Streamable](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/streamable.py)
- [Imgur](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/imgur.py)
