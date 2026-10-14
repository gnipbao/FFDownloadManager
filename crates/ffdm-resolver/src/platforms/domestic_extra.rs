//! Public metadata adapters for CCTV, Ixigua and PearVideo. The site pages and
//! their own public APIs are decoded in Rust; no foreign extractor is run.
use crate::generic::{GenericCandidate, GenericResource};
use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose, Engine};
use regex::Regex;
use reqwest::{header, redirect::Policy, Client, Response, Url};
use serde_json::Value;
use std::{collections::HashSet, time::Duration};

const MAX_BYTES: usize = 4 * 1024 * 1024;
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

pub(crate) struct DomesticExtraResolver {
    cctv: Client,
    ixigua: Client,
    pear: Client,
}

impl DomesticExtraResolver {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            cctv: page_client("CCTV")?,
            ixigua: page_client("Ixigua")?,
            pear: page_client("PearVideo")?,
        })
    }

    pub(crate) async fn resolve(&self, input: &Url, platform: &str) -> Result<GenericResource> {
        ensure!(
            allowed_page(input, platform),
            "请输入该平台的公开单视频页面"
        );
        match platform {
            "CCTV" => self.cctv(input).await,
            "Ixigua" => self.ixigua(input).await,
            "PearVideo" => self.pear(input).await,
            _ => bail!("未接入该平台的公开单视频解析"),
        }
    }

    async fn cctv(&self, input: &Url) -> Result<GenericResource> {
        let (page, html) = read_page(&self.cctv, input).await?;
        ensure!(page.path() == input.path(), "央视页面跳转到了其他作品");
        let id = cctv_id(&html)?;
        let mut endpoint = Url::parse("https://vdn.apps.cntv.cn/api/getHttpVideoInfo.do")?;
        endpoint
            .query_pairs_mut()
            .append_pair("pid", &id)
            .append_pair("url", page.as_str())
            .append_pair("idl", "32")
            .append_pair("idlr", "32")
            .append_pair("modifyed", "false");
        let data = read_json(
            self.cctv
                .get(endpoint)
                .header(header::REFERER, page.as_str())
                .send()
                .await?,
        )
        .await?;
        parse_cctv(&data, &id, &page)
    }

    async fn ixigua(&self, input: &Url) -> Result<GenericResource> {
        let id = ixigua_id(input).context("请输入西瓜单视频链接，例如 /6996881461559165471")?;
        let (page, html) = read_page(&self.ixigua, input).await?;
        ensure!(
            ixigua_id(&page).as_deref() == Some(&id),
            "西瓜页面跳转到了其他作品或验证页"
        );
        parse_ixigua(&html, &id, &page)
    }

    async fn pear(&self, input: &Url) -> Result<GenericResource> {
        let id = pear_id(input).context("请输入梨视频单作品链接，例如 /video_1804394")?;
        let (page, html) = read_page(&self.pear, input).await?;
        ensure!(
            pear_id(&page).as_deref() == Some(&id),
            "梨视频页面跳转到了其他作品"
        );
        // Require the target video's title before requesting its status. A
        // deleted page also contains recommended videos; those are not targets.
        let title = pear_title(&html).context("梨视频作品已失效、未公开或页面需要登录")?;
        let mut endpoint = Url::parse("https://www.pearvideo.com/videoStatus.jsp")?;
        endpoint.query_pairs_mut().append_pair("contId", &id);
        let data = read_json(
            self.pear
                .get(endpoint)
                .header(header::REFERER, page.as_str())
                .send()
                .await?,
        )
        .await?;
        parse_pear(&data, &id, &title, &page)
    }
}

fn host_allowed(host: &str, platform: &str) -> bool {
    match platform {
        "CCTV" => ["cctv.com", "cntv.cn", "cntv.com"]
            .iter()
            .any(|domain| host == *domain || host.ends_with(&format!(".{domain}"))),
        "Ixigua" => matches!(host, "ixigua.com" | "www.ixigua.com" | "m.ixigua.com"),
        "PearVideo" => matches!(
            host,
            "pearvideo.com" | "www.pearvideo.com" | "m.pearvideo.com"
        ),
        _ => false,
    }
}

