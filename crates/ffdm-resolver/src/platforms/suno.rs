//! Suno single songs: static metadata, normal playback permission and audio export.
//! Encoded playback streams require an explicit post-download transform.
use crate::generic::{GenericCandidate, GenericResource};
use anyhow::{anyhow, bail, ensure, Context, Result};
use base64::Engine;
use regex::Regex;
use reqwest::{header, redirect::Policy, Client, Response, Url};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::HashSet, time::Duration};

const MAX_BYTES: usize = 2 * 1024 * 1024;
const API_ORIGIN: &str = "https://suno.com/a";

pub(crate) fn is_song_page(url: &Url) -> bool {
    song_id(url).is_ok()
}

fn song_id(url: &Url) -> Result<String> {
    crate::validation::parse_url(url.as_str())?;
    ensure!(
        url.scheme() == "https"
            && url.port().is_none()
            && matches!(url.host_str(), Some("suno.com" | "www.suno.com")),
        "请输入 Suno 的 HTTPS 单曲分享链接"
    );
    let path = url.path().strip_suffix('/').unwrap_or(url.path());
    let id = path.strip_prefix("/song/").unwrap_or("");
    ensure!(
        valid_id(id),
        "Suno 仅支持 /song/单曲编号，不支持歌单或用户主页"
    );
    Ok(id.to_ascii_lowercase())
}

fn valid_id(id: &str) -> bool {
    id.len() == 36
        && id.split('-').map(str::len).eq([8, 4, 4, 4, 12])
        && id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
}

fn client() -> Result<Client> {
    // Re-read the system proxy for this extraction, including its export calls.
    Ok(client_builder().https_only(true).build()?)
}

fn client_builder() -> reqwest::ClientBuilder {
    Client::builder()
        .connect_timeout(Duration::from_secs(6))
        .timeout(Duration::from_secs(12))
        .user_agent(crate::resource::USER_AGENT)
        .redirect(Policy::none())
}

pub(crate) async fn resolve(input: &Url) -> Result<GenericResource> {
    let id = song_id(input)?;
    let client = client()?;
    let page = Url::parse(&format!("https://suno.com/song/{id}"))?;
    let response = client
        .get(page.clone())
        .header(header::ACCEPT, "text/html,application/xhtml+xml")
        .header(header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(|error| anyhow!("Suno 分享页连接失败：{}", error.without_url()))?;
    check_status(response.status().as_u16())?;
    let body = read_body(response).await?;
    let track = parse_page(&body, &id)?;
    let mut resource = track_resource(&track, &id, &page)?;
    if resource.candidates.is_empty() {
        if let Some(candidate) = encoded_candidate(&track, &id, &page)? {
            crate::suno_audio::check_access(&id).await?;
            resource.candidates = vec![candidate];
        } else {
            let endpoint = Url::parse(&format!("{API_ORIGIN}/api/download/clip/{id}?format=mp3"))?;
            resource.candidates = vec![export_audio(&client, &endpoint, &page, &id).await?];
        }
    }
    Ok(resource)
}

fn check_status(status: u16) -> Result<()> {
    match status {
        200..=299 => Ok(()),
        401 => bail!("这首 Suno 歌曲暂未提供可匿名下载的音频，官方导出需要在 Suno 网站登录。当前无法仅凭分享链接下载这首歌。"),
        403 => bail!("Suno 暂未允许下载器读取这首歌的音频（HTTP 403），请稍后重试；当前不会把加密播放数据保存成音频文件。"),
        404 | 410 => bail!("Suno 歌曲不存在、已删除或尚无可用导出文件"),
        429 => bail!("Suno 请求过于频繁，请稍后重试"),
        300..=399 => bail!("Suno 接口发生跳转，请重新解析"),
        code => bail!("Suno 返回 HTTP {code}，请稍后重试"),
    }
}

fn visitor_headers() -> Result<header::HeaderMap> {
    // Same public, account-free request fields as the webpage's API client.
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| anyhow!("无法生成 Suno 临时访客标识"))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let device = format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    );
    let timestamp = serde_json::json!({"timestamp": crate::utils::now_ms()});
    let browser = serde_json::json!({"token": base64::engine::general_purpose::STANDARD.encode(timestamp.to_string())});
    let mut headers = header::HeaderMap::new();
    headers.insert("Device-Id", device.parse()?);
    headers.insert("Browser-Token", browser.to_string().parse()?);
    Ok(headers)
}

