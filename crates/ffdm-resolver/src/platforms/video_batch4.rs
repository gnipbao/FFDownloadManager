//! Public VOD player adapters for Rumble, Odysee/LBRY and OK.ru.
//! The published player/API layouts are inspected in Rust. No JavaScript is
//! executed and no session cookies, subscription credentials or live recorder
//! are part of this module.
use crate::generic::{GenericCandidate, GenericResource};
use anyhow::{anyhow, bail, ensure, Context, Result};
use percent_encoding::percent_decode_str;
use regex::Regex;
use reqwest::{header, redirect::Policy, Client, Url};
use serde_json::{json, Value};
use std::{collections::HashSet, time::Duration};

const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_FORMATS: usize = 32;

/// Keep platform routing and the actual parser on the same single-work URL
/// contract. Ordinary files on a platform host must still use the file engine.
pub(crate) fn supports(url: &Url, platform: &str) -> bool {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return false;
    }
    match platform {
        "Rumble" => rumble_embed_id(url).is_some() || rumble_watch_path(url).is_some(),
        "Odysee" => odysee_identity(url).is_ok(),
        "OK.ru" => ok_id(url).is_some(),
        _ => false,
    }
}

pub(crate) struct VideoBatch4Resolver {
    client: Client,
}

impl VideoBatch4Resolver {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .connect_timeout(Duration::from_secs(6))
                .timeout(Duration::from_secs(15))
                .user_agent(crate::resource::USER_AGENT)
                .redirect(Policy::custom(|attempt| {
                    let downgrade = attempt.previous().last().is_some_and(|previous| {
                        previous.scheme() == "https" && attempt.url().scheme() == "http"
                    });
                    if attempt.previous().len() >= 5 || downgrade || !metadata_url(attempt.url()) {
                        attempt.error("public VOD metadata redirect rejected")
                    } else {
                        attempt.follow()
                    }
                }))
                .build()?,
        })
    }

    pub(crate) async fn resolve(&self, input: &Url, platform: &str) -> Result<GenericResource> {
        match platform {
            "Rumble" => self.rumble(input).await,
            "Odysee" => self.odysee(input).await,
            "OK.ru" => self.ok(input).await,
            _ => bail!("未接入这个公开点播平台"),
        }
    }

    async fn fetch(&self, url: Url, platform: &str, body: Option<&Value>) -> Result<(Url, String)> {
        ensure!(metadata_url(&url), "点播信息不在平台官方域名内");
        let request = if let Some(body) = body {
            self.client
                .post(url)
                .header(header::CONTENT_TYPE, "application/json-rpc")
                .body(serde_json::to_vec(body)?)
        } else {
            self.client.get(url)
        };
        let mut response = request
            .header(header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(|_| anyhow!("{platform} 公开播放信息请求失败"))?;
        match response.status().as_u16() {
            401 | 403 => bail!("{platform} 要求登录、访问验证或当前公开播放不可用"),
            404 | 410 => bail!("{platform} 作品不存在或接口已经变化"),
            429 => bail!("{platform} 请求过于频繁，请稍后重试"),
            status => ensure!(
                response.status().is_success(),
                "{platform} 返回 HTTP {status}"
            ),
        }
        ensure!(
            response.content_length().unwrap_or(0) <= MAX_BYTES as u64,
            "公开点播信息超过 4 MiB 上限"
        );
        let final_url = response.url().clone();
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow!("读取 {platform} 播放信息失败"))?
        {
            ensure!(
                bytes.len().saturating_add(chunk.len()) <= MAX_BYTES,
                "公开点播信息超过 4 MiB 上限"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok((
            final_url,
            String::from_utf8(bytes).context("公开点播信息不是 UTF-8 文本")?,
        ))
    }

    async fn rumble(&self, input: &Url) -> Result<GenericResource> {
        let id = if let Some(id) = rumble_embed_id(input) {
            id
        } else {
            let watch =
                rumble_watch_path(input).context("请输入 Rumble 单个作品或 embed 播放器链接")?;
            let page = Url::parse(&format!("https://rumble.com/{watch}"))?;
            let (final_url, html) = self.fetch(page, "Rumble", None).await?;
            ensure!(
                rumble_watch_path(&final_url).as_deref() == Some(&watch),
                "Rumble 页面跳转到了其他作品"
            );
            rumble_page_embed(&html)?
        };
        let mut api = Url::parse("https://rumble.com/embedJS/u3/")?;
        api.query_pairs_mut()
            .append_pair("request", "video")
            .append_pair("ver", "2")
            .append_pair("v", &id);
        let (_, body) = self.fetch(api, "Rumble", None).await?;
        let data: Value = serde_json::from_str(&body).context("Rumble 未返回有效播放信息")?;
        parse_rumble(&data, &id)
    }

    async fn odysee(&self, input: &Url) -> Result<GenericResource> {
        let requested = odysee_identity(input)?;
        let page = odysee_page(&requested)?;
        let (final_url, html) = self.fetch(page, "Odysee", None).await?;
        ensure!(
            odysee_identity(&final_url)? == requested,
            "Odysee 页面跳转到了其他作品"
        );
        if let Some(resource) = odysee_public_page(&html, &requested, &final_url)? {
            return Ok(resource);
        }
        // The older public SDK proxy is a fallback for pages without the
        // current SSR VideoObject. It never receives an account/auth token.
        let uri = requested.uri();
        let api = Url::parse("https://api.lbry.tv/api/v1/proxy")?;
        let (_, resolved) = self
            .fetch(
                api.clone(),
                "Odysee",
                Some(&json!({"method":"resolve","params":{"urls":[uri]}})),
            )
            .await?;
        let resolved: Value =
            serde_json::from_str(&resolved).context("Odysee 未返回有效作品信息")?;
        ensure!(resolved.get("error").is_none(), "Odysee 公开作品接口不可用");
        let claim = &resolved["result"][&uri];
        let info = odysee_claim(claim, &requested)?;
        let (_, stream) = self
            .fetch(
                api,
                "Odysee",
                Some(&json!({"method":"get","params":{"uri":uri}})),
            )
            .await?;
        let stream: Value = serde_json::from_str(&stream).context("Odysee 未返回公开播放地址")?;
        ensure!(stream.get("error").is_none(), "Odysee 公开播放接口不可用");
        let raw = text(&stream["result"]["streaming_url"]).context("Odysee 没有公开播放地址")?;
        let url = crate::validation::parse_url(raw)?;
        ensure!(
            media_url(&url, "Odysee") && !preview_url(&url),
            "Odysee 未提供完整公开媒体"
        );
        let extension = if url.path().to_ascii_lowercase().ends_with(".m3u8") {
            "m3u8"
        } else {
            info.extension
        };
        Ok(GenericResource {
            id: info.id,
            title: info.title,
            duration_seconds: Some(info.duration),
            candidates: vec![candidate(
                "odysee-public",
                "公开媒体",
                url,
                extension,
                info.height,
                info.audio_only,
                &final_url,
            )],
            embed: None,
        })
    }

    async fn ok(&self, input: &Url) -> Result<GenericResource> {
        let id = ok_id(input).context("请输入 OK.ru 公开单个 video 或 videoembed 链接")?;
        let page = Url::parse(&format!("https://ok.ru/videoembed/{id}"))?;
        let (final_url, html) = self.fetch(page, "OK.ru", None).await?;
        ensure!(
            ok_id(&final_url).as_deref() == Some(&id),
            "OK.ru 页面跳转到了其他作品"
        );
        let player = ok_player(&html, &id)?;
        if flag(&player["isExternalPlayer"]) {
            let raw = text(&player["url"]).context("OK.ru 外部播放器没有作品地址")?;
            let embed = crate::validation::parse_url(raw)?;
            ensure!(
                crate::routing::platform(embed.as_str()).is_some(),
                "OK.ru 这个外部播放器暂不支持"
            );
            return Ok(GenericResource {
                id,
                title: "OK.ru 外部作品".to_owned(),
                duration_seconds: None,
                candidates: Vec::new(),
                embed: Some(embed),
            });
        }
        let metadata = player["flashvars"]["metadata"].clone();
        let metadata = if let Some(json) = metadata.as_str() {
            serde_json::from_str(json).context("OK.ru 播放信息格式已变化")?
        } else if metadata.is_object() {
            metadata
        } else {
            let raw =
                text(&player["flashvars"]["metadataUrl"]).context("OK.ru 未公开目标播放信息")?;
            let decoded = percent_decode_str(raw).decode_utf8()?;
            let url = crate::validation::parse_url(&decoded)?;
            ensure!(
                ok_metadata_target(&url, &id),
                "OK.ru 播放接口与请求作品不一致"
            );
            let (_, body) = self.fetch(url, "OK.ru", None).await?;
            serde_json::from_str(&body).context("OK.ru 播放接口未返回有效信息")?
        };
        parse_ok(&metadata, &id, &final_url)
    }
}

fn metadata_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && matches!(
            url.host_str(),
            Some(
                "rumble.com"
                    | "www.rumble.com"
                    | "odysee.com"
                    | "www.odysee.com"
                    | "lbry.tv"
                    | "www.lbry.tv"
                    | "api.lbry.tv"
                    | "ok.ru"
                    | "www.ok.ru"
                    | "m.ok.ru"
                    | "odnoklassniki.ru"
                    | "www.odnoklassniki.ru"
            )
        )
}

