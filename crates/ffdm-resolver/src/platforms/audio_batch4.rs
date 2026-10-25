//! Public single-audio adapters using Clyp's documented JSON API, Audioboom's
//! native V5 player properties, and Audius' documented resolve/track API.
//! Metadata layouts were cross-checked with yt-dlp's clyp.py, audioboom.py and
//! audius.py. No external extractor, page script, login or gate is executed.
use crate::generic::{GenericCandidate, GenericResource};
use anyhow::{anyhow, bail, ensure, Context, Result};
use percent_encoding::percent_decode_str;
use regex::Regex;
use reqwest::{header, redirect::Policy, Client, Url};
use serde_json::Value;
use std::time::Duration;

const MAX_BYTES: usize = 2 * 1024 * 1024;

pub(crate) struct AudioBatch4Resolver {
    client: Client,
}

impl AudioBatch4Resolver {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .connect_timeout(Duration::from_secs(6))
                .timeout(Duration::from_secs(15))
                .user_agent(crate::resource::USER_AGENT)
                .redirect(metadata_redirects())
                .build()?,
        })
    }

    pub(crate) async fn resolve(&self, input: &Url, platform: &str) -> Result<GenericResource> {
        crate::validation::parse_url(input.as_str())?;
        ensure!(input.port().is_none(), "音频作品链接不支持自定义端口");
        reject_private_query(input)?;
        match platform {
            "Clyp" => {
                let id = clyp_id(input)?;
                let api = Url::parse(&format!("https://api.clyp.it/{id}"))?;
                let (_, body) = self.fetch(&api, platform).await?;
                let data: Value = serde_json::from_str(&body).context("Clyp 音频信息格式无效")?;
                parse_clyp(&data, &id)
            }
            "AudioBoom" => {
                let id = audioboom_id(input)?;
                let page = Url::parse(&format!("https://audioboom.com/posts/{id}"))?;
                let (final_page, body) = self.fetch(&page, platform).await?;
                ensure!(
                    audioboom_id(&final_page)? == id,
                    "AudioBoom 跳转到了其他作品"
                );
                parse_audioboom(&body, &id)
            }
            "Audius" => {
                let (owner, slug) = audius_identity(input)?;
                let page = audius_canonical(&owner, &slug)?;
                let mut api = Url::parse("https://api.audius.co/v1/resolve")?;
                api.query_pairs_mut()
                    .append_pair("url", page.as_str())
                    .append_pair("app_name", "FFDownload");
                let (final_api, body) = self.fetch(&api, platform).await?;
                let data: Value = serde_json::from_str(&body).context("Audius 音频信息格式无效")?;
                parse_audius(&data, &owner, &slug, &final_api)
            }
            _ => bail!("未接入这个公开音频平台"),
        }
    }

    async fn fetch(&self, url: &Url, platform: &str) -> Result<(Url, String)> {
        ensure!(metadata_url(url), "音频元数据地址不在官方接口内");
        let mut response = self
            .client
            .get(url.clone())
            .header(header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(|error| anyhow!("{platform} 公开音频信息连接失败：{}", error.without_url()))?;
        match response.status().as_u16() {
            401 | 403 => bail!("{platform} 要求登录、授权或当前网络不能访问"),
            404 | 410 => bail!("{platform} 作品不存在或已经删除"),
            429 => bail!("{platform} 请求过于频繁，请稍后重试"),
            code => ensure!(
                response.status().is_success(),
                "{platform} 返回 HTTP {code}"
            ),
        }
        ensure!(
            response.content_length().unwrap_or(0) <= MAX_BYTES as u64,
            "音频元数据超过 2 MiB 上限"
        );
        let final_url = response.url().clone();
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| anyhow!("读取 {platform} 音频信息失败：{}", error.without_url()))?
        {
            ensure!(
                bytes.len().saturating_add(chunk.len()) <= MAX_BYTES,
                "音频元数据超过 2 MiB 上限"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok((
            final_url,
            String::from_utf8(bytes).context("音频元数据不是 UTF-8 文本")?,
        ))
    }
}

fn metadata_redirects() -> Policy {
    Policy::custom(|attempt| {
        if attempt.previous().len() >= 4
            || !metadata_url(attempt.url())
            || attempt
                .previous()
                .first()
                .is_some_and(|first| first.origin() != attempt.url().origin())
        {
            attempt.error("audio metadata redirect changed origin")
        } else {
            attempt.follow()
        }
    })
}

fn metadata_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.host_str(),
            Some("api.clyp.it" | "audioboom.com" | "api.audius.co")
        )
}

