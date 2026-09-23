# 视频解析封装与验证

日期：2026-09-21。运行环境：Apple Silicon macOS、Rust 1.98.1、FFmpeg 7.0.2。

## 实现

- `src/media.rs`：`ytdown 0.8.0`、`bbdown-core 0.5.0` 适配器、平台路由、格式归一化、音视频配对、文件名和短期解析缓存。
- `src/youtube.rs`：将 ytdown 首选的旧 Android VR 播放器请求适配为 VISIONOS，保持客户端请求头、设备信息和会话一致；检查格式可读性，保留其他客户端回退与较高可读清晰度。
- `src/media_download.rs`：使用现有 Rust / gosh-dl 下载每个完整媒体轨道，再调用本地 FFmpeg 流复制合并。FFmpeg 仅允许本地文件协议，不接收平台 URL。
- `src/service.rs`：视频任务和普通下载共用队列、持久化、暂停、继续及状态展示。已有任务记录保持兼容。
- `src/web.rs` 与 `web/`：粘贴视频页面 → 解析 → 选择清晰度或仅音频 → 下载。API 只返回选择标识和展示信息，签名媒体地址及请求头留在服务器状态中。

未调用 yt-dlp、Python 或外部视频解析服务。ytdown 的 YouTube 签名处理依赖内嵌 Boa；网络 TLS、SQLite 等底层依赖与 FFmpeg 仍包含非 Rust 组件。Tauri macOS 客户端已直接复用这套解析器和任务服务，并自带本地合并组件。

YouTube 使用 ytdown 推荐的 native TLS 客户端；下载继续使用 gosh-dl 的 Rust 网络栈。两条请求路径均启用系统代理读取，避免关闭 reqwest 默认特性时意外忽略 macOS 已配置的代理。

## 使用与接口

运行 `./scripts/start-web.sh`，打开 `http://127.0.0.1:17890`，在新建窗口粘贴视频页面链接。FFmpeg 从 `FFDM_FFMPEG`、`PATH` 或常见 Homebrew 路径查找；安装后重启服务。

```text
POST /api/media/resolve
{"url":"https://www.youtube.com/watch?v=jNQXAC9IVRw"}

返回：id、title、platform、duration_seconds、formats、ffmpeg_available、expires_at
formats 包含 id、label、filename、extension、total_bytes、needs_merge、audio_only

POST /api/media/tasks
{"preview_id":"上一步的 id","format_id":"所选格式 id","filename":"视频.mp4","connections":4}
```

两种 POST 都要求 `Content-Type: application/json` 和 `X-FFDM-Client: local-ui`，同样受本地 Host / Origin 检查。文件名和 SHA-256 可省略。媒体后缀须匹配实际格式；已有输出文件不会覆盖。

每个服务最多并行解析两个链接，YouTube 的客户端回退过程串行执行以隔离请求状态；最多缓存 16 次结果，结果有效期 15 分钟。页面修改链接后会丢弃旧请求的返回值，避免把上一个视频的格式提交给新链接。可以用 `ffdm extract <url>` 在命令行只输出解析预览；命令行预览标识与 Web 服务缓存不共享。

YouTube 解析不再把“有格式列表”当作可下载的证据：使用下载内核相同的 HTTP/TLS 栈检查文件最后一个字节，以及完整 GET 的响应头，随后立即丢弃完整响应流。解析后再检查签名解码得到的地址，添加任务前也会验证，成功检查短暂缓存 45 秒。文件尾部检查能发现只允许读取开头部分的地址，但不能保证之后不会遇到断网或平台策略变化。下载失败任务的按钮为“重新解析视频”，让用户重新选择当前可用的清晰度。

下载过程按轨道顺序执行，每条轨道复用所选分段连接数。暂停保留轨道的 SQLite 状态；已完成轨道在重试合并时复用。下载目录中的 `<文件名>.ffdm-state/media/` 保存媒体计划和恢复数据，合并错误日志为 `ffmpeg.log`。最终发布前计算 SHA-256，失败不发布不完整成品。

## 本轮实测

隔离测试目录为 `.bench-work/media-e2e/`，没有将测试任务加入用户现有下载列表。

| 平台样本 | 解析结果 | 实际产物 |
| --- | --- | --- |
| YouTube `jNQXAC9IVRw`（Me at the zoo） | 11 个视频／音频选项 | 744,412 字节 MP4；320×240 H.264 + AAC；19.064 秒 |
| Bilibili `av170001`（当前第一个分 P） | 3 个视频编码选项 + 1 个音频选项 | 6,948,831 字节 MP4；512×288 H.264 + AAC；199.334 秒 |

YouTube 从 Web UI 完成解析与任务创建，B 站从本地 API 完成任务创建；两者都经相同服务与 Rust 下载内核完成下载。Web UI 另验证了 B 站格式展示、仅音频切换、自定义文件名自动补后缀。使用 ffprobe 检查最终文件同时包含视频和音频；样本较小，仅证明流程可用，不构成速度对比。

