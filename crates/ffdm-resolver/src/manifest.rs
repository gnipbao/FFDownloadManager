//! Bounded HLS discovery, independent from platform-specific extractors.
//!
//! Reference: RFC 8216 sections 4.2, 4.3.4.1 and 4.3.4.2. The parser returns
//! individual playlists; `hls::expand` subsequently validates unencrypted VOD
//! segments. It does not recursively follow masters or combine external audio.

use anyhow::{bail, ensure, Context, Result};
use futures_util::StreamExt;
use reqwest::{Client, Url};
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

const MAX_PLAYLIST_BYTES: usize = 2 * 1024 * 1024;
const MAX_DECLARED_VARIANTS: usize = 256;
const MAX_RETURNED_VARIANTS: usize = 16;

#[derive(Debug, Clone)]
pub(crate) struct ManifestVariant {
    pub id: String,
    pub label: String,
    pub height: u32,
    pub url: String,
    pub headers: Vec<(String, String)>,
}

/// Discover playable combined-media HLS choices or an individual VOD playlist.
/// Signed query strings remain part of each URI; they are never logged.
pub(crate) async fn variants(
    url: &str,
    headers: &[(String, String)],
) -> Result<Vec<ManifestVariant>> {
    let url = crate::validation::parse_url(url)?;
    crate::hls::validate_headers(headers)?;
    let mut builder = Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::limited(5));
    if url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    }) {
        builder = builder.no_proxy();
    }
    let mut request = builder.build()?.get(url);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = request.send().await.map_err(|error| {
        anyhow::anyhow!(
            "获取 HLS 主清单失败：{}",
            crate::utils::safe_error(&error.to_string())
        )
    })?;
    ensure!(
        response.status().is_success(),
        "HLS 主清单 HTTP {}",
        response.status()
    );
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= MAX_PLAYLIST_BYTES as u64),
        "HLS 清单超过 2 MiB 上限"
    );
    let base = crate::validation::parse_url(response.url().as_str())?;
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|error| {
            anyhow::anyhow!(
                "读取 HLS 主清单失败：{}",
                crate::utils::safe_error(&error.to_string())
            )
        })?;
        ensure!(
            body.len().saturating_add(chunk.len()) <= MAX_PLAYLIST_BYTES,
            "HLS 清单超过 2 MiB 上限"
        );
        body.extend_from_slice(&chunk);
    }
    let body = std::str::from_utf8(&body).context("HLS 清单不是 UTF-8 文本")?;
    parse(&base, body, headers)
}

#[derive(Debug)]
struct DeclaredVariant {
    ordinal: usize,
    height: u32,
    bandwidth: u64,
    codec_label: Option<&'static str>,
    audio_only: bool,
    audio_group: Option<String>,
    url: Url,
}

