//! Public Bluesky and PeerTube adapters. Protocol references: yt-dlp's
//! bluesky.py / peertube.py and the public AT Protocol / PeerTube APIs.
//! Requested-post identity is checked before any media is accepted.
use crate::generic::{GenericCandidate, GenericResource};
use anyhow::{bail, ensure, Context, Result};
use reqwest::{header, redirect::Policy, Client, Url};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::HashSet, time::Duration};

const MAX_METADATA_BYTES: usize = 2 * 1024 * 1024;
const MAX_CANDIDATES: usize = 32;
const BLUESKY_API: &str = "https://public.api.bsky.app/";

pub(crate) struct FederatedResolver {
    client: Client,
    local_client: Client,
}

impl FederatedResolver {
    pub(crate) fn new() -> Result<Self> {
        let make = || {
            Client::builder()
                .connect_timeout(Duration::from_secs(8))
                .timeout(Duration::from_secs(20))
                .user_agent(crate::resource::USER_AGENT)
                .redirect(Policy::none())
        };
        Ok(Self {
            client: make().build()?,
            local_client: make().no_proxy().build()?,
        })
    }

    pub(crate) async fn resolve(&self, input: &Url, platform: &str) -> Result<GenericResource> {
        let input = crate::validation::parse_url(input.as_str())?;
        match platform {
            "Bluesky" => self.bluesky(&input).await,
            "PeerTube" => self.peertube(&input).await,
            _ => bail!("暂不支持这个联邦媒体平台"),
        }
    }

