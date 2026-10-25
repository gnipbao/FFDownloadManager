//! Bounded, unencrypted HLS VOD expansion. HTTP is handled by Rust only.
use crate::media::MediaStream;
use anyhow::{bail, ensure, Context, Result};
use futures_util::StreamExt;
use reqwest::{Client, Url};
use std::{collections::HashMap, time::Duration};

const MAX_PLAYLIST_BYTES: usize = 2 * 1024 * 1024;
const MAX_STREAMS: usize = 4096;

/// These headers may be persisted with segments and sent to a different CDN.
/// Session credentials are deliberately not part of this contract.
#[doc(hidden)]
pub fn validate_headers(headers: &[(String, String)]) -> Result<()> {
    crate::validation::validate_headers(headers)?;
    let mut seen = std::collections::HashSet::new();
    for (name, value) in headers {
        let name = name.to_ascii_lowercase();
        ensure!(seen.insert(name.clone()), "HLS 请求头重复");
        match name.as_str() {
            "user-agent" | "accept" => {}
            "referer" => {
                let url = crate::validation::parse_url(value)?;
                ensure!(url.fragment().is_none(), "HLS Referer 不能包含 fragment");
            }
            "origin" => {
                let url = crate::validation::parse_url(value)?;
                ensure!(
                    url.path() == "/" && url.query().is_none() && url.fragment().is_none(),
                    "HLS Origin 必须是 HTTP(S) 来源，不含路径或查询参数"
                );
            }
            _ => bail!("HLS 只允许 User-Agent、Referer、Origin 和 Accept 请求头"),
        }
    }
    Ok(())
}

/// Accept a media playlist, after an adapter has selected a master variant.
/// Relative URLs resolve against the final playlist URL after redirects.
pub async fn expand(url: &str, headers: &[(String, String)]) -> Result<Vec<MediaStream>> {
    let url = crate::validation::parse_url(url)?;
    validate_headers(headers)?;
    let mut client = Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::limited(5));
    // System proxy backends do not consistently respect loopback exceptions.
    // Keep local fixtures/local-origin media local; public platforms still use
    // the user's configured network route.
    if url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    }) {
        client = client.no_proxy();
    }
    let client = client.build()?;
    let mut request = client.get(url);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = request.send().await.map_err(|error| {
        anyhow::anyhow!(
            "获取 HLS 清单失败：{}",
            crate::media::safe_error(&error.to_string())
        )
    })?;
    ensure!(
        response.status().is_success(),
        "HLS 清单 HTTP {}",
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
        let chunk = chunk.context("读取 HLS 清单失败")?;
        ensure!(
            body.len().saturating_add(chunk.len()) <= MAX_PLAYLIST_BYTES,
            "HLS 清单超过 2 MiB 上限"
        );
        body.extend_from_slice(&chunk);
    }
    let text = std::str::from_utf8(&body).context("HLS 清单不是 UTF-8 文本")?;
    parse(&base, text, headers)
}

