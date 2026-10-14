//! Public single-work adapters. The site metadata is interpreted in Rust;
//! neither Python nor yt-dlp is loaded at runtime.
use crate::chinese_video::{NativeVariant, NativeVideo};
use anyhow::{bail, ensure, Context, Result};
use reqwest_media::{header, redirect::Policy, Client, RequestBuilder, Response, Url};
use serde_json::{json, Value};
use std::{collections::HashSet, time::Duration};

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
const MOBILE_AGENT: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1";
const MAX_PAGE_BYTES: usize = 8 * 1024 * 1024;

pub struct DomesticResolver {
    kuaishou_client: Client,
    acfun_client: Client,
    weibo_client: Client,
}

impl DomesticResolver {
    pub fn new() -> Result<Self> {
        Ok(Self {
            kuaishou_client: page_client("Kuaishou")?,
            acfun_client: page_client("AcFun")?,
            // The anonymous visitor token is attached to exact metadata
            // endpoints only. Handle the visitor response explicitly so no
            // Cookie can travel with an automatic redirect.
            weibo_client: Client::builder()
                .use_native_tls()
                .https_only(true)
                .connect_timeout(Duration::from_secs(8))
                .timeout(Duration::from_secs(18))
                .redirect(Policy::none())
                .user_agent(USER_AGENT)
                .build()?,
        })
    }

    pub async fn resolve(&self, url: &Url, platform: &str) -> Result<NativeVideo> {
        ensure!(allowed_page(url, platform), "不支持该平台的页面地址");
        match platform {
            "Kuaishou" => self.kuaishou(url).await,
            "Weibo" => self.weibo(url).await,
            "AcFun" => self.acfun(url).await,
            _ => bail!("不支持的视频平台"),
        }
    }

    async fn kuaishou(&self, input: &Url) -> Result<NativeVideo> {
        let id = match kuaishou_id(input) {
            Some(id) => id,
            None => {
                let (page, _) = fetch_page(
                    self.kuaishou_client
                        .get(input.clone())
                        .header(header::USER_AGENT, MOBILE_AGENT),
                    "Kuaishou",
                )
                .await?;
                kuaishou_id(&page).context("快手分享链接没有跳转到单个作品页面")?
            }
        };
        // The ordinary mobile share page exposes the same work's player
        // manifest without the PC website's recommendations or login session.
        let mobile = Url::parse(&format!("https://v.m.chenzhongtech.com/fw/photo/{id}"))?;
        let (_, page) = fetch_page(
            self.kuaishou_client
                .get(mobile)
                .header(header::USER_AGENT, MOBILE_AGENT),
            "Kuaishou",
        )
        .await?;
        parse_kuaishou(&page, &id)
    }

    async fn acfun(&self, input: &Url) -> Result<NativeVideo> {
        let id = acfun_id(input).context("请输入 AcFun 单视频链接，例如 /v/ac123 或 /v/ac123_2")?;
        let page = Url::parse(&format!("https://www.acfun.cn/v/ac{id}"))?;
        let (final_url, html) = fetch_page(self.acfun_client.get(page), "AcFun").await?;
        ensure!(
            acfun_id(&final_url).as_deref() == Some(&id),
            "AcFun 页面跳转到了其他作品或分 P"
        );
        parse_acfun(&html, &id)
    }

    async fn weibo(&self, input: &Url) -> Result<NativeVideo> {
        let target = weibo_target(input).context("请输入微博单条视频状态或 /tv/show/ 链接")?;
        // This is the website's first-visit anonymous guest flow. These
        // request-local cookies do not grant access to logged-in/private posts.
        let guest = self.weibo_guest().await?;
        let id = match target {
            WeiboTarget::Status(id) => id,
            WeiboTarget::Video(oid) => {
                let mut endpoint = Url::parse("https://weibo.com/tv/api/component")?;
                endpoint
                    .query_pairs_mut()
                    .append_pair("page", &format!("/tv/show/{oid}"));
                let form = serde_json::to_string(&json!({
                    "Component_Play_Playinfo": {"oid": oid}
                }))?;
                let data = self
                    .weibo_json(
                        self.weibo_client
                            .post(endpoint)
                            .header(header::REFERER, format!("https://weibo.com/tv/show/{oid}"))
                            .header(header::COOKIE, &guest)
                            .form(&[("data", form)]),
                    )
                    .await?;
                weibo_component_mid(&data, &oid)?
            }
        };
        let mut endpoint = Url::parse("https://weibo.com/ajax/statuses/show")?;
        endpoint.query_pairs_mut().append_pair("id", &id);
        let data = self
            .weibo_json(
                self.weibo_client
                    .get(endpoint)
                    .header(header::REFERER, "https://weibo.com/")
                    .header(header::COOKIE, guest),
            )
            .await?;
        parse_weibo(&data, &id)
    }