    async fn fetch(&self, url: &Url, platform: &str, accept: &str) -> Result<String> {
        crate::validation::parse_url(url.as_str())?;
        let client = if is_loopback(url) {
            &self.local_client
        } else {
            &self.client
        };
        let mut response = client
            .get(url.clone())
            .header(header::ACCEPT, accept)
            .header(header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .with_context(|| format!("{platform} 公开播放接口连接失败"))?;
        let status = response.status();
        match status.as_u16() {
            401 | 403 => bail!("{platform} 作品需要授权或当前网络无法访问"),
            404 | 410 => bail!("{platform} 作品不存在或已经删除"),
            429 => bail!("{platform} 请求过于频繁，请稍后重试"),
            300..=399 => bail!("{platform} 播放接口发生重定向，无法确认作品来源"),
            _ => ensure!(status.is_success(), "{platform} 播放接口返回 HTTP {status}"),
        }
        ensure!(
            response.url().origin() == url.origin(),
            "播放接口来源发生变化"
        );
        ensure!(
            response.content_length().unwrap_or(0) <= MAX_METADATA_BYTES as u64,
            "{platform} 播放信息超过 2 MiB 上限"
        );
        ensure!(
            response
                .headers()
                .get(header::CONTENT_ENCODING)
                .is_none_or(|value| value.as_bytes().eq_ignore_ascii_case(b"identity")),
            "{platform} 播放接口返回了不支持的压缩编码"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.context("读取播放接口失败")? {
            ensure!(
                bytes.len().saturating_add(chunk.len()) <= MAX_METADATA_BYTES,
                "{platform} 播放信息超过 2 MiB 上限"
            );
            bytes.extend_from_slice(&chunk);
        }
        String::from_utf8(bytes).context("播放接口不是有效的 UTF-8")
    }

    async fn json(&self, url: &Url, platform: &str) -> Result<Value> {
        let text = self.fetch(url, platform, "application/json").await?;
        serde_json::from_str(&text).with_context(|| format!("{platform} 没有返回有效播放信息"))
    }

    async fn bluesky(&self, input: &Url) -> Result<GenericResource> {
        let (handle, post_id) = bluesky_post(input)?;
        let api = Url::parse(BLUESKY_API)?;
        let did = if handle.starts_with("did:") {
            ensure!(valid_did(&handle), "Bluesky DID 格式无效");
            handle
        } else {
            let mut endpoint = api.join("xrpc/com.atproto.identity.resolveHandle")?;
            endpoint.query_pairs_mut().append_pair("handle", &handle);
            let data = self.json(&endpoint, "Bluesky").await?;
            let did = nonempty(&data["did"]).context("Bluesky 无法识别这个公开账号")?;
            ensure!(valid_did(did), "Bluesky 返回了无效账号标识");
            did.to_owned()
        };
        let mut endpoint = api.join("xrpc/app.bsky.feed.getPostThread")?;
        endpoint
            .query_pairs_mut()
            .append_pair("uri", &format!("at://{did}/app.bsky.feed.post/{post_id}"))
            .append_pair("depth", "0")
            .append_pair("parentHeight", "0");
        let data = self.json(&endpoint, "Bluesky").await?;
        parse_bluesky(&data, &did, &post_id, input)
    }

    async fn peertube(&self, input: &Url) -> Result<GenericResource> {
        let id = peertube_id(input).context("请输入 PeerTube 单个公开视频链接")?;
        ensure!(
            input.scheme() == "https" || is_loopback(input),
            "PeerTube 实例需要 HTTPS；仅本地测试支持 HTTP"
        );
        let api = input.join(&format!("/api/v1/videos/{id}"))?;
        ensure!(api.origin() == input.origin(), "PeerTube 接口来源无效");
        let data = self.json(&api, "PeerTube").await?;
        parse_peertube(&data, &id, input)
    }
}

fn bluesky_post(url: &Url) -> Result<(String, String)> {
    ensure!(
        matches!(
            url.host_str(),
            Some("bsky.app" | "www.bsky.app" | "main.bsky.dev")
        ),
        "请输入 Bluesky 公开帖子链接"
    );
    let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
    ensure!(
        parts.len() == 4 && parts[0] == "profile" && parts[2] == "post",
        "请输入 Bluesky 单个帖子链接"
    );
    let handle = percent_encoding::percent_decode_str(parts[1])
        .decode_utf8()
        .context("Bluesky 账号编码无效")?
        .into_owned();
    ensure!(
        (valid_did(&handle)
            || (handle.contains('.')
                && handle.len() <= 253
                && handle
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))))
            && parts[3].len() <= 64
            && !parts[3].is_empty()
            && parts[3].bytes().all(|byte| byte.is_ascii_alphanumeric()),
        "Bluesky 帖子地址格式无效"
    );
    Ok((handle, parts[3].to_owned()))
}

fn valid_did(did: &str) -> bool {
    did.len() <= 253
        && ((did.starts_with("did:plc:")
            && did[8..].bytes().all(|byte| byte.is_ascii_alphanumeric()))
            || (did.starts_with("did:web:")
                && did[8..].contains('.')
                && did[8..]
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))))
        && did.len() > 8
}

fn peertube_id(url: &Url) -> Option<String> {
    let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
    let id = match parts.as_slice() {
        ["w", id] | ["videos", "watch" | "embed", id] => *id,
        _ => return None,
    };
    (valid_uuid(id) || (id.len() == 22 && id.bytes().all(|byte| byte.is_ascii_alphanumeric())))
        .then(|| id.to_owned())
}

