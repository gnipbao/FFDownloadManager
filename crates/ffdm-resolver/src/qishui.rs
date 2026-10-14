//! Resolve the current track from actual browser playback traffic, not the
//! platform's separate in-app download endpoint or its commercial download flags.
use crate::browser::HeadlessPage;
use anyhow::{bail, ensure, Context, Result};
use reqwest_media::Url;
use serde_json::{json, Value};
use std::{collections::HashMap, time::Duration};

pub struct QishuiTrack {
    pub id: String,
    pub title: String,
    pub duration_seconds: Option<u64>,
    pub media_url: String,
    pub extension: &'static str,
    pub bitrate_kbps: Option<u64>,
    pub size: Option<u64>,
    pub headers: Vec<(String, String)>,
    pub preview: bool,
}

pub struct QishuiResolver;

impl QishuiResolver {
    pub fn new() -> Self {
        Self
    }

    pub async fn resolve(&self, input: &Url) -> Result<QishuiTrack> {
        let id = track_id(input).context("汽水音乐链接缺少有效的 track_id")?;
        let mut browser = HeadlessPage::launch().await?;
        let result =
            tokio::time::timeout(Duration::from_secs(28), capture_track(&mut browser, &id)).await;
        browser.close().await;
        result.context("浏览器未捕获到当前歌曲的音频；分享页可能暂时无法播放或需要登录")?
    }
}

pub fn track_id(url: &Url) -> Option<String> {
    if !is_track_page(url) {
        return None;
    }
    let id = url
        .query_pairs()
        .find(|(key, _)| key == "track_id")?
        .1
        .into_owned();
    (12..=24)
        .contains(&id.len())
        .then_some(id)
        .filter(|id| id.bytes().all(|byte| byte.is_ascii_digit()))
}

pub fn is_track_page(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some("music.douyin.com")
        && url.path() == "/qishui/share/track"
}

// Only activate the page's own media elements. No stream is constructed from
// metadata, and no URL is assigned to a media element by the resolver.
const INSPECT_PLAYER: &str = r#"(() => {
  const t = globalThis._ROUTER_DATA?.loaderData?.track_page?.audioWithLyricsOption;
  const media = [...document.querySelectorAll('audio,video')];
  for (const m of media) {
    m.muted = true;
    if (m.paused && (m.currentSrc || m.src)) m.play().catch(() => {});
  }
  const control = document.querySelector('.track-play-button-wrap');
  const rect = control?.getBoundingClientRect();
  return {
    pageUrl: location.href,
    userAgent: navigator.userAgent,
    track: t ? { id: t.track_id, title: t.trackName, artist: t.artistName,
      duration: t.duration, encrypt: t.encrypt, url: t.url,
      previewStart: t.previewStart, previewEnd: t.previewEnd } : null,
    media: media.map(m => ({url: m.currentSrc || m.src,
      duration: Number.isFinite(m.duration) && m.readyState >= 1 ? m.duration : null})),
    playControl: rect && rect.width > 0 && rect.height > 0
      ? {x: rect.x + rect.width / 2, y: rect.y + rect.height / 2} : null
  };
})()"#;

