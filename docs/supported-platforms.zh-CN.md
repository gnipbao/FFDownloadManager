# 支持平台与下载验证清单

更新日期：2026-10-14。当前源码包含 **41 个具名平台适配器**，另支持 HTTP/HTTPS 文件直链和通用网页媒体解析。平台适配器表示已实现指定链接与播放数据的处理，**不代表该平台的所有作品、清晰度或账号权限均已支持**。正式安装包的能力以对应 Release 为准；下文近期验证使用本机 macOS Web 工作台，不等同于 Windows 安装包验收。

本文区分三种结果：**完整下载**有成品文件及大小、时长、音视频轨检查；**历史完整下载**来自此前报告，本轮没有重新验证；**待验收／受限**表示已实现适配，但尚无本轮完整成品证据。元数据解析成功、出现格式列表或读取首段，不记为完整下载成功。

## 平台与支持边界

输入单个作品的公开页面，使用“自动识别”解析可用格式。任意 PeerTube、Mastodon 实例可选择“网页媒体解析”；模块先确认页面为 HTML，再查询同源的目标作品接口。同形路径若实际返回 ZIP 等文件，仍按普通文件处理。

### 原有平台

| # | 平台 | 当前范围与限制 | 验证状态 |
| ---: | --- | --- | --- |
| 1 | YouTube | 单视频、短视频与分享链接；支持独立视频／音频格式和分离轨合并。可用清晰度取决于当次播放源；不支持频道、播放列表批量下载、直播或 DRM。 | 历史完整下载：240p、1080p、1440p，见下方历史成品。 |
| 2 | 哔哩哔哩 Bilibili | 单视频、短分享链接与当前分 P；DASH 音视频轨在本地合并。不是整个合集或所有分 P 批量下载。 | 历史完整下载：当前分 P、288p MP4；不据此承诺所有高清档位可用。 |
| 3 | 抖音 Douyin | 单视频、分享链接及带目标 `modal_id` 的作品入口；可用 H.264／HEVC MP4。精确匹配作品，不使用推荐视频替代。 | 历史完整下载：720p MP4；登录验证或访问限制可能使其他作品不可用。 |
| 4 | 小红书 Xiaohongshu | 单条视频笔记与短分享链接；优先使用页面提供的原始播放源。遇到登录页可手动提供本次解析 Cookie；不支持图集批量下载，也不保证所有源均无水印。 | 历史完整下载：720p MP4。 |
| 5 | 微信视频号 WeChat Channels | 普通分享页经一次性元宝 Cookie 解析；已具备处理播放地址与可用 `decodeKey` 的流程。加密视频缺少有效密钥时不能生成可播放成品。 | **真实作品下载与解密仍待验收**。系统代理与资源捕获不属于当前发布版可用功能。 |
| 6 | 汽水音乐 Qishui | 单曲分享页；通过临时独立无头浏览器读取该曲实际播放请求，再交给 Rust 下载。需要本机 Chrome／Edge 等兼容浏览器；仅有试听时明确按片段展示，不承诺完整曲库或无损格式。 | 历史完整下载：M4A／AAC，153.266 秒。 |
| 7 | TikTok | 已接入单条视频链接解析，使用当前适配器返回的可访问格式；不包含主页、合集或直播。 | 适配器已接入；本清单无完整成品验收记录。 |
| 8 | Instagram | 已接入单条帖子／Reel 解析；不承诺账号主页、相册批量、私密内容或登录后内容。 | 适配器已接入；本清单无完整成品验收记录。 |
| 9 | X / Twitter | 已接入单条视频帖子解析；普通归档 ZIP、图片和其他文件链接走文件下载入口。登录文件需使用自己的有效授权。 | 视频解析适配器已接入；本清单无视频完整成品验收记录。文件下载与视频解析分别处理。 |
| 10 | Reddit | 已接入单条帖子的视频解析；不包含社区、用户主页或批量帖子下载。 | 适配器已接入；本清单无完整成品验收记录。 |

### 第一批扩展

