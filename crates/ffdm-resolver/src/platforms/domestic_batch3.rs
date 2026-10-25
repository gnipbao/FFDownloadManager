//! Public single-work Sohu and Haokan metadata, interpreted in Rust.
//! Sohu follows its public VRS/UGC metadata and CDN dispatch. Haokan reads the
//! requested work's mobile-page SSR, never the unrelated recommendation list.
use crate::generic::{GenericCandidate, GenericResource};
use anyhow::{anyhow, bail, ensure, Context, Result};
use base64::{engine::general_purpose, Engine};
use futures_util::{stream, StreamExt};
use regex::Regex;
use reqwest::{header, redirect::Policy, Client, Response, Url};
use serde_json::Value;
use std::{collections::HashSet, time::Duration};

const MAX_BYTES: usize = 4 * 1024 * 1024;
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
const MOBILE_AGENT: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1";

pub(crate) struct DomesticBatch3Resolver {
    sohu: Client,
    haokan: Client,
}

impl DomesticBatch3Resolver {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            sohu: client("Sohu")?,
            haokan: client("Haokan")?,
        })
    }

    pub(crate) async fn resolve(&self, input: &Url, platform: &str) -> Result<GenericResource> {
        crate::validation::parse_url(input.as_str())?;
        match platform {
            "Sohu" => self.sohu(input).await,
            "Haokan" => self.haokan(input).await,
            _ => bail!("未接入该平台的公开视频解析"),
        }
    }

    async fn sohu(&self, input: &Url) -> Result<GenericResource> {
        let target = sohu_target(input)?;
        let response = self
            .sohu
            .get(target.canonical.clone())
            .header(header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(|_| anyhow!("搜狐公开视频页面请求失败"))?;
        let page = response.url().clone();
        ensure!(
            sohu_target(&page)?.id == target.id,
            "搜狐页面跳转到了其他作品"
        );
        let html = read_body(response).await?;
        let vid = sohu_page_vid(&html)?;
        ensure!(vid == target.id, "搜狐播放器不是请求的作品，已拒绝解析");
        let metadata = self.sohu_metadata(&vid, target.mytv, &page).await?;
        let base = sohu_metadata(&metadata, &target.id)?;
        let title = base["tvName"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .context("搜狐作品缺少标题")?
            .to_owned();
        let duration =
            number(&base["totalDurationDouble"]).or_else(|| number(&base["totalDuration"]));
        let choices = sohu_choices(base, &vid);
        let mut results = stream::iter(choices)
            .map(|(format_id, quality_vid)| {
                let metadata = &metadata;
                let page = &page;
                let target = &target;
                let vid = &vid;
                async move {
                    let work = async {
                        let data = if &quality_vid == vid {
                            metadata.clone()
                        } else {
                            self.sohu_metadata(&quality_vid, target.mytv, page).await?
                        };
                        let video = sohu_metadata(&data, &target.id)?;
                        let (clip, replacement, size) = sohu_single_clip(video)?;
                        let allot = data["allot"].as_str().context("搜狐没有提供公开播放调度")?;
                        let url = self.sohu_dispatch(allot, &clip, &replacement, page).await?;
                        let height = number(&video["height"])
                            .filter(|h| *h <= 16384.0)
                            .map(|h| h as u32)
                            .unwrap_or(0);
                        let label = if height > 0 {
                            format!("{height}p · MP4")
                        } else {
                            format!("MP4 · {}", sohu_label(&format_id))
                        };
                        Ok::<_, anyhow::Error>(GenericCandidate {
                            transform: None,
                            id: format_id,
                            label,
                            height,
                            url,
                            extension: "mp4".into(),
                            audio_only: false,
                            headers: media_headers(page, USER_AGENT),
                            size,
                        })
                    };
                    tokio::time::timeout(Duration::from_secs(12), work)
                        .await
                        .context("搜狐清晰度请求超时")?
                }
            })
            .buffer_unordered(3);
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();
        let mut failure = None;
        while let Some(result) = results.next().await {
            match result {
                Ok(candidate) if seen.insert(candidate.url.clone()) => candidates.push(candidate),
                Ok(_) => {}
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        drop(results);
        ensure!(
            !candidates.is_empty(),
            "搜狐未提供可读取的完整单文件。{}",
            failure
                .map(|e| crate::utils::safe_error(&e.to_string()))
                .unwrap_or_default()
        );
        Ok(GenericResource {
            id: target.id,
            title,
            duration_seconds: duration.map(|d| d.round() as u64),
            candidates,
            embed: None,
        })
    }

    async fn sohu_metadata(&self, vid: &str, mytv: bool, page: &Url) -> Result<Value> {
        ensure!(valid_quality_id(vid), "搜狐清晰度标识无效");
        let mut url = Url::parse(if mytv {
            "https://my.tv.sohu.com/play/videonew.do"
        } else {
            "https://hot.vrs.sohu.com/vrs_flash.action"
        })?;
        url.query_pairs_mut().append_pair("vid", vid);
        self.json(&self.sohu, url, page).await
    }

    async fn sohu_dispatch(
        &self,
        allot: &str,
        clip: &str,
        replacement: &str,
        page: &Url,
    ) -> Result<String> {
        ensure!(
            allot.len() <= 253 && !allot.contains(['/', '?', '#', '@']),
            "搜狐播放调度主机无效"
        );
        let endpoint = Url::parse(&format!("https://{allot}/"))?;
        ensure!(
            allowed_url(&endpoint, "Sohu"),
            "搜狐播放调度不在公开 CDN 内"
        );
        let mut cdn_id: Option<String> = None;
        for _ in 0..5 {
            let mut url = endpoint.clone();
            url.query_pairs_mut()
                .append_pair("prot", "9")
                .append_pair("file", clip)
                .append_pair("new", replacement)
                .append_pair("prod", "h5n")
                .append_pair("rb", "1");
            if let Some(id) = &cdn_id {
                url.query_pairs_mut().append_pair("idc", id);
            }
            let response = self.json(&self.sohu, url, page).await?;
            let raw = response["url"]
                .as_str()
                .context("搜狐调度没有返回媒体地址")?;
            let media = safe_media_url(raw, page, "Sohu")?;
            if media.host_str() != Some("newflv.sohu.ccgslb.net") {
                ensure!(
                    media.path().to_ascii_lowercase().ends_with(".mp4"),
                    "搜狐未提供完整 MP4 文件"
                );
                return Ok(media.into());
            }
            cdn_id = scalar_id(&response["nid"]);
        }
        bail!("搜狐媒体 CDN 暂时不可用，请稍后重新解析")
    }

    async fn haokan(&self, input: &Url) -> Result<GenericResource> {
        let id = haokan_id(input)?;
        let mut page = Url::parse("https://haokan.baidu.com/v")?;
        page.query_pairs_mut().append_pair("vid", &id);
        let response = self
            .haokan
            .get(page)
            .header(header::USER_AGENT, MOBILE_AGENT)
            .header(header::REFERER, "https://haokan.baidu.com/")
            .header(header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(|_| anyhow!("好看公开视频页面请求失败"))?;
        let page = response.url().clone();
        ensure!(
            haokan_id(&page)? == id,
            "好看页面跳转到了其他作品或验证页面"
        );
        parse_haokan(&read_body(response).await?, &id, &page)
    }

    async fn json(&self, client: &Client, url: Url, page: &Url) -> Result<Value> {
        let response = client
            .get(url)
            .header(header::REFERER, page.as_str())
            .header(header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(|_| anyhow!("平台公开播放信息请求失败"))?;
        serde_json::from_str(&read_body(response).await?)
            .context("平台未返回公开播放信息，作品可能需要登录或受地区限制")
    }
}

fn client(platform: &'static str) -> Result<Client> {
    Ok(Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(18))
        .user_agent(USER_AGENT)
        .redirect(Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5
                || !allowed_url(attempt.url(), platform)
                || attempt.previous().last().is_some_and(|previous| {
                    previous.scheme() == "https" && attempt.url().scheme() == "http"
                })
            {
                attempt.error("public platform redirect rejected")
            } else {
                attempt.follow()
            }
        }))
        .build()?)
}

fn allowed_url(url: &Url, platform: &str) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
        && url
            .port_or_known_default()
            .is_some_and(|port| port == 80 || port == 443)
        && url.host_str().is_some_and(|host| match platform {
            "Sohu" => ["sohu.com", "itc.cn", "sohu.ccgslb.net", "sohucs.com"]
                .iter()
                .any(|domain| host == *domain || host.ends_with(&format!(".{domain}"))),
            "Haokan" => host == "haokan.baidu.com",
            _ => false,
        })
}

async fn read_body(mut response: Response) -> Result<String> {
    ensure!(
        response.status().is_success(),
        "公开作品接口不可用（HTTP {}）",
        response.status().as_u16()
    );
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= MAX_BYTES as u64),
        "平台响应超过 4 MiB 解析上限"
    );
    ensure!(
        response
            .headers()
            .get(header::CONTENT_ENCODING)
            .is_none_or(|value| value.as_bytes().eq_ignore_ascii_case(b"identity")),
        "平台响应采用不支持的压缩编码"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.context("读取公开作品页面失败")? {
        ensure!(
            bytes.len().saturating_add(chunk.len()) <= MAX_BYTES,
            "平台响应超过 4 MiB 解析上限"
        );
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).context("平台响应不是 UTF-8 文本")
}