async fn read_body(mut response: Response) -> Result<String> {
    ensure!(
        response.content_length().unwrap_or(0) <= MAX_BYTES as u64,
        "Suno 播放信息超过解析上限"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| anyhow!("读取 Suno 播放信息失败：{}", error.without_url()))?
    {
        ensure!(
            bytes.len().saturating_add(chunk.len()) <= MAX_BYTES,
            "Suno 播放信息超过解析上限"
        );
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).context("Suno 播放信息不是有效的 UTF-8 文本")
}

async fn export_audio(
    client: &Client,
    endpoint: &Url,
    page: &Url,
    id: &str,
) -> Result<GenericCandidate> {
    let visitor = visitor_headers()?;
    for attempt in 0..6 {
        let request = client
            .get(endpoint.clone())
            .header(header::ACCEPT, "application/json")
            .header(header::ACCEPT_ENCODING, "identity")
            .header(header::ORIGIN, "https://suno.com")
            .header(header::REFERER, "https://suno.com/")
            .headers(visitor.clone());
        let response = request
            .send()
            .await
            .map_err(|error| anyhow!("Suno 下载导出连接失败：{}", error.without_url()))?;
        check_status(response.status().as_u16())?;
        let body = read_body(response).await?;
        let data: Value =
            serde_json::from_str(&body).context("Suno 官方导出信息格式已变化，请稍后重试")?;
        if let Some(candidate) = export_result(&data, page, id)? {
            return Ok(candidate);
        }
        if attempt < 5 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
    bail!("Suno 正在准备 MP3 导出，请稍后重新解析；尚未创建下载任务")
}

fn export_result(data: &Value, page: &Url, id: &str) -> Result<Option<GenericCandidate>> {
    if let Some(returned_id) = data["clip_id"].as_str() {
        ensure!(
            returned_id.eq_ignore_ascii_case(id),
            "Suno 导出返回了其他歌曲"
        );
    }
    ensure!(
        data["reason"] != "rate_limited",
        "Suno 导出请求过于频繁，请稍后重试"
    );
    ensure!(
        data["ok"] == true,
        "Suno 未返回可直接下载的音频，请确认官网上这首歌的可用状态"
    );
    match data["status"].as_str() {
        Some("ready") => {
            let raw = data["download_url"]
                .as_str()
                .context("Suno 未返回完整音频的导出地址")?;
            Ok(Some(candidate(
                id,
                media_url(raw)?,
                "mp3",
                "MP3 · 官方导出",
                page,
            )))
        }
        Some("processing") => Ok(None),
        Some("error") => bail!("Suno 音频导出失败，请在官网确认该作品能正常下载后重试"),
        _ => bail!("Suno 音频导出状态暂不支持，请稍后重新解析"),
    }
}

fn parse_page(html: &str, id: &str) -> Result<Value> {
    ensure!(html.len() <= MAX_BYTES, "Suno 页面超过解析上限");
    let mut records = String::new();
    let flight =
        Regex::new(r#"self\.__next_f\.push\(\s*(\[\s*1\s*,\s*"(?:[^"\\]|\\.)*"\s*\])\s*\)"#)?;
    for capture in flight.captures_iter(html) {
        let data: Value =
            serde_json::from_str(&capture[1]).context("Suno 页面播放数据格式已变化")?;
        if let Some(text) = data[1].as_str() {
            ensure!(
                records.len().saturating_add(text.len()) <= MAX_BYTES,
                "Suno 播放数据超过解析上限"
            );
            records.push_str(text);
        }
    }
    let next_data = Regex::new(
        r#"(?is)<script\b[^>]*\bid\s*=\s*(?:"__NEXT_DATA__"|'__NEXT_DATA__')[^>]*>(.*?)</script\s*>"#,
    )?;
    let mut matches = Vec::new();
    for capture in next_data.captures_iter(html) {
        let data: Value =
            serde_json::from_str(&capture[1]).context("Suno 页面播放数据格式已变化")?;
        find_tracks(&data, id, &mut matches);
    }
    // Flight text records can end immediately before another JSON record.
    // Parse complete objects rather than relying on line or record numbers.
    let mut offset = 0;
    let mut inspected = 0;
    while let Some(start) = records[offset..].find('{') {
        offset += start;
        inspected += 1;
        ensure!(inspected <= 4096, "Suno 页面包含过多播放对象");
        let mut stream =
            serde_json::Deserializer::from_str(&records[offset..]).into_iter::<Value>();
        if let Some(Ok(data)) = stream.next() {
            find_tracks(&data, id, &mut matches);
            offset += stream.byte_offset();
        } else {
            offset += 1;
        }
    }
    let track = matches
        .first()
        .context("Suno 页面没有返回这首歌的播放信息，作品可能受限或已删除")?;
    ensure!(
        matches.iter().all(|other| other == track),
        "Suno 页面包含互相冲突的目标歌曲信息，请重新解析"
    );
    Ok(track.clone())
}

fn find_tracks(value: &Value, id: &str, tracks: &mut Vec<Value>) {
    match value {
        Value::Object(object) => {
            if object
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|value| value.eq_ignore_ascii_case(id))
                && (object.contains_key("audio_url") || object.contains_key("media_urls"))
            {
                tracks.push(value.clone());
            } else {
                for value in object.values() {
                    find_tracks(value, id, tracks);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                find_tracks(value, id, tracks);
            }
        }
        _ => {}
    }
}

fn track_resource(track: &Value, id: &str, page: &Url) -> Result<GenericResource> {
    ensure!(
        track["id"]
            .as_str()
            .is_some_and(|value| value.eq_ignore_ascii_case(id)),
        "Suno 返回的歌曲与分享链接不一致"
    );
    ensure!(
        track["status"] == "complete" && track["is_trashed"] != true && track["is_hidden"] != true,
        "Suno 歌曲尚未完成、已删除或当前不可访问"
    );
    ensure!(
        track["is_preview"] != true && track["metadata"]["is_preview"] != true,
        "Suno 当前只提供试听，不能作为完整音频下载"
    );
    let title = track["title"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(id)
        .to_owned();
    let duration_seconds = track["metadata"]["duration"]
        .as_f64()
        .filter(|duration| duration.is_finite() && *duration > 0.0 && *duration <= 86400.0)
        .map(|duration| duration.ceil() as u64);
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let media = track["media_urls"].as_array();
    if let Some(media) = media.filter(|items| !items.is_empty()) {
        ensure!(media.len() <= 32, "Suno 页面音频格式超过解析上限");
        for item in media {
            if !item["encoding"].is_null() && item["encoding"] != "" {
                continue;
            }
            if item["delivery"] != "progressive" {
                continue;
            }
            let Some(raw) = item["url"].as_str() else {
                continue;
            };
            let Some((extension, label)) =
                audio_format(item["content_type"].as_str().unwrap_or(""))
            else {
                continue;
            };
            let Ok(url) = media_url(raw) else {
                continue;
            };
            if !url.path().to_ascii_lowercase().contains(id) || !seen.insert(url.to_string()) {
                continue;
            }
            candidates.push(candidate(id, url, extension, label, page));
        }
    } else if let Some(raw) = track["audio_url"].as_str() {
        if let Ok(url) = media_url(raw) {
            let extension = url
                .path()
                .rsplit('.')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if let Some((extension, label)) = audio_format(&extension) {
                if url.path().to_ascii_lowercase().contains(id) {
                    candidates.push(candidate(id, url, extension, label, page));
                }
            }
        }
    }
    Ok(GenericResource {
        id: format!("suno-{id}"),
        title,
        duration_seconds,
        candidates,
        embed: None,
    })
}

fn encoded_candidate(track: &Value, id: &str, page: &Url) -> Result<Option<GenericCandidate>> {
    let Some(media) = track["media_urls"].as_array() else {
        return Ok(None);
    };
    ensure!(media.len() <= 32, "Suno 页面音频格式超过解析上限");
    for item in media {
        if item["encoding"] != "1.0.0" || item["delivery"] != "progressive" {
            continue;
        }
        let Some((extension, _)) = audio_format(item["content_type"].as_str().unwrap_or("")) else {
            continue;
        };
        if extension != "m4a" {
            continue;
        }
        let Some(raw) = item["url"].as_str() else {
            continue;
        };
        let url = media_url(raw)?;
        if !url.path().to_ascii_lowercase().contains(id) {
            continue;
        }
        let mut candidate = candidate(id, url, "m4a", "M4A · 网页音频", page);
        candidate.id.push_str("-playback-v1");
        candidate.transform = Some(crate::media::StreamTransform::SunoAudio {
            content_id: id.into(),
        });
        return Ok(Some(candidate));
    }
    Ok(None)
}

fn audio_format(kind: &str) -> Option<(&'static str, &'static str)> {
    match kind {
        "mp3" | "audio/mpeg" => Some(("mp3", "MP3 · 原始音频")),
        "m4a" | "audio/mp4" | "m4a-aac" => Some(("m4a", "M4A · 原始音频")),
        "m4a-opus" => Some(("m4a", "M4A · Opus")),
        _ => None,
    }
}

fn media_url(raw: &str) -> Result<Url> {
    let url = crate::validation::parse_url(raw)?;
    ensure!(
        url.scheme() == "https" && url.port().is_none(),
        "Suno 音频下载地址必须使用 HTTPS"
    );
    ensure!(
        url.host_str().is_some_and(|host| host.contains('.')
            && host != "localhost"
            && !host.ends_with(".localhost")
            && host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_err()),
        "Suno 未提供有效的公开音频下载地址"
    );
    ensure!(
        !url.path().to_ascii_lowercase().contains("/api/forbidden"),
        "Suno 未提供这首歌的直接音频下载地址"
    );
    Ok(url)
}

fn candidate(id: &str, url: Url, extension: &str, label: &str, page: &Url) -> GenericCandidate {
    let digest = format!("{:x}", Sha256::digest(url.as_str().as_bytes()));
    GenericCandidate {
        id: format!("suno-{id}-{extension}-{}", &digest[..12]),
        label: label.into(),
        height: 0,
        url: url.to_string(),
        extension: extension.into(),
        audio_only: true,
        headers: vec![("Referer".into(), page.to_string())],
        size: None,
        transform: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const ID: &str = "11111111-2222-4333-8444-555555555555";
    fn page() -> Url {
        Url::parse(&format!("https://suno.com/song/{ID}")).unwrap()
    }
    fn track() -> Value {
        json!({"id":ID,"title":"目标歌曲","status":"complete","is_public":false,"is_hidden":false,"metadata":{"duration":209.4},"audio_url":format!("https://cdn1.suno.ai/{ID}.mp3")})
    }
    fn flight(data: &Value) -> String {
        let payload = format!(
            "7:T8,lyrics{{}}8:[\"$\",\"$L9\",null,{}]\n",
            json!({"prefetched":{"queries":[{"data":data}]}})
        );
        let split = payload.find("prefetched").unwrap();
        [payload[..split].to_owned(), payload[split..].to_owned()]
            .iter()
            .map(|part| format!("<script>self.__next_f.push({});</script>", json!([1, part])))
            .collect()
    }
    #[test]
    fn rsc_metadata_matches_the_exact_song_across_chunks_without_running_scripts() {
        let html = flight(
            &json!([{"id":"other","audio_url":"https://example.org/recommendation.mp3"},track()]),
        );
        let found = parse_page(&html, ID).unwrap();
        let resource = track_resource(&found, ID, &page()).unwrap();
        assert_eq!(resource.title, "目标歌曲");
        assert_eq!(resource.duration_seconds, Some(210));
        assert_eq!(resource.candidates.len(), 1);
        assert!(resource.candidates[0].audio_only);
        assert!(parse_page(&html, "11111111-1111-1111-1111-111111111111").is_err());
        assert!(parse_page("<script>fetch('https://example.org')</script>", ID).is_err());
        let conflicting =
            flight(&json!([track(),{"id":ID,"audio_url":"https://example.org/conflict.mp3"}]));
        assert!(parse_page(&conflicting, ID).is_err());
    }
    #[test]
    fn encrypted_playback_and_forbidden_urls_are_not_download_candidates() {
        let mut data = track();
        data["media_urls"] = json!([{"url":format!("https://example.org/{ID}.m4a"),"content_type":"m4a-opus","delivery":"progressive","encoding":"1.0.0"}]);
        assert!(track_resource(&data, ID, &page())
            .unwrap()
            .candidates
            .is_empty());
        let encoded = encoded_candidate(&data, ID, &page()).unwrap().unwrap();
        assert!(
            matches!(encoded.transform, Some(crate::media::StreamTransform::SunoAudio { content_id }) if content_id == ID)
        );
        data["media_urls"][0]["encoding"] = json!("unknown-future-version");
        assert!(encoded_candidate(&data, ID, &page()).unwrap().is_none());
        data["media_urls"][0]["encoding"] = json!("");
        let resource = track_resource(&data, ID, &page()).unwrap();
        assert_eq!(resource.candidates[0].extension, "m4a");
        data["media_urls"] = Value::Null;
        data["audio_url"] = json!("https://studio-api.prod.suno.com/api/forbidden");
        assert!(track_resource(&data, ID, &page())
            .unwrap()
            .candidates
            .is_empty());
        data["is_trashed"] = json!(true);
        assert!(track_resource(&data, ID, &page()).is_err());
    }
    #[test]
    fn export_uses_only_a_ready_download_and_preserves_its_url() {
        let data = json!({"ok":true,"status":"ready","download_url":"https://cdn1.suno.ai/export.mp3?signature=keep%2Bexact"});
        let result = export_result(&data, &page(), ID).unwrap().unwrap();
        assert!(result.url.ends_with("signature=keep%2Bexact"));
        assert_eq!(result.headers, vec![("Referer".into(), page().to_string())]);
        assert!(
            export_result(&json!({"ok":true,"status":"processing"}), &page(), ID)
                .unwrap()
                .is_none()
        );
        for bad in [
            json!({"ok":false,"status":"ready","download_url":"https://example.org/a.mp3"}),
            json!({"ok":true,"status":"ready","download_url":"http://example.org/a.mp3"}),
            json!({"ok":true,"status":"ready","download_url":"https://127.0.0.1/a.mp3"}),
            json!({"ok":true,"status":"ready","download_url":"https://studio-api.prod.suno.com/api/forbidden"}),
            json!({"ok":true,"status":"error"}),
        ] {
            assert!(export_result(&bad, &page(), ID).is_err());
        }
        assert!(check_status(401)
            .unwrap_err()
            .to_string()
            .contains("无法仅凭分享链接"));
        assert!(check_status(302).is_err());
        let headers = visitor_headers().unwrap();
        assert!(valid_id(headers["Device-Id"].to_str().unwrap()));
        let browser: Value =
            serde_json::from_str(headers["Browser-Token"].to_str().unwrap()).unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(browser["token"].as_str().unwrap())
            .unwrap();
        let timestamp: Value = serde_json::from_slice(&decoded).unwrap();
        assert!(timestamp["timestamp"].as_u64().unwrap() > 0);
        assert!(headers.get(header::AUTHORIZATION).is_none());
    }

    #[tokio::test]
    async fn anonymous_export_and_redirects_never_send_account_credentials() {
        use axum::{http::HeaderMap, response::Redirect, routing::get, Json, Router};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router=Router::new()
            .route("/export",get(|headers:HeaderMap| async move {
                assert!(headers.get(header::AUTHORIZATION).is_none());
                assert!(headers.get("Device-Id").is_some());
                assert!(headers.get("Browser-Token").is_some());
                assert!(headers.get(header::COOKIE).is_none());
                Json(json!({"ok":true,"status":"ready","download_url":"https://cdn1.suno.ai/export.mp3"}))
            }))
            .route("/redirect",get(move || async move {Redirect::temporary(&format!("http://{address}/leak"))}))
            .route("/leak",get(move || {let counted=counted.clone();async move {counted.fetch_add(1,Ordering::SeqCst);"unexpected"}}));
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let client = client_builder().no_proxy().build().unwrap();
        let result = export_audio(
            &client,
            &Url::parse(&format!("http://{address}/export")).unwrap(),
            &page(),
            ID,
        )
        .await
        .unwrap();
        assert!(result
            .headers
            .iter()
            .all(|(name, _)| !name.eq_ignore_ascii_case("authorization")));
        let failure = export_audio(
            &client,
            &Url::parse(&format!("http://{address}/redirect")).unwrap(),
            &page(),
            ID,
        )
        .await
        .err()
        .expect("redirect must be rejected");
        assert!(failure.to_string().contains("跳转"));
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        server.abort();
    }
}