自动测试覆盖真实 crate 对本地模拟平台响应的解析、格式归一化与缓存过期、请求头传递、暂停续传、任务和 HTTP 接口。需要 FFmpeg 的测试还验证合并失败后保留已完成轨道、重试不重复下载、最终音视频齐全与输出不覆盖。

验证结果：默认测试 17 项通过；另行执行的 FFmpeg 合并测试 1 项通过，共 18 项。`cargo clippy --all-targets -- -D warnings`、Rust 格式检查和前端 JavaScript 语法检查通过。浏览器控制台未发现警告或错误。样本文件的摘要和 ffprobe 原始结果保存在 `.bench-work/media-e2e/verification.json`。

## 当前范围

- 已实测的是上述两个公开样本，不能保证平台所有链接可下载。TikTok、Instagram、X、Reddit 接入了 ytdown 路由，真实平台测试待补。
- 不自动读取浏览器 Cookie；没有登录或付费内容支持。清晰度以平台当前公开返回结果为准。
- 支持完整独立的 DASH 音视频轨道；不下载 HLS / DASH 清单的分片流，也不录制直播。
- 不批量处理播放列表、频道或合集。B 站使用 `Selection::Current` 选当前分 P；指定分 P 和 b23 短链保留库的解析能力，本轮未专门验证。
- CDN 地址可能早于缓存或任务生命周期过期；遇到拒绝或失效时提示重新解析创建任务，保留已有数据。当前不会自动更新签名地址、验证新旧内容并衔接旧分段。
- 缺少 FFmpeg 时，只有无需合并的格式可以创建任务。恢复期间若 FFmpeg 不可用，下载好的轨道保留，安装并重启后可以重试。

## 高清 403 根因与修复

用户样本 `S8Z3gGqc7W4`（Russian Roulette）：原先默认选择 `400+140` 的 1440p AV1 格式。ytdown 的 iOS 回退返回了格式列表，但视频和音频地址均拒绝完整 GET 和文件尾部读取，首部的一小段仍可读取。因此不能把这个失败简单归因为地址过期，也不能仅检查首字节。

根因在 YouTube 客户端兼容性：ytdown 0.8.0 的客户端配置落后于平台变更，返回了有格式信息但不能完整读取的高清地址。用另一套 HTTP 客户端请求同一地址仍返回 403；更换分段数不能解决。官方 yt-dlp 的 [2026.08.19 发布记录](https://github.com/yt-dlp/yt-dlp/releases/tag/2026.08.19) 已移除默认 Android VR 并加入 VISIONOS；其 [客户端配置](https://github.com/yt-dlp/yt-dlp/blob/2026.08.19/yt_dlp/extractor/youtube/_base.py) 也记录了旧 Android VR 媒体地址被拒绝的现象。平台的 [PO Token 说明](https://github.com/yt-dlp/yt-dlp/wiki/PO-Token-Guide) 解释了部分客户端元数据成功但 CDN 返回 403 的原因；本次没有解析出服务器内部的具体拒绝理由，因此不把所有 403 都归为令牌缺失。

修复通过 ytdown 公开的 `HttpClient` 接口更新首选播放器请求，使用 VISIONOS 1.02 / RealityDevice17,1 配置，保持 JSON、User-Agent 与 X-YouTube-Client 请求头一致，并保留 visitorData、语言和区域。无需替换 gosh-dl，也不需要 yt-dlp / Python 作为运行时依赖。原有播放器解析、签名处理及后续客户端回退仍由 Rust 实现。

仅过滤失败格式的前一版处理最多获得 360p，本次已经恢复高清：同一条 `S8Z3gGqc7W4` 通过修复后的应用 API 解析出 25 个可用选项，默认 1440p，且完整下载、合并了以下文件：

| 格式 | 实际视频与音频 | 最终大小 | 时长 |
| --- | --- | --- | --- |
| `400+140` | 2560×1440 AV1 + AAC，MP4 | 31,894,494 字节 | 68.058 秒 |
| `137+140` | 1920×1080 H.264 + AAC，MP4 | 25,759,097 字节 | 68.058 秒 |

ffprobe 逐包读取确认两个文件各有 2,038 个视频包和 2,931 个音频包；FFmpeg 对完整视频和音频解码到空输出，均无错误。摘要及原始结果保存在 `.bench-work/hd-fix/verification.json`。这些是原始高清轨道合并，没有放大 360p 或重新编码。已知不可读格式仍被过滤，尾部与完整 GET 检查继续保留；平台后续变化仍需持续维护客户端适配。

本次默认回归测试共 21 项通过，Clippy 无警告。新增回归覆盖真正的 ytdown 提取器通过新客户端返回 1440p 和音轨，以及客户端身份一致、访客会话保留；原有客户端失败回退测试仍通过。403 / 410 的旧任务需要点“重新解析视频”使用新地址；网络中断、合并失败保留“重试下载”以复用已有轨道和分段。
