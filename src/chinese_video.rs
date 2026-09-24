//! Public Chinese-platform pages and WeChat Channels' authenticated share flow.
//! The returned URLs are short-lived; callers must keep them inside a media plan.
use anyhow::{bail, ensure, Context, Result};
use percent_encoding::percent_decode_str;
use reqwest_media::{header, redirect::Policy, Client, Url};
use serde_json::{json, Value};
use std::time::Duration;

const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
const DOUYIN_AGENT: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1";
const DOUYIN_FEED_AGENT: &str = "com.ss.android.ugc.aweme/370000 (Linux; U; Android 14; zh_CN)";
const XHS_AGENT: &str = DOUYIN_AGENT;
const MAX_PAGE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const XHS_ORIGINAL_FORMAT: &str = "xhs-original";

#[derive(Debug)]
pub struct NativeVariant {
    pub id: String,
    pub label: String,
    pub height: u32,
    pub url: String,
    pub size: Option<u64>,
    pub headers: Vec<(String, String)>,
    pub decrypt_key: Option<u64>,
}

#[derive(Debug)]
pub struct NativeVideo {
    pub id: String,
    pub title: String,
    pub duration_seconds: Option<u64>,
    pub variants: Vec<NativeVariant>,
}

pub struct ChineseResolver {
    client: Client,
    original_client: Client,
}