fn parse(base: &Url, text: &str, headers: &[(String, String)]) -> Result<Vec<MediaStream>> {
    ensure!(text.len() <= MAX_PLAYLIST_BYTES, "HLS 清单超过 2 MiB 上限");
    let mut lines = text.trim_start_matches('\u{feff}').lines().map(str::trim);
    ensure!(lines.next() == Some("#EXTM3U"), "无效的 HLS 清单");
    let mut streams = Vec::new();
    let mut init = None;
    let mut expecting_segment = false;
    let mut ended = false;
    let mut segments = 0;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if line == "#EXT-X-ENDLIST" {
            ensure!(!expecting_segment, "HLS 缺少媒体分段地址");
            ended = true;
        } else if let Some(attributes) = line
            .strip_prefix("#EXT-X-KEY:")
            .or_else(|| line.strip_prefix("#EXT-X-SESSION-KEY:"))
        {
            let attributes = attributes_map(attributes)?;
            ensure!(
                attributes
                    .get("METHOD")
                    .is_some_and(|method| method == "NONE"),
                "暂不支持加密或 DRM HLS 媒体"
            );
        } else if line.starts_with("#EXT-X-BYTERANGE:") {
            bail!("暂不支持 HLS BYTERANGE 格式");
        } else if line.starts_with("#EXT-X-DISCONTINUITY") {
            bail!("暂不支持 HLS 时间戳或编码切换（DISCONTINUITY）");
        } else if line.starts_with("#EXT-X-STREAM-INF:")
            || line.starts_with("#EXT-X-I-FRAME-STREAM-INF:")
            || line.starts_with("#EXT-X-MEDIA:")
        {
            bail!("请先选择 HLS 主清单中的独立音视频格式；暂不支持分离音频清单");
        } else if let Some(attributes) = line.strip_prefix("#EXT-X-MAP:") {
            ensure!(!ended && !expecting_segment, "HLS 初始化段位置无效");
            let attributes = attributes_map(attributes)?;
            ensure!(
                !attributes.contains_key("BYTERANGE"),
                "暂不支持 HLS BYTERANGE 初始化段"
            );
            let uri = attributes.get("URI").context("HLS 初始化段缺少 URI")?;
            let stream = stream(base, uri, headers)?;
            match &init {
                Some(url) => ensure!(url == &stream.url, "暂不支持 HLS 中途切换初始化段"),
                None => {
                    ensure!(segments == 0, "HLS 初始化段必须在媒体分段之前");
                    init = Some(stream.url.clone());
                    streams.push(stream);
                }
            }
        } else if let Some(duration) = line.strip_prefix("#EXTINF:") {
            ensure!(!ended && !expecting_segment, "HLS 分段声明不完整");
            let duration: f64 = duration
                .split(',')
                .next()
                .unwrap_or_default()
                .parse()
                .context("HLS 分段时长无效")?;
            ensure!(duration.is_finite() && duration >= 0.0, "HLS 分段时长无效");
            expecting_segment = true;
        } else if line.starts_with("#EXT-X-GAP")
            || line.starts_with("#EXT-X-PART:")
            || line.starts_with("#EXT-X-PRELOAD-HINT:")
            || line.starts_with("#EXT-X-DEFINE:")
        {
            bail!("暂不支持缺失分段、低延迟或变量 HLS 清单");
        } else if !line.starts_with('#') {
            ensure!(!ended && expecting_segment, "HLS 媒体分段缺少 EXTINF 声明");
            ensure!(streams.len() < MAX_STREAMS, "HLS 超过 4096 个媒体分段上限");
            streams.push(stream(base, line, headers)?);
            segments += 1;
            expecting_segment = false;
        }
    }
    ensure!(ended, "暂不支持直播 HLS；清单缺少 EXT-X-ENDLIST");
    ensure!(segments > 0 && !expecting_segment, "HLS 没有完整的媒体分段");
    Ok(streams)
}

fn stream(base: &Url, uri: &str, headers: &[(String, String)]) -> Result<MediaStream> {
    ensure!(!uri.is_empty() && !uri.contains("{$"), "HLS 分段 URI 无效");
    let url = base.join(uri).context("HLS 分段 URI 无效")?;
    crate::validation::parse_url(url.as_str())?;
    Ok(MediaStream {
        transform: None,
        url: url.into(),
        headers: headers.to_vec(),
        size: None,
        decrypt_key: None,
    })
}

