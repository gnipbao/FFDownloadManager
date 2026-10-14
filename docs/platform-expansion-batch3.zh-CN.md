# 独立解析模块：第三批平台本地验证

本批在 `ffdm-resolver` 中新增搜狐视频、好看视频、Medal、9GAG、Tumblr、Mastodon、Bandcamp、SoundCloud，累计 **33 个具名平台适配器**，另支持通用网页媒体与 HTTP/HTTPS 文件直链。这个数量表示已接入的适配器，不代表每个平台的所有作品都可下载，也不等于 yt-dlp 的完整覆盖范围。

解析、公开播放接口请求、清单展开、分段传输和文件下载继续使用 Rust。运行时不调用 Python 或 yt-dlp；FFmpeg 只对已经下载的本地音视频进行无转码封装。本批完成本地验证后纳入仓库，正式安装包以 Release 为准。

## 在本地 Web 工作台测试

打开 [本地工作台](http://127.0.0.1:17890/)，点击“新建下载”，粘贴下表中的公开作品链接，使用“自动识别”。解析完成后选择可用格式，确认文件名称与扩展名，再开始下载。任务完成后播放成品，检查画面、声音及完整时长；纯音频应有音轨，原作品无声的视频不应被误判为下载缺少音轨。

未知的 Mastodon 实例可以将“链接处理方式”改为“网页媒体解析”，模块会检查该实例的公开状态接口与目标作品。文件直链仍直接下载，无须先解析成视频链接。媒体地址过期时，应重新解析公开作品页面。

## 公开样例与实测结果

成功项目均通过本地 Web API 创建真实下载任务，核对成品大小、SHA-256、音视频轨与时长。下表只展示公开页面与实际选择的格式，不包含签名媒体地址或 Cookie。

| 平台与公开样例 | 实测结果 | 成品大小 | 时长与编码 |
| --- | --- | ---: | --- |
| [搜狐视频](https://tv.sohu.com/v/dXMvMjMyNzk5ODg5Lzc4NjkzNDY0LnNodG1s.html) | MP4 · 完整视频 完整下载，目录大小提示已修复 | 14,265,796 字节 | 213.07 秒，H.264 + AAC，848×480 |
| [好看视频](https://haokan.baidu.com/v?vid=18435139826687843612) | 480p MP4 完整下载 | 1,521,274 字节 | 47.14 秒，H.264 + AAC，852×480 |
| [Medal](https://medal.tv/games/valorant/clips/jTBFnLKdLy15K) | 1080p MP4 完整下载；首次网络连接失败后继续任务成功 | 15,771,882 字节 | 13.03 秒，H.264 + AAC，1920×1080 |
| [9GAG](https://9gag.com/gag/ae5Ag7B) | 288p MP4 完整下载；另解析到 HEVC、VP8/VP9 格式 | 2,067,118 字节 | 44.65 秒，H.264 + AAC，460×288 |
| [Tumblr](https://maskofthedragon.tumblr.com/post/626907179849564160) | 720p MP4 完整下载 | 1,428,784 字节 | 7.62 秒，H.264 + AAC，1280×720 |
| [Mastodon](https://mastodon.social/@Mastodon/116296397062584045) | 原始 MP4 完整下载，原作品无音轨 | 999,172 字节 | 14.28 秒，H.264，2680×1600 |
| [Bandcamp](https://youtube-dl.bandcamp.com/track/youtube-dl-test-song) | 公开单曲 MP3 完整下载 | 158,405 字节 | 9.87 秒，MP3，128 kbps |
| [SoundCloud](https://soundcloud.com/ethmusic/lostin-powers-she-so-heavy) | 完整 AAC HLS 点播下载并封装 M4A | 1,744,140 字节 | 143.21 秒，AAC |

当前报告中 **8 个平台样例均已完整下载**。样例的可访问性及可用格式会随平台变化，单次成功不能代表所有作品均可下载。本机校验报告保存在 `.bench-work/platform-batch3/download-verification.json`。

搜狐样例的失败原因已定位：平台元数据的文件大小比 CDN 实际响应大 5,607 字节，导致按旧大小探测末字节时出现 HTTP 416。现由响应获取真实长度，继续验证首尾字节与完整响应；修复后完整下载成功，成品时长与元数据一致。SoundCloud 则已经修复新官方 CDN 被误拒绝的问题，公开播放器的 AAC 清单可由 Rust 直接读取。

Bandcamp 还通过 Web UI 实际填写链接、选择 MP3 格式和创建下载任务，成品 `Bandcamp-WebUI-验证.mp3` 已完成。

## 支持边界

- 只解析目标作品的公开播放数据，不以推荐、转发、引用或其他曲目代替。Bandcamp 只接受单曲页面；Mastodon、Tumblr 的多个独立媒体附件不会混选成一个下载任务。
- 视频可使用实际可访问的完整直链格式。搜狐多章节资源不会把第一章伪装成完整视频；未接入的多章节组合会明确拒绝。
- 音频新增完整、未加密、已经结束的 **AAC HLS 媒体清单**，支持 TS、fMP4、ADTS 分段，下载后无转码封装为 M4A。暂不支持纯音频主清单、外部分离音轨、MP3 HLS、直播、DRM/加密、`BYTERANGE` 或时间轴切换。
- SoundCloud 检查 `snipped`、预览地址和完整时长，排除试听片段、未知编码与加密格式。当前公开实测是 AAC HLS；progressive MP3 已有解析测试，但不宣称所有作品都提供 MP3。
- 账号登录、订阅、私密和其他访问限制不会通过本批公开解析器绕过。请求头不会自动读取浏览器 Cookie；网页查询参数与 fragment 不会转发到媒体 Referer，媒体地址自身的签名参数保留。

## 回归验证

本轮 workspace 测试 **210 个通过、12 个环境或网络相关测试默认忽略**；前端 **17 个测试通过**，覆盖 **221 个共享 URL 路由样例**。Workspace Clippy 无警告。另有两个本地 Mastodon 实例发现测试，验证 HTML 才访问状态 API、相同路径的 ZIP 保持文件直链。

另外显式运行了音频 HLS 端到端测试，验证 TS、fMP4、ADTS 三种容器的 AAC 下载与本地封装，并覆盖失败后的再次继续。Bandcamp、SoundCloud 的真实 Rust 公开元数据测试通过；音频模块 6 个专项测试覆盖目标身份、试听过滤、格式识别和请求头边界。安装包及 Windows 客户端尚未作为本轮发布验收范围。

## 技术参考

公开播放器字段和格式发现参考 yt-dlp 官方源码，在独立 Rust 模块中实现：[搜狐](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/sohu.py)、[Medal](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/medaltv.py)、[9GAG](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/ninegag.py)、[Tumblr](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/tumblr.py)、[Bandcamp](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/bandcamp.py)、[SoundCloud](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/soundcloud.py)。

好看视频、Mastodon 的本轮适配依据网站公开播放数据和 [Mastodon 官方状态 API](https://docs.joinmastodon.org/methods/statuses/#get) 实现，并不声称从 yt-dlp 对应的独立适配器移植。平台专属逻辑与通用网页媒体解析共用格式候选及下载计划契约。