impl ChineseResolver {
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .use_native_tls()
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(18))
            .redirect(Policy::custom(|attempt| {
                if attempt.previous().len() >= 5 {
                    attempt.error("too many platform redirects")
                } else if attempt.previous().last().is_some_and(|previous| {
                    (host_matches(previous, "xiaohongshu.com")
                        && !host_matches(attempt.url(), "xiaohongshu.com"))
                        || (host_matches(previous, "yuanbao.tencent.com")
                            && !host_matches(attempt.url(), "yuanbao.tencent.com"))
                }) {
                    attempt.error("authenticated platform redirected to another website")
                } else if allowed_page_host(attempt.url()) {
                    attempt.follow()
                } else {
                    attempt.error("platform redirected outside its public website")
                }
            }))
            .user_agent(USER_AGENT)
            .build()?;
        // Original media requests carry no page Cookie and may redirect only
        // within the platform's CDN. Keep that policy separate from page fetches.
        let original_client = Client::builder()
            .use_native_tls()
            .connect_timeout(Duration::from_secs(4))
            .timeout(Duration::from_secs(6))
            .user_agent(USER_AGENT)
            .redirect(Policy::custom(|attempt| {
                if attempt.previous().len() < 3
                    && attempt.url().scheme() == "https"
                    && host_matches(attempt.url(), "xhscdn.com")
                {
                    attempt.follow()
                } else {
                    attempt.error("original media redirected outside the platform CDN")
                }
            }))
            .build()?;
        Ok(Self {
            client,
            original_client,
        })
    }

    pub async fn resolve(
        &self,
        url: &Url,
        platform: &str,
        auth_cookie: Option<&str>,
    ) -> Result<NativeVideo> {
        match platform {
            "Douyin" => self.douyin(url).await,
            "Xiaohongshu" => self.xiaohongshu(url, auth_cookie).await,
            "WeChat Channels" => self.wechat(url, auth_cookie).await,
            _ => bail!("不支持的视频平台"),
        }
    }

    async fn douyin(&self, input: &Url) -> Result<NativeVideo> {
        // Short links are followed only to obtain the aweme id. Mobile Feed
        // currently returns more videos than the share page's SSR data.
        let id = if let Some(id) = douyin_id(input) {
            id
        } else {
            let response = self
                .client
                .get(input.clone())
                .header(header::USER_AGENT, DOUYIN_AGENT)
                .send()
                .await?;
            ensure!(
                allowed_page_host(response.url()),
                "抖音短链接跳到了其他网站"
            );
            douyin_id(response.url()).context("没有从抖音分享链接识别出作品 ID")?
        };
        for host in ["api5-normal-c-hl.amemv.com", "aweme.snssdk.com"] {
            if let Ok(video) = self.douyin_feed(host, &id).await {
                return Ok(video);
            }
        }
        let page = Url::parse(&format!("https://www.iesdouyin.com/share/video/{id}/"))?;
        let html = self
            .fetch_page_with_url(page, Some(DOUYIN_AGENT), None)
            .await?
            .1;
        parse_douyin(&html, &id)
            .context("抖音公开接口未返回该作品；可能需要登录、受到地区/隐私限制，或作品已失效")
    }

    async fn douyin_feed(&self, host: &str, id: &str) -> Result<NativeVideo> {
        let mut url = Url::parse(&format!("https://{host}/aweme/v1/feed/"))?;
        url.query_pairs_mut()
            .append_pair("aweme_id", id)
            .append_pair("aid", "1128");
        let mut response = self
            .client
            .get(url)
            .header(header::USER_AGENT, DOUYIN_FEED_AGENT)
            .header(header::ACCEPT, "application/json")
            .send()
            .await?;
        ensure!(response.status().is_success(), "抖音 Feed 暂不可用");
        ensure!(
            host_matches(response.url(), "amemv.com") || host_matches(response.url(), "snssdk.com"),
            "抖音 Feed 跳到了其他网站"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= MAX_PAGE_BYTES,
                "抖音 Feed 响应过大"
            );
            bytes.extend_from_slice(&chunk);
        }
        let data: Value = serde_json::from_slice(&bytes)?;
        parse_douyin_feed(&data, id)
    }

    async fn xiaohongshu(&self, input: &Url, cookie: Option<&str>) -> Result<NativeVideo> {
        let cookie = cookie.filter(|value| !value.trim().is_empty());
        if let Some(cookie) = cookie {
            validate_cookie(cookie, "小红书")?;
        }
        let page_url = if host_matches(input, "xhslink.com") || host_matches(input, "xhslink.cn") {
            let (redirected, _) = self
                .fetch_page_with_url(input.clone(), Some(XHS_AGENT), None)
                .await?;
            xiaohongshu_redirect_target(&redirected).unwrap_or(redirected)
        } else {
            input.clone()
        };
        ensure!(
            host_matches(&page_url, "xiaohongshu.com"),
            "小红书短链接没有跳转到笔记页面"
        );
        let (page, html) = self
            .fetch_page_with_url(page_url, Some(XHS_AGENT), cookie)
            .await?;
        ensure!(
            host_matches(&page, "xiaohongshu.com"),
            "小红书分享链接没有跳转到笔记页面"
        );
        if page.path().starts_with("/login") {
            bail!("小红书要求登录；请填写小红书网页版 Cookie 后重新解析");
        }
        if page.path().starts_with("/404") {
            bail!("小红书笔记当前不可浏览；请重新复制有效分享链接");
        }
        let id = page
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .filter(|part| !part.is_empty());
        let mut video = parse_xiaohongshu(&html, id)
            .context("小红书页面没有可下载的视频；请确认是公开视频笔记且分享链接仍有效")?;
        self.verify_xiaohongshu_original(&mut video).await;
        if !video.variants.iter().any(|v| v.id == XHS_ORIGINAL_FORMAT) {
            // Some share pages omit the original key while the desktop state
            // still supplies it. Only accept metadata for this same note.
            if let Ok(Ok((_, html))) = tokio::time::timeout(
                Duration::from_secs(8),
                self.fetch_page_with_url(page.clone(), Some(USER_AGENT), cookie),
            )
            .await
            {
                if let Ok(mut desktop) = parse_xiaohongshu(&html, id) {
                    self.verify_xiaohongshu_original(&mut desktop).await;
                    if let Some(original) = desktop
                        .variants
                        .into_iter()
                        .find(|v| v.id == XHS_ORIGINAL_FORMAT)
                    {
                        video.variants.insert(0, original);
                    }
                }
            }
        }
        ensure!(!video.variants.is_empty(), "小红书原始文件当前不可读取");
        Ok(video)
    }

    async fn verify_xiaohongshu_original(&self, video: &mut NativeVideo) {
        let Some(index) = video
            .variants
            .iter()
            .position(|v| v.id == XHS_ORIGINAL_FORMAT)
        else {
            return;
        };
        match self
            .check_xiaohongshu_original(&video.variants[index].url)
            .await
        {
            Ok(size) => video.variants[index].size = size,
            Err(_) => {
                video.variants.remove(index);
            }
        }
    }

    async fn check_xiaohongshu_original(&self, url: &str) -> Result<Option<u64>> {
        // Read a bounded prefix, never download the whole original while parsing.
        // A CDN error page with HTTP 200 must not become the default MP4 option.
        let mut response = self
            .original_client
            .get(url)
            .header(header::REFERER, "https://www.xiaohongshu.com/")
            .header(header::ACCEPT_ENCODING, "identity")
            .header(header::RANGE, "bytes=0-31")
            .send()
            .await?;
        let size = if response.status() == reqwest_media::StatusCode::PARTIAL_CONTENT {
            let range = response
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .context("原始文件范围响应无效")?;
            let total: u64 = range
                .strip_prefix("bytes 0-31/")
                .context("原始文件范围不匹配")?
                .parse()?;
            ensure!(total > 31, "原始文件为空");
            Some(total)
        } else {
            ensure!(
                response.status() == reqwest_media::StatusCode::OK,
                "原始文件不可读取"
            );
            response.content_length()
        };
        let mut prefix = Vec::with_capacity(32);
        while prefix.len() < 32 {
            let Some(chunk) = response.chunk().await? else {
                break;
            };
            prefix.extend_from_slice(&chunk[..chunk.len().min(32 - prefix.len())]);
        }
        ensure!(
            prefix.len() == 32 && &prefix[4..8] == b"ftyp",
            "原始文件不是 MP4"
        );
        ensure!(&prefix[8..12] != b"qt  ", "原始文件为 QuickTime 容器");
        Ok(size)
    }

    async fn wechat(&self, input: &Url, cookie: Option<&str>) -> Result<NativeVideo> {
        let playable =
            if input.path().ends_with("/feed") && host_matches(input, "channels.weixin.qq.com") {
                input.clone()
            } else {
                let cookie = cookie.filter(|value| !value.trim().is_empty()).context(
                    "视频号普通分享链接需要元宝登录 Cookie；可在解析框中仅为本次解析填写",
                )?;
                validate_cookie(cookie, "元宝")?;
                let share_url =
                    if host_matches(input, "weixin.qq.com") && input.path().starts_with("/sph/") {
                        input.to_string()
                    } else if host_matches(input, "channels.weixin.qq.com")
                        && input.path().ends_with("/sph")
                    {
                        let id = input
                            .query_pairs()
                            .find(|(key, _)| key == "id")
                            .map(|(_, value)| value.into_owned())
                            .context("视频号分享链接缺少 id")?;
                        format!("https://weixin.qq.com/sph/{id}")
                    } else {
                        bail!("请使用视频号分享链接或包含 token、eid 的播放页链接")
                    };
                let request = json!({"type":"video_channel_url","url":share_url,"scene":1});
                let response = self
                    .client
                    .post("https://yuanbao.tencent.com/api/weixin/get_parse_result")
                    .header(header::COOKIE, cookie)
                    .header(header::ORIGIN, "https://yuanbao.tencent.com")
                    .header(header::REFERER, "https://yuanbao.tencent.com/")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(request.to_string())
                    .send()
                    .await
                    .context("元宝分享链接解析请求失败")?;
                ensure!(response.status().is_success(), "元宝拒绝解析此视频号链接");
                let bytes = response.bytes().await?;
                ensure!(bytes.len() <= MAX_PAGE_BYTES, "元宝响应过大");
                let data: Value = serde_json::from_slice(&bytes)?;
                let playable = data["data"]["playable_url"]
                    .as_str()
                    .context("元宝没有返回视频号播放页；请检查登录 Cookie 是否有效")?;
                let playable = Url::parse(playable)?;
                ensure!(
                    host_matches(&playable, "channels.weixin.qq.com"),
                    "元宝返回了非视频号链接"
                );
                playable
            };
        let token = playable
            .query_pairs()
            .find(|(key, _)| key == "token")
            .map(|(_, value)| value.into_owned())
            .filter(|value| !value.is_empty())
            .context("视频号播放页缺少 token")?;
        let eid = playable
            .query_pairs()
            .find(|(key, _)| key == "eid")
            .map(|(_, value)| value.into_owned())
            .filter(|value| !value.is_empty())
            .context("视频号播放页缺少 eid")?;
        let mut endpoint =
            Url::parse("https://channels.weixin.qq.com/finder-preview/api/feed/get_feed_info")?;
        let nonce: String = gosh_dl::DownloadId::new()
            .to_string()
            .chars()
            .take(8)
            .collect();
        let rid = format!("{:x}-{nonce}", crate::service::now_ms() / 1000);
        endpoint
            .query_pairs_mut()
            .append_pair("_rid", &rid)
            .append_pair(
                "_pageUrl",
                "https://channels.weixin.qq.com/finder-preview/pages/feed",
            );
        let request = json!({"baseReq":{"generalToken":token},"exportId":eid});
        let response = self
            .client
            .post(endpoint)
            .header(header::ORIGIN, "https://channels.weixin.qq.com")
            .header(header::REFERER, playable.as_str())
            .header(header::CONTENT_TYPE, "application/json")
            .body(request.to_string())
            .send()
            .await
            .context("视频号详情请求失败")?;
        ensure!(response.status().is_success(), "视频号拒绝获取作品详情");
        let bytes = response.bytes().await?;
        ensure!(bytes.len() <= MAX_PAGE_BYTES, "视频号详情响应过大");
        let data: Value = serde_json::from_slice(&bytes)?;
        parse_wechat(&data, &eid)
    }

    async fn fetch_page_with_url(
        &self,
        url: Url,
        agent: Option<&str>,
        cookie: Option<&str>,
    ) -> Result<(Url, String)> {
        let mut request = self
            .client
            .get(url)
            .header(header::ACCEPT, "text/html,application/xhtml+xml");
        if let Some(agent) = agent {
            request = request.header(header::USER_AGENT, agent);
        }
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        let mut response = request.send().await?;
        ensure!(
            response.status().is_success(),
            "视频页面返回 HTTP {}",
            response.status()
        );
        ensure!(allowed_page_host(response.url()), "视频页面跳到了其他网站");
        let final_url = response.url().clone();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(bytes.len() + chunk.len() <= MAX_PAGE_BYTES, "视频页面过大");
            bytes.extend_from_slice(&chunk);
        }
        Ok((final_url, String::from_utf8_lossy(&bytes).into_owned()))
    }
}

