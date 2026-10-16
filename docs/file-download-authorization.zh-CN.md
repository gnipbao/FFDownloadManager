# Cookie 与自定义 Header 授权下载

普通文件返回 HTTP 401，而同一链接在已登录浏览器中能够下载时，下载请求可能缺少网站登录授权。文件类型识别和多连接下载不会自动继承浏览器的登录状态。

Cookie 入口自 v0.3.6 提供，自定义 Header 自 v0.3.8 提供。Web 工作台与桌面客户端共用这一流程，适用于 HTTPS 文件直链，不会把文件 Header 转发给平台视频解析器。正式安装包请以相应 Release 说明为准。

## 使用

新建文件下载时，展开「下载授权 · Cookie / Header（可选）」。已有返回 401/403 的文件任务可点击「授权后重试」；其他暂停或失败的 HTTPS 文件任务，可在任务详情中点击「编辑请求 Header」。

1. 在已经登录该网站的浏览器中打开开发者工具，进入 Network（网络）。
2. 重新开始该文件的下载，选中实际文件请求。
3. 从 Request Headers（请求标头）中复制该文件所需的授权字段。若使用 Cookie，可只将其值填入 Cookie 快捷输入框；其他字段填入「自定义 Header」，每行一个 `名称: 值`。
4. 在 FFDownload 确认显示的 HTTPS 来源与该文件一致，再点击「开始下载」或「授权并继续」。

格式示例（值需替换为你自己的有效值）：

```text
Authorization: Bearer YOUR_ACCESS_TOKEN
Referer: https://example.com/downloads/
User-Agent: MyDownloadClient/1.0
X-API-Key: YOUR_API_KEY
```

支持单独填写 Header，也支持 Cookie 与其他 Header 同时使用。Cookie 快捷输入框与自定义 Header 中的 `Cookie:` 不能重复填写，Header 名称不区分大小写且不能重复。值中的冒号与内部空格会保留；空行忽略。仅填写实际需要的请求字段，不要粘贴整个 HTTP 请求或响应 Header。

最多 48 个字段，名称与值总计不超过 16 KiB；名称必须符合 HTTP Header 语法，值使用不含控制字符的 ASCII 文本。`Host`、`Range`、`If-Range`、条件校验头、`Accept-Encoding`、连接、代理与传输控制字段由下载器管理，不能自行覆盖，以免破坏分段或续传。

凭据只需填入本机工作台，不要发送到聊天、工单或截图中。补充后仍返回 401/403 时，需要检查字段是否有效、来源是否一致、权限是否足够，以及地址是否已经过期；填写 Header 不会自动获得账号权限。

## 保存与续传

- Cookie 与所有用户自定义 Header（包括 User-Agent、Referer）只保留在当前应用进程内，不写入任务记录、下载数据库或分段恢复文件，不通过任务列表接口返回。
- 文件名探测、完整下载、分段请求和恢复检查均使用相同字段。同一次运行中暂停再继续，会复用该任务的授权。删除任务、下载完成或退出服务后，不再保留对应授权。
- 重启后仍保留分段进度，但需要重新填写 Cookie 或自定义 Header 后才能继续，不会自动改为匿名下载。
- 「授权并继续」中填写的非空授权会替换该任务原来的一整组字段，因此应一次填齐该文件需要的字段。普通「继续下载」不填写新授权时，复用当前进程中的值。
- 授权仅适用于填写时的 HTTPS 来源，包括域名和端口。授权下载不跟随重定向；如源站要求跳转，请从浏览器的实际请求中获取最终文件地址。

网页 Cookie 不等于长期下载权限，其有效期和可访问内容由源站决定。当前功能不读取浏览器配置或自动导出浏览器登录信息。

## 本地接口

`POST /api/tasks` 的 `headers` 是名称、值的二元数组；旧 Cookie-only 和匿名调用继续兼容：

```json
{
  "url": "https://files.example.com/archive.zip",
  "headers": [
    ["Authorization", "Bearer YOUR_ACCESS_TOKEN"],
    ["Referer", "https://example.com/downloads/"]
  ],
  "connections": 4
}
```

`POST /api/tasks/{id}/resume` 接收 `{"headers":[["Authorization","Bearer NEW_TOKEN"]]}`；同时可传 `cookie`。空请求体仍表示用已有进程内授权继续。请求遵循现有的本地来源和客户端标识检查，凭据放在 JSON 正文中，不作为访问工作台自身的 HTTP Header。

## 核对实际运行版本

桌面应用与 `ffdm` 是两个独立的可执行文件。只构建桌面应用，不代表项目中原有的 `target/release/ffdm` 已更新。旧版本可能忽略它不认识的 JSON 字段。

本地 Web 工作台建议用以下脚本启动，它会先构建当前源码，再启动对应文件：

```sh
sh scripts/start-web.sh --port 17890
```

也可手动执行 `cargo build --release --locked --bin ffdm`，再运行 `./target/release/ffdm --version` 核对版本后启动服务。v0.3.9 起，启动日志与 Web 工作台侧栏显示实际后端版本；`GET /api/capabilities` 返回 `version` 和 `file_download_headers: true`，便于本地 API 调用方确认授权下载能力。桌面打包脚本也会同步构建命令行，并检查两者版本一致。

持久化脱敏与传输授权是两个入口：存储仅接收脱敏副本，传输从当前进程的 `FileAuthorization` 恢复。不能通过把凭据写进 SQLite 来解决授权失败。


## 本地验证（2026-10-16）

- 完整 workspace 回归 235 项通过，16 项网络或环境测试默认忽略；前端 28 项通过，Clippy 与格式检查通过。
- 原生 HTTPS 测试源强制检查 Authorization、X-API-Key、Referer、User-Agent。首次授权成功下载分别覆盖 1／4 连接，每次 HEAD／GET 的凭据、User-Agent 与 Accept-Encoding 均恰好一次，成品逐字节一致；传输期间和完成后检查 SQLite／WAL／恢复文件不含凭据。
- 失败后替换授权、保留分段的暂停续传、重定向隔离以及旧 Cookie-only／空请求体续传继续通过测试。
- 使用独立的旧版 v0.3.4 CLI 复现了 Bearer 返回 401、Cookie／Referer 丢失以及重复 `identity` 的报告现象。该 CLI 没有被此前的桌面构建更新，不能根据桌面应用版本判断它的能力。
- 重编的 release v0.3.9 通过 `POST /api/tasks` 访问公开 `httpbin.org/bearer` 和 `httpbin.org/headers`：Bearer、Cookie-only、Cookie 与自定义 Header 组合均完成下载，响应确认服务端实际收到字段；检查断点与任务记录没有保存测试凭据。回显端点下载得到的 JSON 本身包含测试值，是请求的文件内容，不属于断点元数据。
- 公网验证仅使用专用测试值；安全结果在本机 `.bench-work/auth-runtime-fix/new-version-results-safe.json`。