async fn capture_track(browser: &mut HeadlessPage, id: &str) -> Result<QishuiTrack> {
    browser.call("Network.enable", json!({})).await?;
    let result = browser.call("Page.navigate", json!({
        "url": format!("https://music.douyin.com/qishui/share/track?track_id={id}&auto_play_bgm=1")
    })).await?;
    ensure!(
        result.get("errorText").is_none(),
        "浏览器无法打开汽水音乐分享页"
    );
    let mut traffic = AudioTraffic::default();
    let mut clicked_play = false;
    let started = tokio::time::Instant::now();
    loop {
        let result = browser
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": INSPECT_PLAYER, "returnByValue": true, "userGesture": true,
                }),
            )
            .await?;
        for event in browser.take_events() {
            traffic.observe(event);
        }
        let snapshot = &result["result"]["value"];
        if let Some(track) = captured_track(snapshot, id, &traffic)? {
            return Ok(track);
        }
        // The public share page sometimes waits for a click even with autoplay
        // enabled. Click only its observed playback control on this exact track.
        if !clicked_play
            && started.elapsed() > Duration::from_secs(2)
            && snapshot["track"]["id"].as_str() == Some(id)
            && snapshot["track"]["encrypt"].as_bool() == Some(false)
        {
            if let (Some(x), Some(y)) = (
                snapshot["playControl"]["x"].as_f64(),
                snapshot["playControl"]["y"].as_f64(),
            ) {
                for kind in ["mousePressed", "mouseReleased"] {
                    browser
                        .call(
                            "Input.dispatchMouseEvent",
                            json!({
                                "type": kind, "x": x, "y": y, "button": "left", "clickCount": 1,
                            }),
                        )
                        .await?;
                }
                clicked_play = true;
            }
        }
        tokio::time::sleep(Duration::from_millis(350)).await;
    }
}

#[derive(Default)]
struct AudioTraffic {
    requests: HashMap<String, ObservedAudio>,
}

#[derive(Default)]
struct ObservedAudio {
    urls: Vec<String>,
    headers: Vec<(String, String)>,
    response: Option<Value>,
}

impl AudioTraffic {
    fn observe(&mut self, event: Value) {
        let params = &event["params"];
        let Some(id) = params["requestId"].as_str() else {
            return;
        };
        let (payload, is_response) = match event["method"].as_str() {
            Some("Network.requestWillBeSent") => (&params["request"], false),
            Some("Network.responseReceived") => (&params["response"], true),
            _ => return,
        };
        let Some(url) = payload["url"]
            .as_str()
            .filter(|url| media_url(url).is_some())
        else {
            return;
        };
        if self.requests.len() >= 64 && !self.requests.contains_key(id) {
            return;
        }
        let entry = self.requests.entry(id.into()).or_default();
        if !entry.urls.iter().any(|old| old == url) && entry.urls.len() < 8 {
            entry.urls.push(url.into());
        }
        if is_response {
            entry.response = Some(payload.clone());
        } else if let Some(headers) = payload["headers"].as_object() {
            // Keep only headers relevant to this exact public media request.
            // Range and hop-by-hop headers belong to the Rust download engine.
            entry.headers = headers
                .iter()
                .filter_map(|(key, value)| {
                    matches!(
                        key.to_ascii_lowercase().as_str(),
                        "referer" | "origin" | "user-agent"
                    )
                    .then(|| value.as_str().map(|value| (key.clone(), value.to_owned())))
                    .flatten()
                })
                .collect();
        }
    }
}

fn media_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    (url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url
            .host_str()
            .is_some_and(|host| host == "douyinvod.com" || host.ends_with(".douyinvod.com")))
    .then_some(url)
}

fn same_url(left: &str, right: &str) -> bool {
    let (Ok(mut left), Ok(mut right)) = (Url::parse(left), Url::parse(right)) else {
        return false;
    };
    left.set_fragment(None);
    right.set_fragment(None);
    left == right
}