fn valid_uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn parse_bluesky(data: &Value, did: &str, id: &str, page: &Url) -> Result<GenericResource> {
    let post = &data["thread"]["post"];
    ensure!(
        nonempty(&post["uri"]) == Some(format!("at://{did}/app.bsky.feed.post/{id}").as_str())
            && nonempty(&post["author"]["did"]) == Some(did),
        "Bluesky 返回的帖子与请求不一致，或这个帖子不可公开访问"
    );
    let embed = match nonempty(&post["embed"]["$type"]) {
        Some("app.bsky.embed.video#view") => &post["embed"],
        Some("app.bsky.embed.recordWithMedia#view")
            if nonempty(&post["embed"]["media"]["$type"]) == Some("app.bsky.embed.video#view") =>
        {
            &post["embed"]["media"]
        }
        _ => bail!("这个 Bluesky 帖子本身没有公开视频；引用与回复不会当作当前作品下载"),
    };
    let playlist = nonempty(&embed["playlist"]).context("Bluesky 帖子没有公开播放地址")?;
    let url = safe_media_url(page, playlist)?;
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("video.bsky.app")
            && url.path().ends_with(".m3u8"),
        "Bluesky 视频播放地址不在公开视频服务内"
    );
    let title = nonempty(&post["record"]["text"])
        .or_else(|| nonempty(&embed["alt"]))
        .unwrap_or("Bluesky 视频")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(180)
        .collect();
    let identity = format!("at://{did}/app.bsky.feed.post/{id}");
    let digest = format!("{:x}", Sha256::digest(identity.as_bytes()));
    let source_id = format!("bluesky-{}-{id}", &digest[..16]);
    Ok(GenericResource {
        id: source_id.clone(),
        title,
        duration_seconds: None,
        candidates: vec![GenericCandidate {
            transform: None,
            id: format!("{source_id}-hls"),
            label: "HLS · MP4".to_owned(),
            height: 0,
            url: url.to_string(),
            extension: "m3u8".to_owned(),
            audio_only: false,
            headers: media_headers(page),
            size: None,
        }],
        embed: None,
    })
}

fn parse_peertube(data: &Value, id: &str, page: &Url) -> Result<GenericResource> {
    let uuid = nonempty(&data["uuid"]).context("这个网站没有返回 PeerTube 公开视频信息")?;
    ensure!(valid_uuid(uuid), "PeerTube 返回了无效作品标识");
    ensure!(
        (valid_uuid(id) && uuid.eq_ignore_ascii_case(id))
            || nonempty(&data["shortUUID"]) == Some(id),
        "PeerTube 返回的作品与请求不一致"
    );
    ensure!(
        data["isLive"] == Value::Bool(false),
        "暂不支持 PeerTube 直播或未完成直播归档"
    );
    if let Some(privacy) = data["privacy"]["id"].as_u64() {
        ensure!(
            matches!(privacy, 1 | 2),
            "PeerTube 作品需要登录、密码或额外授权"
        );
    }
    let title = nonempty(&data["name"]).context("PeerTube 作品名称为空")?;
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    {
        let mut add_file = |file: &Value| {
            if candidates.len() >= MAX_CANDIDATES {
                return;
            }
            let Some(raw) = nonempty(&file["fileUrl"]) else {
                return;
            };
            let Ok(url) = safe_media_url(page, raw) else {
                return;
            };
            let Some(extension) = media_extension(&url, nonempty(&file["mimeType"])) else {
                return;
            };
            let height = file["resolution"]["id"].as_u64().unwrap_or(0).min(16384) as u32;
            let audio_only = matches!(
                extension,
                "mp3" | "m4a" | "ogg" | "opus" | "aac" | "flac" | "wav"
            ) || file["resolution"]["id"] == Value::Number(0_u64.into());
            if !seen.insert(url.to_string()) {
                return;
            }
            let label = if audio_only {
                format!("音频 · {}", extension.to_ascii_uppercase())
            } else if height > 0 {
                format!("{height}p · {}", extension.to_ascii_uppercase())
            } else {
                format!("视频 · {}", extension.to_ascii_uppercase())
            };
            candidates.push(GenericCandidate {
                transform: None,
                id: format!("peertube-{uuid}-file-{}", candidates.len() + 1),
                label,
                height,
                url: url.to_string(),
                extension: extension.to_owned(),
                audio_only,
                headers: media_headers(page),
                size: file["size"].as_u64().filter(|size| *size > 0),
            });
        };
        for file in data["files"].as_array().into_iter().flatten() {
            add_file(file);
        }
        for playlist in data["streamingPlaylists"].as_array().into_iter().flatten() {
            for file in playlist["files"].as_array().into_iter().flatten() {
                add_file(file);
            }
        }
    }
    for playlist in data["streamingPlaylists"].as_array().into_iter().flatten() {
        if candidates.len() >= MAX_CANDIDATES {
            break;
        }
        let Some(raw) = nonempty(&playlist["playlistUrl"]) else {
            continue;
        };
        let Ok(url) = safe_media_url(page, raw) else {
            continue;
        };
        if !url.path().ends_with(".m3u8") || !seen.insert(url.to_string()) {
            continue;
        }
        candidates.push(GenericCandidate {
            transform: None,
            id: format!("peertube-{uuid}-hls-{}", candidates.len() + 1),
            label: "HLS · MP4".to_owned(),
            height: 0,
            url: url.to_string(),
            extension: "m3u8".to_owned(),
            audio_only: false,
            headers: media_headers(page),
            size: None,
        });
    }
    ensure!(
        !candidates.is_empty(),
        "PeerTube 作品没有公开可下载的音视频格式"
    );
    Ok(GenericResource {
        id: format!("peertube-{uuid}"),
        title: title.chars().take(180).collect(),
        duration_seconds: data["duration"].as_u64(),
        candidates,
        embed: None,
    })
}