fn host_matches(url: &Url, domain: &str) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case(domain)
            || host.to_ascii_lowercase().ends_with(&format!(".{domain}"))
    })
}

fn validate_cookie(cookie: &str, platform: &str) -> Result<()> {
    ensure!(
        cookie.len() <= 8192 && !cookie.chars().any(char::is_control),
        "{platform} Cookie 格式无效"
    );
    Ok(())
}

fn xiaohongshu_redirect_target(page: &Url) -> Option<Url> {
    if !host_matches(page, "xiaohongshu.com")
        || !(page.path().starts_with("/login") || page.path().starts_with("/404"))
    {
        return None;
    }
    let raw = page
        .query_pairs()
        .find(|(key, _)| key == "redirectPath")?
        .1
        .into_owned();
    let mut target = Url::parse(&raw).ok()?;
    if !host_matches(&target, "xiaohongshu.com")
        || !(target.path().starts_with("/explore/")
            || target.path().starts_with("/discovery/item/"))
    {
        return None;
    }
    target.set_scheme("https").ok()?;
    Some(target)
}

fn allowed_page_host(url: &Url) -> bool {
    url.scheme() == "https"
        && [
            "douyin.com",
            "iesdouyin.com",
            "xiaohongshu.com",
            "xhslink.com",
            "xhslink.cn",
            "weixin.qq.com",
            "channels.weixin.qq.com",
            "yuanbao.tencent.com",
            "amemv.com",
            "snssdk.com",
        ]
        .iter()
        .any(|domain| host_matches(url, domain))
}

