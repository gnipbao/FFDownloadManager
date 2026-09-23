# FFDownload 本地 Web 工作台

## 启动

```sh
./scripts/start-web.sh
```

浏览器打开 `http://127.0.0.1:17890`。本地服务只监听回环地址。页面资源直接编译进 Rust 二进制，不依赖 Node、CDN 或外部服务；修改前端后需要重新构建二进制。

也可以直接使用已构建的程序：

```sh
./target/release/ffdm serve --port 17890 \
  --download-dir ./downloads --state-dir ./.ffdm-web
```

在终端按 Ctrl-C 正常退出时，会保存活跃任务的分段进度。关闭浏览器页面不会停止下载。下次使用同样的下载目录和任务目录启动时，未完成任务以“已暂停”恢复；点继续即可重新排队。任务目录与下载目录成对使用，更换下载目录时应选择新的 `--state-dir`。

## 使用流程

1. 点“新建下载”（或 ⌘K），粘贴无需登录的 HTTP / HTTPS 直链。文件名自动读取服务器的 Content-Disposition，支持中文 `filename*`；缺少后缀时结合 Content-Type、跳转后的链接和原链接补全。
2. 可以修改文件名；只输入名称时自动补上识别出的后缀，已手动填写的后缀保留。未知格式不强行改为 `.bin`，可自行填写。选择 1 / 4 / 8 / 16 路分段；默认 4 路，不保证比单连接更快。
3. 在文件行点暂停，等状态变成“已暂停”；点继续，等文件完整落盘。
4. 完成后点文件夹图标在 Finder 定位文件。点文件名或省略号查看来源、保存位置、分段数、时间与 SHA-256。
5. 用左侧分类、搜索和排序管理任务。最多同时运行两个任务，其余排队。同名文件自动添加数字后缀，已有文件不会被覆盖。

可输入发布方的 SHA-256；如果不输入，只计算文件指纹，不宣称经过可信来源校验。“移除记录”只移除列表记录，保留文件与临时分段数据；不会自动清理磁盘。

本地示例链接包含服务端口，跨服务重启继续示例时请使用同一个端口。真实下载源的链接可能会过期；过期后需要重新提供有效直链。

## 下载 YouTube / B 站视频

1. 在同一个“新建下载”窗口粘贴视频页面链接，点击“解析视频”。
2. 选择清晰度、编码和容器，也可选择“仅音频”。视频格式会自动配对音轨；文件后缀随格式切换，不会把只有画面的轨道当作完整视频。
3. 点击“开始下载”，任务沿用原有队列、暂停、继续和详情。分离轨道完成后显示“正在合并”，最终文件完成校验后才发布。

YouTube 的 1080p / 1440p 下载已修复客户端适配并实测。旧的 403 失败任务仍保存旧地址，请点“重新解析视频”，选择高清格式后下载；新任务会使用修复后的地址。可选清晰度取决于原视频和平台返回结果。

解析和下载均在本地 Rust 服务完成。合并需要本机 FFmpeg，使用流复制而非重新编码。服务启动时自动检测 FFmpeg；缺少时需要合并的选项会被禁用。

当前已实测 YouTube 和 B 站公开单视频。其他平台、登录限制、短链、分 P、直播和地址过期等覆盖范围见 [视频解析说明](video-parser-integration.zh-CN.md)。解析结果超过 15 分钟或服务重启后，需要在添加任务前重新解析；已经添加的任务会保存自己的媒体计划。

## 结构与桌面迁移

```text
web/index.html + style.css + app.js   界面、交互与真实进度呈现
               │
          web/api.js                 传输接口
               │
           src/web.rs                Axum 本地 HTTP 宿主
               │
         src/service.rs              持久任务列表、队列、并发与操作
               ├── src/media.rs      ytdown / bbdown-core → 统一媒体计划
               ├── src/media_download.rs  轨道下载、恢复、本地 FFmpeg 合并
               │
         src/download.rs             现有引擎适配、保存进度与最终文件发布
               │
      gosh-dl / Tokio / reqwest       Rust 下载内核
```

macOS 与 Windows 客户端通过 Tauri 宿主复用 `web/` 界面与 `DownloadService`：`api.js` 自动选择桌面 IPC 或浏览器 HTTP，桌面版加入原生菜单、系统下载目录、单实例和退出保存。任务协议与下载控制不依赖 Axum，桌面版不启动 HTTP 服务。详见 [macOS 客户端](macos-client.zh-CN.md)和 [Windows 客户端](windows-client.zh-CN.md)。

当前提供 Web 预览、macOS `.app` / DMG 与 Windows NSIS 安装包；浏览器扩展接管、认证 Cookie 导入、自动更新和自适应连接策略尚未实现。

## 接口

| 方法与路径 | 用途 |
| --- | --- |
| `GET /api/tasks` | 任务快照与保存位置，页面约每秒更新一次 |
| `POST /api/tasks` | 添加 HTTP/HTTPS 任务 |
| `POST /api/filename` | 根据 URL 和服务器响应识别文件名、后缀 |
| `POST /api/media/resolve` | 解析单个视频页面，返回标题、格式和临时选择标识 |
| `POST /api/media/tasks` | 通过解析结果标识和格式标识创建媒体任务 |
| `POST /api/tasks/{id}/pause` | 等内核保存进度后暂停 |
| `POST /api/tasks/{id}/resume` | 继续或重试 |
| `POST /api/pause-all` | 暂停所有进行中和排队任务 |
| `POST /api/tasks/{id}/reveal` | 在 macOS Finder 中定位完整文件 |
| `POST /api/folder/open` | 打开下载文件夹 |
| `DELETE /api/tasks/{id}` | 移除非活跃任务的记录，保留磁盘文件 |
| `GET /api/tasks/{id}/file` | 读取已经完成的文件，用于浏览器另存副本 |

变更请求需要 `X-FFDM-Client: local-ui`，拒绝异源 Origin 和非本机 Host，不启用跨域。保存文件名只允许单层名称；API 不接受任意文件路径。任务状态由临时文件原子替换保存，同一任务目录只能运行一个服务实例。

速度单位为十进制 MB/s、KB/s；文件大小使用二进制 MiB、GiB。图表来自内核返回的实际速度样本，未添加虚构历史数据。预览版每次传输结束会计算 SHA-256，大文件可能在完成传输后短暂显示“正在校验文件”。
