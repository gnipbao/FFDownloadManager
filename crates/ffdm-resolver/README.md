# ffdm-resolver

可复用的 Rust 资源解析模块：识别文件直链、平台作品链接和网页媒体，返回统一的格式列表与下载计划。它不依赖 FFDownload 的界面、任务队列、任务数据库或系统代理抓包，其他 CLI、Web 服务和桌面应用也可以接入。

解析模块负责“找到可下载资源”，宿主负责下载、暂停续传、进度和文件保存。FFDownload 当前通过 Rust `gosh-dl` 执行这些计划，分离音视频及 HLS 分片由宿主使用本地 FFmpeg 封装。解析模块不会运行 FFmpeg，也不需要 Python 或 yt-dlp 运行时。

## 接入

当前为 workspace 内的路径依赖，尚未发布到 crates.io。将 `path` 替换为本机仓库的位置：

```toml
[dependencies]
ffdm-resolver = { path = "path/to/FFDownloadManager/crates/ffdm-resolver" }
anyhow = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

下面的示例解析命令行传入的链接，只输出作品名称与格式标签，不打印可能包含签名参数的下载地址：

```rust
use anyhow::{Context, Result};
use ffdm_resolver::ResourceResolver;

#[tokio::main]
async fn main() -> Result<()> {
    let url = std::env::args().nth(1).context("请传入文件或作品链接")?;
    let resolver = ResourceResolver::with_ffmpeg(None)?;
    let resource = resolver.extract(&url).await?;

    println!("{} · {}", resource.platform, resource.title);
    for format in &resource.formats {
        println!("{} · {}", format.plan.label, format.plan.extension);
    }
    Ok(())
}
```

## 公共接口

| 接口 / 类型 | 用途 |
| --- | --- |
| `ResourceResolver::extract(url)` | 返回无 UI 缓存的 `ExtractedResource`，供宿主自行选择格式和保存计划。 |
| `ExtractedResource` | 原始链接、标题、平台、时长及 `Vec<ExtractedFormat>`。 |
| `ExtractedFormat` | 清晰度高度、是否纯音频，以及私有 `MediaPlan`。 |
| `MediaPlan` | 格式标识、文件扩展名、组装方式及包含 URL、请求头、大小的 `MediaStream` 列表。 |
| `ResourceResolver::resolve(url)` | 返回适合界面展示的 `MediaPreview`，将下载计划保存在解析器实例内。 |
| `ResourceResolver::select(preview_id, format_id)` | 根据界面选择取回计划；需要使用生成预览的同一个解析器实例。 |
| `ResourceResolver::validate_download(plan)` | 在开始下载前检查适用平台的媒体可访问性；不保证后续请求始终有效。 |
| `routing::platform(url)` | 识别已接入平台的作品链接。返回 `None` 时仍可调用 `extract` 进行通用资源解析。 |

`extract` 与 `resolve` 统一按音视频类型、清晰度等排序并去重；宿主仍可按自己的需求筛选。`resolve` 另外缓存界面选择结果；缓存默认 15 分钟，媒体地址也可能提前失效，过期后应重新解析。

界面可以只接收 `MediaPreview`，将 `select` 的结果交给后端队列：

```rust
async fn example(
    resolver: &ffdm_resolver::ResourceResolver,
    url: &str,
) -> anyhow::Result<()> {
    let preview = resolver.resolve(url).await?;
    for format in &preview.formats {
        println!("{} · {}", format.label, format.filename);
    }

    // 由用户选定格式后，在后端调用：
    if let Some(format) = preview.formats.first() {
        let plan = resolver.select(&preview.id, &format.id)?;
        resolver.validate_download(&plan).await?;
        // 将 plan 交给宿主自己的 Rust 下载执行器。
    }
    Ok(())
}
```

`ResourceResolver::with_ffmpeg(Some(path))` 声明宿主可提供 FFmpeg；`new()` 会尝试通过 `FFDM_FFMPEG` 和 PATH 查找。此配置仅影响预览与选择能力检查，解析过程不会启动 FFmpeg。`extract` 仍可返回需要后续封装的计划，由宿主决定如何执行。

`Assembly` 指定执行方式：

- `Direct`：单个完整文件，按 `MediaStream.transform` 的要求处理后保存；没有处理标记时原样保存。
- `Merge`：两个分离轨道，需要音视频封装。
- `Concat`：按顺序拼接媒体后封装。
- `Hls`：按顺序下载已展开的 VOD 分片和可选初始化分片，视频最终封装为 MP4，已声明为音频的 AAC 点播媒体清单封装为 M4A。

宿主应保留计划中的请求头，对每个响应做有效性检查，并负责断点状态、失败重试和最终文件发布。下载地址和请求头属于私有执行数据，避免将 `MediaPlan` 的调试输出写入公开日志；展示时优先使用 `MediaPreview`，或在计划副本上调用 `for_display()`。

`MediaStream.transform` 是可选的下载后处理标记。遇到 `StreamTransform::SunoAudio { content_id }` 时，宿主应先下载完整源文件，再调用 `suno_audio::decode_file(content_id, input, output)`，成功后发布输出并对输出计算校验值。该函数使用平台正常提供的匿名播放许可，不接收账号 Cookie；许可材料仅在内存中使用。输出路径必须尚不存在，失败时保留原始下载文件供重试。忽略此标记会得到无法播放的源文件；无法执行处理的宿主应明确拒绝该格式。

对 Wikimedia 媒体，宿主还应调用 `source_policy::limits` 设置连接数和速率、在整个传输期间持有 `source_policy::acquire` 许可，并在 429 响应时用 `record_retry_after` 记录服务器冷却期。FFDownload 已接入这些限制；其他来源不受该策略影响。

宿主也可以实现 `ResourceExtractor`，通过 `resolver.with_extractor(adapter)` 注册自己的 Rust 适配器；匹配的自定义适配器优先执行，输出仍沿用相同的 `ExtractedResource` / `MediaPlan`。平台授权由适配器自身配置，不转发其他平台的 Cookie。

可运行示例：

```sh
sh scripts/cargo-local.sh run -p ffdm-resolver --example resolve -- <链接>
sh scripts/cargo-local.sh run --example resource_download -- <链接> <输出文件> [格式ID]
```

第二项是 FFDownload 提供的最小下载宿主，复用 Rust 下载执行器；不属于解析 crate 的运行依赖。

## 覆盖范围

仓库开发版现有 **42 个具名平台适配器**。支持的站点与链接类型见 [支持站点与下载范围](../../docs/supported-platforms.zh-CN.md)。Suno 支持可公开访问的单曲分享链接，使用网页提供的音频与正常匿名播放许可；无需单独填写登录授权，不支持访问受限的作品。

适配器存在不代表所有作品或地区都能成功。公开视频仍可能需要登录、访问验证或特定网络，接口与媒体地址也会变化。仓库能力与正式安装包可能不同，安装包以 Release 说明为准。

通用解析支持：

- HTTP/HTTPS 任意文件直链，包括文档、压缩包、图片、音频和视频；依据响应头及链接识别文件类型。
- HTML5 `video` / `audio` / `source`、Open Graph 媒体元数据、JSON-LD 媒体对象和可直接读取的 JWPlayer JSON 配置。
- 已接入平台的单作品嵌入播放器，转交相应平台适配器处理。
- 未加密的 HLS 点播媒体清单，以及包含音视频合流的主清单清晰度选择。

PeerTube、Mastodon 内置常见实例的自动识别；其他实例选择“网页媒体解析”显式解析，确认页面为 HTML 后再读取同源 API。相同路径形状的文件直链仍按完整文件处理。Archive.org 多作品条目需要明确选择单个媒体文件，避免下载错误的作品。

通用网页解析读取静态 HTML，不执行任意网页 JavaScript。汽水音乐适配器需要 Chrome、Edge 或 Chromium，通过独立临时浏览器播放分享页并识别实际音频请求；抖音、Instagram 接口未返回目标作品时，也会使用临时浏览器读取正常加载后的作品数据。可设置 `FFDM_BROWSER` 指定浏览器程序。这些解析过程不会读取日常浏览器的 profile 或保存的 Cookie。

AAC 音频 HLS 支持完整 VOD 媒体清单及 TS、fMP4、ADTS 分片，不支持纯音频主清单或 MP3 HLS。Bandcamp 目前解析公开单曲，SoundCloud 排除试听片段并提供实际可读的完整音频格式。搜狐仅提供完整的单章节视频，多章节作品暂不组合下载；社交平台的多个独立附件不会混选成清晰度档位。

当前不支持直播录制、播放列表批量下载、DRM、加密 HLS、通用独立 DASH 清单、需要外部音频轨道的 HLS，以及只有登录后或脚本运行后才暴露媒体的任意网页。部分平台提供 `extract_with_cookie` / `resolve_with_cookie` 接收本次解析的授权 Cookie；通用网页解析不接收平台 Cookie，也不会自动读取浏览器登录状态。

本模块参考 [yt-dlp 的通用解析思路](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/generic.py)，覆盖范围尚未达到其站点列表。[固定版本的全量对照清单](../../docs/yt-dlp-coverage.json) 列出了 1,731 个 upstream extractor 条目，其中 `partial` 仅表示相近的部分单作品能力，`pending` 表示尚未实现；该清单不参与运行时，也不意味着已支持全部站点。“解析万物、下载万物”是统一接口与持续扩展的目标，不是对所有站点、权限或媒体格式的保证。

## 使用与许可

代码采用 MIT 许可证；依赖遵循各自许可证。仅用于学习研究及下载你拥有权利或已获授权的资源，请遵守平台规则和适用法律。模块不提供绕过 DRM、付费权限或账号验证的能力。