fn safe_media_url(base: &Url, raw: &str) -> Result<Url> {
    ensure!(
        raw.len() <= 16 * 1024 && !raw.trim().is_empty(),
        "媒体地址无效"
    );
    let url = base.join(raw.trim()).context("媒体地址格式无效")?;
    crate::validation::parse_url(url.as_str())?;
    ensure!(
        url.scheme() == "https" || (is_loopback(base) && is_loopback(&url)),
        "公开媒体需要 HTTPS；仅本地测试支持 HTTP"
    );
    ensure!(url.fragment().is_none(), "媒体地址不能包含 fragment");
    Ok(url)
}

fn media_extension(url: &Url, mime: Option<&str>) -> Option<&'static str> {
    let suffix = url.path().rsplit('.').next()?.to_ascii_lowercase();
    match suffix.as_str() {
        "mp4" => Some("mp4"),
        "webm" => Some("webm"),
        "m4v" => Some("mp4"),
        "mov" => Some("mov"),
        "mp3" => Some("mp3"),
        "m4a" => Some("m4a"),
        "ogg" | "oga" => Some("ogg"),
        "opus" => Some("opus"),
        "aac" => Some("aac"),
        "flac" => Some("flac"),
        "wav" => Some("wav"),
        _ => match mime?
            .split(';')
            .next()?
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "video/mp4" => Some("mp4"),
            "video/webm" => Some("webm"),
            "audio/mp4" => Some("m4a"),
            "audio/mpeg" => Some("mp3"),
            "audio/ogg" => Some("ogg"),
            "audio/opus" => Some("opus"),
            _ => None,
        },
    }
}

fn media_headers(page: &Url) -> Vec<(String, String)> {
    let mut referer = page.clone();
    referer.set_fragment(None);
    vec![
        (
            "User-Agent".to_owned(),
            crate::resource::USER_AGENT.to_owned(),
        ),
        ("Referer".to_owned(), referer.to_string()),
    ]
}

