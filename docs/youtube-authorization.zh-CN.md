# YouTube 提示登录或验证

如果还未出现清晰度选项就提示登录，表示解析阶段被 YouTube 拒绝，与下载连接数或 Windows 文件路径无关。浏览器能播放视频，并不代表客户端的匿名请求也能取得下载地址。

如果窗口里没有「YouTube 网页 Cookie」输入框，且仍提示「当前版本支持无需登录的公开视频」，请先确认安装包版本：这是旧版的提示，不能通过调整文件夹权限解决。新版本提供单次网页会话授权；源码更新并不代表 Releases 中的安装包已经更新。

## Windows 系统代理

在 v2rayN 等工具中开启系统代理后重新点击解析即可。解析器现在会在每次请求开始时重新读取系统代理；该次 API 请求、视频地址探测及清晰度复查共用本次网络客户端，不再混用软件启动时的旧代理配置。

如果更换了代理节点，请重新解析获取视频地址。浏览器中的登录或验证仍需使用同一网络；代理刷新本身不会解除账号、年龄或地区访问限制。

v0.3.7 改进了可用格式的保留逻辑：已经验证高清画面和音频可以读取时，不再因为其他格式不可用而继续尝试客户端、拖到解析超时。截图样本 `dip5HXMpujw` 已在本机网络下无需登录返回 22 个格式，包括 1080p 和 720p；不同电脑的网络和平台验证状态可能不同。

## 需要授权时

1. 在 Windows 的 Chrome 或 Edge 中打开同一个视频，按 YouTube 提示登录或验证，确认能够播放。浏览器和 FFDownload 应使用相同网络或代理。
2. 按 F12 打开开发者工具，选择 Network（网络），刷新页面，选择 `www.youtube.com` 的页面或 `player` 请求。
3. 从 Request Headers（请求标头）复制 `Cookie` 字段的单行值，填入新建下载中的「YouTube 网页 Cookie」，再点击「解析视频」。不要复制 `Set-Cookie`、整段请求头或 Cookie 文件。

不要把 Cookie 发到聊天或工单。它只用于本次解析，输入框在提交后清空；不会读取浏览器配置、写入任务或发送给视频 CDN。下一次解析需要时再填写。

授权解析继续使用 Rust，使用接受网页会话的 TV / Web 播放器身份，并仅向 `https://www.youtube.com` 的解析 API 发送 Cookie 和会话签名。授权请求不跟随重定向，解析结束或取消后释放会话。

Cookie 不能代替账号访问权限，不能保证通过 YouTube 的平台验证。授权过期、网络不一致、受限视频或平台规则变化仍可能使解析失败；不要通过提高连接数反复尝试。

协议适配参考 [yt-dlp 的 YouTube 客户端与会话请求实现](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/youtube/_base.py) 和 [YouTube Cookie 说明](https://github.com/yt-dlp/yt-dlp/wiki/Extractors#exporting-youtube-cookies)；运行时不调用 yt-dlp 或 Python。