fn parse(base: &Url, body: &str, headers: &[(String, String)]) -> Result<Vec<ManifestVariant>> {
    ensure!(body.len() <= MAX_PLAYLIST_BYTES, "HLS 清单超过 2 MiB 上限");
    let mut lines = body.trim_start_matches('\u{feff}').lines().map(str::trim);
    ensure!(lines.next() == Some("#EXTM3U"), "无效的 HLS 清单");
    let mut declared = Vec::new();
    let mut audio_groups = HashMap::<String, bool>::new();
    let mut pending = None;
    let mut media_segment = false;
    let mut ended = false;
    let mut media_uri = false;
    let mut expecting_segment = false;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if line.starts_with("#EXT-X-SESSION-KEY:") {
            bail!("暂不支持 HLS SESSION-KEY 或 DRM 媒体");
        }
        if let Some(attributes) = line.strip_prefix("#EXT-X-KEY:") {
            let attributes = attributes_map(attributes)?;
            ensure!(
                attributes
                    .get("METHOD")
                    .is_some_and(|method| method == "NONE"),
                "暂不支持加密或 DRM HLS 媒体"
            );
        } else if let Some(attributes) = line.strip_prefix("#EXT-X-MEDIA:") {
            ensure!(!media_segment && !ended, "HLS 主清单和媒体清单不能混合");
            let attributes = attributes_map(attributes)?;
            let media_type = attributes.get("TYPE").context("HLS 媒体组缺少 TYPE")?;
            if media_type == "AUDIO" {
                let group = attributes
                    .get("GROUP-ID")
                    .context("HLS 音频组缺少 GROUP-ID")?;
                ensure!(
                    audio_groups.len() < MAX_DECLARED_VARIANTS,
                    "HLS 媒体组超过上限"
                );
                let external = attributes.contains_key("URI");
                if let Some(uri) = attributes.get("URI") {
                    resolve_uri(base, uri)?;
                }
                audio_groups
                    .entry(group.clone())
                    .and_modify(|has_external| *has_external |= external)
                    .or_insert(external);
            }
        } else if let Some(attributes) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            ensure!(pending.is_none(), "HLS 格式声明缺少 URI");
            ensure!(!media_segment && !ended, "HLS 主清单和媒体清单不能混合");
            ensure!(
                declared.len() < MAX_DECLARED_VARIANTS,
                "HLS 格式声明超过 256 个上限"
            );
            pending = Some(attributes_map(attributes)?);
        } else if line.starts_with("#EXT-X-I-FRAME-STREAM-INF:") {
            ensure!(!media_segment && !ended, "HLS 主清单和媒体清单不能混合");
            // I-frame playlists are seek previews, not complete videos.
            attributes_map(line.trim_start_matches("#EXT-X-I-FRAME-STREAM-INF:"))?;
        } else if line == "#EXT-X-I-FRAMES-ONLY" {
            bail!("HLS I-frame 预览不包含完整视频");
        } else if line.starts_with("#EXTINF:") {
            ensure!(
                declared.is_empty() && pending.is_none(),
                "HLS 主清单和媒体清单不能混合"
            );
            ensure!(!ended && !expecting_segment, "HLS 媒体分段声明不完整");
            media_segment = true;
            expecting_segment = true;
        } else if line == "#EXT-X-ENDLIST" {
            ensure!(
                declared.is_empty() && pending.is_none(),
                "HLS 主清单和媒体清单不能混合"
            );
            ensure!(!expecting_segment, "HLS 媒体分段缺少 URI");
            ended = true;
        } else if line.starts_with("#EXT-X-DEFINE:") {
            bail!("暂不支持带变量的 HLS 清单");
        } else if !line.starts_with('#') {
            let url = resolve_uri(base, line)?;
            if let Some(attributes) = pending.take() {
                let mut canonical_base = base.clone();
                canonical_base.set_fragment(None);
                ensure!(url != canonical_base, "HLS 主清单不能引用自身");
                declared.push(declared_variant(declared.len(), url, &attributes)?);
            } else {
                ensure!(expecting_segment && !ended, "HLS URI 缺少格式或分段声明");
                media_uri = true;
                expecting_segment = false;
            }
        }
    }
    ensure!(pending.is_none(), "HLS 格式声明缺少 URI");
    if declared.is_empty() {
        ensure!(ended, "暂不支持直播 HLS；清单缺少 EXT-X-ENDLIST");
        ensure!(
            media_segment && media_uri && !expecting_segment,
            "HLS 没有完整的媒体分段"
        );
        return Ok(vec![ManifestVariant {
            id: "hls-vod".into(),
            label: "HLS 原始画质".into(),
            height: 0,
            url: base.as_str().into(),
            headers: headers.to_vec(),
        }]);
    }
    // A referenced external audio group means the variant URI can carry only
    // video. Never offer it as a complete video while silently discarding audio.
    let mut external_audio_skipped = false;
    declared.retain(|variant| {
        let unsupported_audio = variant
            .audio_group
            .as_ref()
            .is_some_and(|group| audio_groups.get(group).is_none_or(|external| *external));
        external_audio_skipped |= unsupported_audio;
        !unsupported_audio && !variant.audio_only
    });
    if declared.is_empty() {
        if external_audio_skipped {
            bail!("HLS 清单使用分离音频组；暂不支持将分离音视频清单合并");
        }
        bail!("HLS 清单没有可下载的完整视频格式（仅音频或预览）");
    }
    declared.sort_by(|left, right| {
        right
            .height
            .cmp(&left.height)
            .then_with(|| right.bandwidth.cmp(&left.bandwidth))
            .then_with(|| left.ordinal.cmp(&right.ordinal))
    });
    let mut seen = HashSet::new();
    Ok(declared
        .into_iter()
        .filter(|variant| seen.insert(variant.url.as_str().to_owned()))
        .take(MAX_RETURNED_VARIANTS)
        .map(|variant| {
            let quality = if variant.height > 0 {
                format!("{}p", variant.height)
            } else {
                format!("{} kbps", variant.bandwidth / 1000)
            };
            let codec = variant
                .codec_label
                .map_or(String::new(), |label| format!(" · {label}"));
            ManifestVariant {
                id: format!("hls-{}", variant.ordinal + 1),
                label: format!("{quality}{codec} · HLS"),
                height: variant.height,
                url: variant.url.into(),
                headers: headers.to_vec(),
            }
        })
        .collect())
}

