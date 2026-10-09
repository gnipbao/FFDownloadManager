# YouTube 提示登录或验证

如果还未出现清晰度选项就提示登录，表示解析阶段被 YouTube 拒绝，与下载连接数或 Windows 文件路径无关。浏览器能播放视频，并不代表客户端的匿名请求也能取得下载地址。

v0.3.7 改进了可用格式的保留逻辑：已经验证高清画面和音频可以读取时，不再因为其他格式不可用而继续尝试客户端、拖到解析超时。截图样本 `dip5HXMpujw` 已在本机网络下无需登录返回 22 个格式，包括 1080p 和 720p；不同电脑的网络和平台验证状态可能不同。

## 需要授权时

1. 在 Windows 的 Chrome 或 Edge 中打开同一个视频，按 YouTube 提示登录或验证，确认能够播放。浏览器和 FFDownload 应使用相同网络或代理。
2. 按 F12 打开开发者工具，选择 Network（网络），刷新页面，选择 `www.youtube.com` 的页面或 `player` 请求。
3. 从 Request Headers（请求标头）复制 `Cookie` 字段的单行值，填入新建下载中的「YouTube 网页 Cookie」，再点击「解析视频」。不要复制 `Set-Cookie`、整段请求头或 Cookie 文件。

不要把 Cookie 发到聊天或工单。它只用于本次解析，输入框在提交后清空；不会读取浏览器配置、写入任务或发送给视频 CDN。下一次解析需要时再填写。

授权解析继续使用 Rust，使用接受网页会话的 TV / Web 播放器身份，并仅向 `https://www.youtube.com` 的解析 API 发送 Cookie 和会话签名。授权请求不跟随重定向，解析结束或取消后释放会话。

Cookie 不能代替账号访问权限，不能保证通过 YouTube 的平台验证。授权过期、网络不一致、受限视频或平台规则变化仍可能使解析失败；不要通过提高连接数反复尝试。

协议适配参考 [yt-dlp 的 YouTube 客户端与会话请求实现](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/youtube/_base.py) 和 [YouTube Cookie 说明](https://github.com/yt-dlp/yt-dlp/wiki/Extractors#exporting-youtube-cookies)；运行时不调用 yt-dlp 或 Python。