fn captured_track(
    snapshot: &Value,
    id: &str,
    traffic: &AudioTraffic,
) -> Result<Option<QishuiTrack>> {
    let track = &snapshot["track"];
    if track.is_null() {
        return Ok(None);
    }
    ensure!(
        track["id"].as_str() == Some(id),
        "汽水音乐页面返回了其他曲目"
    );
    let page = snapshot["pageUrl"]
        .as_str()
        .and_then(|raw| Url::parse(raw).ok());
    ensure!(
        page.as_ref().and_then(track_id).as_deref() == Some(id),
        "汽水音乐页面发生了意外跳转"
    );
    ensure!(
        track["encrypt"].as_bool() == Some(false),
        "网页使用加密音频，当前不支持保存此格式"
    );
    let expected_url = track["url"]
        .as_str()
        .filter(|raw| !raw.is_empty())
        .context("分享页没有提供可播放音频，请确认当前歌曲可在网页播放")?;
    ensure!(
        media_url(expected_url).is_some(),
        "汽水音乐网页返回了不受支持的播放地址"
    );
    let observed = traffic.requests.values().find(|entry| {
        entry.urls.iter().any(|url| same_url(url, expected_url))
            && entry
                .response
                .as_ref()
                .is_some_and(|r| matches!(r["status"].as_u64(), Some(200 | 206)))
    });
    let Some(observed) = observed else {
        return Ok(None);
    };
    let response = observed.response.as_ref().unwrap();
    let url = response["url"]
        .as_str()
        .and_then(media_url)
        .context("捕获的音频地址无效")?;
    let mime = response["mimeType"]
        .as_str()
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("");
    let extension = match mime {
        "audio/mp4" | "audio/x-m4a" => "m4a",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/aac" => "aac",
        "audio/flac" | "audio/x-flac" => "flac",
        // Some ByteDance audio CDNs use the MP4 container's generic MIME.
        "video/mp4"
            if url
                .query_pairs()
                .any(|(k, v)| k == "mime_type" && v == "audio_mp4") =>
        {
            "m4a"
        }
        _ => bail!("网页返回的媒体不是支持的独立音频文件"),
    };
    let title = track["title"]
        .as_str()
        .filter(|v| !v.is_empty() && *v != "-")
        .context("汽水音乐页面缺少歌曲名称")?;
    let title = track["artist"]
        .as_str()
        .filter(|v| !v.is_empty() && *v != "-")
        .map_or_else(|| title.into(), |artist| format!("{artist} - {title}"));
    // Use browser-loaded media metadata for duration, rather than claiming that
    // the full catalog duration is present in a possibly preview-only response.
    let duration = snapshot["media"].as_array().and_then(|media| {
        media.iter().find_map(|media| {
            let src = media["url"].as_str()?;
            observed
                .urls
                .iter()
                .any(|url| same_url(url, src))
                .then(|| media["duration"].as_f64())
                .flatten()
                .filter(|d| d.is_finite() && *d > 0.0)
        })
    });
    let total_duration = track["duration"]
        .as_f64()
        .filter(|d| d.is_finite() && *d > 0.0);
    let preview_span = track["previewEnd"]
        .as_f64()
        .zip(track["previewStart"].as_f64())
        .map(|(end, start)| end - start)
        .filter(|d| d.is_finite() && *d > 0.0);
    let preview = total_duration.is_some_and(|full| {
        duration.is_some_and(|d| d + 2.0 < full) || preview_span.is_some_and(|d| d + 2.0 < full)
    });
    let bitrate_kbps = url
        .query_pairs()
        .find(|(key, _)| key == "br")
        .and_then(|(_, value)| value.parse::<u64>().ok())
        .filter(|v| (16..=2_000).contains(v));
    let mut headers = observed.headers.clone();
    if !headers
        .iter()
        .any(|(key, _)| key.eq_ignore_ascii_case("user-agent"))
    {
        if let Some(agent) = snapshot["userAgent"].as_str() {
            headers.push(("User-Agent".into(), agent.into()));
        }
    }
    Ok(Some(QishuiTrack {
        id: id.into(),
        title,
        duration_seconds: duration.map(|d| d.round() as u64),
        media_url: url.into(),
        extension,
        bitrate_kbps,
        size: response_size(response),
        headers,
        preview,
    }))
}

