//! Public Huya VOD player JSON and NetEase News single-video metadata.
//! Static JSON is decoded in Rust. No page script or external extractor runs.
use crate::generic::{GenericCandidate, GenericResource};
use anyhow::{anyhow, bail, ensure, Context, Result};
use regex::Regex;
use reqwest::{header, redirect::Policy, Client, Response, Url};
use serde_json::Value;
use std::{collections::HashSet, time::Duration};

const MAX_BYTES: usize = 4 * 1024 * 1024;
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

pub(crate) struct DomesticBatch4Resolver {
    huya: Client,
    news: Client,
}

impl DomesticBatch4Resolver {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            huya: client("HuyaVideo")?,
            news: client("NetEaseNews")?,
        })
    }

    pub(crate) async fn resolve(&self, input: &Url, platform: &str) -> Result<GenericResource> {
        crate::validation::parse_url(input.as_str())?;
        match platform {
            "HuyaVideo" => {
                let id = huya_id(input)
                    .context("请输入虎牙点播单视频 /video/play/数字.html；不支持直播房间")?;
                let page = Url::parse(&format!("https://www.huya.com/video/play/{id}.html"))?;
                let response = self
                    .huya
                    .get(page)
                    .header(header::ACCEPT_ENCODING, "identity")
                    .send()
                    .await
                    .map_err(|_| anyhow!("虎牙公开点播页面连接失败"))?;
                let page = response.url().clone();
                ensure!(
                    huya_id(&page).as_deref() == Some(&id),
                    "虎牙页面跳转到了其他作品"
                );
                parse_huya(&read_body(response).await?, &id, &page)
            }
            "NetEaseNews" => {
                let id = news_id(input).context("请输入网易新闻单视频 /v/video/视频ID.html")?;
                let endpoint =
                    Url::parse(&format!("https://c.m.163.com/nc/video/detail/{id}.html"))?;
                let response = self
                    .news
                    .get(endpoint)
                    .header(header::REFERER, input.as_str())
                    .header(header::ACCEPT_ENCODING, "identity")
                    .send()
                    .await
                    .map_err(|_| anyhow!("网易新闻公开播放接口连接失败"))?;
                let data: Value = serde_json::from_str(&read_body(response).await?)
                    .context("网易新闻未返回公开播放数据")?;
                parse_news(&data, &id, input)
            }
            _ => bail!("未接入该平台的公开点播解析"),
        }
    }
}

fn client(platform: &'static str) -> Result<Client> {
    Ok(Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(18))
        .user_agent(USER_AGENT)
        .redirect(Policy::custom(move |attempt| {
            let host = attempt.url().host_str().unwrap_or_default();
            let official = match platform {
                "HuyaVideo" => matches!(host, "huya.com" | "www.huya.com"),
                "NetEaseNews" => host == "c.m.163.com",
                _ => false,
            };
            if attempt.previous().len() >= 5
                || !official
                || attempt.url().scheme() != "https"
                || !attempt.url().username().is_empty()
                || attempt.url().password().is_some()
            {
                attempt.error("public metadata redirect rejected")
            } else {
                attempt.follow()
            }
        }))
        .build()?)
}

async fn read_body(mut response: Response) -> Result<String> {
    ensure!(
        response.status().is_success(),
        "公开视频不可读取（HTTP {}）",
        response.status().as_u16()
    );
    ensure!(
        response
            .content_length()
            .is_none_or(|n| n <= MAX_BYTES as u64),
        "播放信息超过 4 MiB 解析上限"
    );
    ensure!(
        response
            .headers()
            .get(header::CONTENT_ENCODING)
            .is_none_or(|value| value.as_bytes().eq_ignore_ascii_case(b"identity")),
        "播放信息使用不支持的压缩编码"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.context("读取播放信息失败")? {
        ensure!(
            bytes.len().saturating_add(chunk.len()) <= MAX_BYTES,
            "播放信息超过 4 MiB 解析上限"
        );
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).context("公开播放数据不是 UTF-8")
}

fn digits(raw: &str) -> bool {
    !raw.is_empty() && raw.len() <= 24 && raw.bytes().all(|b| b.is_ascii_digit())
}
fn huya_id(url: &Url) -> Option<String> {
    if !matches!(url.host_str(), Some("huya.com" | "www.huya.com")) {
        return None;
    }
    let id = url
        .path()
        .strip_prefix("/video/play/")?
        .strip_suffix(".html")?;
    digits(id).then(|| id.into())
}
fn news_id(url: &Url) -> Option<String> {
    if !matches!(
        url.host_str(),
        Some("163.com" | "www.163.com" | "m.163.com")
    ) {
        return None;
    }
    let id = url
        .path()
        .strip_prefix("/v/video/")?
        .strip_suffix(".html")?;
    (id.starts_with('V')
        && id.len() > 1
        && id.len() <= 32
        && id.bytes().all(|b| b.is_ascii_alphanumeric()))
    .then(|| id.into())
}
fn scalar_id(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|n| n.to_string()))
}
fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|n| n.is_finite() && *n >= 0.0)
}
fn restricted(value: &Value) -> bool {
    value.as_bool() == Some(true) || number(value).is_some_and(|n| n > 0.0)
}