fn allowed_page(url: &Url, platform: &str) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
        && url
            .port_or_known_default()
            .is_some_and(|p| p == 80 || p == 443)
        && url.host_str().is_some_and(|h| host_allowed(h, platform))
}

fn page_client(platform: &'static str) -> Result<Client> {
    Ok(Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(18))
        .user_agent(USER_AGENT)
        .redirect(Policy::custom(move |attempt| {
            let downgrade = attempt.previous().last().is_some_and(|previous| {
                previous.scheme() == "https" && attempt.url().scheme() == "http"
            });
            if attempt.previous().len() >= 5 || downgrade || !allowed_page(attempt.url(), platform)
            {
                attempt.error("public platform redirect rejected")
            } else {
                attempt.follow()
            }
        }))
        .build()?)
}

async fn read_page(client: &Client, input: &Url) -> Result<(Url, String)> {
    let response = client
        .get(input.clone())
        .header(header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .context("公开作品页面请求失败")?;
    let page = response.url().clone();
    Ok((page, read_body(response).await?))
}

async fn read_json(response: Response) -> Result<Value> {
    serde_json::from_str(&read_body(response).await?)
        .context("平台未返回公开播放信息；可能需要登录或浏览器验证")
}

async fn read_body(mut response: Response) -> Result<String> {
    ensure!(
        response.status().is_success(),
        "平台公开接口不可用（HTTP {}）",
        response.status().as_u16()
    );
    ensure!(
        response
            .content_length()
            .is_none_or(|n| n <= MAX_BYTES as u64),
        "平台响应超过 4 MiB 解析上限"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.context("读取公开播放信息失败")? {
        ensure!(
            bytes.len().saturating_add(chunk.len()) <= MAX_BYTES,
            "平台响应超过 4 MiB 解析上限"
        );
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).context("平台响应不是 UTF-8 文本")
}

fn cctv_id(html: &str) -> Result<String> {
    ensure!(html.len() <= MAX_BYTES, "央视页面超过解析上限");
    for pattern in [
        r#"\bvar\s+guid\s*=\s*["']([a-fA-F0-9]{32})["']"#,
        r#"\bvideoCenterId(?:["']\s*,|\s*:)\s*["']([a-fA-F0-9]{32})["']"#,
        r#"\b(?:changePlayer|load[Vv]ideo)\s*\(\s*["']([a-fA-F0-9]{32})["']"#,
    ] {
        let regex = Regex::new(pattern)?;
        let ids: HashSet<_> = regex
            .captures_iter(html)
            .map(|capture| capture[1].to_ascii_lowercase())
            .collect();
        ensure!(ids.len() <= 1, "央视页面包含多个视频，请使用单作品页面");
        if let Some(id) = ids.into_iter().next() {
            return Ok(id);
        }
    }
    bail!("央视页面未公开目标视频标识；请确认这是单作品页面")
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|n| n.is_finite() && *n >= 0.0)
}

fn is_one(value: &Value) -> bool {
    value.as_str() == Some("1") || value.as_u64() == Some(1) || value.as_bool() == Some(true)
}

fn parse_cctv(data: &Value, id: &str, page: &Url) -> Result<GenericResource> {
    ensure!(
        !is_one(&data["is_preview"]),
        "央视当前只开放试看，未提供完整作品"
    );
    ensure!(
        !is_one(&data["is_invalid_copyright"]) && !is_one(&data["is_protected"]),
        "央视该作品受访问或版权限制"
    );
    let title = data["title"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .context("央视未返回目标作品信息")?
        .to_owned();
    let total = number(&data["video"]["totalLength"]);
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for (key, label) in [
        ("lowChapters", "MP4 · 低码率"),
        ("chapters", "MP4 · 标准码率"),
        ("chapters2", "MP4 · 档位 2"),
        ("chapters3", "MP4 · 档位 3"),
        ("chapters4", "MP4 · 档位 4"),
    ] {
        let Some(chapters) = data["video"][key].as_array().filter(|list| list.len() == 1) else {
            continue;
        };
        let chapter = &chapters[0];
        // Individual chapters can be clips, not the full programme. Do not
        // present a clip or the first part of a multi-part asset as a full file.
        let (Some(total), Some(duration)) = (total, number(&chapter["duration"])) else {
            continue;
        };
        if (duration - total).abs() > 1.0 {
            continue;
        }
        if let Some(raw) = chapter["url"].as_str() {
            if let Some(candidate) = candidate(raw, page, key, label, 0, false, None) {
                if seen.insert(candidate.url.clone()) {
                    candidates.push(candidate);
                }
            }
        }
    }
    if let Some(raw) = data["hls_url"].as_str() {
        if let Some(candidate) = candidate(raw, page, "hls", "HLS · 完整视频", 0, false, None)
        {
            if candidate.extension == "m3u8" {
                // Keep the exact bandwidth-limited public URL as a fallback.
                // The host verifies the uncapped public variant before use.
                if let Some(url) = uncapped_cctv_hls(&candidate.url) {
                    if seen.insert(url.clone()) {
                        let mut uncapped = candidate.clone();
                        uncapped.id = "hls-uncapped".into();
                        uncapped.label = "HLS · 完整视频全部码率".into();
                        uncapped.url = url;
                        candidates.push(uncapped);
                    }
                }
                if seen.insert(candidate.url.clone()) {
                    candidates.push(candidate);
                }
            }
        }
    }
    ensure!(
        !candidates.is_empty(),
        "央视未提供可下载的完整单文件或公开 HLS；不支持逐章节节目"
    );
    Ok(GenericResource {
        id: id.into(),
        title,
        duration_seconds: total.map(|n| n.round() as u64),
        candidates,
        embed: None,
    })
}

fn uncapped_cctv_hls(raw: &str) -> Option<String> {
    let mut url = Url::parse(raw).ok()?;
    let query = url.query()?;
    let mut removed = false;
    let kept: Vec<_> = query
        .split('&')
        .filter(|pair| {
            if pair.strip_prefix("maxbr=").is_some_and(|value| {
                !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
            }) {
                removed = true;
                false
            } else {
                true
            }
        })
        .collect();
    if !removed {
        return None;
    }
    let query = kept.join("&");
    // Retain encoded signature bytes and literal '+', unlike query_pairs_mut.
    url.set_query((!query.is_empty()).then_some(query.as_str()));
    Some(url.into())
}

fn ixigua_id(url: &Url) -> Option<String> {
    let path = url.path().trim_matches('/');
    let id = path.strip_prefix("video/").unwrap_or(path);
    (!id.is_empty() && id.len() <= 24 && id.bytes().all(|b| b.is_ascii_digit())).then(|| id.into())
}

fn parse_ixigua(html: &str, id: &str, page: &Url) -> Result<GenericResource> {
    ensure!(html.len() <= MAX_BYTES, "西瓜页面超过解析上限");
    let script = Regex::new(
        r#"(?is)<script\b[^>]*\bid\s*=\s*["']SSR_HYDRATED_DATA["'][^>]*>(.*?)</script\s*>"#,
    )?;
    let raw = script
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
        .context("西瓜页面未公开视频数据；该作品可能需要登录或浏览器验证")?;
    let assignment = Regex::new(r#"^\s*(?:window\.)?_SSR_HYDRATED_DATA\s*=\s*"#)?;
    let raw = assignment.replace(raw, "");
    let data: Value = serde_json::from_str(&json_literals(raw.trim().trim_end_matches(';'))?)
        .context("西瓜页面视频数据格式发生变化")?;
    let video = &data["anyVideo"]["gidInformation"]["packerData"]["video"];
    ensure!(video.is_object(), "西瓜未返回目标作品信息");
    for value in [
        &video["gid"],
        &video["group_id"],
        &video["item_id"],
        &data["anyVideo"]["gidInformation"]["gid"],
    ] {
        if let Some(returned) = value
            .as_str()
            .map(str::to_owned)
            .or_else(|| value.as_u64().map(|n| n.to_string()))
        {
            ensure!(returned == id, "西瓜返回的是其他作品，已拒绝解析");
        }
    }
    let title = video["title"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .context("西瓜作品缺少标题")?
        .to_owned();
    let resources = &video["videoResource"]["video_list"];
    let entries: Vec<(&str, &Value)> = match resources {
        Value::Object(map) => map
            .iter()
            .take(32)
            .map(|(key, value)| (key.as_str(), value))
            .collect(),
        _ => Vec::new(),
    };
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    // video_list contains complete progressive A/V files. Dynamic DASH video
    // and audio tracks are deliberately excluded until paired-track plans can
    // be represented; otherwise a higher-quality selection would be silent.
    for (key, media) in entries {
        let Some(encoded) = media["main_url"].as_str().filter(|s| s.len() <= 16_384) else {
            continue;
        };
        let raw = general_purpose::STANDARD
            .decode(encoded)
            .or_else(|_| general_purpose::STANDARD_NO_PAD.decode(encoded));
        let Ok(bytes) = raw else {
            continue;
        };
        let Ok(raw) = String::from_utf8(bytes) else {
            continue;
        };
        let height = number(&media["vheight"])
            .map(|n| n.min(16_384.0) as u32)
            .unwrap_or(0);
        let label = if height > 0 {
            format!("{height}p · MP4")
        } else {
            format!("MP4 · {key}")
        };
        let size = number(&media["size"])
            .filter(|n| *n > 0.0)
            .map(|n| n as u64);
        if let Some(url) = safe_media_url(&raw, page) {
            if seen.insert(url.to_string()) {
                // The player names the container in this progressive MP4
                // list. CDN transfer endpoints often have no file suffix.
                candidates.push(GenericCandidate {
                    id: key.into(),
                    label,
                    height,
                    url: url.into(),
                    extension: "mp4".into(),
                    audio_only: false,
                    headers: media_headers(page),
                    size,
                });
            }
        }
    }
    ensure!(
        !candidates.is_empty(),
        "西瓜未提供完整音视频文件；登录、受限或分离音视频格式暂不支持"
    );
    Ok(GenericResource {
        id: id.into(),
        title,
        duration_seconds: number(&video["duration"]).map(|n| n.round() as u64),
        candidates,
        embed: None,
    })
}

// Replace only unquoted JavaScript null-like values from the static SSR object.
// This is a tokenizer, not an evaluator: calls, templates and assignments fail
// the subsequent JSON parser and are never executed.
fn json_literals(raw: &str) -> Result<String> {
    let mut output = String::with_capacity(raw.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut token = String::new();
    let flush = |token: &mut String, output: &mut String| {
        if matches!(token.as_str(), "undefined" | "NaN" | "Infinity") {
            output.push_str("null");
        } else {
            output.push_str(token);
        }
        token.clear();
    };
    for ch in raw.chars() {
        if in_string {
            output.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else if ch == '"' {
            flush(&mut token, &mut output);
            in_string = true;
            output.push(ch);
        } else if ch.is_ascii_alphabetic() || ch == '_' {
            token.push(ch);
        } else {
            flush(&mut token, &mut output);
            output.push(ch);
        }
    }
    flush(&mut token, &mut output);
    ensure!(!in_string, "SSR 数据包含未结束的字符串");
    Ok(output)
}

fn pear_id(url: &Url) -> Option<String> {
    let id = url.path().trim_end_matches('/').strip_prefix("/video_")?;
    (!id.is_empty() && id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit())).then(|| id.into())
}

fn pear_title(html: &str) -> Option<String> {
    let regex = Regex::new(
        r#"(?is)<h1\b[^>]*\bclass\s*=\s*["'][^"']*\bvideo-tt\b[^"']*["'][^>]*>([^<]+)</h1\s*>"#,
    )
    .ok()?;
    let title = regex.captures(html)?.get(1)?.as_str().trim();
    (!title.is_empty()).then(|| decode_entities(title))
}

fn parse_pear(data: &Value, id: &str, title: &str, page: &Url) -> Result<GenericResource> {
    ensure!(
        is_one(&data["resultCode"]) && is_one(&data["videoInfo"]["playSta"]),
        "梨视频作品未公开、已删除或受访问限制"
    );
    for value in [&data["contId"], &data["videoInfo"]["contId"]] {
        if let Some(returned) = value
            .as_str()
            .map(str::to_owned)
            .or_else(|| value.as_u64().map(|n| n.to_string()))
        {
            ensure!(returned == id, "梨视频接口返回了其他作品");
        }
    }
    let videos = data["videoInfo"]["videos"]
        .as_object()
        .context("梨视频未返回公开播放地址")?;
    let system_time = data["systemTime"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 24 && s.bytes().all(|b| b.is_ascii_digit()));
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for (key, label) in [
        ("srcUrl", "原始 MP4"),
        ("hdUrl", "高清 MP4"),
        ("sdUrl", "标准 MP4"),
        ("hdflvUrl", "高清 FLV"),
        ("sdflvUrl", "标准 FLV"),
    ] {
        let Some(raw) = videos
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let Some(mut parsed) = safe_media_url(raw, page) else {
            continue;
        };
        if key == "srcUrl" {
            // The public player API uses the response clock as a placeholder
            // for the asset's content ID. Substitute in the filename only;
            // never alter a host, signature query or an unrelated timestamp.
            if let Some(clock) = system_time {
                let filename = parsed.path().rsplit('/').next().unwrap_or("");
                if let Some(rest) = filename.strip_prefix(&format!("{clock}-")) {
                    let prefix = &parsed.path()[..parsed.path().len() - filename.len()];
                    parsed.set_path(&format!("{prefix}cont-{id}-{rest}"));
                }
            }
        }
        if let Some(candidate) = candidate(parsed.as_str(), page, key, label, 0, false, None) {
            if seen.insert(candidate.url.clone()) {
                candidates.push(candidate);
            }
        }
    }
    ensure!(!candidates.is_empty(), "梨视频未提供可读取的完整媒体文件");
    Ok(GenericResource {
        id: id.into(),
        title: title.into(),
        duration_seconds: number(&data["videoInfo"]["duration"]).map(|n| n.round() as u64),
        candidates,
        embed: None,
    })
}

fn decode_entities(raw: &str) -> String {
    raw.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn media_headers(page: &Url) -> Vec<(String, String)> {
    let mut referer = page.clone();
    referer.set_fragment(None);
    vec![
        ("User-Agent".into(), USER_AGENT.into()),
        ("Referer".into(), referer.into()),
    ]
}

fn safe_media_url(raw: &str, page: &Url) -> Option<Url> {
    if raw.len() > 16_384 {
        return None;
    }
    let url = page.join(&decode_entities(raw)).ok()?;
    crate::validation::parse_url(url.as_str()).ok()
}

fn candidate(
    raw: &str,
    page: &Url,
    id: &str,
    label: &str,
    height: u32,
    audio_only: bool,
    size: Option<u64>,
) -> Option<GenericCandidate> {
    let url = safe_media_url(raw, page)?;
    let path = url.path().to_ascii_lowercase();
    let extension = if path.ends_with(".m3u8") {
        "m3u8"
    } else if path.ends_with(".flv") {
        "flv"
    } else if path.ends_with(".webm") {
        "webm"
    } else if path.ends_with(".mp4")
        || url.host_str().is_some_and(|h| {
            h.ends_with(".bytecdn.cn")
                || h.ends_with(".douyinvod.com")
                || h.ends_with(".snssdk.com")
        })
    {
        "mp4"
    } else {
        return None;
    };
    Some(GenericCandidate {
        id: id.into(),
        label: label.into(),
        height,
        url: url.into(),
        extension: extension.into(),
        audio_only,
        headers: media_headers(page),
        size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cctv_rejects_first_chapter_and_preview() {
        let page = Url::parse("https://tv.cctv.com/2026/10/14/VIDEfixture.shtml").unwrap();
        let mut data = json!({"title":"完整节目","video":{"totalLength":"120","chapters":[{"duration":"60","url":"https://cdn.example/a.mp4"},{"duration":"60","url":"https://cdn.example/b.mp4"}]}});
        assert!(parse_cctv(&data, "abc", &page).is_err());
        data["hls_url"] = json!("https://cdn.example/master.m3u8?sig=a&maxbr=5000");
        let parsed = parse_cctv(&data, "abc", &page).unwrap();
        assert_eq!(parsed.candidates.len(), 2);
        assert_eq!(
            parsed.candidates[1].url,
            "https://cdn.example/master.m3u8?sig=a&maxbr=5000"
        );
        data["is_preview"] = json!("1");
        assert!(parse_cctv(&data, "abc", &page).is_err());
    }

    #[test]
    fn cctv_single_complete_file_and_target_guid() {
        assert_eq!(cctv_id(r#"var guid_Ad_VideoCode="ffffffffffffffffffffffffffffffff";var guid="efc5d49e5b3b4ab2b34f3a502b73d3ae";"#).unwrap(), "efc5d49e5b3b4ab2b34f3a502b73d3ae");
        assert!(cctv_id(r#"var guid="ffffffffffffffffffffffffffffffff";var guid="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";"#).is_err());
        let page = Url::parse("https://tv.cctv.com/fixture.shtml").unwrap();
        let data = json!({"title":"节目","video":{"totalLength":"37.0","chapters":[{"duration":"37","url":"https://cdn.example/clip.mp4?token=A%2BB&v=1"}]}});
        let parsed = parse_cctv(&data, "abc", &page).unwrap();
        assert_eq!(parsed.duration_seconds, Some(37));
        assert_eq!(
            parsed.candidates[0].url,
            "https://cdn.example/clip.mp4?token=A%2BB&v=1"
        );
    }

    #[test]
    fn ixigua_progressive_only_and_requested_id() {
        let page = Url::parse("https://www.ixigua.com/123").unwrap();
        let mut video = json!({"gid":"123","title":"undefined 保留标题","duration":24,"videoResource":{"video_list":{"normal":{"main_url":general_purpose::STANDARD.encode("https://cdn.example/video.mp4?sig=A%2BB"),"vheight":720,"size":1234}},"dynamic_video":{"dynamic_video_list":{"1080":{"main_url":general_purpose::STANDARD.encode("https://cdn.example/silent.mp4")}}}}});
        let make = |video: &Value| {
            let data = json!({
                "anyVideo": {"gidInformation": {"packerData": {"video": video}}},
                "optional": null,
            });
            format!(
                "<script id='SSR_HYDRATED_DATA'>window._SSR_HYDRATED_DATA={};</script>",
                data.to_string()
                    .replace("\"optional\":null", "\"optional\":undefined")
            )
        };
        let parsed = parse_ixigua(&make(&video), "123", &page).unwrap();
        assert_eq!(parsed.title, "undefined 保留标题");
        assert_eq!(parsed.candidates.len(), 1);
        assert_eq!(parsed.candidates[0].height, 720);
        assert_eq!(parsed.candidates[0].size, Some(1234));
        assert_eq!(
            parsed.candidates[0].url,
            "https://cdn.example/video.mp4?sig=A%2BB"
        );
        video["gid"] = json!("999");
        assert!(parse_ixigua(&make(&video), "123", &page).is_err());
        assert!(parse_ixigua("<html>登录验证</html>", "123", &page).is_err());
    }

    #[test]
    fn pear_clock_substitution_preserves_signature_and_identity() {
        let page = Url::parse("https://www.pearvideo.com/video_1804394").unwrap();
        let data = json!({"resultCode":"1","systemTime":"1791990000","videoInfo":{"playSta":"1","videos":{"srcUrl":"https://video.pearvideo.com/mp4/1791990000-test-hd.mp4?sig=1791990000%2B","hdUrl":"","sdUrl":null}}});
        let parsed = parse_pear(&data, "1804394", "目标视频", &page).unwrap();
        assert_eq!(
            parsed.candidates[0].url,
            "https://video.pearvideo.com/mp4/cont-1804394-test-hd.mp4?sig=1791990000%2B"
        );
        assert_eq!(
            pear_title(r#"<h1 class="video-tt">A &amp; B</h1><video src="recommendation.mp4">"#)
                .as_deref(),
            Some("A & B")
        );
        assert!(pear_title("<title>删除作品</title><video src='recommended.mp4'>").is_none());
    }

    #[test]
    fn pear_numeric_response_identity_is_checked() {
        let page = Url::parse("https://www.pearvideo.com/video_1804394").unwrap();
        let mut data = json!({"contId":1804394,"resultCode":"1","videoInfo":{"playSta":1,"contId":"1804394","videos":{"srcUrl":"https://video.pearvideo.com/correct.mp4"}}});
        assert!(parse_pear(&data, "1804394", "目标视频", &page).is_ok());
        data["contId"] = json!(9999999);
        assert!(parse_pear(&data, "1804394", "目标视频", &page).is_err());
        data["contId"] = json!(1804394);
        data["videoInfo"]["contId"] = json!(9999999);
        assert!(parse_pear(&data, "1804394", "目标视频", &page).is_err());
    }

    #[test]
    fn cctv_uncapped_hls_retains_original_and_encoded_query_bytes() {
        let original =
            "https://cdn.example/master.m3u8?sig=A%2Bb+Z%2f&maxbr=5000&empty=&token=one%26two";
        let uncapped = "https://cdn.example/master.m3u8?sig=A%2Bb+Z%2f&empty=&token=one%26two";
        assert_eq!(uncapped_cctv_hls(original).as_deref(), Some(uncapped));
        assert!(uncapped_cctv_hls("https://cdn.example/master.m3u8?maxbr=invalid&sig=x").is_none());
        let page = Url::parse("https://tv.cctv.com/fixture.shtml#t=10").unwrap();
        let data = json!({"title":"完整节目","video":{"totalLength":"37.0"},"hls_url":original});
        let parsed = parse_cctv(&data, "abc", &page).unwrap();
        assert_eq!(parsed.candidates.len(), 2);
        assert_eq!(parsed.candidates[0].url, uncapped);
        assert_eq!(parsed.candidates[1].url, original);
        assert!(parsed.candidates[0]
            .headers
            .iter()
            .all(|(_, value)| !value.contains("#t=10")));
    }

    #[test]
    fn unsupported_paths_credentials_and_forged_platform_hosts() {
        assert!(ixigua_id(&Url::parse("https://www.ixigua.com/123/other").unwrap()).is_none());
        assert!(
            pear_id(&Url::parse("https://www.pearvideo.com/video_123/other").unwrap()).is_none()
        );
        assert!(!allowed_page(
            &Url::parse("https://tv.cctv.com.evil.test/a.shtml").unwrap(),
            "CCTV"
        ));
        assert!(!allowed_page(
            &Url::parse("https://user:pass@www.ixigua.com/123").unwrap(),
            "Ixigua"
        ));
        assert!(candidate(
            "https://user:secret@cdn.example/a.mp4",
            &Url::parse("https://www.pearvideo.com/video_1").unwrap(),
            "1",
            "MP4",
            0,
            false,
            None
        )
        .is_none());
    }

    #[test]
    fn ssr_tokens_do_not_rewrite_quoted_text_or_evaluate_code() {
        let data: Value = serde_json::from_str(
            &json_literals(r#"{"title":"undefined NaN Infinity","optional":undefined,"n":NaN}"#)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(data["title"], "undefined NaN Infinity");
        assert!(data["optional"].is_null());
        assert!(serde_json::from_str::<Value>(
            &json_literals(r#"{"url":fetch("https://evil.example/")}"#).unwrap()
        )
        .is_err());
    }

    #[tokio::test]
    #[ignore = "requires current public CCTV and PearVideo metadata endpoints"]
    async fn live_public_domestic_metadata() {
        let resolver = DomesticExtraResolver::new().unwrap();
        for (platform, raw) in [
            (
                "CCTV",
                "https://tv.cctv.com/2016/02/05/VIDEUS7apq3lKrHG9Dncm03B160205.shtml",
            ),
            ("PearVideo", "https://www.pearvideo.com/video_1804394"),
        ] {
            let parsed = resolver
                .resolve(&Url::parse(raw).unwrap(), platform)
                .await
                .unwrap();
            assert!(!parsed.title.is_empty());
            assert!(!parsed.candidates.is_empty());
            // Only metadata counts are reported; signed media URLs stay private.
            eprintln!("{platform}: {} public formats", parsed.candidates.len());
        }
    }
}