fn douyin_id(url: &Url) -> Option<String> {
    if !host_matches(url, "douyin.com") && !host_matches(url, "iesdouyin.com") {
        return None;
    }
    if let Some((_, id)) = url
        .query_pairs()
        .find(|(key, value)| key == "modal_id" && value.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Some(id.into_owned());
    }
    let parts: Vec<_> = url.path_segments()?.collect();
    let id = parts
        .windows(2)
        .find(|pair| matches!(pair[0], "video" | "note"))
        .map(|pair| pair[1])?;
    (id.len() >= 12 && id.bytes().all(|byte| byte.is_ascii_digit())).then(|| id.into())
}

fn embedded_object(html: &str, marker: &str) -> Option<Value> {
    let start = html.find(marker)? + marker.len();
    let start = start + html[start..].find('{')?;
    let bytes = html.as_bytes();
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for i in start..bytes.len() {
        match bytes[i] {
            b'\\' if quoted => escaped = !escaped,
            b'"' if !escaped => quoted = !quoted,
            b'{' if !quoted => depth += 1,
            b'}' if !quoted => {
                depth -= 1;
                if depth == 0 {
                    let raw = &html[start..=i];
                    return serde_json::from_str(raw)
                        .ok()
                        .or_else(|| serde_json::from_str(&clean_js_values(raw)).ok());
                }
            }
            _ => escaped = false,
        }
        if bytes[i] != b'\\' {
            escaped = false;
        }
    }
    None
}

fn clean_js_values(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let mut quoted = false;
    let mut escaped = false;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte == b'"' && !escaped {
            quoted = !quoted;
        }
        if !quoted {
            let literal = [b"undefined".as_slice(), b"NaN", b"-Infinity", b"Infinity"]
                .into_iter()
                .find(|literal| {
                    bytes[i..].starts_with(literal)
                        && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
                        && bytes
                            .get(i + literal.len())
                            .is_none_or(|next| !next.is_ascii_alphanumeric() && *next != b'_')
                });
            if let Some(literal) = literal {
                output.extend_from_slice(b"null");
                i += literal.len();
                escaped = false;
                continue;
            }
        }
        output.push(byte);
        escaped = byte == b'\\' && !escaped;
        i += 1;
    }
    String::from_utf8(output).unwrap_or_else(|_| raw.into())
}

fn find_video_object<'a>(value: &'a Value, id: Option<&str>, depth: usize) -> Option<&'a Value> {
    if depth > 24 {
        return None;
    }
    if let Some(object) = value.as_object() {
        let object_id = object
            .get("aweme_id")
            .or_else(|| object.get("awemeId"))
            .or_else(|| object.get("noteId"))
            .and_then(Value::as_str);
        let video = object.get("video");
        if video.is_some()
            && id.is_none_or(|expected| object_id == Some(expected) || object_id.is_none())
            && (video.unwrap().get("play_addr").is_some()
                || video.unwrap().get("bit_rate").is_some()
                || video.unwrap().get("media").is_some())
        {
            return Some(value);
        }
        for nested in object.values() {
            if let Some(found) = find_video_object(nested, id, depth + 1) {
                return Some(found);
            }
        }
    } else if let Some(array) = value.as_array() {
        for nested in array {
            if let Some(found) = find_video_object(nested, id, depth + 1) {
                return Some(found);
            }
        }
    }
    None
}

fn first_url(value: &Value) -> Option<&str> {
    value
        .get("url_list")
        .or_else(|| value.get("urlList"))
        .and_then(Value::as_array)
        .and_then(|urls| {
            urls.iter()
                .filter_map(Value::as_str)
                .find(|url| url.starts_with("http"))
        })
}