#[derive(Debug)]
struct SohuTarget {
    id: String,
    mytv: bool,
    canonical: Url,
}

fn sohu_target(url: &Url) -> Result<SohuTarget> {
    ensure!(
        matches!(url.host_str(), Some("tv.sohu.com" | "my.tv.sohu.com")),
        "请输入搜狐视频公开作品链接"
    );
    crate::validation::parse_url(url.as_str())?;
    let encoded = url.host_str() == Some("tv.sohu.com") && url.path().starts_with("/v/");
    let (path, mytv) = if encoded {
        let token = url
            .path()
            .strip_prefix("/v/")
            .and_then(|path| path.strip_suffix(".html"))
            .filter(|token| {
                token.len() <= 512
                    && token
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'='))
            })
            .context("搜狐嵌入地址格式无效")?;
        let bytes = general_purpose::URL_SAFE
            .decode(token)
            .or_else(|_| general_purpose::URL_SAFE_NO_PAD.decode(token))
            .context("搜狐嵌入地址编码无效")?;
        let path = String::from_utf8(bytes).context("搜狐嵌入地址不是 UTF-8")?;
        let mytv = path.starts_with("us/") || path.starts_with("pl/");
        (path, mytv)
    } else {
        (
            url.path().trim_start_matches('/').into(),
            url.host_str() == Some("my.tv.sohu.com"),
        )
    };
    let parts: Vec<_> = path.split('/').collect();
    let id = match parts.as_slice() {
        ["us" | "pl", owner, name] if mytv && digits(owner, 24) => name.strip_suffix(".shtml"),
        [date, name] if !mytv && date.len() == 8 && digits(date, 8) => name
            .strip_prefix('n')
            .and_then(|name| name.strip_suffix(".shtml")),
        _ => None,
    }
    .filter(|id| digits(id, 24))
    .context("请输入搜狐点播单作品链接，暂不支持电影付费页或列表")?;
    let canonical = Url::parse(&format!(
        "https://{}.sohu.com/{path}",
        if mytv { "my.tv" } else { "tv" }
    ))?;
    Ok(SohuTarget {
        id: id.into(),
        mytv,
        canonical,
    })
}