fn response_size(response: &Value) -> Option<u64> {
    let headers = response["headers"].as_object()?;
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))?
            .1
            .as_str()
    };
    let size = if response["status"].as_u64() == Some(206) {
        // Content-Length describes only this range, not the complete file.
        header("content-range")?
            .rsplit_once('/')?
            .1
            .parse::<u64>()
            .ok()?
    } else {
        header("content-length")?.parse::<u64>().ok()?
    };
    (size > 0).then_some(size)
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "7687463141597169715";
    const AUDIO: &str =
        "https://v26-luna.douyinvod.com/audio?br=126&mime_type=audio_mp4&sig=example";

    fn snapshot() -> Value {
        json!({"pageUrl": format!("https://music.douyin.com/qishui/share/track?track_id={ID}"),
            "userAgent":"test-browser", "track": {"id":ID,"title":"测试曲目","artist":"测试歌手",
                "duration":153.266,"encrypt":false,"url":AUDIO,"previewStart":0,"previewEnd":153.266,
                "group_download_level":"pay", "only_vip_download":true},
            "media":[{"url":AUDIO,"duration":153.266}]})
    }
    fn traffic(url: &str, mime: &str) -> AudioTraffic {
        let mut traffic = AudioTraffic::default();
        traffic.observe(json!({"method":"Network.requestWillBeSent","params":{"requestId":"one",
            "request":{"url":url,"headers":{"Referer":"https://music.douyin.com/","Range":"bytes=0-1023","Cookie":"not copied"}}}}));
        traffic.observe(
            json!({"method":"Network.responseReceived","params":{"requestId":"one",
            "response":{"url":url,"status":206,"mimeType":mime,
                "headers":{"Content-Range":"bytes 0-1023/2533293","Content-Length":"1024"}}}}),
        );
        traffic
    }

    #[test]
    fn browser_playback_is_independent_of_in_app_download_flags() {
        let track = captured_track(&snapshot(), ID, &traffic(AUDIO, "audio/mp4"))
            .unwrap()
            .unwrap();
        assert_eq!(track.title, "测试歌手 - 测试曲目");
        assert_eq!(track.extension, "m4a");
        assert_eq!(track.duration_seconds, Some(153));
        assert_eq!(track.size, Some(2533293));
        assert!(!track.preview);
        assert!(!track
            .headers
            .iter()
            .any(|(key, _)| matches!(key.as_str(), "Range" | "Cookie")));
    }
    #[test]
    fn page_metadata_alone_and_other_tracks_are_not_captured_audio() {
        assert!(captured_track(&snapshot(), ID, &AudioTraffic::default())
            .unwrap()
            .is_none());
        assert!(captured_track(
            &snapshot(),
            ID,
            &traffic("https://v26-luna.douyinvod.com/recommendation", "audio/mp4")
        )
        .unwrap()
        .is_none());
        assert!(captured_track(
            &snapshot(),
            "1111111111111111111",
            &traffic(AUDIO, "audio/mp4")
        )
        .is_err());
    }
    #[test]
    fn keeps_preview_duration_and_rejects_encrypted_or_non_audio_responses() {
        let mut state = snapshot();
        state["media"][0]["duration"] = json!(30);
        let track = captured_track(&state, ID, &traffic(AUDIO, "audio/mp4"))
            .unwrap()
            .unwrap();
        assert_eq!(track.duration_seconds, Some(30));
        assert!(track.preview);
        state["track"]["encrypt"] = json!(true);
        assert!(captured_track(&state, ID, &traffic(AUDIO, "audio/mp4")).is_err());
        assert!(captured_track(&snapshot(), ID, &traffic(AUDIO, "text/html")).is_err());
    }
    #[test]
    fn accepts_only_qishui_pages_and_known_audio_cdn() {
        let valid = Url::parse(&format!(
            "https://music.douyin.com/qishui/share/track?track_id={ID}&sec_sharer_id=ignored"
        ))
        .unwrap();
        assert_eq!(track_id(&valid).as_deref(), Some(ID));
        assert!(track_id(
            &Url::parse(&format!(
                "https://music.douyin.com.evil.example/qishui/share/track?track_id={ID}"
            ))
            .unwrap()
        )
        .is_none());
        assert!(media_url("https://evil.example/audio").is_none());
        assert!(media_url("https://user:password@v26-luna.douyinvod.com/audio").is_none());
        assert!(media_url("http://v26-luna.douyinvod.com/audio").is_none());
    }
}