| # | 平台 | 当前范围与限制 | 验证状态 |
| ---: | --- | --- | --- |
| 11 | 快手 Kuaishou | 公开单作品、移动分享与短链；AVC／HEVC MP4，不抓取推荐流或主页。 | 本机完整下载：720p AVC。 |
| 12 | 微博 Weibo | 单条状态、移动详情与 TV 视频页；公开 MP4。匿名访客初始化仅用于元数据请求。 | 本机完整下载：720p MP4。 |
| 13 | AcFun | `/v/ac数字` 与指定 `_分P`；H.264／HEVC HLS。只处理选定分 P，不批量下载全部分 P。 | 本机完整下载：360p H.264；封装失败后复用 35 个片段继续成功。 |
| 14 | Vimeo | 公开单视频／嵌入页的可访问非 DRM MP4／HLS。 | 实验适配；样本遇到访问限制、失效或 DRM，未完成下载验收。 |
| 15 | Dailymotion | 公开单视频、短链；可访问 MP4／HLS。 | 实验适配；本机样本媒体请求被 CDN 拒绝，未完成下载验收。 |
| 16 | Streamable | 公开单视频、嵌入／分享页；MP4。 | 本机完整下载：480p MP4。 |
| 17 | Imgur | 单个视频与 gifv 页面；MP4。图集不作为单个视频处理，原作品可能无声。 | 本机完整下载：MP4；样本原本无音轨。 |

### 第二批扩展

| # | 平台 | 当前范围与限制 | 验证状态 |
| ---: | --- | --- | --- |
| 18 | 央视网 CCTV | 公开单视频页面；完整 HLS 点播档位，下载后封装 MP4。 | 本机完整下载：270p HLS → MP4，并通过 Web UI 创建任务。 |
| 19 | 西瓜视频 Ixigua | 公开单作品 SSR 数据中的完整音视频文件；不列出无法配对的动态分离轨。 | 当前公开样本没有返回播放数据，遇到验证或登录限制，未完成下载验收。 |
| 20 | 梨视频 PearVideo | `/video_数字` 的公开单视频；原始 MP4。 | 本机完整下载：MP4。 |
| 21 | Internet Archive / Archive.org | 指定 item 的原始媒体；多个独立作品时需明确目标文件，不自动把集合首条当作所需作品。 | 本机完整下载：240p MP4；另有格式可用性取决于条目。 |
| 22 | TED | `/talks/作品名` 的公开单个演讲；返回的可访问 MP4／HLS。 | 本机完整下载：480p MP4。 |
| 23 | Wikimedia Commons | `File:` 视频／音频页面与可访问原始媒体，保留 WebM 等原容器。来源策略为每任务单连接、上限 1.4 MB/s，并遵循限流冷却。 | 本机完整下载：1080p WebM；旧失败任务继续成功。 |
| 24 | Bluesky | 精确匹配公开单条帖子的原生视频 HLS；不替换为引用或回复中的媒体。 | 本机完整下载：360p HLS → MP4；同一样本另解析到 720p。 |
| 25 | PeerTube | 公开单视频 `/w/短ID` 或 `/videos/watch/UUID`；支持常见实例和显式网页解析发现的其他实例。不含私密、口令、直播或播放列表。 | 本机完整下载：1080p MP4；本地回归覆盖未知实例发现与同形 ZIP 文件。 |

### 第三批扩展

| # | 平台 | 当前范围与限制 | 验证状态 |
| ---: | --- | --- | --- |
| 26 | 搜狐视频 Sohu | 公开单视频／单章节及对应编码链接；不把多章节的第一章当成完整作品。文件大小以实际媒体响应为准。 | 本机完整下载：480p MP4。 |
| 27 | 好看视频 Haokan | 指定 `vid` 的公开单视频 MP4；不取推荐项替代。 | 本机完整下载：480p MP4。 |
| 28 | Medal | 公开单个游戏片段；MP4。 | 本机完整下载：1080p MP4；首次连接失败后继续任务成功。 |
| 29 | 9GAG | 单个 gag 中的目标视频与可访问格式；不支持相册／多个独立媒体合并选择。 | 本机完整下载：288p H.264 MP4；另解析到 HEVC、VP8／VP9。 |
| 30 | Tumblr | 公开单条帖子的原生视频／音频，或可委派给已支持平台的嵌入内容；多个独立附件不混选，引用媒体不冒充本帖。 | 本机完整下载：720p MP4。 |
| 31 | Mastodon | 精确匹配作者、状态 ID 的 `public`／`unlisted` 单媒体帖子；视频、音频与 gifv 原文件。不支持私密状态、转发替代或多附件混选。 | 本机完整下载：原始 MP4；样本原本无音轨。 |
| 32 | Bandcamp | 艺术家站点的 `/track/曲名` 公开完整单曲 MP3；不批量下载专辑，不把试听当完整曲目。 | 本机完整下载：MP3；另通过 Web UI 创建任务。 |
| 33 | SoundCloud | 作者／曲名单曲页；完整 progressive MP3 或 AAC HLS。检查完整时长与 `snipped` 等字段，排除试听、未知编码与加密格式。 | 本机完整下载：AAC HLS → M4A；不承诺每首歌都提供 MP3。 |