fn declared_variant(
    ordinal: usize,
    url: Url,
    attributes: &HashMap<String, String>,
) -> Result<DeclaredVariant> {
    let bandwidth = positive_integer(attributes.get("BANDWIDTH"), "BANDWIDTH")?;
    if let Some(average) = attributes.get("AVERAGE-BANDWIDTH") {
        positive_integer(Some(average), "AVERAGE-BANDWIDTH")?;
    }
    let height = if let Some(resolution) = attributes.get("RESOLUTION") {
        let (width, height) = resolution.split_once('x').context("HLS RESOLUTION 无效")?;
        let width: u32 = width.parse().context("HLS RESOLUTION 无效")?;
        let height: u32 = height.parse().context("HLS RESOLUTION 无效")?;
        ensure!(width > 0 && height > 0, "HLS RESOLUTION 无效");
        height
    } else {
        0
    };
    if let Some(frame_rate) = attributes.get("FRAME-RATE") {
        let frame_rate: f64 = frame_rate.parse().context("HLS FRAME-RATE 无效")?;
        ensure!(
            frame_rate.is_finite() && frame_rate > 0.0,
            "HLS FRAME-RATE 无效"
        );
    }
    let codecs = attributes
        .get("CODECS")
        .map(String::as_str)
        .unwrap_or_default();
    let codecs: Vec<_> = codecs
        .split(',')
        .map(str::trim)
        .filter(|codec| !codec.is_empty())
        .collect();
    let audio_only =
        height == 0 && !codecs.is_empty() && codecs.iter().all(|codec| is_audio_codec(codec));
    let codec_label = codecs.iter().find_map(|codec| {
        if codec.starts_with("avc1") || codec.starts_with("avc3") {
            Some("AVC")
        } else if codec.starts_with("hvc1") || codec.starts_with("hev1") {
            Some("HEVC")
        } else if codec.starts_with("av01") {
            Some("AV1")
        } else if codec.starts_with("vp09") || codec.starts_with("vp9") {
            Some("VP9")
        } else {
            None
        }
    });
    Ok(DeclaredVariant {
        ordinal,
        height,
        bandwidth,
        codec_label,
        audio_only,
        audio_group: attributes.get("AUDIO").cloned(),
        url,
    })
}

fn is_audio_codec(codec: &str) -> bool {
    ["mp4a", "ac-3", "ec-3", "opus", "flac", "alac", "vorbis"]
        .iter()
        .any(|prefix| codec == *prefix || codec.starts_with(&format!("{prefix}.")))
}

fn positive_integer(value: Option<&String>, name: &str) -> Result<u64> {
    let value = value.with_context(|| format!("HLS 格式缺少 {name}"))?;
    ensure!(
        value.bytes().all(|byte| byte.is_ascii_digit()),
        "HLS {name} 无效"
    );
    let value: u64 = value.parse().with_context(|| format!("HLS {name} 无效"))?;
    ensure!(value > 0, "HLS {name} 必须大于零");
    Ok(value)
}

fn resolve_uri(base: &Url, uri: &str) -> Result<Url> {
    ensure!(!uri.is_empty() && !uri.contains("{$"), "HLS URI 无效");
    let mut url = base.join(uri).context("HLS URI 无效")?;
    crate::validation::parse_url(url.as_str())?;
    // Fragments are never sent to the server and must not defeat cycle checks.
    url.set_fragment(None);
    Ok(url)
}