    async fn weibo_guest(&self) -> Result<String> {
        let fp = serde_json::to_string(&json!({
            "os": "1", "browser": "Chrome140,0,0,0", "fonts": "undefined",
            "screenInfo": "1920*1080*24", "plugins": ""
        }))?;
        let response = self
            .weibo_client
            .post("https://passport.weibo.com/visitor/genvisitor")
            .header(header::REFERER, "https://weibo.com/")
            .form(&[("cb", "gen_callback"), ("fp", fp.as_str())])
            .send()
            .await?;
        ensure!(response.status().is_success(), "微博匿名访客初始化不可用");
        let data = parse_jsonp(&read_response(response).await?)?;
        ensure!(
            data["retcode"].as_u64() == Some(20_000_000),
            "微博匿名访客初始化被拒绝；当前仅支持公开作品"
        );
        let tid = data["data"]["tid"]
            .as_str()
            .filter(|value| token(value) && value.len() <= 4096)
            .context("微博匿名访客初始化没有返回有效 token")?;
        let mut endpoint = Url::parse("https://passport.weibo.com/visitor/visitor")?;
        endpoint
            .query_pairs_mut()
            .append_pair("a", "incarnate")
            .append_pair("t", tid)
            .append_pair(
                "w",
                if data["data"]["new_tid"] == true {
                    "3"
                } else {
                    "2"
                },
            )
            .append_pair("c", "100")
            .append_pair("gc", "")
            .append_pair("cb", "cross_domain")
            .append_pair("from", "weibo");
        let response = self
            .weibo_client
            .get(endpoint)
            .header(header::REFERER, "https://weibo.com/")
            .send()
            .await?;
        ensure!(response.status().is_success(), "微博匿名访客初始化不可用");
        let data = parse_jsonp(&read_response(response).await?)?;
        ensure!(
            data["retcode"].as_u64() == Some(20_000_000),
            "微博未建立匿名访客会话；当前仅支持公开作品"
        );
        guest_cookie(&data["data"])
    }

    async fn weibo_json(&self, request: RequestBuilder) -> Result<Value> {
        let response = request.send().await?;
        ensure!(
            response.status().is_success(),
            "微博公开视频接口不可用；作品可能需要登录、已失效或受地区限制"
        );
        serde_json::from_str(&read_response(response).await?)
            .context("微博未返回公开作品数据；可能需要登录或浏览器验证")
    }
}

fn page_client(platform: &'static str) -> Result<Client> {
    Ok(Client::builder()
        .use_native_tls()
        .https_only(true)
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(18))
        .user_agent(USER_AGENT)
        .redirect(Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("too many platform redirects")
            } else if allowed_page(attempt.url(), platform) {
                attempt.follow()
            } else {
                attempt.error("platform redirected outside its public website")
            }
        }))
        .build()?)
}

async fn fetch_page(request: RequestBuilder, platform: &str) -> Result<(Url, String)> {
    let response = request.send().await?;
    ensure!(
        response.status().is_success(),
        "{platform} 公开页面当前不可用"
    );
    ensure!(
        allowed_page(response.url(), platform),
        "平台跳转到了其他网站"
    );
    let url = response.url().clone();
    Ok((url, read_response(response).await?))
}

async fn read_response(mut response: Response) -> Result<String> {
    ensure!(
        response
            .content_length()
            .is_none_or(|size| size <= MAX_PAGE_BYTES as u64),
        "平台响应过大"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(bytes.len() + chunk.len() <= MAX_PAGE_BYTES, "平台响应过大");
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).context("平台响应不是 UTF-8")
}

fn allowed_page(url: &Url, platform: &str) -> bool {
    if url.scheme() != "https"
        || url.port().is_some_and(|port| port != 443)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return false;
    }
    let host = url.host_str().unwrap_or_default();
    match platform {
        "Kuaishou" => matches!(
            host,
            "kuaishou.com"
                | "www.kuaishou.com"
                | "m.kuaishou.com"
                | "v.kuaishou.com"
                | "v.m.chenzhongtech.com"
                | "v.chenzhongtech.com"
                | "chenzhongtech.com"
        ),
        "AcFun" => matches!(host, "acfun.cn" | "www.acfun.cn" | "m.acfun.cn"),
        "Weibo" => matches!(
            host,
            "weibo.com" | "www.weibo.com" | "m.weibo.cn" | "video.weibo.com"
        ),
        _ => false,
    }
}

fn token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn number(value: &str) -> bool {
    !value.is_empty() && value.len() <= 24 && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn string_id(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|value| value.to_string()))
}