fn reject_private_query(url: &Url) -> Result<()> {
    ensure!(
        !url.query_pairs().any(|(key, _)| matches!(
            key.to_ascii_lowercase().as_str(),
            "token" | "password" | "secret" | "secret_token" | "access_token"
        )),
        "这里只支持公开单音频；口令、私密或授权分享链接暂不支持"
    );
    Ok(())
}

fn clyp_id(url: &Url) -> Result<String> {
    ensure!(
        matches!(url.host_str(), Some("clyp.it" | "www.clyp.it")),
        "请输入 Clyp 公开单音频链接"
    );
    let id = url.path().trim_matches('/');
    ensure!(
        id.len() == 8
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
        "请输入 Clyp 单音频链接，列表和用户主页不支持"
    );
    Ok(id.to_owned())
}

fn audioboom_id(url: &Url) -> Result<String> {
    ensure!(
        matches!(url.host_str(), Some("audioboom.com" | "www.audioboom.com")),
        "请输入 AudioBoom 公开单音频链接"
    );
    let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
    let [kind, slug] = parts.as_slice() else {
        bail!("AudioBoom 仅支持单期作品，合集和频道不支持")
    };
    ensure!(
        matches!(*kind, "posts" | "boos"),
        "AudioBoom 仅支持 posts 或 boos 单期作品"
    );
    let id: String = slug.chars().take_while(char::is_ascii_digit).collect();
    let rest = &slug[id.len()..];
    ensure!(
        !id.is_empty()
            && id.len() <= 20
            && (rest.is_empty() || rest.starts_with('-') || rest == ".mp3"),
        "AudioBoom 作品标识无效"
    );
    Ok(id)
}

fn audius_identity(url: &Url) -> Result<(String, String)> {
    ensure!(
        matches!(url.host_str(), Some("audius.co" | "www.audius.co")),
        "请输入 Audius 公开单曲链接"
    );
    let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
    let [owner, slug] = parts.as_slice() else {
        bail!("Audius 仅支持公开单曲，专辑、歌单和用户主页不支持")
    };
    ensure!(
        !owner.is_empty()
            && owner.len() <= 100
            && owner
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')),
        "Audius 作者标识无效"
    );
    let slug = percent_decode_str(slug)
        .decode_utf8()
        .context("Audius 单曲标识编码无效")?;
    ensure!(
        !slug.is_empty()
            && slug.len() <= 500
            && !matches!(slug.as_ref(), "album" | "playlist")
            && !slug
                .chars()
                .any(|c| c.is_control() || matches!(c, '/' | '\\')),
        "Audius 单曲标识无效"
    );
    Ok(((*owner).to_owned(), slug.into_owned()))
}

fn audius_canonical(owner: &str, slug: &str) -> Result<Url> {
    let mut page = Url::parse("https://audius.co/")?;
    page.path_segments_mut()
        .unwrap()
        .pop_if_empty()
        .push(owner)
        .push(slug);
    Ok(page)
}

