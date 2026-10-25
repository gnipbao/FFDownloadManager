//! Public Medal / 9GAG player APIs, following the corresponding yt-dlp
//! extractors' format discovery. API identities are verified before media is
//! accepted; preview and privacy-placeholder URLs never become download plans.
use crate::generic::{GenericCandidate, GenericResource};
use anyhow::{bail, ensure, Context, Result};
use reqwest::{header, redirect::Policy, Client, Url};
use serde_json::Value;
use std::{collections::HashSet, time::Duration};

const MAX_BYTES: usize = 4 * 1024 * 1024;

pub(crate) struct ClipSitesResolver {
    client: Client,
}

impl ClipSitesResolver {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .connect_timeout(Duration::from_secs(8))
                .timeout(Duration::from_secs(20))
                .user_agent(crate::resource::USER_AGENT)
                .redirect(Policy::none())
                .build()?,
        })
    }

    pub(crate) async fn resolve(&self, input: &Url, platform: &str) -> Result<GenericResource> {
        let id = clip_identity(input, platform)?;
        let mut api = match platform {
            "Medal" => Url::parse(&format!("https://medal.tv/api/content/{id}"))?,
            "9GAG" => Url::parse("https://9gag.com/v1/post")?,
            _ => bail!("未接入这个短视频平台"),
        };
        if platform == "9GAG" {
            api.query_pairs_mut().append_pair("id", &id);
        }
        let data = self.json(api, input, platform).await?;
        match platform {
            "Medal" => parse_medal(&data, &id, input),
            "9GAG" => parse_ninegag(&data, &id, input),
            _ => unreachable!(),
        }
    }

    async fn json(&self, api: Url, page: &Url, platform: &str) -> Result<Value> {
        let mut response = self
            .client
            .get(api)
            .header(header::ACCEPT, "application/json")
            .header(header::REFERER, page.as_str())
            .header(header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .with_context(|| format!("{platform} 公开播放接口连接失败"))?;
        match response.status().as_u16() {
            401 | 403 => bail!("{platform} 要求登录、访问验证或当前网络无法读取"),
            404 | 410 => bail!("{platform} 作品不存在或已经删除"),
            429 => bail!("{platform} 请求过于频繁，请稍后重试"),
            _ => ensure!(
                response.status().is_success(),
                "{platform} 播放接口返回 HTTP {}",
                response.status().as_u16()
            ),
        }
        ensure!(
            response.content_length().unwrap_or(0) <= MAX_BYTES as u64,
            "{platform} 播放信息超过 4 MiB 上限"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len().saturating_add(chunk.len()) <= MAX_BYTES,
                "{platform} 播放信息超过 4 MiB 上限"
            );
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).with_context(|| format!("{platform} 未返回有效公开播放信息"))
    }
}

fn token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn clip_identity(input: &Url, platform: &str) -> Result<String> {
    crate::validation::parse_url(input.as_str())?;
    let parts: Vec<_> = input.path().trim_matches('/').split('/').collect();
    let id = match (platform, input.host_str(), parts.as_slice()) {
        ("Medal", Some("medal.tv" | "www.medal.tv"), ["games", game, "clips", id])
            if token(game) && token(id) =>
        {
            *id
        }
        ("9GAG", Some("9gag.com" | "www.9gag.com"), ["gag", id]) if token(id) => *id,
        _ => bail!("请提供 {platform} 的公开单作品链接"),
    };
    Ok(id.to_owned())
}

fn value_id(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|value| value.to_string()))
}

fn restricted(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::Number(number) => number.as_u64() != Some(0),
        Value::String(value) => !matches!(
            value.to_ascii_lowercase().as_str(),
            "" | "false" | "0" | "none"
        ),
        _ => true,
    }
}

