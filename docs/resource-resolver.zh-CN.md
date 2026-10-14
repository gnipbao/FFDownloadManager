# 可复用 Rust 资源解析模块

解析模块位于 `crates/ffdm-resolver`，与下载执行器分开。原有平台规则已迁入独立 crate；`src/media.rs`、`src/hls.rs` 保留兼容导出，现有任务记录、下载流程和桌面调用沿用原来的数据结构。

```text
文件 / 平台作品 / 网页媒体 / HLS 链接
                    ↓
              ffdm-resolver
      平台适配器 → 通用页面识别 → 格式验证
                    ↓
            ExtractedResource / MediaPlan
                    ↓
            Rust 下载执行器与任务队列
       分段下载 → 暂停续传 → 本地封装 → 成品
```

## 接口与边界

`ResourceResolver::extract` 返回无界面缓存的资源信息和私有下载计划；`resolve` / `select` 用于交互式格式选择。两条接口共用格式验证、排序和去重。实现 `ResourceExtractor` 并通过 `with_extractor` 注册，可扩展平台而无需改动宿主的下载队列。

计划保留精确媒体地址、请求头、容器及轨道组装方式。标题、文件名和选项可交给前端；签名媒体地址与请求头仅供后端执行。预览不会发送这些私有字段。平台 Cookie 仅用于本次对应平台解析，通用页面和自定义适配器不会接收其他平台的 Cookie。

解析模块不依赖 FFDownload 的 UI、任务数据库、系统代理抓包或下载内核，不执行 FFmpeg。下载执行器仍使用 Rust；音视频和 HLS 封装由宿主调用本地 FFmpeg。汽水音乐适配器保留独立临时 Chrome / Edge 会话，不读取日常浏览器 profile。

## 本轮新增

- 通用静态网页：HTML5 视频/音频标签、Open Graph、JSON-LD 媒体对象、JSON 格式的 JWPlayer 播放器配置。
- 已适配平台的嵌入播放器：转交对应 Rust 解析器，限制层级和循环。
- 文件资源：显式解析接口支持 HTTP/HTTPS 直链，依据响应头及文件名确定容器；压缩包、文档、图片等直接保存，不强制 MP4。
- HLS：发现未加密点播主清单的清晰度，再展开媒体清单，复用已有 Rust 分片下载与恢复流程。
- 并行与超时：地址识别和格式验证通过有限并发串成流水线，慢来源不会丢弃已验证选项。

提取时包含原有 17 个平台适配器，后续扩展的当前清单见 [平台与下载清单](supported-platforms.zh-CN.md)。通用解析读取静态页面，不保证任何网站都可解析。多部互不相关作品、直播、DRM、加密 HLS、通用 DASH、外部音频 HLS 等会明确拒绝，不以错误格式替代。平台访问权限和网络限制仍然有效。

## 本地 Web 验证

```sh
sh scripts/cargo-local.sh build --offline --locked
NO_PROXY=127.0.0.1,localhost,::1 \
FFDM_FFMPEG="$PWD/src-tauri/resources/ffmpeg" \
./target/debug/ffdm serve --port 17890 \
  --download-dir .bench-work/platform-expansion/downloads \
  --state-dir .bench-work/platform-expansion/web-state
```

在本地工作台新建下载：文件直链保持自动下载；已有平台作品和 `.m3u8` 链接自动进入解析；普通网页选择“网页媒体解析”，再选择格式、开始下载。测试状态与日常任务隔离。

独立解析示例与最小 Rust 下载宿主：

```sh
sh scripts/cargo-local.sh run -p ffdm-resolver --example resolve -- <链接>
sh scripts/cargo-local.sh run --example resource_download -- <链接> <输出文件> [格式ID]
```

本模块完成本地验证后纳入仓库；本次未创建新 Release，正式安装包以 Releases 为准。

## 已完成的本机验证

- 完整 Rust 工作区回归 149 项通过，新增文件元数据回归另 1 项通过；前端路由与表单测试 17 项通过。
- 原有平台解析、任务恢复、文件授权、历史资源过滤、桌面 IPC 与 HLS 任务继续通过回归。
- W3C 的公开 [HTML5 视频示例](https://www.w3.org/2010/05/video/mediaevents.html) 已通过独立模块与 Rust 下载执行器完整下载：MP4 4,372,373 字节，52.208 秒，H.264 854×480 + AAC。SHA-256 为 `b670602fa00934ca27c4351bb0efe7ea7a07fae57284e44226025eeed7c51254`。
- 通用解析使用自身客户端标识；无需将 Rust 请求伪装为 Chrome。真实页面测试中，源站会拒绝浏览器标识与本地 HTTP 客户端不一致的请求。

验证记录存放于本机 `.bench-work/resolver-real-sample.json` 及 `resolver-workspace-final-tests.log`，不包含媒体签名地址或 Cookie。上述结果证明本轮解析与下载链路可用，不代表所有网站都可访问；本轮尚未执行 Windows 实机运行验证。

## 回归与参考

```sh
sh scripts/cargo-local.sh fmt --all -- --check
NO_PROXY=127.0.0.1,localhost,::1 sh scripts/cargo-local.sh test --workspace --offline --locked
sh scripts/cargo-local.sh clippy --workspace --all-targets --offline --locked -- -D warnings
npm run test:web
```

解析原则参考 [yt-dlp GenericIE](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/generic.py) 和 [HLS RFC 8216](https://www.rfc-editor.org/rfc/rfc8216)，运行时继续采用 Rust 实现，不调用 Python 或 yt-dlp。