fn parse_clyp(data: &Value, id: &str) -> Result<GenericResource> {
    ensure!(
        text(&data["AudioFileId"]) == Some(id),
        "Clyp 返回的音频与请求不一致"
    );
    let page = Url::parse(text(&data["Url"]).context("Clyp 作品地址缺失")?)?;
    ensure!(clyp_id(&page)? == id, "Clyp 作品地址与请求不一致");
    ensure!(
        text(&data["Status"]) == Some("Public"),
        "Clyp 作品未公开、已经删除或作者关闭了下载"
    );
    reject_restricted(data)?;
    let mut candidates = Vec::new();
    for (extension, fields) in [
        ("mp3", ["SecureMp3Url", "Mp3Url"]),
        ("ogg", ["SecureOggUrl", "OggUrl"]),
    ] {
        if let Some(raw) = fields.into_iter().find_map(|field| text(&data[field])) {
            let url = audio_url(raw)?;
            ensure!(
                url.host_str() == Some("a.clyp.it") && url.path() == format!("/{id}.{extension}"),
                "Clyp 音频地址不属于请求作品"
            );
            candidates.push(candidate(
                &format!("clyp-{id}-{extension}"),
                url,
                extension,
                &page,
            ));
        }
    }
    ensure!(!candidates.is_empty(), "Clyp 未提供完整公开音频地址");
    Ok(resource(
        format!("clyp-{id}"),
        data,
        "Title",
        "Duration",
        candidates,
    ))
}

fn parse_audioboom(html: &str, id: &str) -> Result<GenericResource> {
    let tags = Regex::new(r"(?is)<[a-z][^>]*>")?;
    let mut stores = Vec::new();
    for tag in tags.find_iter(html) {
        if attribute(tag.as_str(), "data-react-class")?.as_deref() == Some("V5DetailPagePlayer") {
            let raw =
                attribute(tag.as_str(), "data-react-props")?.context("AudioBoom 播放器信息缺失")?;
            stores.push(
                serde_json::from_str::<Value>(&decode_entities(&raw))
                    .context("AudioBoom 播放器 JSON 无效")?,
            );
        }
    }
    ensure!(stores.len() == 1, "AudioBoom 页面没有唯一的公开单期播放器");
    let clips = stores[0]["clips"]
        .as_array()
        .context("AudioBoom 作品音频信息缺失")?;
    ensure!(
        clips.len() == 1,
        "AudioBoom 播放器包含多个作品，暂不支持合集"
    );
    let clip = &clips[0];
    ensure!(
        value_id(&clip["clipId"]).as_deref() == Some(id),
        "AudioBoom 返回了其他作品"
    );
    let page = Url::parse(text(&clip["staticLink"]).context("AudioBoom 单期来源缺失")?)?;
    ensure!(audioboom_id(&page)? == id, "AudioBoom 作品来源不匹配");
    reject_restricted(clip)?;
    let url =
        audio_url(text(&clip["clipURLPriorToLoading"]).context("AudioBoom 没有完整公开音频")?)?;
    ensure!(
        url.path().to_ascii_lowercase().ends_with(".mp3"),
        "AudioBoom 未提供完整 MP3 音频"
    );
    let candidates = vec![candidate(&format!("audioboom-{id}-mp3"), url, "mp3", &page)];
    Ok(resource(
        format!("audioboom-{id}"),
        clip,
        "title",
        "duration",
        candidates,
    ))
}