fn parse_douyin(html: &str, id: &str) -> Result<NativeVideo> {
    let data = embedded_object(html, "_ROUTER_DATA")
        .or_else(|| {
            let marker = "id=\"RENDER_DATA\"";
            let start = html.find(marker)?;
            let content = &html[start..];
            let start = content.find('>')? + 1;
            let end = content[start..].find('<')? + start;
            serde_json::from_str(
                &percent_decode_str(&content[start..end])
                    .decode_utf8()
                    .ok()?,
            )
            .ok()
        })
        .context("抖音服务端数据不存在")?;
    let item = find_video_object(&data, Some(id), 0).context("没有找到对应的抖音视频")?;
    parse_douyin_item(item, id)
}

fn parse_douyin_item(item: &Value, id: &str) -> Result<NativeVideo> {
    let video = &item["video"];
    let title = item["desc"]
        .as_str()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or("抖音视频")
        .to_owned();
    let mut variants = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut add = |candidate: &Value, codec: &str, bitrate: Option<u64>| {
        if let Some(url) = first_url(candidate) {
            if !seen.insert(url.to_owned()) {
                return;
            }
            let actual_height = candidate["height"]
                .as_u64()
                .or_else(|| video["height"].as_u64())
                .unwrap_or(0) as u32;
            let width = candidate["width"]
                .as_u64()
                .or_else(|| video["width"].as_u64())
                .unwrap_or(0) as u32;
            let height = if width > 0 {
                width.min(actual_height)
            } else {
                actual_height
            };
            let size = candidate["data_size"]
                .as_u64()
                .or_else(|| candidate["dataSize"].as_u64());
            let bitrate = bitrate.map(|bps| format!(" · {:.1} Mb/s", bps as f64 / 1_000_000.0));
            let label = if height > 0 {
                format!("{height}p · MP4 · {codec}{}", bitrate.unwrap_or_default())
            } else {
                format!("视频 · MP4 · {codec}{}", bitrate.unwrap_or_default())
            };
            variants.push(NativeVariant {
                id: format!("douyin-{}", variants.len()),
                label,
                height,
                url: url.to_owned(),
                size,
                headers: media_headers("https://www.douyin.com/"),
                decrypt_key: None,
            });
        }
    };
    if let Some(rates) = video["bit_rate"].as_array() {
        for rate in rates {
            let codec = if rate["is_h265"].as_i64() == Some(1) {
                "HEVC"
            } else {
                "H.264"
            };
            add(&rate["play_addr"], codec, rate["bit_rate"].as_u64());
        }
    }
    add(&video["play_addr_h264"], "H.264", None);
    add(&video["play_addr_265"], "HEVC", None);
    add(&video["play_addr"], "默认", None);
    ensure!(!variants.is_empty(), "抖音没有提供完整 MP4 地址");
    Ok(NativeVideo {
        id: id.into(),
        title,
        duration_seconds: video["duration"].as_u64().map(|ms| ms / 1000),
        variants,
    })
}

fn parse_douyin_feed(data: &Value, id: &str) -> Result<NativeVideo> {
    let item = data["aweme_list"]
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .find(|item| item["aweme_id"].as_str() == Some(id))
        })
        .context("抖音 Feed 没有收录这个作品")?;
    parse_douyin_item(item, id)
}