### 第四批扩展

| # | 平台 | 当前范围与限制 | 验证状态 |
| ---: | --- | --- | --- |
| 34 | 虎牙点播 HuyaVideo | `/video/play/数字.html` 的公开完整点播；MP4／未加密 VOD HLS，匹配作品 ID、可播放状态和完整时长。不支持直播房间、付费保护或短预览。 | 本机完整下载：360p MP4；另解析到 720p、1080p。 |
| 35 | 网易新闻视频 NetEaseNews | `/v/video/V字母数字.html` 的公开单视频；MP4／未加密 VOD HLS，匹配目标 `vid` 与完整时长。不支持直播、付费保护、试看或推荐替代。 | 本机完整下载：39 秒 HLS → MP4。网易公开课另属待支持项目。 |
| 36 | Rumble | 单作品页面与 embed 播放器；可访问 MP4／WebM／HLS 点播。不接受直播、预约、尚在生成的录播或受保护／预览内容。 | 本机完整下载：360p MP4。 |
| 37 | Odysee / LBRY | 指定作品 claim 的公开音视频及嵌入页；核对作品与频道身份。仅免费公开点播，不含付费、私密、直播、频道或列表批量下载。 | 公开元数据验证通过；媒体请求返回 HTTP 429，尚未完整下载。 |
| 38 | OK.ru | `/video/数字`、`/videoembed/数字` 的公开视频；原生 MP4／HLS 或已支持平台的嵌入内容。需匹配目标 ID，不含登录限制、DRM 或直播。 | 本机完整下载：144p MP4；其他档位以当次可用性为准。 |
| 39 | Clyp | 单个公开音频 ID；API 明确允许的 MP3／OGG。拒绝私密、已删除、禁止下载和需要私密令牌的条目。 | 解析回归与真实禁止下载样本的拒绝检查通过；**尚无允许下载样本的完整成品验收**。 |
| 40 | AudioBoom | `/posts/数字-标题` 或 `/boos/数字-标题` 的公开单集 MP3；核对集数 ID，不选择播放器其他集数，不接受私密或试听。 | 本机完整下载：MP3；另通过 Web UI 完成自动识别、解析和创建任务。 |
| 41 | Audius | `/作者/曲名` 的公开完整单曲 MP3；核对作者、永久链接与曲目 ID。拒绝未公开、流播放／下载门槛、不可访问或试听替代。 | 本机完整下载：MP3。 |

**尚未实现：网易公开课、斗鱼。** 它们不计入上述 41 个平台，也不因为列在参考清单中而变成可用功能。

## 完整下载成品清单

大小均为实际文件字节数，时长由成品检测得到。下表仅公开作品页面与本地成品名称，不发布媒体 CDN 签名地址或 Cookie。近期本机报告另记录 SHA-256 和音视频轨；文件名是验证时的输出名称，不代表产品会固定使用这些名字。

### 历史样本（非本轮重新验证）

记录来自 [视频解析验证](video-parser-integration.zh-CN.md) 与 [汽水音乐验证](qishui-browser-parser.zh-CN.md)。旧报告未记录成品文件名的项目明确保留为空，不补造文件名。