fn parse_audius(data: &Value, owner: &str, slug: &str, final_api: &Url) -> Result<GenericResource> {
    let track = data
        .get("data")
        .filter(|value| value.is_object())
        .context("Audius 没有返回单曲对象")?;
    let id = text(&track["id"]).context("Audius 单曲 ID 缺失")?;
    ensure!(
        !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric()),
        "Audius 单曲 ID 无效"
    );
    ensure!(
        final_api.host_str() == Some("api.audius.co")
            && (final_api.path() == "/v1/resolve"
                || final_api.path() == format!("/v1/tracks/{id}")),
        "Audius API 返回了其他作品"
    );
    let permalink = text(&track["permalink"]).context("Audius 作品链接缺失")?;
    let page = Url::parse("https://audius.co/")?.join(permalink)?;
    let actual = audius_identity(&page)?;
    ensure!(
        actual.0.eq_ignore_ascii_case(owner) && actual.1 == slug,
        "Audius 返回的单曲与请求不一致"
    );
    ensure!(
        text(&track["user"]["handle"]).is_some_and(|actual| actual.eq_ignore_ascii_case(owner)),
        "Audius 返回的单曲作者与请求不一致"
    );
    ensure!(
        track["is_streamable"] == true
            && track["is_available"] != false
            && track["access"]["stream"] != false
            && track["access"]["download"] != false,
        "Audius 作品没有完整公开音频访问权限"
    );
    reject_restricted(track)?;
    for key in [
        "is_unlisted",
        "is_delete",
        "is_scheduled_release",
        "is_stream_gated",
        "is_download_gated",
    ] {
        ensure!(
            track[key] != true,
            "Audius 作品未公开、已删除或需要额外授权"
        );
    }
    for key in [
        "access_authorities",
        "stream_conditions",
        "download_conditions",
        "allowed_api_keys",
    ] {
        ensure!(
            absent_or_empty(&track[key]),
            "Audius 音频需要额外授权，不能使用预览替代"
        );
    }
    // Only the validated track's full stream is eligible. A preview URL is
    // deliberately never a fallback, even if it is publicly readable.
    let url = if let Some(raw) = text(&track["stream"]["url"]) {
        audio_url(raw)?
    } else {
        let mut url = Url::parse(&format!("https://api.audius.co/v1/tracks/{id}/stream"))?;
        url.query_pairs_mut().append_pair("app_name", "FFDownload");
        url
    };
    let candidates = vec![candidate(&format!("audius-{id}-mp3"), url, "mp3", &page)];
    Ok(resource(
        format!("audius-{id}"),
        track,
        "title",
        "duration",
        candidates,
    ))
}

fn reject_restricted(data: &Value) -> Result<()> {
    for key in [
        "private",
        "isPrivate",
        "is_private",
        "unlisted",
        "isPreview",
        "is_preview",
        "passwordRequired",
        "password_required",
        "requiresSubscription",
        "subscriberOnly",
        "drm",
        "is_drm",
    ] {
        ensure!(data[key] != true, "音频未公开、需要额外授权或仅提供预览");
    }
    Ok(())
}

fn absent_or_empty(value: &Value) -> bool {
    value.is_null()
        || value.as_array().is_some_and(Vec::is_empty)
        || value.as_object().is_some_and(serde_json::Map::is_empty)
}

fn audio_url(raw: &str) -> Result<Url> {
    let url = crate::validation::parse_url(raw)?;
    ensure!(
        url.scheme() == "https" && url.port().is_none(),
        "公开音频地址必须使用 HTTPS"
    );
    ensure!(
        url.host_str().is_some_and(|host| host != "localhost"
            && !host.ends_with(".localhost")
            && host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_err()),
        "公开音频地址不是有效的媒体域名"
    );
    Ok(url)
}

fn candidate(id: &str, url: Url, extension: &str, page: &Url) -> GenericCandidate {
    let mut page = page.clone();
    page.set_query(None);
    page.set_fragment(None);
    GenericCandidate {
        transform: None,
        id: id.into(),
        label: format!("音频 · {}", extension.to_ascii_uppercase()),
        height: 0,
        url: url.to_string(),
        extension: extension.into(),
        audio_only: true,
        headers: vec![("Referer".into(), page.to_string())],
        size: None,
    }
}

fn resource(
    id: String,
    data: &Value,
    title: &str,
    duration: &str,
    candidates: Vec<GenericCandidate>,
) -> GenericResource {
    GenericResource {
        title: text(&data[title])
            .map(clean_text)
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| id.clone()),
        id,
        duration_seconds: data[duration]
            .as_f64()
            .filter(|v| v.is_finite() && *v > 0.0)
            .map(|v| v.ceil() as u64),
        candidates,
        embed: None,
    }
}

