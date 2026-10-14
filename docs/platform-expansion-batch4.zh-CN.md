# 独立解析模块：第四批验证与仓库更新

验证日期：2026-10-14。本批在 `ffdm-resolver` 新增虎牙点播、网易新闻视频、Rumble、Odysee、OK.ru、Clyp、AudioBoom、Audius，共 **41 个具名平台适配器**。解析、格式验证和下载继续使用 Rust；运行时不调用 Python 或 yt-dlp。FFmpeg 只封装已下载的本地文件。

## 完整下载清单

本批通过本地 Web 服务创建真实下载任务，成品由 ffprobe 检查编码、音轨与时长，并计算 SHA-256。已完成的项目如下；校验数据见 [下载验证报告](platform-download-verification.json)，所有平台的能力与样例见 [支持平台清单](supported-platforms.zh-CN.md)。

| 平台与公开作品 | 实际格式 | 成品大小（字节） | 成品时长（秒） |
| --- | --- | ---: | ---: |
| [网易新闻](https://www.163.com/v/video/VD868N253.html) | HLS → MP4，H.264 + AAC | 4,206,066 | 39.126 |
| [虎牙点播](https://www.huya.com/video/play/1002412640.html) | 360p MP4，H.264 + AAC | 823,278 | 14.211 |
| [Rumble](https://rumble.com/embed/vb0ofn) | 360p MP4，H.264 + AAC | 8,231,997 | 103.637 |
| [OK.ru](https://ok.ru/videoembed/20079905452) | 144p MP4，H.264 + AAC | 1,401,621 | 100.960 |
| [AudioBoom](https://audioboom.com/posts/7364167-nhl-therink-trailer) | MP3，保留内嵌封面 | 1,970,350 | 118.592 |
| [Audius](https://audius.co/radiojuicy/tohaj-long-drive) | MP3 | 6,027,982 | 150.672 |

AudioBoom 另通过实际 Web UI 完成自动识别、解析音频、选择格式、自定义文件名称与创建任务。输入无后缀名称，输出为 `AudioBoom-WebUI-验证.mp3`；成品已完成。MP3 内嵌封面由 ffprobe 标记为 `attached_pic`，不是误下载的视频。

以下项目不计为完整下载成功：

- **Odysee**：公开作品页元数据解析成功，媒体探测返回 HTTP 429；尚未完整下载，未通过重复请求规避限流。
- **Clyp**：URL、格式与权限字段回归通过；官方旧样例明确设置 `DownloadDisabled`，真实验证确认拒绝该样例，尚无允许下载的完整成品。

## 支持边界与实现

虎牙读取目标页面的公开 `videoData` 和 `videoDefinitions`，核对作品 ID、完整时长和可播放状态；网易新闻读取目标 `vid` 的公开详情，并依据[当前官方播放器](https://static.ws.126.net/163/wap/f2e/hbs/static/video/js/main.31e2416c.js) 的枚举语义，仅接受 `purchaseType=1` 的完整公开播放，拒绝付费试看；不会将普通新闻中的数字 `payFlag=1` 错当成 Boolean 付费标记。Rumble 通过作品页实际指定的 embed ID 请求官方播放器；不会假设作品 URL 的 ID 等于 embed ID。Odysee 优先读取当前公开页面的完整 claim 信息与官方播放地址，旧 RPC 路径仅作为回退，不宣称旧接口已实测可用。OK.ru 校验 `data-options` 的目标播放器和 movie ID。

Clyp 校验公开音频标识和允许下载标志；AudioBoom 精确选择播放器的单期 clip；Audius 使用官方 resolve 接口，核对永久链接、作者及曲目 ID，拒绝播放或下载 gate。多个无关作品不会伪装成一个清晰度列表，试听、直播、私密、DRM 和其他受保护内容会明确拒绝。

主清单中有清晰度时使用该档位；独立 HLS 媒体清单没有分辨率字段时，保留平台元数据提供的档位名称，避免不同清晰度同名。文件、压缩包和安装包保持直链下载，不因平台域名而误走视频解析；前后端共享 269 个 URL 路由样例。

本次验证使用本机 macOS Web 工作台。完整 workspace 回归 **223 项通过、16 项网络或环境测试默认忽略**，前端 **17 项通过**，Workspace Clippy 与格式检查通过。新增三组适配器的公网元数据测试另行运行；元数据成功不会替代完整下载证据。Windows 实机与正式安装包未作为本次本地验收结论。

## 与 yt-dlp 的覆盖差距

[全量参考对照](yt-dlp-coverage.json) 固定于官方提交 `897341241f2bfb2439fe0367f4c7c9ccd29a3828`，包含 1,731 个 extractor 条目，41 项标为 `partial`、1,690 项标为 `pending`。数量单位是解析器条目，含列表、频道、搜索、直播等，不能与本项目的 41 个平台适配器直接比较。`partial` 仅表示相关的部分单作品能力，仍不具备 yt-dlp 的完整覆盖范围；`pending` 不参与运行时。

网易公开课、斗鱼尚未接入，后续需找到可验证的 Rust 实现方案。当前仍不支持通用 DASH、播放列表批量、直播录制、加密 HLS、纯音频 HLS 主清单、MP3 HLS 或外部音频组。

## 本地验证与更新清单

运行步骤见 [模块说明](resource-resolver.zh-CN.md)，本地工作台为 [http://127.0.0.1:17890/](http://127.0.0.1:17890/)。新建下载后粘贴上表链接，自动识别平台，选择可用格式，再检查实际文件。

参考清单生成脚本仅供维护文档使用，不参与产品解析或下载：

```sh
gh api -H 'Accept: application/vnd.github.raw+json' \
  'repos/yt-dlp/yt-dlp/contents/supportedsites.md?ref=897341241f2bfb2439fe0367f4c7c9ccd29a3828' \
  > /tmp/ffdm-yt-dlp-supportedsites.md
python3 scripts/update-yt-dlp-coverage.py /tmp/ffdm-yt-dlp-supportedsites.md \
  897341241f2bfb2439fe0367f4c7c9ccd29a3828
```

技术参考来自 yt-dlp 官方适配器：[虎牙](https://github.com/yt-dlp/yt-dlp/blob/897341241f2bfb2439fe0367f4c7c9ccd29a3828/yt_dlp/extractor/huya.py)、[Rumble](https://github.com/yt-dlp/yt-dlp/blob/897341241f2bfb2439fe0367f4c7c9ccd29a3828/yt_dlp/extractor/rumble.py)、[Odysee/LBRY](https://github.com/yt-dlp/yt-dlp/blob/897341241f2bfb2439fe0367f4c7c9ccd29a3828/yt_dlp/extractor/lbry.py)、[OK.ru](https://github.com/yt-dlp/yt-dlp/blob/897341241f2bfb2439fe0367f4c7c9ccd29a3828/yt_dlp/extractor/odnoklassniki.py)、[Clyp](https://github.com/yt-dlp/yt-dlp/blob/897341241f2bfb2439fe0367f4c7c9ccd29a3828/yt_dlp/extractor/clyp.py)、[AudioBoom](https://github.com/yt-dlp/yt-dlp/blob/897341241f2bfb2439fe0367f4c7c9ccd29a3828/yt_dlp/extractor/audioboom.py)、[Audius](https://github.com/yt-dlp/yt-dlp/blob/897341241f2bfb2439fe0367f4c7c9ccd29a3828/yt_dlp/extractor/audius.py)。网易新闻依据当前官方公开播放器独立实现，不宣称来自不存在的 yt-dlp 网易公开课适配器。
