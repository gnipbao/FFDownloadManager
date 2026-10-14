# 独立解析模块：第二批平台本地验证

本批在 `ffdm-resolver` 中增加央视网、西瓜视频、梨视频、Archive.org、TED、Wikimedia Commons、Bluesky、PeerTube。当前包含 **25 个具名平台适配器**，另外支持通用网页媒体与 HTTP/HTTPS 文件直链；适配器数量不代表所有作品都已验证，也不等于 yt-dlp 的完整覆盖范围。

解析、清单读取、媒体请求和文件下载继续使用 Rust，运行时不调用 Python 或 yt-dlp。FFmpeg 只对已下载的本地音视频进行无转码封装。本批完成本地验证后纳入仓库，正式安装包以 Release 为准。

## 在 Web 工作台验证

打开 [本地工作台](http://127.0.0.1:17890/)，点击“新建下载”，粘贴下表中的公开作品链接，等待解析后选择下载格式并开始下载。检查任务完成、文件扩展名与格式一致，并播放成品确认画面、声音和时长。

已接入的常见平台作品页可使用“自动识别”。其他网站或任意 PeerTube 实例，请将“链接处理方式”改为“网页媒体解析”；模块会验证该实例的公开播放接口和目标作品 ID。文件直链继续直接下载，不会因为没有视频后缀而被拒绝。

如需重新启动开发服务：

```sh
sh scripts/cargo-local.sh build --offline --locked
NO_PROXY=127.0.0.1,localhost,::1 \
FFDM_FFMPEG="$PWD/src-tauri/resources/ffmpeg" \
./target/debug/ffdm serve --port 17890 \
  --download-dir .bench-work/platform-expansion/downloads \
  --state-dir .bench-work/platform-expansion/web-state
```

HLS 封装需要上述本地 FFmpeg 工具已构建。`NO_PROXY` 仅作用于本次进程的本机连接，不修改系统代理。

## 样例与当前结果

验证日期：2026-10-14。成功项目均通过本地 Web API 创建真实下载任务，并检查文件大小、SHA-256、音视频轨与时长；这里只记录实际选中的格式。

| 平台与公开样例 | 验证结果 | 成品大小 | 时长与编码 |
| --- | --- | ---: | --- |
| [梨视频](https://www.pearvideo.com/video_1804394) | 原始 MP4 完整下载 | 45,437,385 字节 | 412.22 秒，H.264 + AAC，720×406 |
| [PeerTube](https://framatube.org/w/kkGMgK9ZtnKfYAgnEtQxbv) | 1080p MP4 完整下载 | 16,815,344 字节 | 114.06 秒，H.264 + AAC，1920×1080 |
| [TED](https://www.ted.com/talks/terry_moore_how_to_tie_your_shoes) | MP4 完整下载 | 21,820,397 字节 | 158.59 秒，H.264 + AAC，640×480 |
| [Bluesky](https://bsky.app/profile/bsky.app/post/3l3vgf77uco2g) | 360p HLS 完整下载并封装 MP4；另解析到 720p | 1,824,537 字节 | 29.76 秒，H.264 + AAC，640×360 |
| [Archive.org](https://archive.org/details/BigBuckBunny_328) | 240p MP4 完整下载；另解析到 AVI、OGV | 43,315,070 字节 | 596.42 秒，H.264 + AAC，416×240 |
| [央视网](https://tv.cctv.com/2016/02/05/VIDEUS7apq3lKrHG9Dncm03B160205.shtml) | 270p HLS 通过 Web UI 完整下载并封装 MP4 | 2,206,756 字节 | 37.96 秒，H.264 + AAC，480×270 |
| [Wikimedia Commons](https://commons.wikimedia.org/wiki/File:Two-toed_sloth_rail_walking_%E4%BA%8C%E8%B6%BE%E6%A8%B9%E7%8D%BA%E7%88%AC%E8%A1%8C_(HD).webm) | 原始 WebM 完整下载；旧失败任务采用单连接继续成功 | 46,191,451 字节 | 25.69 秒，VP9 + OPUS, 1920×1080 |
| [西瓜视频](https://www.ixigua.com/7047333627111440904) | 适配公开 SSR 数据中的完整音视频文件；当前样例未提供播放数据，遇到验证或登录限制 | — | 尚未通过真实下载验证 |

本机报告为 `.bench-work/platform-batch2/download-verification.json`，包含成功文件校验信息和失败原因，不包含 Cookie 或签名媒体地址。样例的可访问性和可用格式可能随平台变化；遇到过期媒体地址，请重新解析。

## 回归检查

`cargo test --workspace --offline --locked`：180 个测试通过、7 个网络或环境相关测试默认忽略；另显式运行了新增三个适配器模块的公开元数据测试，均通过。后续请求头修复的 8 个限流专项测试、服务层回归、旧任务连接限制迁移测试也通过。前端 17 个测试通过，含 163 个共享 URL 路由样例；workspace Clippy 无警告。

## 下载边界与来源限流

- 只接受请求作品自身的公开播放数据，不用推荐、回复或引用中的其他作品替代。Archive.org 多作品集合需指定目标文件；西瓜暂不列出无法配对的动态分离音视频轨。
- 支持未加密、已经结束的 HLS 点播，以及公开主清单中的完整音视频档位。直播、DRM/加密、分离音频组、`BYTERANGE`、缺失分段、时间轴切换等不支持的清单会明确拒绝。
- 通用 DASH、播放列表批量下载和直播录制尚未接入。登录、付费或受访问限制的作品，不通过本批公开解析器获取授权。
- Wikimedia 媒体使用单任务 1 路连接、上限 1.4 MB/s，同一应用进程最多 2 个共享请求许可；收到 HTTP 429 时遵循 `Retry-After` 冷却，缺少有效等待时间时采用保守退避。这些限制用于适配[官方来源预算](https://wikitech.wikimedia.org/wiki/Robot_policy)，不会为了跑满带宽突破源站限制，也不保证所有请求都能通过。

## 技术参考

参考 yt-dlp 官方的 URL 匹配、公开播放器数据与格式发现方式，在独立 Rust 模块中实现：

[央视网](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/cctv.py)、[西瓜视频](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/ixigua.py)、[梨视频](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/pearvideo.py)、[Archive.org](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/archiveorg.py)、[TED](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/ted.py)、[Wikimedia](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/wikimedia.py)、[Bluesky](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/bluesky.py)、[PeerTube](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/peertube.py)。