fn kuaishou_id(url: &Url) -> Option<String> {
    let parts: Vec<_> = url
        .path_segments()?
        .filter(|part| !part.is_empty())
        .collect();
    match parts.as_slice() {
        ["short-video" | "photo" | "video", id] | ["fw", "photo", id] if token(id) => {
            Some((*id).into())
        }
        _ => url
            .query_pairs()
            .find(|(key, value)| key == "photoId" && token(value))
            .map(|(_, value)| value.into_owned()),
    }
}

fn acfun_id(url: &Url) -> Option<String> {
    let parts: Vec<_> = url
        .path_segments()?
        .filter(|part| !part.is_empty())
        .collect();
    let raw = match parts.as_slice() {
        ["v", id] => id.strip_prefix("ac")?,
        _ => return None,
    };
    let mut ids = raw.split('_');
    let id = ids.next()?;
    if !number(id) {
        return None;
    }
    if let Some(part) = ids.next() {
        if !number(part) || part.parse::<usize>().ok()? == 0 || ids.next().is_some() {
            return None;
        }
    }
    Some(raw.into())
}

enum WeiboTarget {
    Status(String),
    Video(String),
}

fn weibo_target(url: &Url) -> Option<WeiboTarget> {
    let parts: Vec<_> = url
        .path_segments()?
        .filter(|part| !part.is_empty())
        .collect();
    let host = url.host_str()?;
    if host == "m.weibo.cn" {
        return match parts.as_slice() {
            ["status" | "detail", id] if token(id) => Some(WeiboTarget::Status((*id).into())),
            _ => None,
        };
    }
    let oid = if host == "video.weibo.com" && parts.as_slice() == ["show"] {
        url.query_pairs()
            .find(|(key, _)| key == "fid")
            .map(|(_, value)| value.into_owned())
    } else if let ["tv", "show", id] = parts.as_slice() {
        Some((*id).into())
    } else {
        None
    };
    if let Some(oid) = oid {
        return valid_weibo_oid(&oid).then_some(WeiboTarget::Video(oid));
    }
    match parts.as_slice() {
        [uid, id] if number(uid) && token(id) => Some(WeiboTarget::Status((*id).into())),
        _ => None,
    }
}

fn valid_weibo_oid(value: &str) -> bool {
    value.split_once(':').is_some_and(|(namespace, id)| {
        number(namespace)
            && (number(id) && id.len() >= 16
                || id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
    })
}

fn parse_jsonp(text: &str) -> Result<Value> {
    if let Ok(data) = serde_json::from_str(text) {
        return Ok(data);
    }
    let start = text.find('{').context("平台未返回 JSON 数据")?;
    let end = text.rfind('}').context("平台未返回 JSON 数据")?;
    serde_json::from_str(&text[start..=end]).context("平台 JSON 数据不完整")
}

fn guest_cookie(data: &Value) -> Result<String> {
    let mut pairs = Vec::new();
    for (field, name) in [("sub", "SUB"), ("subp", "SUBP")] {
        if let Some(value) = data[field].as_str() {
            ensure!(
                !value.is_empty()
                    && value.len() <= 8192
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_graphic() && byte != b';' && byte != b','),
                "微博匿名访客 token 无效"
            );
            pairs.push(format!("{name}={value}"));
        }
    }
    ensure!(
        pairs.iter().any(|pair| pair.starts_with("SUB=")),
        "微博未返回匿名访客会话"
    );
    Ok(pairs.join("; "))
}