fn sohu_page_vid(html: &str) -> Result<String> {
    ensure!(html.len() <= MAX_BYTES, "搜狐页面超过解析上限");
    let regex = Regex::new(r#"\bvar\s+vid\s*=\s*["']([0-9]+)["']"#)?;
    let ids: HashSet<_> = regex.captures_iter(html).map(|c| c[1].to_owned()).collect();
    ensure!(
        ids.len() == 1,
        "搜狐页面未公开唯一目标视频，可能已失效或需要验证"
    );
    Ok(ids.into_iter().next().unwrap())
}

fn sohu_metadata<'a>(metadata: &'a Value, id: &str) -> Result<&'a Value> {
    ensure!(
        number(&metadata["play"]) == Some(1.0),
        "搜狐作品未公开或受地区限制"
    );
    ensure!(
        scalar_id(&metadata["tvid"]).as_deref() == Some(id),
        "搜狐接口返回的作品与请求不一致"
    );
    for key in ["fee", "is_member_play", "isdrm", "preview", "tvfee"] {
        ensure!(
            !truthy(&metadata[key]),
            "搜狐作品需要会员、付费或只开放试看"
        );
    }
    for key in ["isPay", "payFlag", "is_member_play", "isdrm"] {
        ensure!(
            !truthy(&metadata["pay_data"][key]),
            "搜狐作品需要付费或额外授权"
        );
    }
    let data = &metadata["data"];
    ensure!(data.is_object(), "搜狐没有返回公开播放信息");
    sohu_single_clip(data)?;
    Ok(data)
}