// A quoted CODECS/URI attribute can contain commas, so split_once(',') would
// corrupt both codec lists and signed URLs.
fn attributes_map(text: &str) -> Result<HashMap<String, String>> {
    let mut result = HashMap::new();
    let mut rest = text;
    while !rest.is_empty() {
        ensure!(result.len() < 128, "HLS 属性超过上限");
        rest = rest.trim_start_matches([' ', '\t']);
        let (name, value) = rest.split_once('=').context("HLS 属性缺少等号")?;
        let name = name.trim_matches([' ', '\t']);
        let value = value.trim_start_matches([' ', '\t']);
        ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'-'),
            "HLS 属性名称无效"
        );
        let (value, tail) = if let Some(quoted) = value.strip_prefix('"') {
            let end = quoted.find('"').context("HLS 属性引号未闭合")?;
            (&quoted[..end], &quoted[end + 1..])
        } else {
            let end = value.find(',').unwrap_or(value.len());
            (value[..end].trim_end_matches([' ', '\t']), &value[end..])
        };
        ensure!(
            !value.is_empty() && value.trim() == value,
            "HLS 属性为空或包含空白"
        );
        ensure!(
            result.insert(name.into(), value.into()).is_none(),
            "HLS 属性重复"
        );
        let tail = tail.trim_start_matches([' ', '\t']);
        rest = if tail.is_empty() {
            ""
        } else {
            let next = tail.strip_prefix(',').context("HLS 属性分隔符无效")?;
            ensure!(!next.is_empty(), "HLS 属性不能以逗号结束");
            next
        };
    }
    ensure!(!result.is_empty(), "HLS 属性为空");
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.example/vod/master.m3u8?token=master").unwrap()
    }

    #[test]
    fn ranks_qualities_and_preserves_signed_relative_urls() {
        let list = "\u{feff}#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1200000,RESOLUTION=1280x720,CODECS=\"avc1.4d401f,mp4a.40.2\"\nlow.m3u8?sig=one,two\n#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080,CODECS=\"hev1.1.6.L120,mp4a.40.2\"\n../high.m3u8?sig=three\n#EXT-X-STREAM-INF:BANDWIDTH=8000000,RESOLUTION=1920x1080,CODECS=\"avc1.640028,mp4a.40.2\"\n//other.example/avc.m3u8?sig=four\n";
        let headers = vec![("Referer".into(), "https://page.example/watch".into())];
        let formats = parse(&base(), list, &headers).unwrap();
        assert_eq!(formats.len(), 3);
        assert_eq!(formats[0].height, 1080);
        assert_eq!(formats[0].id, "hls-3");
        assert_eq!(formats[0].label, "1080p · AVC · HLS");
        assert_eq!(formats[1].url, "https://cdn.example/high.m3u8?sig=three");
        assert_eq!(
            formats[2].url,
            "https://cdn.example/vod/low.m3u8?sig=one,two"
        );
        assert_eq!(formats[0].headers, headers);
    }

    #[test]
    fn legacy_cctv_horizontal_whitespace_keeps_values_signatures_and_duplicate_guard() {
        let list="#EXTM3U\n#EXT-X-STREAM-INF:PROGRAM-ID = 1, BANDWIDTH = 460800,\tRESOLUTION=480x270, CODECS = \"avc1,mp4a\" \nvideo.m3u8?sig=A%2Bb+raw\n";
        let variants = parse(&base(), list, &[]).unwrap();
        assert_eq!(variants[0].height, 270);
        assert!(variants[0].url.ends_with("?sig=A%2Bb+raw"));
        assert!(parse(
            &base(),
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1, BANDWIDTH = 2\nv.m3u8\n",
            &[]
        )
        .is_err());
    }

    #[test]
    fn omits_external_audio_and_audio_only_without_losing_safe_variants() {
        let list = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=6000000,RESOLUTION=1920x1080,AUDIO=\"external\"\nsilent.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720,AUDIO=\"inband\"\ncomplete.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=100000,CODECS=\"mp4a.40.2\"\naudio.m3u8\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"external\",NAME=\"English\",URI=\"audio.m3u8?sig=a,b\"\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"inband\",NAME=\"Original\"\n";
        let formats = parse(&base(), list, &[]).unwrap();
        assert_eq!(formats.len(), 1);
        assert!(formats[0].url.ends_with("complete.m3u8"));
        let external_only = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"A\",URI=\"audio.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=123,RESOLUTION=10x10,AUDIO=\"audio\"\nvideo.m3u8\n";
        assert!(parse(&base(), external_only, &[])
            .unwrap_err()
            .to_string()
            .contains("分离音频"));
        let undefined = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=123,AUDIO=\"missing\"\nvideo.m3u8\n";
        assert!(parse(&base(), undefined, &[]).is_err());
    }

    #[test]
    fn accepts_individual_vod_but_rejects_live_encrypted_or_incomplete_lists() {
        let vod = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:2,\npart.ts?sig=one\n#EXT-X-ENDLIST\n";
        let formats = parse(&base(), vod, &[]).unwrap();
        assert_eq!(formats.len(), 1);
        assert_eq!(formats[0].url, base().as_str());
        for list in [
            "#EXTM3U\n#EXTINF:2,\npart.ts\n",
            "#EXTM3U\n#EXT-X-ENDLIST\n",
            "#EXTM3U\n#EXTINF:2,\n#EXT-X-ENDLIST\n",
            "#EXTM3U\n#EXT-X-I-FRAMES-ONLY\n#EXTINF:2,\npart.ts\n#EXT-X-ENDLIST\n",
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key\"\n#EXTINF:2,\npart.ts\n#EXT-X-ENDLIST\n",
            "#EXTM3U\n#EXT-X-SESSION-KEY:METHOD=NONE\n#EXTINF:2,\npart.ts\n#EXT-X-ENDLIST\n",
        ] {
            assert!(parse(&base(), list, &[]).is_err(), "accepted {list}");
        }
    }

    #[test]
    fn malformed_attributes_cycles_and_non_http_references_are_rejected() {
        for attributes in [
            "BANDWIDTH=",
            "BANDWIDTH=10,BANDWIDTH=20",
            "BANDWIDTH=10,CODECS=\"avc1,mp4a",
            "BANDWIDTH=10,CODECS=\"avc1\"garbage",
            "BANDWIDTH=10,",
            "BANDWIDTH=10,RESOLUTION=1920x0",
            "BANDWIDTH=10,FRAME-RATE=NaN",
            "RESOLUTION=1280x720",
            "BANDWIDTH=-1",
        ] {
            let list = format!("#EXTM3U\n#EXT-X-STREAM-INF:{attributes}\nvideo.m3u8\n");
            assert!(parse(&base(), &list, &[]).is_err(), "accepted {attributes}");
        }
        for uri in [
            "master.m3u8?token=master#cycle",
            "file:///private/video.m3u8",
            "https://user:password@cdn.example/media.m3u8",
            "{$variant}.m3u8",
        ] {
            let list = format!("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=100\n{uri}\n");
            assert!(parse(&base(), &list, &[]).is_err(), "accepted {uri}");
        }
        assert!(parse(&base(), "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=100\n", &[]).is_err());
    }

    #[test]
    fn bounds_input_choices_and_deduplicates_after_ranking() {
        let mut list = String::from("#EXTM3U\n");
        for index in 1..=32 {
            list.push_str(&format!(
                "#EXT-X-STREAM-INF:BANDWIDTH={},RESOLUTION=100x{}\nquality-{}.m3u8\n",
                index * 1000,
                index * 10,
                index / 2
            ));
        }
        let formats = parse(&base(), &list, &[]).unwrap();
        assert_eq!(formats.len(), MAX_RETURNED_VARIANTS);
        assert_eq!(formats[0].height, 320);
        let urls: HashSet<_> = formats.iter().map(|format| &format.url).collect();
        assert_eq!(urls.len(), formats.len());
        let list = format!(
            "#EXTM3U\n{}",
            "#EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8\n".repeat(257)
        );
        assert!(parse(&base(), &list, &[]).is_err());
        let list = "#".repeat(MAX_PLAYLIST_BYTES + 1);
        assert!(parse(&base(), &list, &[]).is_err());
    }

    #[tokio::test]
    async fn fetch_uses_final_redirect_base_headers_and_streaming_size_limit() {
        use axum::{
            body::Body,
            http::{Request, StatusCode},
            response::Redirect,
            routing::get,
            Router,
        };
        let app = Router::new()
            .route(
                "/redirect.m3u8",
                get(|| async { Redirect::temporary("/actual/master.m3u8?redirect=token") }),
            )
            .route(
                "/actual/master.m3u8",
                get(|request: Request<Body>| async move {
                    assert_eq!(request.headers()["user-agent"], "FFDM-manifest-fixture");
                    "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000,RESOLUTION=640x360\nchild.m3u8?sig=one,two\n"
                }),
            )
            .route(
                "/too-big.m3u8",
                get(|| async {
                    let chunks = futures_util::stream::iter((0..34).map(|_| {
                        Ok::<_, std::io::Error>(vec![b'#'; 64 * 1024])
                    }));
                    Body::from_stream(chunks)
                }),
            )
            .route(
                "/unavailable.m3u8",
                get(|| async { (StatusCode::FORBIDDEN, "private server content") }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let headers = vec![("User-Agent".into(), "FFDM-manifest-fixture".into())];
        let formats = variants(&format!("http://{address}/redirect.m3u8"), &headers)
            .await
            .unwrap();
        assert_eq!(
            formats[0].url,
            format!("http://{address}/actual/child.m3u8?sig=one,two")
        );
        assert_eq!(formats[0].headers, headers);
        let oversized = variants(&format!("http://{address}/too-big.m3u8"), &[])
            .await
            .unwrap_err();
        assert!(oversized.to_string().contains("2 MiB"));
        let unavailable = variants(
            &format!("http://{address}/unavailable.m3u8?private=secret"),
            &[],
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(unavailable.contains("403"));
        assert!(!unavailable.contains("secret"));
        assert!(!unavailable.contains("private server content"));
        let disallowed_headers = [("Cookie".into(), "session=secret".into())];
        assert!(variants(
            &format!("http://{address}/redirect.m3u8"),
            &disallowed_headers
        )
        .await
        .is_err());
        server.abort();
    }
}