// HLS attribute lists allow commas inside a quoted URI. Splitting on commas
// would corrupt signed URLs and can accidentally change the selected resource.
fn attributes_map(text: &str) -> Result<HashMap<&str, String>> {
    let mut result = HashMap::new();
    let mut rest = text;
    while !rest.is_empty() {
        rest = rest.trim_start_matches([' ', '\t']);
        let (name, value) = rest.split_once('=').context("HLS 属性缺少等号")?;
        let name = name.trim_matches([' ', '\t']);
        let value = value.trim_start_matches([' ', '\t']);
        ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-'),
            "HLS 属性名称无效"
        );
        let (value, next) = if let Some(quoted) = value.strip_prefix('"') {
            let end = quoted.find('"').context("HLS 属性引号未闭合")?;
            let tail = quoted[end + 1..].trim_start_matches([' ', '\t']);
            ensure!(
                tail.is_empty() || tail.starts_with(','),
                "HLS 属性分隔符无效"
            );
            (&quoted[..end], tail.strip_prefix(',').unwrap_or_default())
        } else {
            let (value, next) = value.split_once(',').unwrap_or((value, ""));
            (value.trim_end_matches([' ', '\t']), next)
        };
        ensure!(
            !value.is_empty() && result.insert(name, value.to_owned()).is_none(),
            "HLS 属性为空或重复"
        );
        rest = next;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmp4_init_relative_and_quoted_urls_preserve_order_and_signatures() {
        let base = Url::parse("https://cdn.example/media/vod/list.m3u8?token=page").unwrap();
        let list = "#EXTM3U\n#EXT-X-MAP:URI=\"../init.mp4?token=a,b\"\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:2,one\nseg-1.m4s?sig=one\n#EXTINF:2,two\n/seg-2.m4s?sig=two\n#EXT-X-ENDLIST\n";
        let headers = vec![("Origin".into(), "https://example.com".into())];
        let streams = parse(&base, list, &headers).unwrap();
        assert_eq!(streams.len(), 3);
        assert_eq!(
            streams[0].url,
            "https://cdn.example/media/init.mp4?token=a,b"
        );
        assert_eq!(
            streams[1].url,
            "https://cdn.example/media/vod/seg-1.m4s?sig=one"
        );
        assert_eq!(streams[2].url, "https://cdn.example/seg-2.m4s?sig=two");
        assert_eq!(streams[1].headers, headers);
    }

    #[test]
    fn init_attributes_accept_horizontal_space_without_changing_signed_uri() {
        let base = Url::parse("https://cdn.example/list.m3u8").unwrap();
        let streams=parse(&base,"#EXTM3U\n#EXT-X-MAP: URI = \"init.mp4?sig=a%2Bb+raw\" \n#EXTINF:1,\na.m4s\n#EXT-X-ENDLIST\n",&[]).unwrap();
        assert!(streams[0].url.ends_with("?sig=a%2Bb+raw"));
    }

    #[test]
    fn rejects_encrypted_live_partial_master_and_resource_bombs() {
        let base = Url::parse("https://cdn.example/list.m3u8").unwrap();
        let tail = "#EXTINF:1,\na.ts\n#EXT-X-ENDLIST\n";
        for tag in [
            "#EXT-X-KEY:METHOD=AES-128,URI=\"key\"\n",
            "#EXT-X-KEY:METHOD=SAMPLE-AES,KEYFORMAT=\"com.apple.streamingkeydelivery\"\n",
            "#EXT-X-BYTERANGE:10@0\n",
            "#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"10@0\"\n",
            "#EXT-X-STREAM-INF:BANDWIDTH=10000\n",
            "#EXT-X-MEDIA:TYPE=AUDIO,URI=\"audio.m3u8\"\n",
            "#EXT-X-GAP\n",
            "#EXT-X-DISCONTINUITY\n",
            "#EXT-X-DISCONTINUITY-SEQUENCE:1\n",
        ] {
            assert!(
                parse(&base, &format!("#EXTM3U\n{tag}{tail}"), &[]).is_err(),
                "{tag}"
            );
        }
        assert!(parse(&base, "#EXTM3U\n#EXTINF:1,\na.ts\n", &[]).is_err());
        assert!(parse(
            &base,
            "#EXTM3U\n#EXTINF:1,\nfile:///tmp/a\n#EXT-X-ENDLIST",
            &[]
        )
        .is_err());
        let large = format!(
            "#EXTM3U\n{}#EXT-X-ENDLIST\n",
            "#EXTINF:1,\na.ts\n".repeat(MAX_STREAMS + 1)
        );
        assert!(parse(&base, &large, &[])
            .unwrap_err()
            .to_string()
            .contains("4096"));
    }

    #[test]
    fn rejects_credentials_and_malformed_origin_before_network() {
        for (name, value) in [
            ("Cookie", "private=value"),
            ("Authorization", "Bearer token"),
            ("Origin", "https://example.com/video"),
            ("Origin", "https://example.com/?private=token"),
            ("Referer", "https://user:secret@example.com/video"),
        ] {
            assert!(validate_headers(&[(name.into(), value.into())]).is_err());
        }
    }
}