fn sohu_single_clip(data: &Value) -> Result<(String, String, Option<u64>)> {
    ensure!(
        number(&data["totalBlocks"]) == Some(1.0),
        "搜狐该作品分为多个章节，暂未提供可下载的完整单文件；不会只下载第一章"
    );
    let clips = data["mp4PlayUrl"]
        .as_array()
        .filter(|array| array.len() == 1)
        .or_else(|| data["clipsURL"].as_array().filter(|array| array.len() == 1))
        .context("搜狐没有提供完整单文件")?;
    let replacements = data["su"]
        .as_array()
        .filter(|array| array.len() == 1)
        .context("搜狐播放调度信息不完整")?;
    let clip = clips[0]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 16_384)
        .context("搜狐播放地址无效")?;
    crate::validation::parse_url(clip)?;
    let replacement = replacements[0]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 16_384)
        .context("搜狐播放路径无效")?;
    let total = number(&data["totalDurationDouble"])
        .or_else(|| number(&data["totalDuration"]))
        .context("搜狐完整作品时长缺失")?;
    let durations = data["clipsDuration"]
        .as_array()
        .filter(|array| array.len() == 1)
        .context("搜狐章节时长缺失")?;
    let duration = number(&durations[0]).context("搜狐章节时长无效")?;
    ensure!(
        total > 0.0 && (duration - total).abs() <= 1.0,
        "搜狐只提供了部分试看片段，未提供完整作品"
    );
    // Catalogue clipsBytes is not an authoritative size after CDN dispatch.
    // A live complete 213-second clip declared 14,271,403 bytes, while both
    // dispatched variants reported 14,265,796 in Content-Range. Trusting the
    // former makes a valid tail probe request bytes beyond the actual file.
    // Leave the size unknown so MediaProbe discovers the real extent, checks
    // its final byte and verifies full-response headers as usual.
    Ok((clip.into(), replacement.into(), None))
}

fn sohu_choices(data: &Value, initial: &str) -> Vec<(String, String)> {
    let mut choices = Vec::new();
    let mut seen = HashSet::new();
    for name in ["nor", "high", "super", "ori", "h2644k", "h2654k"] {
        if let Some(id) = scalar_id(&data[format!("{name}Vid")]).filter(|id| valid_quality_id(id)) {
            if seen.insert(id.clone()) {
                choices.push((name.into(), id));
            }
        }
    }
    if seen.insert(initial.into()) {
        choices.push(("default".into(), initial.into()));
    }
    choices
}

fn sohu_label(format: &str) -> &str {
    match format {
        "nor" => "标清",
        "high" => "高清",
        "super" => "超清",
        "ori" => "原始",
        "h2644k" => "4K AVC",
        "h2654k" => "4K HEVC",
        _ => "完整视频",
    }
}