fn nonempty(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn is_loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const UUID: &str = "9c9de5e8-0a1e-484a-b099-e80766180a6d";
    const SHORT: &str = "hFdJoTuyhNJVa1cDWd1d12";
    const DID: &str = "did:plc:z72i7hdynmk6r22z27h6tvur";

    fn bluesky_data() -> Value {
        json!({"thread":{"post": {
            "uri":format!("at://{DID}/app.bsky.feed.post/3l3vgf77uco2g"),
            "author":{"did":DID}, "record":{"text":"Bluesky now has video!"},
            "embed":{"$type":"app.bsky.embed.video#view", "playlist":"https://video.bsky.app/watch/example/playlist.m3u8?token=keep"}
        }}})
    }

    fn peertube_data() -> Value {
        json!({"uuid":UUID,"shortUUID":SHORT,"name":"Public video","isLive":false,"privacy":{"id":1},"duration":12,
        "files":[
            {"fileUrl":"https://cdn.example/video.mp4?signature=keep", "size":1234,"resolution":{"id":720}},
            {"fileUrl":"/static/video.webm","size":5678,"resolution":{"id":1080}},
            {"fileUrl":"/static/audio.m4a","resolution":{"id":0}},
            {"fileUrl":"file:///tmp/leak.mp4","resolution":{"id":1080}},
            {"fileUrl":"https://user:password@cdn.example/no.mp4","resolution":{"id":1080}}
        ]})
    }

    #[test]
    fn exact_post_and_video_identity_are_required() {
        let page = Url::parse("https://bsky.app/profile/bsky.app/post/3l3vgf77uco2g").unwrap();
        let data = bluesky_data();
        let resource = parse_bluesky(&data, DID, "3l3vgf77uco2g", &page).unwrap();
        assert_eq!(resource.candidates.len(), 1);
        assert!(resource.candidates[0].url.ends_with("?token=keep"));
        assert!(parse_bluesky(&data, DID, "wrongpost", &page).is_err());
        assert!(parse_bluesky(&data, "did:plc:wrong", "3l3vgf77uco2g", &page).is_err());
        let mut other_author = data.clone();
        let other_did = "did:plc:anotherpublicauthor";
        other_author["thread"]["post"]["author"]["did"] = json!(other_did);
        other_author["thread"]["post"]["uri"] =
            json!(format!("at://{other_did}/app.bsky.feed.post/3l3vgf77uco2g"));
        let other = parse_bluesky(&other_author, other_did, "3l3vgf77uco2g", &page).unwrap();
        assert_ne!(resource.id, other.id);
        assert_ne!(resource.candidates[0].id, other.candidates[0].id);
        let peer = Url::parse(&format!("https://framatube.org/w/{SHORT}")).unwrap();
        let resource = parse_peertube(&peertube_data(), SHORT, &peer).unwrap();
        assert_eq!(resource.candidates.len(), 3);
        assert_eq!(resource.candidates[0].size, Some(1234));
        assert!(resource.candidates[0].url.ends_with("?signature=keep"));
        assert_eq!(resource.candidates[1].extension, "webm");
        assert!(resource.candidates[2].audio_only);
        assert_eq!(resource.candidates[2].extension, "m4a");
        assert!(parse_peertube(&peertube_data(), "aDifferentInstanceVideo", &peer).is_err());
    }

    #[test]
    fn quotes_and_replies_are_not_current_post_media() {
        let page = Url::parse("https://bsky.app/profile/bsky.app/post/3l3vgf77uco2g").unwrap();
        let mut data = bluesky_data();
        let video = data["thread"]["post"]["embed"].clone();
        data["thread"]["post"]["embed"] =
            json!({"$type":"app.bsky.embed.record#view","record":{"embeds":[video.clone()]}});
        data["thread"]["replies"] = json!([{"post":{"embed":video.clone()}}]);
        assert!(parse_bluesky(&data, DID, "3l3vgf77uco2g", &page).is_err());
        data["thread"]["post"]["embed"] =
            json!({"$type":"app.bsky.embed.recordWithMedia#view","media":video});
        assert!(parse_bluesky(&data, DID, "3l3vgf77uco2g", &page).is_ok());
    }

    #[test]
    fn bluesky_public_video_urls_cannot_be_replaced_with_other_sources() {
        let page = Url::parse("https://bsky.app/profile/bsky.app/post/3l3vgf77uco2g").unwrap();
        for address in [
            "https://video.bsky.app.evil.example/wrong/playlist.m3u8",
            "https://user:password@video.bsky.app/wrong/playlist.m3u8",
            "http://video.bsky.app/wrong/playlist.m3u8",
            "file:///tmp/video.m3u8",
            "https://video.bsky.app/watch/video.mp4",
        ] {
            let mut data = bluesky_data();
            data["thread"]["post"]["embed"]["playlist"] = json!(address);
            assert!(parse_bluesky(&data, DID, "3l3vgf77uco2g", &page).is_err());
        }
        let did_page = Url::parse(&format!(
            "https://bsky.app/profile/{DID}/post/3l3vgf77uco2g"
        ))
        .unwrap();
        assert_eq!(bluesky_post(&did_page).unwrap().0, DID);
    }

    #[test]
    fn livestreams_private_videos_and_unmatched_paths_are_rejected() {
        let page = Url::parse(&format!("https://framatube.org/videos/watch/{UUID}")).unwrap();
        assert_eq!(peertube_id(&page).as_deref(), Some(UUID));
        assert!(
            peertube_id(&Url::parse(&format!("https://framatube.org/w/p/{SHORT}")).unwrap())
                .is_none()
        );
        assert!(
            peertube_id(&Url::parse("https://example.org/w/not-a-valid-video-id").unwrap())
                .is_none()
        );
        let mut data = peertube_data();
        data["isLive"] = json!(true);
        assert!(parse_peertube(&data, UUID, &page).is_err());
        data["isLive"] = json!(false);
        data["privacy"]["id"] = json!(3);
        assert!(parse_peertube(&data, UUID, &page).is_err());
    }

    #[tokio::test]
    async fn loopback_api_checks_identity_bounds_and_redirect_origin() {
        use axum::{extract::Path, http::StatusCode, routing::get, Json, Router};
        let app = Router::new().route("/api/v1/videos/{id}", get(|Path(id): Path<String>| async move {
            if id == SHORT {
                (StatusCode::OK, Json(peertube_data()))
            } else {
                (StatusCode::OK, Json(json!({"uuid":UUID,"isLive":false,"name":"wrong video","files":[{"fileUrl":"https://cdn.example/video.mp4"}]})))
            }
        })).route("/redirect", get(|| async { axum::response::Redirect::temporary("https://example.org/wrong") }))
            .route("/oversized", get(|| async { "a".repeat(MAX_METADATA_BYTES + 1) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let resolver = FederatedResolver::new().unwrap();
        assert_eq!(
            resolver
                .resolve(&base.join(&format!("w/{SHORT}")).unwrap(), "PeerTube")
                .await
                .unwrap()
                .candidates
                .len(),
            3
        );
        assert!(resolver
            .resolve(&base.join("w/aaaaaaaaaaaaaaaaaaaaaa").unwrap(), "PeerTube")
            .await
            .is_err());
        assert!(resolver
            .fetch(
                &base.join("redirect").unwrap(),
                "PeerTube",
                "application/json"
            )
            .await
            .is_err());
        assert!(resolver
            .fetch(
                &base.join("oversized").unwrap(),
                "PeerTube",
                "application/json"
            )
            .await
            .is_err());
        server.abort();
    }

    #[tokio::test]
    #[ignore = "requires public Bluesky / PeerTube network access"]
    async fn public_federated_samples_have_owned_media_candidates() {
        let resolver = FederatedResolver::new().unwrap();
        for (platform, sample) in [
            (
                "Bluesky",
                "https://bsky.app/profile/bsky.app/post/3l3vgf77uco2g",
            ),
            (
                "PeerTube",
                "https://framatube.org/videos/watch/9c9de5e8-0a1e-484a-b099-e80766180a6d",
            ),
        ] {
            let resource = resolver
                .resolve(&Url::parse(sample).unwrap(), platform)
                .await
                .unwrap();
            assert!(!resource.candidates.is_empty());
            assert!(resource
                .candidates
                .iter()
                .all(|candidate| !candidate.url.contains('@')));
            println!(
                "{platform}: {} ({} formats)",
                resource.title,
                resource.candidates.len()
            );
        }
    }
}