fn text(value: &Value) -> Option<&str> {
    value.as_str().filter(|v| !v.trim().is_empty())
}
fn value_id(value: &Value) -> Option<String> {
    text(value)
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|v| v.to_string()))
}
fn attribute(tag: &str, name: &str) -> Result<Option<String>> {
    let re = Regex::new(&format!(
        r#"(?is)\s{}\s*=\s*(?:"([^"]*)"|'([^']*)')"#,
        regex::escape(name)
    ))?;
    Ok(re
        .captures(tag)
        .and_then(|c| c.get(1).or(c.get(2)))
        .map(|v| v.as_str().to_owned()))
}
fn decode_entities(raw: &str) -> String {
    raw.replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&#x22;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}
fn clean_text(raw: &str) -> String {
    let plain = Regex::new(r"(?s)<[^>]*>").unwrap().replace_all(raw, " ");
    decode_entities(&plain)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(180)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn clyp() -> Value {
        json!({"AudioFileId":"abcd1234","Url":"https://clyp.it/abcd1234","Status":"Public",
            "Title":"Original &amp; public","Duration":12.2,
            "SecureMp3Url":"https://a.clyp.it/abcd1234.mp3","Mp3Url":"https://a.clyp.it/abcd1234.mp3",
            "SecureOggUrl":"https://a.clyp.it/abcd1234.ogg"})
    }
    fn boom() -> Value {
        json!({"clips":[{"clipId":12345,"staticLink":"https://audioboom.com/posts/12345-title",
            "title":"Exact episode","duration":60.5,"author":"Public author",
            "clipURLPriorToLoading":"https://dts.podtrac.com/redirect.mp3/audioboom.com/posts/12345.mp3?source=web&signature=keep%2Bexact"}]})
    }
    fn boom_html(data: &Value) -> String {
        format!(
            "<div data-react-props=\"{}\" data-react-class=\"V5DetailPagePlayer\"></div>",
            data.to_string()
                .replace('&', "&amp;")
                .replace('"', "&quot;")
        )
    }
    fn audius() -> Value {
        json!({"data":{"id":"AbC12","permalink":"/alice/exact-track","user":{"handle":"alice"},
            "title":"Full track","duration":30,"is_streamable":true,"is_available":true,
            "is_unlisted":false,"is_stream_gated":false,"is_download_gated":false,
            "access":{"stream":true,"download":true},"stream":{"url":"https://audio.example/full.mp3?sig=keep%2Bexact"},
            "preview":{"url":"https://audio.example/preview.mp3"}}})
    }
    fn audius_api() -> Url {
        Url::parse("https://api.audius.co/v1/tracks/AbC12").unwrap()
    }

    #[test]
    fn clyp_exact_public_audio_is_deduplicated_and_download_disabled_is_rejected() {
        let resource = parse_clyp(&clyp(), "abcd1234").unwrap();
        assert_eq!(resource.title, "Original & public");
        assert_eq!(resource.duration_seconds, Some(13));
        assert_eq!(resource.candidates.len(), 2);
        assert!(resource
            .candidates
            .iter()
            .all(|c| c.audio_only && c.height == 0));
        // Legacy responses contain both secure and insecure versions of the
        // same encode. A valid secure URL must win without fetching HTTP.
        let mut legacy = clyp();
        legacy["Mp3Url"] = json!("http://a.clyp.it/abcd1234.mp3");
        assert_eq!(parse_clyp(&legacy, "abcd1234").unwrap().candidates.len(), 2);
        assert!(parse_clyp(&clyp(), "abcd9999").is_err());
        for status in [
            "Private",
            "DownloadDisabled",
            "PrivateDownloadDisabled",
            "Deleted",
        ] {
            let mut data = clyp();
            data["Status"] = json!(status);
            assert!(parse_clyp(&data, "abcd1234").is_err());
        }
        let mut data = clyp();
        data["Url"] = json!("https://clyp.it/wrong123");
        assert!(parse_clyp(&data, "abcd1234").is_err());
        let mut data = clyp();
        data["SecureMp3Url"] = json!("https://a.clyp.it/wrong123.mp3");
        assert!(parse_clyp(&data, "abcd1234").is_err());
    }

    #[test]
    fn audioboom_uses_exact_player_clip_and_preserves_escaped_media_query() {
        let resource = parse_audioboom(&boom_html(&boom()), "12345").unwrap();
        assert_eq!(resource.duration_seconds, Some(61));
        assert_eq!(resource.candidates.len(), 1);
        assert!(resource.candidates[0].audio_only);
        assert!(resource.candidates[0]
            .url
            .ends_with("?source=web&signature=keep%2Bexact"));
        assert!(parse_audioboom(&boom_html(&boom()), "54321").is_err());
        let mut data = boom();
        data["clips"][0]["staticLink"] = json!("https://audioboom.com/posts/54321");
        assert!(parse_audioboom(&boom_html(&data), "12345").is_err());
        let mut data = boom();
        data["clips"][0]["isPreview"] = json!(true);
        assert!(parse_audioboom(&boom_html(&data), "12345").is_err());
        let mut data = boom();
        let extra = data["clips"][0].clone();
        data["clips"].as_array_mut().unwrap().push(extra);
        assert!(parse_audioboom(&boom_html(&data), "12345").is_err());
        let html = format!("{}{}", boom_html(&boom()), boom_html(&boom()));
        assert!(parse_audioboom(&html, "12345").is_err());
    }

    #[test]
    fn audius_requires_matching_owner_permalink_and_api_id_never_preview() {
        let data = audius();
        let resource = parse_audius(&data, "alice", "exact-track", &audius_api()).unwrap();
        assert!(resource.candidates[0].audio_only);
        assert_eq!(resource.candidates[0].extension, "mp3");
        assert_eq!(
            resource.candidates[0].url,
            "https://audio.example/full.mp3?sig=keep%2Bexact"
        );
        assert!(parse_audius(&data, "other", "exact-track", &audius_api()).is_err());
        assert!(parse_audius(&data, "alice", "another-track", &audius_api()).is_err());
        assert!(parse_audius(
            &data,
            "alice",
            "exact-track",
            &Url::parse("https://api.audius.co/v1/tracks/Other").unwrap()
        )
        .is_err());
        let mut data = audius();
        data["data"]["user"]["handle"] = json!("other");
        assert!(parse_audius(&data, "alice", "exact-track", &audius_api()).is_err());
        let mut data = audius();
        data["data"]["stream"] = Value::Null;
        let resource = parse_audius(&data, "alice", "exact-track", &audius_api()).unwrap();
        assert!(resource.candidates[0]
            .url
            .starts_with("https://api.audius.co/v1/tracks/AbC12/stream?"));
        let mut data = audius();
        data["data"] = json!([data["data"].clone()]);
        assert!(parse_audius(&data, "alice", "exact-track", &audius_api()).is_err());
    }

    #[test]
    fn audius_refuses_gates_unlisted_unavailable_and_access_denial() {
        for field in [
            "is_unlisted",
            "is_private",
            "is_preview",
            "is_delete",
            "is_stream_gated",
            "is_download_gated",
            "is_scheduled_release",
        ] {
            let mut data = audius();
            data["data"][field] = json!(true);
            assert!(
                parse_audius(&data, "alice", "exact-track", &audius_api()).is_err(),
                "{field}"
            );
        }
        for field in ["is_streamable", "is_available"] {
            let mut data = audius();
            data["data"][field] = json!(false);
            assert!(
                parse_audius(&data, "alice", "exact-track", &audius_api()).is_err(),
                "{field}"
            );
        }
        for field in [
            "stream_conditions",
            "download_conditions",
            "access_authorities",
            "allowed_api_keys",
        ] {
            let mut data = audius();
            data["data"][field] = json!(["required"]);
            assert!(
                parse_audius(&data, "alice", "exact-track", &audius_api()).is_err(),
                "{field}"
            );
        }
        let mut data = audius();
        data["data"]["access"]["stream"] = json!(false);
        assert!(parse_audius(&data, "alice", "exact-track", &audius_api()).is_err());
    }

    #[test]
    fn single_audio_identity_rejects_playlists_lookalikes_and_secret_links() {
        for raw in [
            "https://clyp.it/playlist/abcd1234",
            "https://clyp.it.evil.test/abcd1234",
        ] {
            assert!(clyp_id(&Url::parse(raw).unwrap()).is_err());
        }
        for raw in [
            "https://audioboom.com/channels/12345",
            "https://audioboom.com/posts/12345/wrong",
            "https://audioboom.com/posts/12345wrong",
        ] {
            assert!(audioboom_id(&Url::parse(raw).unwrap()).is_err());
        }
        for raw in [
            "https://audius.co/alice/playlist/set",
            "https://audius.co/alice/album/set",
            "https://audius.co/alice",
            "https://audius.co/alice/track%2Fother",
        ] {
            assert!(audius_identity(&Url::parse(raw).unwrap()).is_err());
        }
        assert!(reject_private_query(
            &Url::parse("https://clyp.it/abcd1234?token=private").unwrap()
        )
        .is_err());
        assert!(!metadata_url(
            &Url::parse("https://api.audius.co.evil.test/v1/resolve").unwrap()
        ));
        for raw in [
            "http://audio.example/a.mp3",
            "https://user:secret@audio.example/a.mp3",
            "https://127.0.0.1/a.mp3",
            "https://[::1]/a.mp3",
        ] {
            assert!(audio_url(raw).is_err());
        }
    }

    #[tokio::test]
    #[ignore = "reads official Clyp public metadata; verifies author-disabled downloads stay refused"]
    async fn live_clyp_disabled_public_sample() {
        let resolver = AudioBatch4Resolver::new().unwrap();
        let result = resolver
            .resolve(&Url::parse("https://clyp.it/iynkjk4b").unwrap(), "Clyp")
            .await;
        assert!(result
            .err()
            .is_some_and(|e| e.to_string().contains("关闭了下载")));
    }

    #[tokio::test]
    #[ignore = "reads public AudioBoom/Audius metadata and one small response block per original audio"]
    async fn live_public_audio_metadata_and_small_samples() {
        let resolver = AudioBatch4Resolver::new().unwrap();
        let media = Client::builder()
            .timeout(Duration::from_secs(20))
            .redirect(Policy::limited(5))
            .build()
            .unwrap();
        for (platform, raw) in [
            (
                "AudioBoom",
                "https://audioboom.com/posts/7364167-nhl-therink-trailer",
            ),
            ("Audius", "https://audius.co/radiojuicy/tohaj-long-drive"),
        ] {
            let resource = resolver
                .resolve(&Url::parse(raw).unwrap(), platform)
                .await
                .unwrap();
            assert_eq!(resource.candidates.len(), 1);
            let candidate = &resource.candidates[0];
            assert!(candidate.audio_only);
            let mut request = media
                .get(&candidate.url)
                .header(header::RANGE, "bytes=0-1023");
            for (key, value) in &candidate.headers {
                request = request.header(key, value);
            }
            let mut response = request
                .send()
                .await
                .map_err(|error| error.without_url())
                .unwrap();
            assert!(
                response.status().is_success(),
                "{platform}: HTTP {}",
                response.status()
            );
            let mime = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            assert!(!mime.contains("text/html") && !mime.contains("application/json"));
            let bytes = response.chunk().await.unwrap().unwrap();
            assert!(!bytes.is_empty());
            assert!(
                bytes.starts_with(b"ID3")
                    || bytes
                        .windows(2)
                        .any(|v| v[0] == 0xff && v[1] & 0xe0 == 0xe0),
                "{platform}: no MP3 signature"
            );
            println!("{platform}: full public audio metadata verified; first audio block readable");
        }
    }
}