fn haokan_id(url: &Url) -> Result<String> {
    ensure!(
        url.host_str() == Some("haokan.baidu.com") && url.path().trim_end_matches('/') == "/v",
        "请输入好看单作品链接，例如 /v?vid=18435139826687843612"
    );
    crate::validation::parse_url(url.as_str())?;
    let ids: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| key == "vid")
        .map(|(_, value)| value.into_owned())
        .collect();
    ensure!(
        ids.len() == 1 && digits(&ids[0], 24),
        "好看视频链接需要唯一的数字 vid"
    );
    Ok(ids.into_iter().next().unwrap())
}

fn parse_haokan(html: &str, id: &str, page: &Url) -> Result<GenericResource> {
    ensure!(html.len() <= MAX_BYTES, "好看页面超过解析上限");
    let regex = Regex::new(r#"(?:window\.)?__PRELOADED_STATE__\s*=\s*"#)?;
    let matches: Vec<_> = regex.find_iter(html).collect();
    ensure!(
        matches.len() == 1,
        "好看页面未公开目标视频，可能需要登录或浏览器验证"
    );
    let data = serde_json::Deserializer::from_str(&html[matches[0].end()..])
        .into_iter::<Value>()
        .next()
        .context("好看页面缺少播放数据")?
        .context("好看公开视频数据格式已变化")?;
    let meta = &data["curVideoMeta"];
    ensure!(
        meta.is_object() && scalar_id(&meta["id"]).as_deref() == Some(id),
        "好看未返回请求作品的公开数据，不使用推荐视频代替"
    );
    ensure!(
        !truthy(&meta["is_pay_column"]),
        "好看该作品属于付费栏目，未提供公开下载授权"
    );
    ensure!(
        !truthy(&meta["live_type"]) && !meta["live_room"].is_object(),
        "暂不支持好看直播作品"
    );
    let title = meta["title"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .context("好看作品缺少标题")?
        .to_owned();
    let duration = number(&meta["duration"]).context("好看作品缺少完整时长")?;
    ensure!(duration > 0.0, "好看作品尚未完成或未提供完整点播");
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for item in meta["clarityUrl"].as_array().into_iter().flatten().take(16) {
        let Some(raw) = item["url"].as_str() else {
            continue;
        };
        let Ok(url) = safe_media_url(raw, page, "Haokan") else {
            continue;
        };
        if !seen.insert(url.to_string()) {
            continue;
        }
        let height = item["vodVideoHW"]
            .as_str()
            .and_then(|value| value.split_once("$$"))
            .and_then(|(height, _)| height.parse::<u32>().ok())
            .filter(|height| *height <= 16384)
            .unwrap_or(0);
        let id = item["key"]
            .as_str()
            .filter(|key| {
                key.len() <= 64 && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
            .map(str::to_owned)
            .unwrap_or_else(|| format!("format-{}", candidates.len() + 1));
        let title = item["title"].as_str().unwrap_or("公开视频");
        let label = if height > 0 {
            format!("{height}p · {title} · MP4")
        } else {
            format!("{title} · MP4")
        };
        candidates.push(GenericCandidate {
            transform: None,
            id,
            label,
            height,
            url: url.into(),
            extension: "mp4".into(),
            audio_only: false,
            headers: media_headers(page, MOBILE_AGENT),
            size: None,
        });
    }
    if let Some(raw) = meta["playurl"].as_str() {
        if let Ok(url) = safe_media_url(raw, page, "Haokan") {
            if seen.insert(url.to_string()) {
                candidates.push(GenericCandidate {
                    transform: None,
                    id: "original".into(),
                    label: "原始 MP4".into(),
                    height: 0,
                    url: url.into(),
                    extension: "mp4".into(),
                    audio_only: false,
                    headers: media_headers(page, MOBILE_AGENT),
                    size: None,
                });
            }
        }
    }
    ensure!(
        !candidates.is_empty(),
        "好看没有提供目标作品的完整媒体文件，可能需要登录或受访问限制"
    );
    Ok(GenericResource {
        id: id.into(),
        title,
        duration_seconds: Some(duration.round() as u64),
        candidates,
        embed: None,
    })
}

fn safe_media_url(raw: &str, page: &Url, platform: &str) -> Result<Url> {
    ensure!(!raw.is_empty() && raw.len() <= 16_384, "媒体地址无效");
    let url = page.join(raw)?;
    crate::validation::parse_url(url.as_str())?;
    ensure!(url.fragment().is_none(), "媒体地址包含无效 fragment");
    let host = url.host_str().unwrap_or_default();
    let allowed = match platform {
        "Sohu" => allowed_url(&url, "Sohu"),
        "Haokan" => {
            ["bdstatic.com", "baidu.com"]
                .iter()
                .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
                && url.scheme() == "https"
                && url.path().to_ascii_lowercase().ends_with(".mp4")
        }
        _ => false,
    };
    ensure!(allowed, "公开媒体地址不在平台 CDN 内或不是完整 MP4 文件");
    Ok(url)
}

fn media_headers(page: &Url, user_agent: &str) -> Vec<(String, String)> {
    let mut referer = page.clone();
    referer.set_fragment(None);
    vec![
        ("User-Agent".into(), user_agent.into()),
        ("Referer".into(), referer.into()),
    ]
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|n| n.is_finite() && *n >= 0.0)
}

fn scalar_id(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|number| number.to_string()))
}

fn truthy(value: &Value) -> bool {
    value.as_bool() == Some(true) || number(value).is_some_and(|n| n > 0.0)
}
fn digits(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && value.bytes().all(|b| b.is_ascii_digit())
}
fn valid_quality_id(value: &str) -> bool {
    match value.split_once('_') {
        Some((base, suffix)) => digits(base, 24) && digits(suffix, 4),
        None => digits(value, 24),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sohu_fixture() -> Value {
        json!({"play":1,"tvid":78693464,"allot":"data.vod.itc.cn","data":{"tvName":"目标短片","totalBlocks":1,"totalDurationDouble":213,"clipsDuration":[213],"clipsBytes":["14271403"],"mp4PlayUrl":["https://data.vod.itc.cn/?new=asset"],"su":["/asset.mp4"],"highVid":"78693464_1"}})
    }

    #[test]
    fn sohu_canonical_encoded_and_requested_work_identity() {
        let legacy = Url::parse("http://my.tv.sohu.com/us/232799889/78693464.shtml").unwrap();
        let encoded =
            Url::parse("https://tv.sohu.com/v/dXMvMjMyNzk5ODg5Lzc4NjkzNDY0LnNodG1s.html").unwrap();
        assert_eq!(sohu_target(&legacy).unwrap().id, "78693464");
        assert_eq!(
            sohu_target(&encoded).unwrap().canonical,
            sohu_target(&legacy).unwrap().canonical
        );
        let external = general_purpose::URL_SAFE.encode("https://evil.example/78693464.shtml");
        assert!(sohu_target(
            &Url::parse(&format!("https://tv.sohu.com/v/{external}.html")).unwrap()
        )
        .is_err());
        assert!(sohu_metadata(&sohu_fixture(), "999").is_err());
        assert_eq!(
            sohu_page_vid("var vid='78693464'; var relatedVid='999';").unwrap(),
            "78693464"
        );
    }

    #[test]
    fn sohu_never_presents_first_chapter_or_preview_as_full_work() {
        let mut metadata = sohu_fixture();
        assert_eq!(
            sohu_single_clip(sohu_metadata(&metadata, "78693464").unwrap())
                .unwrap()
                .2,
            None
        );
        metadata["data"]["totalBlocks"] = json!(3);
        assert!(sohu_metadata(&metadata, "78693464").is_err());
        metadata["data"]["totalBlocks"] = json!(1);
        metadata["data"]["clipsDuration"] = json!([30]);
        assert!(sohu_metadata(&metadata, "78693464").is_err());
        metadata["data"]["clipsDuration"] = json!([213]);
        for key in ["fee", "isdrm", "preview"] {
            metadata[key] = json!(1);
            assert!(sohu_metadata(&metadata, "78693464").is_err());
            metadata[key] = json!(0);
        }
    }

    fn haokan_fixture() -> Value {
        json!({"curVideoMeta":{"id":"18435139826687843612","title":"目标作品","duration":47,"clarityUrl":[{"key":"hd","title":"高清","url":"https://vd4.bdstatic.com/target.mp4?authorization=A%2Bb+raw","vodVideoHW":"480$$852","videoSize":1.5}],"playurl":"https://vd4.bdstatic.com/target.mp4?authorization=A%2Bb+raw","6s_play_url":"https://vd4.bdstatic.com/preview.mp4"},"curVideoRelate":[{"id":"999","playurl":"https://vd4.bdstatic.com/recommended.mp4"}]})
    }

    #[tokio::test]
    async fn sohu_stale_catalogue_size_does_not_replace_cdn_tail_validation() {
        use axum::{
            body::Body,
            http::{HeaderMap, Response, StatusCode},
            routing::get,
            Router,
        };
        use std::sync::{Arc, Mutex};
        let metadata = sohu_fixture();
        assert_eq!(metadata["data"]["clipsBytes"][0], "14271403");
        let (_, _, size) = sohu_single_clip(sohu_metadata(&metadata, "78693464").unwrap()).unwrap();
        assert_eq!(size, None);
        // The CDN extent is independent of the catalogue. Use a tiny analogue
        // of the reported production mismatch without downloading real media.
        let requests = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let seen = requests.clone();
        let app = Router::new().route(
            "/dispatched.mp4",
            get(move |headers: HeaderMap| {
                let seen = seen.clone();
                async move {
                    let range = headers
                        .get("range")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    seen.lock().unwrap().push(range.clone());
                    match range.as_deref() {
                        Some("bytes=0-0") | Some("bytes=7-7") => {
                            let bounds = range.unwrap().strip_prefix("bytes=").unwrap().to_owned();
                            Response::builder()
                                .status(StatusCode::PARTIAL_CONTENT)
                                .header("content-type", "video/mp4")
                                .header("content-range", format!("bytes {bounds}/8"))
                                .body(Body::from(vec![0_u8]))
                                .unwrap()
                        }
                        Some(_) => Response::builder()
                            .status(StatusCode::RANGE_NOT_SATISFIABLE)
                            .header("content-range", "bytes */8")
                            .body(Body::empty())
                            .unwrap(),
                        None => Response::builder()
                            .header("content-type", "video/mp4")
                            .body(Body::from(vec![0_u8; 8]))
                            .unwrap(),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/dispatched.mp4", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let probe = crate::youtube::MediaProbe::new().unwrap();
        let mut media = crate::media::MediaStream {
            transform: None,
            url,
            headers: Vec::new(),
            size: Some(17),
            decrypt_key: None,
        };
        assert!(probe
            .check(&media)
            .await
            .unwrap_err()
            .to_string()
            .contains("416"));
        requests.lock().unwrap().clear();
        media.size = size;
        probe.check(&media).await.unwrap();
        assert_eq!(
            *requests.lock().unwrap(),
            vec![Some("bytes=0-0".into()), Some("bytes=7-7".into()), None]
        );
        server.abort();
    }

    fn haokan_html(data: &Value) -> String {
        format!("<script>window.__PRELOADED_STATE__ = {data};</script>")
    }

    #[test]
    fn haokan_target_formats_preserve_authorization_and_ignore_recommendations() {
        let page = Url::parse("https://haokan.baidu.com/v?vid=18435139826687843612#t=10").unwrap();
        let parsed = parse_haokan(
            &haokan_html(&haokan_fixture()),
            "18435139826687843612",
            &page,
        )
        .unwrap();
        assert_eq!(parsed.candidates.len(), 1);
        assert_eq!(parsed.candidates[0].height, 480);
        assert_eq!(
            parsed.candidates[0].url,
            "https://vd4.bdstatic.com/target.mp4?authorization=A%2Bb+raw"
        );
        assert_eq!(parsed.candidates[0].size, None); // videoSize is rounded MB, not exact bytes.
        assert_eq!(parsed.duration_seconds, Some(47));
        assert!(parsed.candidates[0]
            .headers
            .iter()
            .all(|(_, value)| !value.contains("#t=10")));
        assert!(parse_haokan(&haokan_html(&haokan_fixture()), "999", &page).is_err());
        let mut data = haokan_fixture();
        data["curVideoMeta"] = Value::Null;
        assert!(parse_haokan(&haokan_html(&data), "999", &page).is_err());
    }

    #[test]
    fn haokan_numeric_id_pay_live_and_invalid_media_are_checked() {
        let page = Url::parse("https://haokan.baidu.com/v?vid=18435139826687843612").unwrap();
        let mut data = haokan_fixture();
        data["curVideoMeta"]["id"] = json!(18435139826687843612_u64);
        assert!(parse_haokan(&haokan_html(&data), "18435139826687843612", &page).is_ok());
        data["curVideoMeta"]["is_pay_column"] = json!(1);
        assert!(parse_haokan(&haokan_html(&data), "18435139826687843612", &page).is_err());
        data["curVideoMeta"]["is_pay_column"] = json!(0);
        data["curVideoMeta"]["live_type"] = json!(1);
        assert!(parse_haokan(&haokan_html(&data), "18435139826687843612", &page).is_err());
        assert!(safe_media_url(
            "https://vd4.bdstatic.com.evil.example/v.mp4",
            &page,
            "Haokan"
        )
        .is_err());
        assert!(safe_media_url(
            "https://user:secret@vd4.bdstatic.com/v.mp4",
            &page,
            "Haokan"
        )
        .is_err());
    }

    #[test]
    fn rejects_duplicate_ids_script_execution_and_forged_hosts() {
        assert_eq!(
            haokan_id(&Url::parse("https://haokan.baidu.com/v/?vid=18435139826687843612").unwrap())
                .unwrap(),
            "18435139826687843612"
        );
        assert!(haokan_id(&Url::parse("https://haokan.baidu.com/v?vid=1&vid=2").unwrap()).is_err());
        assert!(
            haokan_id(&Url::parse("https://haokan.baidu.com.evil.example/v?vid=1").unwrap())
                .is_err()
        );
        assert!(sohu_target(
            &Url::parse("https://tv.sohu.com.evil.example/20261014/n1.shtml").unwrap()
        )
        .is_err());
        let page = Url::parse("https://haokan.baidu.com/v?vid=1").unwrap();
        assert!(parse_haokan(
            "<script>window.__PRELOADED_STATE__=fetch('https://evil.example');</script>",
            "1",
            &page
        )
        .is_err());
        assert!(!allowed_url(
            &Url::parse("https://data.vod.itc.cn.evil.example/").unwrap(),
            "Sohu"
        ));
    }

    #[test]
    fn sohu_quality_ids_and_dispatch_source_are_bounded() {
        let mut data = sohu_fixture()["data"].clone();
        data["superVid"] = json!("999999_2");
        data["oriVid"] = json!("https://evil.example/");
        let choices = sohu_choices(&data, "78693464");
        assert_eq!(choices.len(), 3);
        assert!(!choices.iter().any(|(_, id)| id.contains("evil")));
        let page = Url::parse("https://tv.sohu.com/v/test.html").unwrap();
        assert!(safe_media_url("https://vod.tv.itc.cn/a.mp4?sig=A%2Bb", &page, "Sohu").is_ok());
        assert!(safe_media_url("file:///tmp/a.mp4", &page, "Sohu").is_err());
    }

    #[tokio::test]
    #[ignore = "requires current public Sohu and Haokan metadata/CDN dispatch"]
    async fn live_public_domestic_batch3_metadata() {
        let resolver = DomesticBatch3Resolver::new().unwrap();
        for (platform, raw) in [
            (
                "Sohu",
                "https://tv.sohu.com/v/dXMvMjMyNzk5ODg5Lzc4NjkzNDY0LnNodG1s.html",
            ),
            (
                "Haokan",
                "https://haokan.baidu.com/v?vid=18435139826687843612",
            ),
        ] {
            let resource = resolver
                .resolve(&Url::parse(raw).unwrap(), platform)
                .await
                .unwrap();
            assert!(!resource.title.is_empty());
            assert!(!resource.candidates.is_empty());
            eprintln!(
                "{platform}: {} complete public formats",
                resource.candidates.len()
            );
        }
    }
}