fn parse_xiaohongshu(html: &str, id: Option<&str>) -> Result<NativeVideo> {
    let data =
        embedded_object(html, "window.__INITIAL_STATE__").context("小红书服务端数据不存在")?;
    let notes = &data["note"]["noteDetailMap"];
    let mobile_note = &data["noteData"]["data"]["noteData"];
    let note = if mobile_note.is_object()
        && id.is_none_or(|id| mobile_note["noteId"].as_str() == Some(id))
    {
        Some(mobile_note)
    } else if let Some(id) = id {
        notes
            .get(id)
            .and_then(|entry| entry.get("note"))
            .or_else(|| {
                notes.as_object().and_then(|map| {
                    map.values().find_map(|entry| {
                        let note = entry.get("note")?;
                        (note["noteId"].as_str() == Some(id) || note["id"].as_str() == Some(id))
                            .then_some(note)
                    })
                })
            })
    } else {
        notes
            .as_object()
            .and_then(|map| map.values().find_map(|entry| entry.get("note")))
    }
    .context("小红书笔记数据不存在")?;
    ensure!(
        note["type"].as_str() == Some("video"),
        "这是一篇图文笔记，不是视频"
    );
    let stream = &note["video"]["media"]["stream"];
    let title = note["title"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| note["desc"].as_str())
        .unwrap_or("小红书视频")
        .to_owned();
    let mut variants = Vec::new();
    let mut seen = std::collections::HashSet::new();
    if let Some(url) = note["video"]["consumer"]["originVideoKey"]
        .as_str()
        .and_then(xiaohongshu_original_url)
    {
        // These dimensions describe the source, not the smaller playback stream.
        let metadata = &note["video"]["media"]["video"];
        let width = metadata["width"].as_u64().unwrap_or(0) as u32;
        let height = metadata["height"].as_u64().unwrap_or(0) as u32;
        let dimensions = if width > 0 && height > 0 {
            format!(" · {width}×{height}")
        } else {
            String::new()
        };
        seen.insert(url.clone());
        variants.push(NativeVariant {
            id: XHS_ORIGINAL_FORMAT.into(),
            label: format!("原始文件{dimensions} · MP4"),
            height: width.min(height),
            url,
            size: None,
            headers: media_headers("https://www.xiaohongshu.com/"),
            decrypt_key: None,
        });
    }
    for (codec, key) in [("H.264", "h264"), ("HEVC", "h265"), ("AV1", "av1")] {
        if let Some(formats) = stream[key].as_array() {
            for format in formats {
                let Some(url) = format["masterUrl"].as_str().or_else(|| {
                    format["backupUrls"]
                        .as_array()
                        .and_then(|urls| urls.first())
                        .and_then(Value::as_str)
                }) else {
                    continue;
                };
                if !seen.insert(url.to_owned()) {
                    continue;
                }
                let actual_height = format["height"].as_u64().unwrap_or(0) as u32;
                let width = format["width"].as_u64().unwrap_or(0) as u32;
                let height = if width > 0 {
                    width.min(actual_height)
                } else {
                    actual_height
                };
                variants.push(NativeVariant {
                    id: format!("xhs-{key}-{}", variants.len()),
                    label: if height > 0 {
                        format!("网页播放版 · {height}p · MP4 · {codec} · 可能含水印")
                    } else {
                        format!("网页播放版 · MP4 · {codec} · 可能含水印")
                    },
                    height,
                    url: url.into(),
                    size: format["size"].as_u64(),
                    headers: media_headers("https://www.xiaohongshu.com/"),
                    decrypt_key: None,
                });
            }
        }
    }
    ensure!(!variants.is_empty(), "小红书没有提供完整视频文件");
    let duration_seconds = stream["h264"]
        .as_array()
        .and_then(|items| items.first())
        .and_then(|item| item["duration"].as_u64())
        .map(|ms| ms / 1000)
        .or_else(|| note["video"]["capa"]["duration"].as_u64());
    Ok(NativeVideo {
        id: id.unwrap_or("note").into(),
        title,
        duration_seconds,
        variants,
    })
}