fn clean_title(raw: Option<&str>, fallback: &str) -> String {
    raw.unwrap_or(fallback)
        .trim()
        .chars()
        .take(240)
        .collect::<String>()
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn duration(value: &Value) -> Option<u64> {
    value
        .as_f64()
        .filter(|value| value.is_finite() && *value > 0.0 && *value < 86400.0)
        .map(|value| value.ceil() as u64)
}

fn media_url(raw: &str, platform: &str) -> Option<Url> {
    if raw.len() > 16 * 1024 {
        return None;
    }
    let url = crate::validation::parse_url(raw).ok()?;
    let host = url.host_str()?;
    let official = match platform {
        "Medal" => host == "cdn.medal.tv" || host.ends_with(".medal.tv"),
        "9GAG" => host == "9cache.com" || host.ends_with(".9cache.com"),
        _ => false,
    };
    (official
        && !url.path().contains("privacy-protected-guest")
        && !url.path().contains("/preview/"))
    .then_some(url)
}

fn headers(page: &Url) -> Vec<(String, String)> {
    let mut page = page.clone();
    page.set_query(None);
    page.set_fragment(None);
    vec![
        ("User-Agent".into(), crate::resource::USER_AGENT.into()),
        ("Referer".into(), page.into()),
    ]
}

fn parse_medal(data: &Value, id: &str, page: &Url) -> Result<GenericResource> {
    ensure!(
        value_id(&data["contentId"]).as_deref() == Some(id),
        "Medal 返回了其他作品的信息"
    );
    let has_access = match &data["hasAccess"] {
        Value::Null | Value::Bool(true) => true,
        Value::Number(number) => number.as_u64() == Some(1),
        Value::String(value) => matches!(value.to_ascii_lowercase().as_str(), "true" | "1"),
        _ => false,
    };
    ensure!(
        !restricted(&data["requireLogin"]) && has_access,
        "Medal 作品需要登录或访问授权"
    );
    ensure!(
        !restricted(&data["drm"]) && !restricted(&data["isLive"]),
        "暂不支持 Medal 的受保护媒体或直播"
    );
    let title = clean_title(data["contentTitle"].as_str(), "Medal 视频");
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for (key, height) in [
        (
            "contentUrl",
            data["sourceHeight"]
                .as_u64()
                .filter(|height| *height <= 16384)
                .unwrap_or(0) as u32,
        ),
        ("contentUrl1080p", 1080),
        ("contentUrl720p", 720),
        ("contentUrl480p", 480),
        ("contentUrl360p", 360),
        ("contentUrl240p", 240),
        ("contentUrl144p", 144),
        ("contentUrlHls", 0),
    ] {
        let Some(url) = data[key].as_str().and_then(|raw| media_url(raw, "Medal")) else {
            continue;
        };
        let hls = key == "contentUrlHls";
        let path = url.path().to_ascii_lowercase();
        if !(if hls {
            path.ends_with(".m3u8")
        } else {
            path.ends_with(".mp4")
        }) || !seen.insert(url.to_string())
        {
            continue;
        }
        candidates.push(GenericCandidate {
            transform: None,
            id: format!("medal-{key}"),
            label: if hls {
                "完整 HLS".into()
            } else if height > 0 {
                format!("{height}p · MP4")
            } else {
                "原始 MP4".into()
            },
            height,
            url: url.into(),
            extension: if hls { "m3u8" } else { "mp4" }.into(),
            audio_only: false,
            headers: headers(page),
            size: None,
        });
    }
    ensure!(
        !candidates.is_empty(),
        "Medal 没有提供可公开下载的完整媒体，不能使用预览或权限占位文件"
    );
    Ok(GenericResource {
        id: format!("medal-{id}"),
        title,
        duration_seconds: duration(&data["videoLengthSeconds"]),
        candidates,
        embed: None,
    })
}

fn parse_ninegag(data: &Value, id: &str, page: &Url) -> Result<GenericResource> {
    let post = &data["data"]["post"];
    ensure!(
        value_id(&post["id"]).as_deref() == Some(id),
        "9GAG 返回了其他帖子的媒体"
    );
    ensure!(post["type"] == "Animated", "这个 9GAG 帖子没有可下载的视频");
    ensure!(
        !restricted(&post["drm"]) && !restricted(&post["isPrivate"]),
        "9GAG 媒体需要访问授权"
    );
    let images = post["images"]
        .as_object()
        .context("9GAG 没有提供本帖的公开播放数据")?;
    ensure!(images.len() <= 32, "9GAG 媒体格式超过上限");
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let mut seconds = None;
    for (name, image) in images {
        if !token(name) {
            continue;
        }
        for (key, codec) in [
            ("url", ""),
            ("vp8Url", "VP8"),
            ("vp9Url", "VP9"),
            ("h265Url", "HEVC"),
        ] {
            let Some(url) = image[key].as_str().and_then(|raw| media_url(raw, "9GAG")) else {
                continue;
            };
            let ext = url
                .path()
                .rsplit('.')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if !matches!(ext.as_str(), "mp4" | "webm") || !seen.insert(url.to_string()) {
                continue;
            }
            let height = image["height"]
                .as_u64()
                .filter(|height| *height <= 16384)
                .unwrap_or(0) as u32;
            seconds = seconds.or_else(|| duration(&image["duration"]));
            let quality = if height > 0 {
                format!("{height}p")
            } else {
                "原始视频".into()
            };
            let codec = if codec.is_empty() {
                String::new()
            } else {
                format!(" · {codec}")
            };
            candidates.push(GenericCandidate {
                transform: None,
                id: format!("9gag-{name}-{key}"),
                label: format!("{quality}{codec} · {}", ext.to_ascii_uppercase()),
                height,
                url: url.into(),
                extension: ext,
                audio_only: false,
                headers: headers(page),
                size: None,
            });
        }
    }
    ensure!(
        !candidates.is_empty(),
        "9GAG 没有提供完整视频格式，缩略图不能作为视频下载"
    );
    Ok(GenericResource {
        id: format!("9gag-{id}"),
        title: clean_title(post["title"].as_str(), "9GAG 视频"),
        duration_seconds: seconds,
        candidates,
        embed: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn medal() -> (Url, Value) {
        (
            Url::parse("https://medal.tv/games/valorant/clips/Test_1").unwrap(),
            json!({"contentId":"Test_1","contentTitle":"Public &amp; full","videoLengthSeconds":13.25,"hasAccess":true,"requireLogin":false,"sourceHeight":1080,"contentUrl":"https://cdn.medal.tv/full.mp4?sig=a%2Bb+c","contentUrl1080p":"https://cdn.medal.tv/full.mp4?sig=a%2Bb+c","contentPreview720p":"https://cdn.medal.tv/preview.mp4","contentUrl720p":"https://cdn.medal.tv/720.mp4"}),
        )
    }

    #[test]
    fn medal_matches_requested_clip_and_keeps_full_formats_and_signatures() {
        let (page, data) = medal();
        let resource = parse_medal(&data, "Test_1", &page).unwrap();
        assert_eq!(resource.title, "Public & full");
        assert_eq!(resource.duration_seconds, Some(14));
        assert_eq!(resource.candidates.len(), 2);
        assert!(resource
            .candidates
            .iter()
            .any(|format| format.height == 1080 && format.url.ends_with("?sig=a%2Bb+c")));
        assert!(parse_medal(&data, "Other", &page).is_err());
    }

    #[test]
    fn medal_refuses_permission_placeholders_preview_and_unknown_media_hosts() {
        let (page, mut data) = medal();
        data["hasAccess"] = json!(false);
        assert!(parse_medal(&data, "Test_1", &page).is_err());
        data["hasAccess"] = json!(true);
        for key in ["contentUrl", "contentUrl1080p", "contentUrl720p"] {
            data[key] = json!("https://cdn.medal.tv/video/privacy-protected-guest/file.mp4");
        }
        assert!(parse_medal(&data, "Test_1", &page).is_err());
        assert!(media_url("https://cdn.medal.tv.evil.test/file.mp4", "Medal").is_none());
        assert!(media_url("https://user:pass@cdn.medal.tv/file.mp4", "Medal").is_none());
    }

    #[test]
    fn medal_rejects_mislabeled_containers_and_drops_page_fragment_from_referer() {
        let (mut page, mut data) = medal();
        page.set_query(Some("session=private"));
        page.set_fragment(Some("t=10"));
        let resource = parse_medal(&data, "Test_1", &page).unwrap();
        assert!(resource
            .candidates
            .iter()
            .flat_map(|candidate| &candidate.headers)
            .filter(|(name, _)| name == "Referer")
            .all(|(_, value)| !value.contains('#') && !value.contains('?')));
        for key in ["contentUrl", "contentUrl1080p", "contentUrl720p"] {
            data[key] = json!("https://cdn.medal.tv/thumbnail.jpg");
        }
        data["contentUrlHls"] = json!("https://cdn.medal.tv/not-a-playlist.webm");
        assert!(parse_medal(&data, "Test_1", &page).is_err());
        for denied in [json!(false), json!(0), json!("false")] {
            let (_, mut data) = medal();
            data["hasAccess"] = denied;
            assert!(parse_medal(&data, "Test_1", &page).is_err());
        }
    }

    #[test]
    fn ninegag_keeps_multiple_codecs_and_real_containers_but_not_thumbnails() {
        let page = Url::parse("https://9gag.com/gag/Test1").unwrap();
        let data = json!({"data":{"post":{"id":"Test1","type":"Animated","title":"A &amp; B","images":{"image460sv":{"height":460,"duration":12,"url":"https://img-9gag-fun.9cache.com/photo/Test1_460sv.mp4?token=a%2Bb+c","vp9Url":"https://img-9gag-fun.9cache.com/photo/Test1_460svvp9.webm"},"image460":{"url":"https://img-9gag-fun.9cache.com/photo/Test1_460s.jpg"}}}}});
        let resource = parse_ninegag(&data, "Test1", &page).unwrap();
        assert_eq!(resource.title, "A & B");
        assert_eq!(resource.candidates.len(), 2);
        assert_eq!(resource.duration_seconds, Some(12));
        assert!(resource
            .candidates
            .iter()
            .any(|candidate| candidate.extension == "webm"));
        assert!(resource
            .candidates
            .iter()
            .any(|candidate| candidate.url.ends_with("token=a%2Bb+c")));
        assert!(parse_ninegag(&data, "Recommendation", &page).is_err());
    }

    #[test]
    fn ninegag_image_only_and_spoofed_cdn_are_not_video_formats() {
        let page = Url::parse("https://9gag.com/gag/Test1").unwrap();
        let mut data = json!({"data":{"post":{"id":"Test1","type":"Photo","images":{}}}});
        assert!(parse_ninegag(&data, "Test1", &page).is_err());
        data["data"]["post"]["type"] = json!("Animated");
        data["data"]["post"]["images"] =
            json!({"image":{"url":"https://img.9cache.com.evil.test/file.mp4"}});
        assert!(parse_ninegag(&data, "Test1", &page).is_err());
    }

    #[test]
    fn clips_reject_profiles_collections_binary_paths_and_lookalikes() {
        for (url, platform) in [
            ("https://medal.tv/users/42", "Medal"),
            ("https://medal.tv/games/v/clips/Test.zip", "Medal"),
            ("https://medal.tv.evil.test/games/v/clips/Test1", "Medal"),
            ("https://9gag.com/gag/Test1/download.zip", "9GAG"),
            ("https://9gag.com/u/user", "9GAG"),
        ] {
            assert!(clip_identity(&Url::parse(url).unwrap(), platform).is_err());
        }
        assert_eq!(clip_identity(&medal().0, "Medal").unwrap(), "Test_1");
    }

    #[tokio::test]
    #[ignore = "reads public Medal metadata; no full download"]
    async fn public_medal_metadata() {
        let page = Url::parse("https://medal.tv/games/valorant/clips/jTBFnLKdLy15K").unwrap();
        let resource = ClipSitesResolver::new()
            .unwrap()
            .resolve(&page, "Medal")
            .await
            .unwrap();
        assert_eq!(resource.id, "medal-jTBFnLKdLy15K");
        assert!(resource
            .candidates
            .iter()
            .any(|format| format.height == 1080 && format.extension == "mp4"));
    }
}