| 平台／公开样例 | 成品文件名 | 格式／编码 | 大小（字节） | 时长（秒） |
| --- | --- | --- | ---: | ---: |
| [YouTube：Me at the zoo](https://www.youtube.com/watch?v=jNQXAC9IVRw) | `YouTube原生下载测试.mp4` | MP4，H.264 + AAC，320×240 | 744,412 | 19.064 |
| [YouTube：Russian Roulette](https://www.youtube.com/watch?v=S8Z3gGqc7W4) | `Russian Roulette (1080p).mp4` | MP4，H.264 + AAC，1920×1080 | 25,759,097 | 68.058 |
| [YouTube：Russian Roulette](https://www.youtube.com/watch?v=S8Z3gGqc7W4) | `Russian Roulette (1440p).mp4` | MP4，AV1 + AAC，2560×1440 | 31,894,494 | 68.058 |
| [Bilibili：av170001 当前分 P](https://www.bilibili.com/video/av170001) | `Bilibili原生下载测试.mp4` | MP4，H.264 + AAC，512×288 | 6,948,831 | 199.334 |
| [抖音公开样例](https://www.douyin.com/video/7683055498556023653) | 旧报告未记录 | MP4，H.264 + AAC，720×1280 | 2,449,067 | 12.300 |
| [小红书公开短链](https://xhslink.cn/o/5cK7n3LRjpK) | 旧报告未记录 | MP4，H.264 + AAC，720×1280 | 3,660,686 | 30.059 |
| [汽水音乐公开单曲页](https://music.douyin.com/qishui/share/track?track_id=7687463141597169715) | 旧报告未记录 | M4A，AAC，44.1 kHz 双声道 | 2,480,336 | 153.266 |

### 第一批本机验证

详细范围见 [第一批验证说明](platform-expansion-local.zh-CN.md)。本机报告：`.bench-work/platform-expansion/domestic-verification.json`、`.bench-work/platform-expansion/international-verification.json`。

| 平台／公开样例 | 成品文件名 | 格式／编码 | 大小（字节） | 时长（秒） |
| --- | --- | --- | ---: | ---: |
| [快手](https://www.kuaishou.com/short-video/3x25msbsnncn8cs) | `domestic-kuaishou-1791963254298.mp4` | MP4，H.264 + AAC，720×720 | 9,409,048 | 33.707 |
| [微博](https://weibo.com/tv/show/1034:4797699866951785) | `domestic-weibo-1791963254298.mp4` | MP4，H.264 + AAC，1280×720 | 7,067,211 | 75.953 |
| [AcFun](https://www.acfun.cn/v/ac35457073) | `domestic-acfun-1791963254298.mp4` | HLS → MP4，H.264 + AAC，360×640 | 12,947,235 | 174.372 |
| [Streamable](https://streamable.com/moo) | `me irl.mp4` | MP4，H.264 + AAC，852×480 | 3,044,857 | 12.012 |
| [Imgur](https://imgur.com/A61SaA1) | `international-imgur-1791963444777.mp4` | MP4，H.264，916×390；原作品无声 | 2,744,289 | 12.000 |

Vimeo、Dailymotion 没有成功成品，未列入上表。

### 第二批本机验证

详细范围见 [第二批验证说明](platform-expansion-batch2.zh-CN.md)。本机报告：`.bench-work/platform-batch2/download-verification.json`。

| 平台／公开样例 | 成品文件名 | 格式／编码 | 大小（字节） | 时长（秒） |
| --- | --- | --- | ---: | ---: |
| [央视网](https://tv.cctv.com/2016/02/05/VIDEUS7apq3lKrHG9Dncm03B160205.shtml) | `央视网-WebUI-验证.mp4` | HLS → MP4，H.264 + AAC，480×270 | 2,206,756 | 37.960 |
| [梨视频](https://www.pearvideo.com/video_1804394) | `扩展2-PearVideo-验证.mp4` | MP4，H.264 + AAC，720×406 | 45,437,385 | 412.224 |
| [PeerTube](https://framatube.org/w/kkGMgK9ZtnKfYAgnEtQxbv) | `扩展2-PeerTube-验证.mp4` | MP4，H.264 + AAC，1920×1080 | 16,815,344 | 114.063 |
| [TED](https://www.ted.com/talks/terry_moore_how_to_tie_your_shoes) | `扩展2-TED-验证.mp4` | MP4，H.264 + AAC，640×480 | 21,820,397 | 158.592 |
| [Bluesky](https://bsky.app/profile/bsky.app/post/3l3vgf77uco2g) | `扩展2-Bluesky-验证.mp4` | HLS → MP4，H.264 + AAC，640×360 | 1,824,537 | 29.760 |
| [Wikimedia Commons](https://commons.wikimedia.org/wiki/File:Two-toed_sloth_rail_walking_%E4%BA%8C%E8%B6%BE%E6%A8%B9%E7%8D%BA%E7%88%AC%E8%A1%8C_(HD).webm) | `扩展2-WikimediaCommons-验证.webm` | WebM，VP9 + Opus，1920×1080 | 46,191,451 | 25.688 |
| [Archive.org](https://archive.org/details/BigBuckBunny_328) | `扩展2-Archive-org-验证.mp4` | MP4，H.264 + AAC，416×240 | 43,315,070 | 596.417 |

西瓜视频样本未完成下载，未列入成品表。

### 第三批本机验证

详细范围见 [第三批验证说明](platform-expansion-batch3.zh-CN.md)。本机报告：`.bench-work/platform-batch3/download-verification.json`。本批 8 个平台各有一个完整下载样本。

| 平台／公开样例 | 成品文件名 | 格式／编码 | 大小（字节） | 时长（秒） |
| --- | --- | --- | ---: | ---: |
| [搜狐视频](https://tv.sohu.com/v/dXMvMjMyNzk5ODg5Lzc4NjkzNDY0LnNodG1s.html) | `扩展3-Sohu-验证.mp4` | MP4，H.264 + AAC，848×480 | 14,265,796 | 213.066 |
| [好看视频](https://haokan.baidu.com/v?vid=18435139826687843612) | `扩展3-Haokan-验证.mp4` | MP4，H.264 + AAC，852×480 | 1,521,274 | 47.136 |
| [Medal](https://medal.tv/games/valorant/clips/jTBFnLKdLy15K) | `扩展3-Medal-验证.mp4` | MP4，H.264 + AAC，1920×1080 | 15,771,882 | 13.035 |
| [9GAG](https://9gag.com/gag/ae5Ag7B) | `扩展3-9GAG-验证.mp4` | MP4，H.264 + AAC，460×288 | 2,067,118 | 44.650 |
| [Tumblr](https://maskofthedragon.tumblr.com/post/626907179849564160) | `扩展3-Tumblr-验证.mp4` | MP4，H.264 + AAC，1280×720 | 1,428,784 | 7.621 |
| [Mastodon](https://mastodon.social/@Mastodon/116296397062584045) | `扩展3-Mastodon-验证.mp4` | MP4，H.264，2680×1600；原作品无声 | 999,172 | 14.283 |
| [Bandcamp](https://youtube-dl.bandcamp.com/track/youtube-dl-test-song) | `扩展3-Bandcamp-验证.mp3` | MP3，128 kbps | 158,405 | 9.874 |
| [SoundCloud](https://soundcloud.com/ethmusic/lostin-powers-she-so-heavy) | `扩展3-SoundCloud-验证.m4a` | AAC HLS → M4A | 1,744,140 | 143.206 |

Bandcamp 另经实际 Web UI 解析、选格式并创建任务，得到 `Bandcamp-WebUI-验证.mp3`。

### 第四批本机验证（持续补充）

本机报告：`.bench-work/platform-batch4/download-verification.json`。另有视频报告 `.bench-work/platform-batch4/video-download-verification.json`。完整结果见 [公开校验报告](platform-download-verification.json) 与 [第四批说明](platform-expansion-batch4.zh-CN.md)；未完成项不计作下载成功。

| 平台／公开样例 | 成品文件名 | 格式／编码 | 大小（字节） | 时长（秒） |
| --- | --- | --- | ---: | ---: |
| [AudioBoom：NHL @TheRink Trailer](https://audioboom.com/posts/7364167-nhl-therink-trailer) | `扩展4-AudioBoom-验证.mp3` | MP3，保留内嵌封面 | 1,970,350 | 118.592 |
| [Audius：Tohaj — Long Drive](https://audius.co/radiojuicy/tohaj-long-drive) | `扩展4-Audius-验证.mp3` | MP3 | 6,027,982 | 150.672 |
| [网易新闻](https://www.163.com/v/video/VD868N253.html) | `扩展4-NetEaseNews-验证.mp4` | HLS → MP4，H.264 + AAC，1080×1920 | 4,206,066 | 39.126 |
| [虎牙点播](https://www.huya.com/video/play/1002412640.html) | `扩展4-HuyaVideo-验证.mp4` | MP4，H.264 + AAC，576×360 | 823,278 | 14.211 |
| [Rumble](https://rumble.com/embed/vb0ofn) | `扩展4-Rumble-验证.mp4` | MP4，H.264 + AAC，640×360 | 8,231,997 | 103.637 |
| [OK.ru](https://ok.ru/videoembed/20079905452) | `扩展4-OK-ru-验证.mp4` | MP4，H.264 + AAC，256×144 | 1,401,621 | 100.960 |

AudioBoom 另经 Web UI 完成“自动识别 → 解析音频 → 自定义无后缀名称 → 创建任务”，生成 `AudioBoom-WebUI-验证.mp3`。MP3 内嵌的 MJPEG 封面不是视频轨内容，保留封面不影响其作为音频文件下载。

本批尚待完整下载验收的公开页面包括 [Odysee](https://odysee.com/@Mantega:1/First-day-LBRY:1)；页面可访问、解析到格式或读到首段都不能替代成品验收。

## 通用下载能力

- **文件直链**：HTTP／HTTPS 的 ZIP、PDF、安装包、任意自定义后缀或无后缀文件均可进入下载任务。名称和后缀结合响应头与 URL 识别；支持 Range 时分段并行与暂停续传，不支持时回退单连接。需要登录的文件可手动提供本次授权；没有有效权限时保留 HTTP 错误，不把错误页面保存成成品。
- **通用网页媒体**：发现静态 HTML5 `video`／`audio`／`source`、Open Graph、JSON-LD、播放器配置和已支持平台的嵌入内容。并不等于可解析任意动态网站；汽水音乐使用单独的浏览器播放捕获流程。
- **HLS 点播**：支持未加密、已经结束的媒体清单，TS／fMP4 视频以及 TS／fMP4／ADTS 中的 AAC 音频；音频封装为 M4A，视频封装为 MP4。支持初始化段、已有片段复用和失败后继续。平台主清单需能选出独立完整档位。
- **明确不支持**：通用 DASH 清单、播放列表批量、直播录制、DRM／加密 HLS、`BYTERANGE`、时间轴或编码切换、缺失分段、低延迟 HLS、外部分离音频组、纯音频主清单与 MP3 HLS。已有平台专用的分离轨方案（例如 YouTube、B 站）不等同于通用 DASH 支持。
- **来源限制**：签名地址过期后可能需要重新解析；平台登录、地区、付费与访问限制可能使样例失效。清晰度以当次实际可读取的格式为准，下载速度还受源站限流、网络和磁盘影响。

解析、清单读取与下载由 Rust 完成；运行时不调用 Python 或 yt-dlp。FFmpeg 对已下载的本地音视频做无转码封装。汽水音乐的页面执行依赖本机兼容浏览器。

## yt-dlp 参考清单的含义

[覆盖追踪 JSON](yt-dlp-coverage.json) 固定于 yt-dlp 提交 `897341241f2bfb2439fe0367f4c7c9ccd29a3828` 的站点快照，记录 **1,731 个 extractor 条目：41 个 `partial`、1,690 个 `pending`**。

- `partial` 仅表示存在有限的 Rust 单作品能力，既不代表该 extractor 完整实现，也不代表已完成真实下载验收。
- `pending` 表示尚未实现，不能当作支持平台宣传。
- extractor 条目与平台数量不是同一口径：同一平台可能有视频、频道、合集等多个 extractor，本项目也有独立接入的平台。此处两个“41”不能据此建立逐项对应关系。

参考清单用于后续适配规划，**不表示本项目已经具备 yt-dlp 的完整平台覆盖**。具体可用范围与下载证据以上表为准。