fn embedded_object(html: &str, marker: &str) -> Option<Value> {
    let start = html.find(marker)? + marker.len();
    let start = start + html[start..].find('{')?;
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for (offset, byte) in html.as_bytes()[start..].iter().enumerate() {
        if quoted {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                quoted = false;
            }
            continue;
        }
        match *byte {
            b'"' => quoted = true,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return serde_json::from_str(&html[start..=start + offset]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

fn media_url(raw: &str) -> Option<String> {
    let raw = if raw.starts_with("//") {
        format!("https:{raw}")
    } else {
        raw.to_owned()
    };
    let url = Url::parse(&raw).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none())
    .then(|| url.to_string())
}

fn media_headers(referer: &str) -> Vec<(String, String)> {
    vec![
        ("User-Agent".into(), USER_AGENT.into()),
        ("Referer".into(), referer.into()),
    ]
}

fn encrypted(value: &Value) -> bool {
    ["encrypted", "isEncrypted", "hasDRM", "isDrm", "drm"]
        .iter()
        .any(|key| value[*key].as_bool() == Some(true))
        || ["encryptionType", "encryptMethod", "drmType", "drm"]
            .iter()
            .any(|key| {
                value[*key]
                    .as_str()
                    .is_some_and(|text| !text.is_empty() && !matches!(text, "NONE" | "none" | "0"))
                    || value[*key].as_u64().is_some_and(|number| number != 0)
            })
}

fn ensure_public(value: &Value) -> Result<()> {
    ensure!(!encrypted(value), "不支持加密或 DRM 媒体");
    ensure!(
        !["isPaid", "is_paid", "is_pay_audio", "isVip", "needLogin"].iter().any(|key| value[*key] == true || value[*key].as_u64().is_some_and(|number| number > 0)),
        "该作品需要登录或付费，当前仅支持公开可播放的作品"
    );
    Ok(())
}

fn push_variant(
    variants: &mut Vec<NativeVariant>,
    seen: &mut HashSet<String>,
    value: &Value,
    id: String,
    label: String,
    referer: &str,
) {
    if encrypted(value) || value["hidden"] == true {
        return;
    }
    if value["m3u8Slice"].as_str().is_some_and(|text| {
        text.lines()
            .any(|line| line.starts_with("#EXT-X-KEY:") && !line.contains("METHOD=NONE"))
    }) {
        return;
    }
    let Some(url) = value["url"].as_str().and_then(media_url) else {
        return;
    };
    if !seen.insert(url.clone()) {
        return;
    }
    let height = value["height"]
        .as_u64()
        .and_then(|value| value.try_into().ok())
        .unwrap_or(0);
    variants.push(NativeVariant {
        id,
        label,
        height,
        url,
        size: value["size"]
            .as_u64()
            .or_else(|| value["fileSize"].as_u64()),
        headers: media_headers(referer),
        decrypt_key: None,
    });
}

fn matching_kuaishou_photo<'a>(data: &'a Value, expected: &str, depth: usize) -> Option<&'a Value> {
    if depth > 24 {
        return None;
    }
    if let Some(object) = data.as_object() {
        let id_matches = ["photoId", "id", "photo_id"]
            .iter()
            .any(|key| object.get(*key).and_then(string_id).as_deref() == Some(expected))
            || object
                .get("share_info")
                .and_then(Value::as_str)
                .is_some_and(|info| {
                    Url::parse(&format!("https://www.kuaishou.com/?{info}"))
                        .ok()
                        .is_some_and(|url| {
                            url.query_pairs()
                                .any(|(key, value)| key == "photoId" && value == expected)
                        })
                });
        if id_matches
            && (object.contains_key("manifest")
                || object.contains_key("photoUrl")
                || object.contains_key("mainMvUrls"))
        {
            return Some(data);
        }
        for value in object.values() {
            if let Some(photo) = matching_kuaishou_photo(value, expected, depth + 1) {
                return Some(photo);
            }
        }
    } else if let Some(array) = data.as_array() {
        for value in array {
            if let Some(photo) = matching_kuaishou_photo(value, expected, depth + 1) {
                return Some(photo);
            }
        }
    }
    None
}