fn xiaohongshu_original_url(key: &str) -> Option<String> {
    // The page supplies a CDN object key, not an arbitrary URL. Reject absolute
    // URLs, queries, escaping and path traversal instead of guessing source IDs.
    if key.len() > 1024
        || !key.contains('/')
        || key.split('/').any(|part| {
            part.is_empty()
                || matches!(part, "." | "..")
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
    {
        return None;
    }
    Some(format!("https://sns-video-bd.xhscdn.com/{key}"))
}

fn parse_wechat(data: &Value, eid: &str) -> Result<NativeVideo> {
    ensure!(
        data["errCode"].as_i64().unwrap_or(-1) == 0,
        "视频号无法访问此作品；播放令牌可能已失效"
    );
    let feed = &data["data"]["feedInfo"];
    ensure!(
        feed["mediaType"].as_i64().unwrap_or(1) == 1,
        "这不是视频号视频"
    );
    let title = feed["description"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("视频号视频")
        .to_owned();
    let key = feed["decodeKey"]
        .as_u64()
        .or_else(|| feed["decodeKey"].as_str().and_then(|key| key.parse().ok()))
        .filter(|key| *key != 0);
    let mut variants = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (label, field) in [
        ("原画", &feed["originVideoUrl"]),
        ("H.264", &feed["h264VideoInfo"]["videoUrl"]),
        ("HEVC", &feed["h265VideoInfo"]["videoUrl"]),
        ("默认", &feed["videoUrl"]),
    ] {
        if let Some(url) = field.as_str().filter(|url| !url.is_empty()) {
            if seen.insert(url.to_owned()) {
                variants.push(NativeVariant {
                    id: format!("wechat-{}", variants.len()),
                    label: format!("视频 · MP4 · {label}"),
                    height: 0,
                    url: url.into(),
                    size: None,
                    headers: media_headers("https://channels.weixin.qq.com/"),
                    decrypt_key: key,
                });
            }
        }
    }
    ensure!(!variants.is_empty(), "视频号没有返回完整媒体地址");
    Ok(NativeVideo {
        id: eid.into(),
        title,
        duration_seconds: None,
        variants,
    })
}

fn media_headers(referer: &str) -> Vec<(String, String)> {
    vec![
        ("Referer".into(), referer.into()),
        ("User-Agent".into(), USER_AGENT.into()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_douyin_mobile_page_and_quality_variants() {
        let page = r#"<script>window._ROUTER_DATA = {"loaderData":{"video_1234567890123456789/page":{"videoInfoRes":{"item_list":[{"aweme_id":"1234567890123456789","desc":"城市夜景","video":{"duration":123000,"height":1080,"bit_rate":[{"gear_name":"高清","play_addr":{"url_list":["https://cdn.example/high.mp4"],"height":1080,"data_size":999}},{"gear_name":"流畅","play_addr":{"url_list":["https://cdn.example/low.mp4"],"height":540}}]}}]}}}};</script>"#;
        let video = parse_douyin(page, "1234567890123456789").unwrap();
        assert_eq!(video.title, "城市夜景");
        assert_eq!(video.duration_seconds, Some(123));
        assert_eq!(video.variants.len(), 2);
        assert_eq!(video.variants[0].height, 1080);
    }

    #[test]
    fn douyin_feed_only_selects_requested_video() {
        let data = json!({"aweme_list":[
            {"aweme_id":"other","video":{"play_addr":{"url_list":["https://cdn.example/wrong.mp4"]}}},
            {"aweme_id":"1234567890123456789","desc":"需要的作品","video":{"play_addr":{"url_list":["https://cdn.example/right.mp4"]}}}
        ]});
        let video = parse_douyin_feed(&data, "1234567890123456789").unwrap();
        assert_eq!(video.title, "需要的作品");
        assert_eq!(video.variants[0].url, "https://cdn.example/right.mp4");
        assert!(parse_douyin_feed(&data, "missing").is_err());
    }

    #[test]
    fn parses_xiaohongshu_video_and_rejects_image_note() {
        let video = r#"<script>window.__INITIAL_STATE__={"note":{"noteDetailMap":{"abc123":{"note":{"type":"video","title":"海边","video":{"media":{"stream":{"h264":[{"masterUrl":"https://sns-video.example/video.mp4","height":1080,"size":1234,"duration":2000}]}}}}}}}};</script>"#;
        let result = parse_xiaohongshu(video, Some("abc123")).unwrap();
        assert_eq!(result.variants[0].height, 1080);
        assert_eq!(result.duration_seconds, Some(2));
        let image = video.replace("\"type\":\"video\"", "\"type\":\"normal\"");
        assert!(parse_xiaohongshu(&image, Some("abc123"))
            .unwrap_err()
            .to_string()
            .contains("图文"));
    }

    #[test]
    fn parses_xiaohongshu_mobile_share_state_and_portrait_resolution() {
        let page = r#"<script>window.__INITIAL_STATE__={"noteData":{"data":{"noteData":{"noteId":"abc123","type":"video","title":"竖屏","video":{"media":{"stream":{"h264":[{"masterUrl":"https://sns-video.example/video.mp4","width":720,"height":1280,"size":3660686,"duration":30059}]}}}}}}};</script>"#;
        let result = parse_xiaohongshu(page, Some("abc123")).unwrap();
        assert_eq!(result.variants[0].height, 720);
        assert!(result.variants[0].label.contains("网页播放版"));
        assert!(result.variants[0].label.contains("可能含水印"));
        assert_eq!(result.duration_seconds, Some(30));
        assert!(parse_xiaohongshu(page, Some("other")).is_err());
    }

    #[test]
    fn xiaohongshu_original_uses_source_metadata_in_both_page_layouts() {
        let note = json!({"noteId":"abc123","type":"video","title":"原视频",
            "video":{"consumer":{"originVideoKey":"pre_post/original_123"},
            "capa":{"duration":7},"media":{"video":{"width":882,"height":1920},
            "stream":{"h264":[{"masterUrl":"https://sns-video.example/playback.mp4",
            "width":720,"height":1568,"size":1607131,"duration":7477}]}}}});
        for data in [
            json!({"noteData":{"data":{"noteData":note.clone()}}}),
            json!({"note":{"noteDetailMap":{"abc123":{"note":note}}}}),
        ] {
            let video =
                parse_xiaohongshu(&format!("window.__INITIAL_STATE__={data};"), Some("abc123"))
                    .unwrap();
            let original = &video.variants[0];
            assert_eq!(original.id, XHS_ORIGINAL_FORMAT);
            assert_eq!(
                original.url,
                "https://sns-video-bd.xhscdn.com/pre_post/original_123"
            );
            assert_eq!(original.label, "原始文件 · 882×1920 · MP4");
            assert_eq!(original.height, 882);
            assert_eq!(original.size, None);
            assert_eq!(video.variants[1].size, Some(1607131));
            assert!(video.variants[1].label.contains("可能含水印"));
        }
    }

    #[test]
    fn xiaohongshu_original_keys_cannot_redirect_or_escape_the_cdn() {
        for key in [
            "",
            "https://evil.example/video.mp4",
            "//evil.example/video",
            "/pre_post/abc",
            "pre_post/../abc",
            "pre_post/%2e%2e/abc",
            "pre_post/abc?redirect=evil",
            "pre_post/abc#fragment",
            "pre_post/a\\b",
        ] {
            assert!(xiaohongshu_original_url(key).is_none(), "accepted {key}");
        }
        assert_eq!(
            xiaohongshu_original_url("spectrum/1040g0jg123.mp4").unwrap(),
            "https://sns-video-bd.xhscdn.com/spectrum/1040g0jg123.mp4"
        );
    }

    #[tokio::test]
    async fn xiaohongshu_original_probe_checks_container_and_drops_unreadable_sources() {
        use axum::{body::Body, extract::Request, http::Response, Router};
        async fn fixture(request: Request) -> Response<Body> {
            assert_eq!(request.headers()["range"], "bytes=0-31");
            assert_eq!(request.headers()["referer"], "https://www.xiaohongshu.com/");
            assert!(!request.headers().contains_key("cookie"));
            let mut data = b"\x00\x00\x00\x20ftypisom".to_vec();
            data.resize(32, 0);
            let mut response = Response::builder()
                .status(206)
                .header("content-range", "bytes 0-31/4096");
            match request.uri().path() {
                "/full" => {
                    response = Response::builder().status(200);
                    data.resize(4096, 0);
                }
                "/html" => {
                    response = Response::builder().status(200);
                    data = b"<html>temporarily unavailable</html>".to_vec();
                }
                "/denied" => {
                    response = Response::builder().status(403);
                }
                "/wrong-range" => {
                    response = Response::builder()
                        .status(206)
                        .header("content-range", "bytes 4-35/4096");
                }
                "/truncated" => {
                    data.truncate(12);
                }
                "/quicktime" => {
                    data[8..12].copy_from_slice(b"qt  ");
                }
                _ => {}
            }
            response
                .header("content-length", data.len())
                .body(Body::from(data))
                .unwrap()
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, Router::new().fallback(fixture))
                .await
                .unwrap();
        });
        let mut resolver = ChineseResolver::new().unwrap();
        resolver.original_client = Client::builder().no_proxy().build().unwrap();
        for path in ["/range", "/full"] {
            assert_eq!(
                resolver
                    .check_xiaohongshu_original(&format!("{base}{path}"))
                    .await
                    .unwrap(),
                Some(4096)
            );
        }
        for path in [
            "/html",
            "/denied",
            "/wrong-range",
            "/truncated",
            "/quicktime",
        ] {
            let mut video = NativeVideo {
                id: "note".into(),
                title: "Test".into(),
                duration_seconds: None,
                variants: vec![
                    NativeVariant {
                        id: XHS_ORIGINAL_FORMAT.into(),
                        label: "原始文件".into(),
                        height: 0,
                        url: format!("{base}{path}"),
                        size: None,
                        headers: vec![],
                        decrypt_key: None,
                    },
                    NativeVariant {
                        id: "xhs-h264".into(),
                        label: "网页播放版 · 可能含水印".into(),
                        height: 720,
                        url: format!("{base}/range"),
                        size: Some(4096),
                        headers: vec![],
                        decrypt_key: None,
                    },
                ],
            };
            resolver.verify_xiaohongshu_original(&mut video).await;
            assert_eq!(video.variants.len(), 1, "invalid original offered: {path}");
            assert_eq!(video.variants[0].id, "xhs-h264");
        }
        server.abort();
    }

    #[test]
    fn parses_wechat_decryption_key_without_exposing_it_in_label() {
        let data = json!({"errCode":0,"data":{"feedInfo":{"mediaType":1,"description":"风景","decodeKey":"2136343393","h264VideoInfo":{"videoUrl":"https://cdn.example/encrypted.mp4"}}}});
        let result = parse_wechat(&data, "export/test").unwrap();
        assert_eq!(result.variants[0].decrypt_key, Some(2136343393));
        assert!(!result.variants[0].label.contains("2136343393"));
    }

    #[test]
    fn embedded_state_accepts_js_undefined_without_rewriting_string_values() {
        let html = r#"<script>window.__INITIAL_STATE__={"missing":undefined,"rate":NaN,"text":"undefined"};</script>"#;
        let value = embedded_object(html, "window.__INITIAL_STATE__").unwrap();
        assert!(value["missing"].is_null());
        assert!(value["rate"].is_null());
        assert_eq!(value["text"], "undefined");
    }

    #[test]
    fn xiaohongshu_shortlink_login_redirect_recovers_note_without_leaking_cookie() {
        let page = Url::parse("https://www.xiaohongshu.com/login?redirectPath=http%3A%2F%2Fwww.xiaohongshu.com%2Fdiscovery%2Fitem%2Fabc123%3Fxsec_token%3Dtest").unwrap();
        let target = xiaohongshu_redirect_target(&page).unwrap();
        assert_eq!(target.scheme(), "https");
        assert_eq!(target.path(), "/discovery/item/abc123");
        assert_eq!(
            target
                .query_pairs()
                .find(|(key, _)| key == "xsec_token")
                .unwrap()
                .1,
            "test"
        );
        let foreign = Url::parse(
            "https://www.xiaohongshu.com/login?redirectPath=https%3A%2F%2Fevil.example%2Fvideo",
        )
        .unwrap();
        assert!(xiaohongshu_redirect_target(&foreign).is_none());
    }
}