fn assignment(html: &str, name: &str) -> Result<Value> {
    let regex = Regex::new(&format!(r"\bvar\s+{}\s*=\s*", regex::escape(name)))?;
    let matches: Vec<_> = regex.find_iter(html).collect();
    ensure!(matches.len() == 1, "点播页面没有唯一的目标播放器数据");
    // Decode one JSON value only. Trailing JavaScript is never evaluated.
    serde_json::Deserializer::from_str(&html[matches[0].end()..])
        .into_iter::<Value>()
        .next()
        .context("播放器数据为空")?
        .context("播放器静态 JSON 格式已变化")
}

fn parse_huya(html: &str, id: &str, page: &Url) -> Result<GenericResource> {
    ensure!(html.len() <= MAX_BYTES, "虎牙页面超过解析上限");
    let video = assignment(html, "videoData")?;
    ensure!(
        scalar_id(&video["vid"]).as_deref() == Some(id),
        "虎牙返回的作品与请求不一致"
    );
    ensure!(
        number(&video["canPlay"]) == Some(1.0),
        "虎牙该作品未公开、已删除或需要授权"
    );
    for key in ["isPay", "isVip", "drm", "isLive"] {
        ensure!(
            !restricted(&video[key]),
            "虎牙作品需要付费授权、采用保护格式或仍在直播"
        );
    }
    let title = video["videoTitle"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .context("虎牙作品缺少标题")?
        .to_owned();
    let duration = number(&video["rawDuration"])
        .filter(|n| *n > 0.0)
        .context("虎牙未提供完整点播时长")?;
    let definitions = assignment(html, "videoDefinitions")?;
    let records = definitions["value"]
        .as_array()
        .context("虎牙没有公开点播清晰度")?;
    ensure!(records.len() <= 32, "虎牙清晰度超过解析上限");
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for (index, item) in records.iter().enumerate() {
        if let Some(returned) = scalar_id(&item["videoId"]).or_else(|| scalar_id(&item["vid"])) {
            ensure!(returned == id, "虎牙清晰度属于其他作品");
        }
        let Some(length) = number(&item["sDuration"]) else {
            continue;
        };
        if length <= 0.0 || (length - duration).abs() > 1.0 {
            continue;
        } // Never offer a short preview as the full VOD.
        let name = item["sDefName"].as_str().unwrap_or("原始画质");
        let height = name
            .trim_end_matches(['P', 'p'])
            .parse::<u32>()
            .ok()
            .filter(|h| *h <= 16384)
            .unwrap_or(0);
        let quality = if height > 0 {
            format!("{height}p")
        } else {
            name.into()
        };
        for key in ["sUrl", "sM3u8"] {
            let Some(raw) = item[key].as_str() else {
                continue;
            };
            let Some(candidate) = candidate(
                raw,
                page,
                "HuyaVideo",
                &format!("huya-{index}-{key}"),
                &quality,
                height,
            ) else {
                continue;
            };
            let url = Url::parse(&candidate.url)?;
            if url
                .query_pairs()
                .filter(|(key, _)| key == "hyvid")
                .any(|(_, value)| value != id)
            {
                continue;
            }
            if seen.insert(candidate.url.clone()) {
                candidates.push(candidate);
            }
        }
    }
    ensure!(
        !candidates.is_empty(),
        "虎牙未提供完整点播文件或公开 HLS；不会下载推荐作品或短预览"
    );
    Ok(GenericResource {
        id: id.into(),
        title,
        duration_seconds: Some(duration.round() as u64),
        candidates,
        embed: None,
    })
}

fn parse_news(data: &Value, id: &str, page: &Url) -> Result<GenericResource> {
    ensure!(
        scalar_id(&data["vid"]).as_deref() == Some(id),
        "网易新闻返回的作品与请求不一致"
    );
    // The official mobile player defaults purchaseType to 1 and treats other
    // values as paid flows. Ordinary free news also carries payFlag=1; that
    // legacy numeric enum must not be interpreted as a Boolean paid flag.
    ensure!(
        number(&data["purchaseType"]) == Some(1.0),
        "网易新闻该视频未明确开放免费完整播放"
    );
    ensure!(
        number(&data["paidPreview"]) != Some(2.0),
        "网易新闻只开放付费视频试看"
    );
    for key in [
        "isLive",
        "drm",
        "isPreview",
        "isPay",
        "needPay",
        "requireLogin",
    ] {
        ensure!(
            !restricted(&data[key]),
            "网易新闻该视频需要授权、受保护或仅开放试看"
        );
    }
    let duration = number(&data["length"])
        .filter(|n| *n > 0.0)
        .context("网易新闻未提供完整视频时长")?;
    let title = data["title"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .context("网易新闻作品缺少标题")?
        .to_owned();
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for (key, label) in [
        ("mp4Hd_url", "高清"),
        ("mp4Shd_url", "超清"),
        ("mp4_url", "完整视频"),
        ("m3u8Hd_url", "高清 HLS"),
        ("m3u8_url", "完整 HLS"),
    ] {
        let Some(raw) = data[key].as_str() else {
            continue;
        };
        // Some current APIs put an m3u8 address in mp4_url. The URL container
        // wins over the historic field name; catalogue byte sizes stay untrusted.
        if let Some(candidate) = candidate(raw, page, "NetEaseNews", key, label, 0) {
            if seen.insert(candidate.url.clone()) {
                candidates.push(candidate);
            }
        }
    }
    ensure!(
        !candidates.is_empty(),
        "网易新闻没有提供请求作品的完整文件或公开 HLS"
    );
    Ok(GenericResource {
        id: id.into(),
        title,
        duration_seconds: Some(duration.round() as u64),
        candidates,
        embed: None,
    })
}

fn candidate(
    raw: &str,
    page: &Url,
    site: &str,
    id: &str,
    label: &str,
    height: u32,
) -> Option<GenericCandidate> {
    if raw.is_empty() || raw.len() > 16_384 {
        return None;
    }
    let mut url = page.join(raw).ok()?;
    crate::validation::parse_url(url.as_str()).ok()?;
    if url.fragment().is_some() {
        return None;
    }
    let host = url.host_str()?;
    let domains: &[&str] = if site == "HuyaVideo" {
        &["huya.com", "huyacdn.com", "msstatic.com"]
    } else {
        &["netease.com", "163.com", "126.net", "127.net"]
    };
    if !domains
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
    {
        return None;
    }
    // Match the official HTTPS player's protocol upgrade, without changing
    // path or signature bytes. Huya's signed URLs stay exactly as supplied.
    if site == "NetEaseNews" && url.scheme() == "http" {
        url.set_scheme("https").ok()?;
    }
    let path = url.path().to_ascii_lowercase();
    let extension = if path.ends_with(".mp4") {
        "mp4"
    } else if path.ends_with(".m3u8") {
        "m3u8"
    } else {
        return None;
    };
    let mut referer = page.clone();
    referer.set_fragment(None);
    Some(GenericCandidate {
        transform: None,
        id: id.into(),
        label: format!(
            "{label} · {}",
            if extension == "m3u8" { "HLS" } else { "MP4" }
        ),
        height,
        url: url.into(),
        extension: extension.into(),
        audio_only: false,
        headers: vec![
            ("User-Agent".into(), USER_AGENT.into()),
            ("Referer".into(), referer.into()),
        ],
        size: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn huya_html(id: u64, length: f64) -> String {
        let video = json!({"vid":id,"canPlay":1,"rawDuration":14.211,"videoTitle":"目标点播"});
        let definitions = json!({"value":[{"sDuration":length,"sDefName":"1080P","sUrl":"https://v.huya.com/full.mp4?hyvid=1002412640&sig=A%2Bb+raw","sM3u8":"https://vod.huya.com/full.m3u8?hyvid=1002412640"}]});
        format!("<script>var videoData={video};var videoDefinitions={definitions};</script>")
    }
    #[test]
    fn huya_complete_duration_target_and_signed_media() {
        let page = Url::parse("https://www.huya.com/video/play/1002412640.html#t=10").unwrap();
        let parsed = parse_huya(&huya_html(1002412640, 14.211), "1002412640", &page).unwrap();
        assert_eq!(parsed.candidates.len(), 2);
        assert_eq!(parsed.candidates[0].height, 1080);
        assert!(parsed.candidates[0].url.ends_with("sig=A%2Bb+raw"));
        assert!(parsed.candidates[0]
            .headers
            .iter()
            .all(|(_, v)| !v.contains("#t=10")));
        assert!(parse_huya(&huya_html(999, 14.211), "1002412640", &page).is_err());
        assert!(parse_huya(&huya_html(1002412640, 3.0), "1002412640", &page).is_err());
    }
    #[test]
    fn news_uses_actual_container_and_rejects_wrong_id_or_pay_preview() {
        let page = Url::parse("https://www.163.com/v/video/VK87KDIT2.html").unwrap();
        let mut data = json!({"vid":"VK87KDIT2","title":"目标新闻","length":426,"purchaseType":1,"payFlag":1,"mp4_url":"http://flv0.bn.netease.com/full.m3u8?sig=A%2Bb+raw","m3u8_url":"http://flv0.bn.netease.com/full.m3u8?sig=A%2Bb+raw","preview_url":"https://flv0.bn.netease.com/preview.mp4","sizeHD":999999});
        let parsed = parse_news(&data, "VK87KDIT2", &page).unwrap();
        assert_eq!(parsed.candidates.len(), 1);
        assert_eq!(parsed.candidates[0].extension, "m3u8");
        assert_eq!(parsed.candidates[0].size, None);
        assert_eq!(
            parsed.candidates[0].url,
            "https://flv0.bn.netease.com/full.m3u8?sig=A%2Bb+raw"
        );
        assert!(parse_news(&data, "Other", &page).is_err());
        data["paidPreview"] = json!(2);
        assert!(parse_news(&data, "VK87KDIT2", &page).is_err());
        data["paidPreview"] = json!(1);
        data["purchaseType"] = json!(2);
        assert!(parse_news(&data, "VK87KDIT2", &page).is_err());
    }
    #[test]
    fn no_live_routes_collections_binary_paths_or_foreign_media() {
        assert!(huya_id(&Url::parse("https://www.huya.com/572329").unwrap()).is_none());
        assert!(news_id(
            &Url::parse("https://www.163.com/v/video/VK87KDIT2.html/file.zip").unwrap()
        )
        .is_none());
        let page = Url::parse("https://www.163.com/v/video/VK87KDIT2.html").unwrap();
        assert!(candidate(
            "https://flv0.bn.netease.com.evil.example/full.mp4",
            &page,
            "NetEaseNews",
            "1",
            "MP4",
            0
        )
        .is_none());
        assert!(candidate(
            "https://user:secret@v.huya.com/full.mp4",
            &page,
            "HuyaVideo",
            "1",
            "MP4",
            0
        )
        .is_none());
        assert!(assignment("var videoData=fetch('https://evil.example');", "videoData").is_err());
    }
    #[tokio::test]
    #[ignore = "requires current public Huya VOD and NetEase News metadata"]
    async fn live_public_domestic_batch4_metadata() {
        let resolver = DomesticBatch4Resolver::new().unwrap();
        for (site, raw) in [
            (
                "HuyaVideo",
                "https://www.huya.com/video/play/1002412640.html",
            ),
            ("NetEaseNews", "https://www.163.com/v/video/VD868N253.html"),
        ] {
            let resource = resolver
                .resolve(&Url::parse(raw).unwrap(), site)
                .await
                .unwrap();
            assert!(!resource.candidates.is_empty());
            eprintln!(
                "{site}: {} complete public formats",
                resource.candidates.len()
            );
        }
    }
}