fn parse_kuaishou(html: &str, expected: &str) -> Result<NativeVideo> {
    let state = [
        "window.INIT_STATE",
        "window.__INITIAL_STATE__",
        "window.__APOLLO_STATE__",
    ]
    .iter()
    .find_map(|marker| embedded_object(html, marker))
    .context("快手页面未返回公开作品数据；可能需要登录、验证或作品已失效")?;
    let photo = matching_kuaishou_photo(&state, expected, 0)
        .context("快手页面没有该作品的播放数据，未使用推荐视频替代")?;
    ensure_public(photo)?;
    ensure!(
        photo["singlePicture"] != true
            && photo["photoType"]
                .as_str()
                .is_none_or(|kind| kind == "VIDEO"),
        "快手作品不是视频"
    );
    let mut variants = Vec::new();
    let mut seen = HashSet::new();
    let manifest = match &photo["manifest"] {
        Value::String(raw) => serde_json::from_str(raw).context("快手播放器清单无效")?,
        value => value.clone(),
    };
    ensure_public(&manifest)?;
    if let Some(sets) = manifest["adaptationSet"].as_array() {
        for set in sets {
            if let Some(representations) = set["representation"].as_array() {
                for representation in representations {
                    let id = string_id(&representation["id"])
                        .unwrap_or_else(|| variants.len().to_string());
                    let quality = representation["qualityLabel"]
                        .as_str()
                        .unwrap_or("公开视频");
                    let codec = representation["videoCodec"].as_str().unwrap_or("");
                    push_variant(
                        &mut variants,
                        &mut seen,
                        representation,
                        format!("kuaishou-{id}"),
                        format!("{quality} {codec}").trim().into(),
                        "https://www.kuaishou.com/",
                    );
                }
            }
        }
    }
    let has_representations = manifest["adaptationSet"].as_array().is_some_and(|sets| {
        sets.iter().any(|set| {
            set["representation"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
        })
    });
    if variants.is_empty() && !has_representations {
        if let Some(url) = photo["photoUrl"].as_str() {
            push_variant(
                &mut variants,
                &mut seen,
                &json!({"url":url,"height":photo["height"]}),
                "kuaishou-public".into(),
                "公开视频".into(),
                "https://www.kuaishou.com/",
            );
        } else if let Some(url) = photo["mainMvUrls"].as_array().and_then(|urls| urls.first()) {
            push_variant(
                &mut variants,
                &mut seen,
                url,
                "kuaishou-public".into(),
                "公开视频".into(),
                "https://www.kuaishou.com/",
            );
        }
    }
    ensure!(!variants.is_empty(), "快手作品没有公开可下载的非加密视频");
    Ok(NativeVideo {
        id: expected.into(),
        title: photo["caption"]
            .as_str()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or("快手视频")
            .into(),
        duration_seconds: photo["duration"].as_u64().map(|millis| millis / 1000),
        variants,
    })
}

fn parse_acfun(html: &str, expected: &str) -> Result<NativeVideo> {
    let data = embedded_object(html, "window.videoInfo")
        .context("AcFun 页面没有公开播放器信息；作品可能已失效或需要登录")?;
    let (base, part) = expected
        .split_once('_')
        .map_or((expected, 1), |(base, part)| {
            (base, part.parse::<usize>().unwrap_or(0))
        });
    ensure!(
        string_id(&data["dougaId"]).as_deref() == Some(base),
        "AcFun 页面返回了其他作品"
    );
    ensure_public(&data)?;
    let video = &data["currentVideoInfo"];
    ensure_public(video)?;
    let list = data["videoList"]
        .as_array()
        .context("AcFun 分 P 信息不存在")?;
    let selected = list
        .get(part.saturating_sub(1))
        .context("AcFun 指定分 P 不存在")?;
    ensure!(
        string_id(&video["id"]).is_some() && string_id(&video["id"]) == string_id(&selected["id"]),
        "AcFun 页面未返回指定分 P 的播放数据"
    );
    let mut variants = Vec::new();
    let mut seen = HashSet::new();
    for (field, codec) in [("ksPlayJson", "H.264"), ("ksPlayJsonHevc", "HEVC")] {
        let Some(raw) = video[field].as_str().filter(|raw| !raw.is_empty()) else {
            continue;
        };
        let play: Value = serde_json::from_str(raw).context("AcFun 播放器清单无效")?;
        ensure_public(&play)?;
        if let Some(representations) = play["adaptationSet"]
            .get(0)
            .and_then(|set| set["representation"].as_array())
        {
            for representation in representations {
                let id =
                    string_id(&representation["id"]).unwrap_or_else(|| variants.len().to_string());
                let quality = representation["qualityLabel"]
                    .as_str()
                    .or_else(|| representation["qualityType"].as_str())
                    .unwrap_or("公开视频");
                push_variant(
                    &mut variants,
                    &mut seen,
                    representation,
                    format!("acfun-{field}-{id}"),
                    format!("{quality} {codec}"),
                    "https://www.acfun.cn/",
                );
            }
        }
    }
    ensure!(!variants.is_empty(), "AcFun 没有公开可下载的非加密视频清单");
    let mut title = data["title"].as_str().unwrap_or("AcFun 视频").to_owned();
    if list.len() > 1 {
        title.push_str(&format!(
            " P{part:02} {}",
            selected["title"].as_str().unwrap_or_default()
        ));
    }
    Ok(NativeVideo {
        id: expected.into(),
        title,
        duration_seconds: video["durationMillis"].as_u64().map(|millis| millis / 1000),
        variants,
    })
}

fn weibo_component_mid(data: &Value, expected: &str) -> Result<String> {
    let play = &data["data"]["Component_Play_Playinfo"];
    ensure_public(play)?;
    ensure!(
        ["oid", "id"]
            .iter()
            .any(|key| play[*key].as_str() == Some(expected)),
        "微博播放页未返回指定视频，未使用推荐视频替代"
    );
    let mid = string_id(&play["mid"]).context("微博播放页未返回作品状态 ID")?;
    ensure!(number(&mid), "微博作品状态 ID 无效");
    Ok(mid)
}

fn parse_weibo(data: &Value, expected: &str) -> Result<NativeVideo> {
    ensure!(
        ["idstr", "id_str", "id", "mid", "mblogid"]
            .iter()
            .any(|key| string_id(&data[*key]).as_deref() == Some(expected)),
        "微博未返回该作品的数据；可能需要登录或作品已失效"
    );
    ensure_public(data)?;
    let mixed = data["mix_media_info"]["items"].as_array();
    let videos: Vec<_> = mixed
        .into_iter()
        .flatten()
        .filter(|item| {
            item["type"].as_str() != Some("pic") && item["data"]["media_info"].is_object()
        })
        .collect();
    ensure!(
        videos.len() <= 1,
        "微博多视频作品暂不支持，请使用单视频链接"
    );
    let media = if let Some(item) = videos.first() {
        &item["data"]["media_info"]
    } else {
        &data["page_info"]["media_info"]
    };
    ensure_public(media)?;
    let mut variants = Vec::new();
    let mut seen = HashSet::new();
    if let Some(playbacks) = media["playback_list"].as_array() {
        for playback in playbacks {
            let info = &playback["play_info"];
            if info["type"].as_u64().is_some_and(|kind| kind != 1) {
                continue;
            }
            if info["mime"].as_str().is_some_and(|mime| {
                !matches!(
                    mime,
                    "video/mp4" | "application/x-mpegURL" | "application/vnd.apple.mpegurl"
                )
            }) {
                continue;
            }
            let id = info["label"].as_str().unwrap_or("public");
            let quality = info["quality_desc"].as_str().unwrap_or("公开视频");
            push_variant(
                &mut variants,
                &mut seen,
                info,
                format!("weibo-{id}"),
                format!("{id} {quality}"),
                "https://weibo.com/",
            );
        }
    }
    if media["playback_list"].as_array().is_none_or(Vec::is_empty) {
        for (field, height) in [
            ("mp4_720p_mp4", 720),
            ("mp4_hd_url", 720),
            ("stream_url_hd", 720),
            ("mp4_sd_url", 480),
            ("stream_url", 480),
        ] {
            if let Some(url) = media[field].as_str() {
                push_variant(
                    &mut variants,
                    &mut seen,
                    &json!({"url":url,"height":height}),
                    format!("weibo-{field}"),
                    format!("{height}p"),
                    "https://weibo.com/",
                );
            }
        }
    }
    ensure!(!variants.is_empty(), "微博作品没有公开可下载的非加密视频");
    let title = ["video_title", "kol_title", "name"]
        .iter()
        .find_map(|key| {
            media[*key]
                .as_str()
                .filter(|title| !title.trim().is_empty())
        })
        .or_else(|| data["text_raw"].as_str())
        .unwrap_or("微博视频");
    Ok(NativeVideo {
        id: string_id(&data["idstr"]).unwrap_or_else(|| expected.into()),
        title: title.chars().take(160).collect(),
        duration_seconds: media["duration"].as_u64().or_else(|| {
            media["duration"]
                .as_f64()
                .map(|seconds| seconds.max(0.0) as u64)
        }),
        variants,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kuaishou_state(photo: Value) -> String {
        format!(
            "<script>window.INIT_STATE = {};</script>",
            json!({"opaque-request-key":{"photo":photo}})
        )
    }

    #[test]
    fn kuaishou_public_manifest_matches_share_id_not_recommendation() {
        let requested = json!({"photoId":"5243316032516583413","share_info":"userId=user123&photoId=3x25msbsnncn8cs","caption":"目标作品","duration":33666,"photoType":"VIDEO","manifest":{"adaptationSet":[{"representation":[{"id":1,"url":"https://v1.kwaicdn.com/right.mp4","height":720,"fileSize":9409048,"qualityLabel":"高清","videoCodec":"avc"},{"id":2,"url":"https://v1.kwaicdn.com/right-hevc.mp4","height":720,"videoCodec":"hevc"}]}]}});
        let html = format!(
            "window.INIT_STATE={};",
            json!({"recommend":{"photo":{"photoId":"other","mainMvUrls":[{"url":"https://cdn.example/wrong.mp4"}]}},"work":{"photo":requested}})
        );
        let video = parse_kuaishou(&html, "3x25msbsnncn8cs").unwrap();
        assert_eq!(video.id, "3x25msbsnncn8cs");
        assert_eq!(video.variants.len(), 2);
        assert_eq!(video.variants[0].size, Some(9409048));
        assert!(video
            .variants
            .iter()
            .all(|variant| !variant.url.contains("wrong")));
        assert!(parse_kuaishou(&html, "another-video").is_err());
    }

    #[test]
    fn kuaishou_rejects_encrypted_and_empty_media() {
        let empty = kuaishou_state(json!({"photoId":"wanted","manifest":{},"photoType":"VIDEO"}));
        assert!(parse_kuaishou(&empty, "wanted").is_err());
        let encrypted = kuaishou_state(
            json!({"photoId":"wanted","manifest":{"encrypted":true},"mainMvUrls":[{"url":"https://cdn.example/fallback.mp4"}]}),
        );
        assert!(parse_kuaishou(&encrypted, "wanted")
            .unwrap_err()
            .to_string()
            .contains("加密"));
        let encrypted_representation = kuaishou_state(
            json!({"photoId":"wanted","manifest":{"adaptationSet":[{"representation":[{"url":"https://cdn.example/encrypted.mp4","isEncrypted":true}]}]},"mainMvUrls":[{"url":"https://cdn.example/fallback.mp4"}]}),
        );
        assert!(parse_kuaishou(&encrypted_representation, "wanted").is_err());
    }

    fn acfun_state(id: &str, current_id: &str, representations: Value) -> String {
        format!(
            "window.pageInfo = window.videoInfo = {};",
            json!({"dougaId":id,"title":"公开作品","videoList":[{"id":"first","title":"第一集"},{"id":"second","title":"第二集"}],"currentVideoInfo":{"id":current_id,"durationMillis":90000,"ksPlayJson":json!({"adaptationSet":[{"representation":representations}]}).to_string()}})
        )
    }

    #[test]
    fn acfun_selects_exact_part_and_representation_quality() {
        let html = acfun_state(
            "35468952",
            "second",
            json!([{"id":1,"url":"https://video.acfun.cn/high.m3u8","height":1080,"qualityLabel":"1080P"},{"id":2,"url":"https://video.acfun.cn/low.m3u8","height":720,"qualityLabel":"720P"}]),
        );
        let video = parse_acfun(&html, "35468952_2").unwrap();
        assert_eq!(video.variants.len(), 2);
        assert_eq!(video.title, "公开作品 P02 第二集");
        assert_eq!(video.duration_seconds, Some(90));
        assert!(parse_acfun(&html, "35468952_1").is_err());
        assert!(parse_acfun(&html, "99999_2").is_err());
        assert!(parse_acfun(&html, "35468952_3").is_err());
    }

    #[test]
    fn acfun_rejects_encrypted_inline_hls_and_empty_formats() {
        let encrypted = acfun_state(
            "123",
            "first",
            json!([{"id":1,"url":"https://cdn.example/encrypted.m3u8","m3u8Slice":"#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=key.bin\n"}]),
        );
        assert!(parse_acfun(&encrypted, "123").is_err());
        assert!(parse_acfun(&acfun_state("123", "first", json!([])), "123").is_err());
    }

    fn weibo_state() -> Value {
        json!({"idstr":"4910815147462302","mblogid":"N4xlMvjhI","text_raw":"作品说明","page_info":{"media_info":{"video_title":"公开作品","duration":918,"playback_list":[{"play_info":{"type":1,"mime":"video/mp4","label":"mp4_1080p","url":"https://f.video.weibocdn.com/1080.mp4","height":1080,"size":118485736}},{"play_info":{"mime":"video/mp4","label":"mp4_720p","url":"https://f.video.weibocdn.com/720.mp4","height":720}},{"play_info":{"mime":"image/jpeg","label":"scrubber_hd","url":"https://wx1.sinaimg.cn/preview.jpg","height":180}}]}}})
    }

    #[test]
    fn weibo_status_checks_identity_and_filters_scrubber_images() {
        let data = weibo_state();
        let video = parse_weibo(&data, "N4xlMvjhI").unwrap();
        assert_eq!(video.id, "4910815147462302");
        assert_eq!(video.variants.len(), 2);
        assert_eq!(video.variants[0].height, 1080);
        assert_eq!(video.variants[0].size, Some(118485736));
        assert!(parse_weibo(&data, "4910815147462302").is_ok());
        assert!(parse_weibo(&data, "other").is_err());
        assert!(parse_weibo(&json!({"error":"Forbidden"}), "N4xlMvjhI").is_err());
    }

    #[test]
    fn weibo_component_requires_requested_oid_and_status_mid() {
        let component = json!({"data":{"Component_Play_Playinfo":{"oid":"1034:4797699866951785","id":"1034:4797699866951785","mid":4797700463137878u64}}});
        assert_eq!(
            weibo_component_mid(&component, "1034:4797699866951785").unwrap(),
            "4797700463137878"
        );
        assert!(weibo_component_mid(&component, "1034:1234567890123456").is_err());
    }

    #[test]
    fn weibo_rejects_multiple_videos_paid_and_encrypted_media() {
        let mut data = weibo_state();
        data["is_paid"] = json!(true);
        assert!(parse_weibo(&data, "N4xlMvjhI").is_err());
        data["is_paid"] = json!(false);
        data["page_info"]["media_info"]["drm"] = json!(true);
        assert!(parse_weibo(&data, "N4xlMvjhI").is_err());
        data["page_info"]["media_info"]["drm"] = json!(false);
        data["mix_media_info"] = json!({"items":[{"type":"video","data":{"media_info":{}}},{"type":"video","data":{"media_info":{}}}]});
        assert!(parse_weibo(&data, "N4xlMvjhI")
            .unwrap_err()
            .to_string()
            .contains("多视频"));
        let mut empty = weibo_state();
        empty["page_info"]["media_info"]["playback_list"] = json!([]);
        assert!(parse_weibo(&empty, "N4xlMvjhI").is_err());
        empty["page_info"]["media_info"]["playback_list"] = json!([{"play_info":{"mime":"video/mp4","isEncrypted":true,"url":"https://cdn.example/encrypted.mp4"}}]);
        empty["page_info"]["media_info"]["stream_url"] = json!("https://cdn.example/fallback.mp4");
        assert!(parse_weibo(&empty, "N4xlMvjhI").is_err());
    }

    #[test]
    fn page_redirect_and_url_matching_are_platform_specific() {
        assert!(allowed_page(
            &Url::parse("https://v.m.chenzhongtech.com/fw/photo/3x25msbsnncn8cs").unwrap(),
            "Kuaishou"
        ));
        for invalid in [
            "https://weibo.com/1/id",
            "https://kuaishou.com.evil.example/photo/id",
            "http://www.kuaishou.com/photo/id",
            "https://user@www.kuaishou.com/photo/id",
        ] {
            assert!(!allowed_page(&Url::parse(invalid).unwrap(), "Kuaishou"));
        }
        assert_eq!(
            acfun_id(&Url::parse("https://www.acfun.cn/v/ac35468952_2").unwrap()).as_deref(),
            Some("35468952_2")
        );
        assert!(acfun_id(&Url::parse("https://www.acfun.cn/v/ac35468952_0").unwrap()).is_none());
        assert!(matches!(
            weibo_target(&Url::parse("https://m.weibo.cn/detail/4189191225395228").unwrap()),
            Some(WeiboTarget::Status(_))
        ));
        assert!(matches!(
            weibo_target(
                &Url::parse("https://video.weibo.com/show?fid=1034:4967272104787984").unwrap()
            ),
            Some(WeiboTarget::Video(_))
        ));
        assert!(weibo_target(&Url::parse("https://weibo.com/u/12345").unwrap()).is_none());
    }

    #[test]
    fn visitor_jsonp_tokens_are_bounded_and_cannot_inject_headers() {
        let data = parse_jsonp("window.cb && cb({\"retcode\":20000000,\"data\":{\"sub\":\"guest-token\",\"subp\":\"guest-extra\"}});").unwrap();
        assert_eq!(
            guest_cookie(&data["data"]).unwrap(),
            "SUB=guest-token; SUBP=guest-extra"
        );
        assert!(guest_cookie(&json!({"sub":"bad\r\nCookie:value"})).is_err());
        assert!(guest_cookie(&json!({"sub":"bad; another=secret"})).is_err());
        assert!(guest_cookie(&json!({})).is_err());
    }

    #[tokio::test]
    #[ignore = "explicit smoke test against public platform websites"]
    async fn public_pages_native_http_smoke() {
        let resolver = DomesticResolver::new().unwrap();
        for (platform, page, expected_id) in [
            (
                "Kuaishou",
                "https://www.kuaishou.com/short-video/3x25msbsnncn8cs",
                "3x25msbsnncn8cs",
            ),
            (
                "Weibo",
                "https://weibo.com/tv/show/1034:4797699866951785",
                "4797700463137878",
            ),
            ("AcFun", "https://www.acfun.cn/v/ac35457073", "35457073"),
        ] {
            let video = resolver
                .resolve(&Url::parse(page).unwrap(), platform)
                .await
                .unwrap();
            assert_eq!(video.id, expected_id);
            assert!(!video.variants.is_empty());
            assert!(video
                .variants
                .iter()
                .all(|variant| variant.headers.iter().all(|(name, _)| !matches!(
                    name.to_ascii_lowercase().as_str(),
                    "cookie" | "authorization"
                ))));
            eprintln!(
                "{platform}: id={} duration={:?} formats={:?}",
                video.id,
                video.duration_seconds,
                video
                    .variants
                    .iter()
                    .map(|variant| (&variant.label, variant.height, variant.size))
                    .collect::<Vec<_>>()
            );
        }
    }
}