fn rumble_embed_id(url: &Url) -> Option<String> {
    if !matches!(url.host_str(), Some("rumble.com" | "www.rumble.com")) {
        return None;
    }
    let raw = url.path().trim_end_matches('/').strip_prefix("/embed/")?;
    let id = if let Some((uploader, id)) = raw.split_once('.') {
        if uploader.is_empty()
            || uploader.len() > 24
            || !uploader
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        {
            return None;
        }
        id
    } else {
        raw
    };
    (!id.is_empty()
        && id.len() <= 24
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()))
    .then(|| id.to_owned())
}

fn rumble_watch_path(url: &Url) -> Option<String> {
    if !matches!(url.host_str(), Some("rumble.com" | "www.rumble.com")) {
        return None;
    }
    let path = url.path().trim_matches('/');
    let pattern = Regex::new(r"^v[a-z0-9]+(?:-[a-zA-Z0-9_.-]+)?\.html$").ok()?;
    (path.len() <= 500 && pattern.is_match(path)).then(|| path.to_owned())
}

fn rumble_page_embed(html: &str) -> Result<String> {
    ensure!(html.len() <= MAX_BYTES, "Rumble 页面超过解析上限");
    let urls = Regex::new(r#"https?://(?:www\.)?rumble\.com/embed/(?:[0-9a-z]+\.)?([0-9a-z]+)"#)?;
    let play = Regex::new(
        r#"\bRumble\s*\(\s*["']play["']\s*,\s*\{[^}]*["']?video["']?\s*:\s*["']([0-9a-z]+)["']"#,
    )?;
    let mut ids: HashSet<_> = urls
        .captures_iter(&html.replace("\\/", "/"))
        .map(|capture| capture[1].to_owned())
        .collect();
    ids.extend(
        play.captures_iter(html)
            .map(|capture| capture[1].to_owned()),
    );
    ensure!(
        ids.len() == 1,
        "Rumble 页面没有唯一的目标播放器，请提供该作品的 embed 链接"
    );
    Ok(ids.into_iter().next().unwrap())
}

fn parse_rumble(data: &Value, id: &str) -> Result<GenericResource> {
    let expected_vid = u64::from_str_radix(id.strip_prefix('v').unwrap_or(id), 36)
        .context("Rumble 播放器标识无效")?;
    ensure!(
        integer(&data["vid"]) == Some(expected_vid),
        "Rumble 返回了其他作品的播放信息"
    );
    ensure!(
        integer(&data["live"]) == Some(0),
        "Rumble 直播、预告或仍在生成的回放暂不支持"
    );
    ensure!(!restricted(data), "Rumble 作品私密、仅提供预览或受授权限制");
    let title = text(&data["title"]).context("Rumble 作品缺少标题")?;
    let duration = duration_number(&data["duration"])
        .filter(|value| *value > 0)
        .context("Rumble 未公开完整作品时长")?;
    let formats = data["ua"].as_object().context("Rumble 没有公开播放格式")?;
    let page = Url::parse(&format!("https://rumble.com/embed/{id}"))?;
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for (kind, values) in formats {
        let extension = match kind.as_str() {
            "mp4" => "mp4",
            "webm" => "webm",
            "hls" => "m3u8",
            _ => continue,
        };
        let values: Vec<(&str, &Value)> = if let Some(values) = values.as_object() {
            values
                .iter()
                .map(|(key, value)| (key.as_str(), value))
                .collect()
        } else if let Some(values) = values.as_array() {
            values.iter().map(|value| ("", value)).collect()
        } else {
            continue;
        };
        ensure!(
            values.len() <= MAX_FORMATS,
            "Rumble 单个编码格式超过解析上限"
        );
        for (key, value) in values {
            if restricted(value) || flag(&value["meta"]["live"]) {
                continue;
            }
            let Some(raw) = text(&value["url"]) else {
                continue;
            };
            let Ok(url) = crate::validation::parse_url(raw) else {
                continue;
            };
            if !media_url(&url, "Rumble") || preview_url(&url) || !seen.insert(url.to_string()) {
                continue;
            }
            let height = integer(&value["meta"]["h"])
                .or_else(|| key.parse().ok())
                .unwrap_or(0)
                .min(16384) as u32;
            let index = candidates.len();
            candidates.push(candidate(
                &format!("rumble-{kind}-{index}"),
                &format!("{height}p · {}", extension.to_ascii_uppercase()),
                url,
                extension,
                height,
                false,
                &page,
            ));
            ensure!(candidates.len() <= MAX_FORMATS, "Rumble 格式超过 32 个上限");
        }
    }
    ensure!(!candidates.is_empty(), "Rumble 没有完整的公开点播格式");
    Ok(GenericResource {
        id: id.to_owned(),
        title: decode_entities(title),
        duration_seconds: Some(duration),
        candidates,
        embed: None,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct OdyseeIdentity {
    channel: Option<(String, Option<String>)>,
    name: String,
    claim: Option<String>,
}
impl OdyseeIdentity {
    fn uri(&self) -> String {
        let claim = self
            .claim
            .as_ref()
            .map_or(String::new(), |id| format!("#{id}"));
        let channel = self.channel.as_ref().map_or(String::new(), |(name, id)| {
            format!(
                "{name}{}/",
                id.as_ref().map_or(String::new(), |id| format!("#{id}"))
            )
        });
        format!("lbry://{channel}{}{claim}", self.name)
    }
}

fn odysee_identity(url: &Url) -> Result<OdyseeIdentity> {
    ensure!(
        matches!(
            url.host_str(),
            Some("odysee.com" | "www.odysee.com" | "lbry.tv" | "www.lbry.tv")
        ),
        "请输入 Odysee 公开单个作品链接"
    );
    let path = percent_decode_str(url.path()).decode_utf8()?;
    let mut parts: Vec<_> = path.trim_matches('/').split('/').collect();
    if matches!(parts.first(), Some(&"$")) {
        ensure!(
            parts.len() == 4 && matches!(parts[1], "embed" | "download"),
            "Odysee 此类页面暂不支持"
        );
        let (name, claim) = odysee_part(&format!("{}:{}", parts[2], parts[3]), false)?;
        return Ok(OdyseeIdentity {
            channel: None,
            name,
            claim,
        });
    }
    let channel = if parts.first().is_some_and(|part| part.starts_with('@')) {
        Some(odysee_part(parts.remove(0), true)?)
    } else {
        None
    };
    ensure!(parts.len() == 1, "请输入 Odysee 单个作品；频道与合集不支持");
    let (name, claim) = odysee_part(parts[0], false)?;
    Ok(OdyseeIdentity {
        channel,
        name,
        claim,
    })
}

fn odysee_part(raw: &str, channel: bool) -> Result<(String, Option<String>)> {
    let (name, claim) = raw
        .rsplit_once(':')
        .map_or((raw, None), |(name, id)| (name, Some(id)));
    ensure!(
        !name.is_empty()
            && name.len() <= 300
            && name.starts_with('@') == channel
            && !name
                .chars()
                .any(|ch| ch.is_control() || "/$:#?&".contains(ch)),
        "Odysee 作品名称无效"
    );
    if let Some(id) = claim {
        ensure!(
            !id.is_empty() && id.len() <= 40 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Odysee claim ID 无效"
        );
    }
    Ok((name.to_owned(), claim.map(|id| id.to_ascii_lowercase())))
}

fn odysee_page(identity: &OdyseeIdentity) -> Result<Url> {
    let mut page = Url::parse("https://odysee.com/")?;
    {
        let mut path = page.path_segments_mut().unwrap();
        path.pop_if_empty();
        if let Some((name, id)) = &identity.channel {
            path.push(&format!(
                "{name}{}",
                id.as_ref().map_or(String::new(), |id| format!(":{id}"))
            ));
        }
        path.push(&format!(
            "{}{}",
            identity.name,
            identity
                .claim
                .as_ref()
                .map_or(String::new(), |id| format!(":{id}"))
        ));
    }
    Ok(page)
}

fn odysee_public_page(
    html: &str,
    expected: &OdyseeIdentity,
    page: &Url,
) -> Result<Option<GenericResource>> {
    ensure!(html.len() <= MAX_BYTES, "Odysee 页面超过解析上限");
    let scripts = Regex::new(
        r#"(?is)<script\b[^>]*type\s*=\s*["']application/ld\+json["'][^>]*>(.*?)</script\s*>"#,
    )?;
    let mut target: Option<GenericResource> = None;
    for captured in scripts.captures_iter(html).take(16) {
        let data: Value =
            serde_json::from_str(&captured[1]).context("Odysee 公开作品信息格式已变化")?;
        let items: Vec<&Value> = if let Some(items) = data.as_array() {
            items.iter().collect()
        } else {
            vec![&data]
        };
        for item in items {
            let kind = text(&item["@type"]).unwrap_or("");
            if !matches!(kind, "VideoObject" | "AudioObject") {
                continue;
            }
            let Some(raw_page) = text(&item["url"]) else {
                continue;
            };
            let Ok(canonical) = Url::parse(raw_page) else {
                continue;
            };
            if odysee_identity(&canonical).ok().as_ref() != Some(expected) {
                continue;
            }
            ensure!(
                !restricted(item)
                    && !flag(&item["isLiveBroadcast"])
                    && !flag(&item["publication"]["isLiveBroadcast"]),
                "Odysee 私密、预览或直播作品暂不支持"
            );
            ensure!(
                item.get("isAccessibleForFree")
                    .is_none_or(|value| { value.is_null() || flag(value) }),
                "Odysee 该作品需要付费授权"
            );
            let duration = iso_duration(text(&item["duration"]).unwrap_or(""))
                .filter(|duration| *duration > 0)
                .context("Odysee 未公开完整点播时长")?;
            let title = text(&item["name"]).context("Odysee 作品缺少标题")?;
            let url = crate::validation::parse_url(
                text(&item["contentUrl"]).context("Odysee 没有公开播放地址")?,
            )?;
            ensure!(
                media_url(&url, "Odysee") && !preview_url(&url),
                "Odysee 没有完整的公开播放地址"
            );
            let decoded = percent_decode_str(url.path()).decode_utf8()?;
            let components: Vec<_> = decoded.trim_matches('/').split('/').collect();
            let index = components
                .iter()
                .position(|part| *part == "free")
                .context("Odysee 当前页面未提供明确的免费完整媒体")?;
            ensure!(
                components.get(index + 1) == Some(&expected.name.as_str()),
                "Odysee 播放地址与作品名称不一致"
            );
            let id = components
                .get(index + 2)
                .context("Odysee 播放地址缺少完整 claim ID")?;
            ensure!(
                id.len() == 40
                    && id.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && expected
                        .claim
                        .as_ref()
                        .is_none_or(|prefix| id.starts_with(prefix)),
                "Odysee 播放地址与请求的 claim ID 不一致"
            );
            let extension = extension(&url, None).context("Odysee 此公开媒体格式暂不支持")?;
            let audio = kind == "AudioObject";
            let height = integer(&item["height"]).unwrap_or(0).min(16384) as u32;
            let resource = GenericResource {
                id: (*id).to_owned(),
                title: title.to_owned(),
                duration_seconds: Some(duration),
                candidates: vec![candidate(
                    "odysee-public-original",
                    "公开原始文件",
                    url,
                    extension,
                    height,
                    audio,
                    page,
                )],
                embed: None,
            };
            ensure!(target.is_none(), "Odysee 页面包含多个不同的目标媒体");
            target = Some(resource);
        }
    }
    Ok(target)
}

struct OdyseeClaim {
    id: String,
    title: String,
    duration: u64,
    height: u32,
    audio_only: bool,
    extension: &'static str,
}
fn odysee_claim(data: &Value, expected: &OdyseeIdentity) -> Result<OdyseeClaim> {
    let id = text(&data["claim_id"]).context("Odysee 作品不存在")?;
    ensure!(
        id.len() == 40
            && id.bytes().all(|byte| byte.is_ascii_hexdigit())
            && expected
                .claim
                .as_ref()
                .is_none_or(|prefix| id.starts_with(prefix)),
        "Odysee 返回了其他 claim 的信息"
    );
    ensure!(
        text(&data["name"]) == Some(expected.name.as_str())
            && text(&data["value_type"]) == Some("stream"),
        "Odysee 返回了其他作品或非媒体 claim"
    );
    if let Some((name, prefix)) = &expected.channel {
        ensure!(
            text(&data["signing_channel"]["name"]) == Some(name.as_str())
                && prefix
                    .as_ref()
                    .is_none_or(|prefix| text(&data["signing_channel"]["claim_id"])
                        .is_some_and(|id| id.starts_with(prefix))),
            "Odysee 返回了其他频道的作品"
        );
    }
    let value = &data["value"];
    let free = match value.get("fee") {
        None | Some(Value::Null) => true,
        Some(fee) => {
            fee["amount"]
                .as_f64()
                .or_else(|| text(&fee["amount"])?.parse::<f64>().ok())
                == Some(0.0)
        }
    };
    ensure!(
        !restricted(value) && free,
        "Odysee 该作品需要付费或其他授权"
    );
    let stream_type = text(&value["stream_type"]).unwrap_or("");
    ensure!(
        matches!(stream_type, "video" | "audio"),
        "Odysee 直播及非音视频 claim 暂不支持"
    );
    let duration = duration_number(&value[stream_type]["duration"])
        .filter(|duration| *duration > 0)
        .context("Odysee 未公开完整点播时长")?;
    let file_name = text(&value["source"]["name"]).context("Odysee 作品缺少媒体源")?;
    let fake_url = Url::parse("https://player.odycdn.com/")?.join(file_name)?;
    let extension = extension(&fake_url, text(&value["source"]["media_type"]))
        .context("Odysee 源媒体格式暂不支持")?;
    Ok(OdyseeClaim {
        id: id.to_owned(),
        title: text(&value["title"])
            .context("Odysee 作品缺少标题")?
            .to_owned(),
        duration,
        height: integer(&value["video"]["height"]).unwrap_or(0).min(16384) as u32,
        audio_only: stream_type == "audio",
        extension,
    })
}

fn ok_id(url: &Url) -> Option<String> {
    if !matches!(
        url.host_str(),
        Some("ok.ru" | "www.ok.ru" | "m.ok.ru" | "odnoklassniki.ru" | "www.odnoklassniki.ru")
    ) {
        return None;
    }
    let path = url.path().trim_end_matches('/');
    let id = ["/video/", "/videoembed/", "/web-api/video/moviePlayer/"]
        .iter()
        .find_map(|prefix| path.strip_prefix(prefix))?;
    let valid = !id.is_empty()
        && id.len() <= 40
        && id.split('-').count() <= 2
        && id
            .split('-')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    valid.then(|| id.to_owned())
}

fn ok_player(html: &str, id: &str) -> Result<Value> {
    ensure!(html.len() <= MAX_BYTES, "OK.ru 页面超过解析上限");
    ensure!(
        !html.contains(">Access to this video is restricted</div>"),
        "OK.ru 此视频要求登录授权"
    );
    let attrs = Regex::new(r#"(?is)\bdata-options\s*=\s*(?:"([^"]*)"|'([^']*)')"#)?;
    let mut players = Vec::new();
    for captures in attrs.captures_iter(html).take(64) {
        let raw = captures
            .get(1)
            .or_else(|| captures.get(2))
            .unwrap()
            .as_str();
        let Ok(player) = serde_json::from_str::<Value>(&decode_entities(raw)) else {
            continue;
        };
        let player_id = text(&player["playerId"]).unwrap_or("");
        let has_id = player_id.rsplit('_').next() == Some(id)
            || text(&player["url"])
                .and_then(|url| Url::parse(url).ok())
                .and_then(|url| ok_id(&url))
                .as_deref()
                == Some(id);
        if has_id && player["flashvars"].is_object() {
            players.push(player);
        }
    }
    ensure!(
        players.len() == 1,
        "OK.ru 没有唯一的目标播放器；作品可能私密、已删除或需要登录"
    );
    Ok(players.pop().unwrap())
}

fn ok_metadata_target(url: &Url, id: &str) -> bool {
    matches!(
        url.host_str(),
        Some("ok.ru" | "www.ok.ru" | "odnoklassniki.ru" | "www.odnoklassniki.ru")
    ) && url.scheme() == "https"
        && (ok_id(url).as_deref() == Some(id)
            || url
                .query_pairs()
                .any(|(key, value)| matches!(key.as_ref(), "st.mid" | "st.mvId") && value == id))
}

fn parse_ok(data: &Value, id: &str, page: &Url) -> Result<GenericResource> {
    let movie = &data["movie"];
    ensure!(
        text(&movie["id"]) == Some(id),
        "OK.ru 返回了其他视频的播放信息"
    );
    ensure!(
        !restricted(data)
            && !restricted(movie)
            && !flag(&movie["isLive"])
            && !flag(&movie["notPublished"]),
        "OK.ru 作品私密、未公开、仅提供预览或仍在直播"
    );
    let provider = text(&data["provider"]).unwrap_or("");
    ensure!(provider != "LIVE_TV_APP", "OK.ru 直播播放器暂不支持");
    if provider == "USER_YOUTUBE" {
        let video = text(&movie["contentId"]).context("OK.ru 外部视频缺少标识")?;
        ensure!(
            video.len() == 11
                && video
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte)),
            "OK.ru 外部视频标识无效"
        );
        return Ok(GenericResource {
            id: id.to_owned(),
            title: text(&movie["title"]).unwrap_or("OK.ru 外部视频").to_owned(),
            duration_seconds: None,
            candidates: Vec::new(),
            embed: Some(Url::parse(&format!(
                "https://www.youtube.com/watch?v={video}"
            ))?),
        });
    }
    ensure!(provider == "UPLOADED_ODKL", "OK.ru 此播放器来源暂不支持");
    let title = text(&movie["title"]).context("OK.ru 作品缺少标题")?;
    let duration = duration_number(&movie["duration"])
        .filter(|duration| *duration > 0)
        .context("OK.ru 未公开完整点播时长")?;
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    if let Some(videos) = data["videos"].as_array() {
        ensure!(videos.len() <= MAX_FORMATS, "OK.ru 格式超过解析上限");
        for (index, video) in videos.iter().enumerate() {
            if restricted(video) {
                continue;
            }
            let Some(raw) = text(&video["url"]) else {
                continue;
            };
            let Ok(url) = crate::validation::parse_url(raw) else {
                continue;
            };
            if !media_url(&url, "OK.ru") || preview_url(&url) || !seen.insert(url.to_string()) {
                continue;
            }
            let name = text(&video["name"]).unwrap_or("video");
            let height = match name {
                "mobile" => 144,
                "lowest" => 240,
                "low" => 360,
                "sd" => 480,
                "hd" => 720,
                "full" => 1080,
                "quad" => 1440,
                "ultra" => 2160,
                _ => 0,
            };
            candidates.push(candidate(
                &format!("ok-{index}"),
                &format!("{height}p · MP4"),
                url,
                "mp4",
                height,
                false,
                page,
            ));
        }
    }
    for key in ["hlsManifestUrl", "ondemandHls"] {
        if candidates.len() >= MAX_FORMATS {
            break;
        }
        let Some(raw) = text(&data[key]) else {
            continue;
        };
        let Ok(url) = crate::validation::parse_url(raw) else {
            continue;
        };
        if media_url(&url, "OK.ru") && !preview_url(&url) && seen.insert(url.to_string()) {
            candidates.push(candidate(
                &format!("ok-{key}"),
                "HLS · 完整点播",
                url,
                "m3u8",
                0,
                false,
                page,
            ));
        }
    }
    ensure!(
        !candidates.is_empty(),
        "OK.ru 未提供可公开读取的完整点播格式"
    );
    Ok(GenericResource {
        id: id.to_owned(),
        title: title.to_owned(),
        duration_seconds: Some(duration),
        candidates,
        embed: None,
    })
}

fn media_url(url: &Url, platform: &str) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
        && url.host_str().is_some_and(|host| match platform {
            "Rumble" => {
                host == "rumble.com"
                    || host.ends_with(".rmbl.ws")
                    || host == "rmbl.ws"
                    || host == "1a-1791.com"
                    || host.ends_with(".1a-1791.com")
                    || host.ends_with(".cdn.rumble.cloud")
            }
            "Odysee" => {
                host == "player.odycdn.com"
                    || host == "cdn.lbryplayer.xyz"
                    || host.ends_with(".odycdn.com")
            }
            "OK.ru" => {
                host == "ok.ru"
                    || host.ends_with(".okcdn.ru")
                    || host == "okcdn.ru"
                    || host.ends_with(".mycdn.me")
                    || host == "mycdn.me"
            }
            _ => false,
        })
}

fn candidate(
    id: &str,
    label: &str,
    url: Url,
    extension: &str,
    height: u32,
    audio_only: bool,
    page: &Url,
) -> GenericCandidate {
    let mut referer = page.clone();
    referer.set_query(None);
    referer.set_fragment(None);
    GenericCandidate {
        id: id.to_owned(),
        label: label.to_owned(),
        height,
        url: url.to_string(),
        extension: extension.to_owned(),
        audio_only,
        headers: vec![
            (
                "User-Agent".to_owned(),
                crate::resource::USER_AGENT.to_owned(),
            ),
            ("Referer".to_owned(), referer.to_string()),
        ],
        size: None,
    }
}
fn text(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}
fn integer(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| text(value)?.parse().ok())
}
fn duration_number(value: &Value) -> Option<u64> {
    value
        .as_f64()
        .or_else(|| text(value)?.parse().ok())
        .filter(|value| value.is_finite() && (0.0..=604800.0).contains(value))
        .map(|value| value.ceil() as u64)
}
fn flag(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_u64().is_none_or(|value| value != 0),
        Value::String(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "false" | "0"
        ),
        _ => true,
    }
}
fn restricted(value: &Value) -> bool {
    [
        "private",
        "isPrivate",
        "preview",
        "isPreview",
        "previewOnly",
        "drm",
        "hasDRM",
        "isPaid",
        "paid",
        "requiresAuth",
    ]
    .iter()
    .any(|key| flag(&value[*key]))
}
fn preview_url(url: &Url) -> bool {
    let path = percent_decode_str(url.path())
        .decode_utf8_lossy()
        .to_ascii_lowercase();
    path.contains("/preview/")
        || path.contains("/trailer/")
        || path.contains("encrypted")
        || url
            .query_pairs()
            .any(|(key, value)| key == "preview" && matches!(value.as_ref(), "1" | "true"))
}
fn extension(url: &Url, mime: Option<&str>) -> Option<&'static str> {
    let suffix = url.path().rsplit('.').next()?.to_ascii_lowercase();
    match suffix.as_str() {
        "mp4" => Some("mp4"),
        "webm" => Some("webm"),
        "m3u8" => Some("m3u8"),
        "mp3" => Some("mp3"),
        "m4a" => Some("m4a"),
        "ogg" => Some("ogg"),
        "ogv" => Some("ogv"),
        "wav" => Some("wav"),
        "flac" => Some("flac"),
        _ => match mime.unwrap_or("").split(';').next().unwrap_or("").trim() {
            "video/mp4" => Some("mp4"),
            "video/webm" => Some("webm"),
            "audio/mpeg" => Some("mp3"),
            "audio/mp4" => Some("m4a"),
            _ => None,
        },
    }
}
fn iso_duration(raw: &str) -> Option<u64> {
    let regex = Regex::new(r"^PT(?:(\d+)H)?(?:(\d+)M)?(?:(\d+(?:\.\d+)?)S)?$").ok()?;
    let captures = regex.captures(raw)?;
    let number = |index| {
        captures
            .get(index)
            .and_then(|value| value.as_str().parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    let seconds = number(1) * 3600.0 + number(2) * 60.0 + number(3);
    (seconds.is_finite() && (0.0..=604800.0).contains(&seconds)).then_some(seconds.ceil() as u64)
}
fn decode_entities(raw: &str) -> String {
    let regex = Regex::new(r"&(?:quot|apos|amp|lt|gt|#\d+|#x[0-9a-fA-F]+);").unwrap();
    regex
        .replace_all(raw, |capture: &regex::Captures<'_>| {
            let entity = &capture[0];
            match entity {
                "&quot;" => "\"".to_owned(),
                "&apos;" => "'".to_owned(),
                "&amp;" => "&".to_owned(),
                "&lt;" => "<".to_owned(),
                "&gt;" => ">".to_owned(),
                _ => {
                    let body = &entity[2..entity.len() - 1];
                    let number = body
                        .strip_prefix('x')
                        .and_then(|number| u32::from_str_radix(number, 16).ok())
                        .or_else(|| body.parse().ok());
                    number
                        .and_then(char::from_u32)
                        .map_or_else(|| entity.to_owned(), |character| character.to_string())
                }
            }
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rumble_checks_embed_identity_and_rejects_live_timeline_and_private() {
        let mut data = json!({"vid":9604851,"title":"A &amp; B","live":0,"duration":12,"ua":{"mp4":{"720":{"url":"https://hugh.cdn.rumble.cloud/clip.mp4?sig=a%2Bb","meta":{"h":720,"size":999}}},"timeline":[{"url":"https://hugh.cdn.rumble.cloud/frames.mp4"}],"hls":[{"url":"https://sp.rmbl.ws/vod.m3u8"}]}});
        let resource = parse_rumble(&data, "v5pv5f").unwrap();
        assert_eq!(resource.candidates.len(), 2);
        assert_eq!(resource.title, "A & B");
        assert!(resource
            .candidates
            .iter()
            .all(|candidate| candidate.size.is_none()));
        assert!(resource
            .candidates
            .iter()
            .any(|candidate| candidate.url.ends_with("?sig=a%2Bb")));
        assert!(parse_rumble(&data, "v5pv5g").is_err());
        data["live"] = json!(2);
        assert!(parse_rumble(&data, "v5pv5f").is_err());
        data["live"] = json!(0);
        data["private"] = json!(true);
        assert!(parse_rumble(&data, "v5pv5f").is_err());
        assert_eq!(
            rumble_page_embed(r#"<script>Rumble("play", {"video":"v5pv5f"});</script>"#).unwrap(),
            "v5pv5f"
        );
        assert!(rumble_page_embed(r#"<iframe src="https://rumble.com/embed/v5pv5f"></iframe><iframe src="https://rumble.com/embed/vb0ofn"></iframe>"#).is_err());
    }
    #[test]
    fn odysee_current_ssr_verifies_name_claim_and_public_full_asset() {
        let page = Url::parse("https://odysee.com/@channel:a/clip:1").unwrap();
        let expected = odysee_identity(&page).unwrap();
        let mut data = json!({"@type":"VideoObject","name":"A clip","url":page.as_str(),"duration":"PT1M2.5S","height":720,"contentUrl":"https://player.odycdn.com/api/v3/streams/free/clip/17f983b61f53091fb8ea58a9c56804e4ff8cff4d/file.mp4?sig=opaque"});
        let html = |value: &Value| format!("<script type='application/ld+json'>{value}</script>");
        let resource = odysee_public_page(&html(&data), &expected, &page)
            .unwrap()
            .unwrap();
        assert_eq!(resource.duration_seconds, Some(63));
        assert_eq!(resource.candidates[0].extension, "mp4");
        assert!(resource.candidates[0].url.ends_with("?sig=opaque"));
        for paid in [json!(false), json!("false"), json!(0)] {
            data["isAccessibleForFree"] = paid;
            assert!(odysee_public_page(&html(&data), &expected, &page).is_err());
        }
        data["isAccessibleForFree"] = json!(true);
        data["contentUrl"]=json!("https://player.odycdn.com/api/v3/streams/free/clip/27f983b61f53091fb8ea58a9c56804e4ff8cff4d/file.mp4");
        assert!(odysee_public_page(&html(&data), &expected, &page).is_err());
        data["contentUrl"]=json!("https://player.odycdn.com/api/v3/streams/preview/clip/17f983b61f53091fb8ea58a9c56804e4ff8cff4d/file.mp4");
        assert!(odysee_public_page(&html(&data), &expected, &page).is_err());
        data["isLiveBroadcast"] = json!(true);
        assert!(odysee_public_page(&html(&data), &expected, &page).is_err());
    }
    #[test]
    fn odysee_rpc_does_not_accept_paid_or_wrong_channel_claims() {
        let expected =
            odysee_identity(&Url::parse("https://odysee.com/@channel:a/clip:1").unwrap()).unwrap();
        let mut data = json!({"claim_id":"17f983b61f53091fb8ea58a9c56804e4ff8cff4d","name":"clip","value_type":"stream","signing_channel":{"name":"@channel","claim_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"value":{"title":"A clip","stream_type":"video","video":{"duration":12,"height":720},"source":{"name":"clip.webm","media_type":"video/webm"}}});
        assert_eq!(odysee_claim(&data, &expected).unwrap().extension, "webm");
        data["value"]["fee"] = json!({"amount":"0.5"});
        assert!(odysee_claim(&data, &expected).is_err());
        data["value"]["fee"] = json!({"amount":"unknown"});
        assert!(odysee_claim(&data, &expected).is_err());
        data["value"]["fee"] = Value::Null;
        data["signing_channel"]["name"] = json!("@another");
        assert!(odysee_claim(&data, &expected).is_err());
    }
    #[test]
    fn ok_parser_selects_target_movie_and_refuses_preview_live_and_metadata_mismatch() {
        let page = Url::parse("https://ok.ru/videoembed/123?tracking=private#player").unwrap();
        let mut data = json!({"provider":"UPLOADED_ODKL","movie":{"id":"123","contentId":"999","title":"A video","duration":"10","isLive":false},"videos":[{"name":"hd","url":"https://vd560.okcdn.ru/full.mp4?sig=a%2Bb"}],"hlsManifestUrl":"https://vd560.okcdn.ru/full.m3u8"});
        let resource = parse_ok(&data, "123", &page).unwrap();
        assert_eq!(resource.candidates.len(), 2);
        assert_eq!(
            resource.candidates[0].headers[1].1,
            "https://ok.ru/videoembed/123"
        );
        assert!(resource.candidates[0].url.ends_with("?sig=a%2Bb"));
        let player =
            json!({"playerId":"VideoPopup_player_123","flashvars":{"metadata":data.to_string()}});
        let html = format!(
            "<div data-options=\"{}\"></div>",
            player.to_string().replace('"', "&quot;")
        );
        assert!(ok_player(&html, "123").is_ok());
        assert!(ok_player(&html, "999").is_err());
        data["movie"]["id"] = json!("999");
        assert!(parse_ok(&data, "123", &page).is_err());
        data["movie"]["id"] = json!("123");
        data["movie"]["isLive"] = json!(true);
        assert!(parse_ok(&data, "123", &page).is_err());
        data["movie"]["isLive"] = json!(false);
        data["isPreview"] = json!(true);
        assert!(parse_ok(&data, "123", &page).is_err());
    }
    #[test]
    fn urls_reject_lookalikes_collections_and_cross_movie_metadata() {
        assert!(
            rumble_embed_id(&Url::parse("https://rumble.com.evil.test/embed/v5pv5f").unwrap())
                .is_none()
        );
        assert!(rumble_watch_path(&Url::parse("https://rumble.com/videos").unwrap()).is_none());
        assert!(!supports(
            &Url::parse("https://rumble.com/video.zip").unwrap(),
            "Rumble"
        ));
        assert!(supports(
            &Url::parse("https://rumble.com/vdmum1-moose-the-dog-helps-girls-dig-a-snow-fort.html")
                .unwrap(),
            "Rumble"
        ));
        assert_eq!(
            rumble_embed_id(&Url::parse("https://rumble.com/embed/ufe9n.v5pv5f/").unwrap())
                .as_deref(),
            Some("v5pv5f")
        );
        for path in [
            "/embed/a.b.v5pv5f",
            "/embed/path/ufe9n.v5pv5f",
            "/embed/bad-name.v5pv5f",
        ] {
            assert!(!supports(
                &Url::parse(&format!("https://rumble.com{path}")).unwrap(),
                "Rumble"
            ));
        }
        assert!(odysee_identity(&Url::parse("https://odysee.com/@channel:a").unwrap()).is_err());
        let embed = odysee_identity(
            &Url::parse("https://odysee.com/$/embed/clip/17f983b61f53091fb8ea58a9c56804e4ff8cff4d")
                .unwrap(),
        )
        .unwrap();
        assert!(embed.uri().starts_with("lbry://clip#17"));
        assert!(ok_id(&Url::parse("https://ok.ru/live/123").unwrap()).is_none());
        assert!(!ok_metadata_target(
            &Url::parse("https://evil.test/dk?st.mid=123").unwrap(),
            "123"
        ));
        assert!(!ok_metadata_target(
            &Url::parse("https://ok.ru/dk?st.mid=999").unwrap(),
            "123"
        ));
        assert!(!media_url(
            &Url::parse("https://vd560.okcdn.ru.evil.test/full.mp4").unwrap(),
            "OK.ru"
        ));
    }
    #[tokio::test]
    #[ignore = "reads current public Rumble/Odysee/OK.ru VOD metadata"]
    async fn public_vod_metadata() {
        let resolver = VideoBatch4Resolver::new().unwrap();
        for (platform, raw) in [
            ("Rumble", "https://rumble.com/embed/vb0ofn"),
            ("Odysee", "https://odysee.com/@Mantega:1/First-day-LBRY:1"),
            ("OK.ru", "https://ok.ru/videoembed/20079905452"),
        ] {
            let resource = resolver
                .resolve(&Url::parse(raw).unwrap(), platform)
                .await
                .unwrap();
            assert!(!resource.candidates.is_empty());
            println!(
                "{platform}: {} public VOD format(s), {} s",
                resource.candidates.len(),
                resource.duration_seconds.unwrap_or(0)
            );
        }
    }
}
