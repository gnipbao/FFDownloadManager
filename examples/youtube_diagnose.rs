//! Inspect extraction/client failures without logging signed CDN URLs.
use std::{sync::Arc, time::Duration};
use ytdown::{Extractor, HttpClient, HttpRequest, HttpResponse};

struct Trace {
    inner: ytdown::ReqwestClient,
    only: Option<String>,
}

#[async_trait::async_trait]
impl HttpClient for Trace {
    async fn execute(&self, mut req: HttpRequest) -> ytdown::Result<HttpResponse> {
        let mut body: serde_json::Value = req
            .body
            .as_deref()
            .and_then(|b| serde_json::from_slice(b).ok())
            .unwrap_or_default();
        let mut client = body["context"]["client"]["clientName"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        let player = req.url.contains("/youtubei/v1/player");
        if player && client == "ANDROID_VR" && self.only.as_deref() == Some("VISIONOS") {
            let visitor = body["context"]["client"]["visitorData"].clone();
            let ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";
            body["context"]["client"] = serde_json::json!({
                "clientName":"VISIONOS", "clientVersion":"1.02", "deviceMake":"Apple",
                "deviceModel":"RealityDevice17,1", "userAgent":ua, "osName":"visionOS",
                "osVersion":"26.5.23O471", "hl":"en", "gl":"US", "visitorData":visitor
            });
            for (key, value) in &mut req.headers {
                if key.eq_ignore_ascii_case("user-agent") {
                    *value = ua.into();
                }
                if key.eq_ignore_ascii_case("x-youtube-client-name") {
                    *value = "101".into();
                }
                if key.eq_ignore_ascii_case("x-youtube-client-version") {
                    *value = "1.02".into();
                }
            }
            req.body = Some(serde_json::to_vec(&body).unwrap());
            client = "VISIONOS".into();
        }
        if player && self.only.as_ref().is_some_and(|c| c != &client) {
            return Err(ytdown::Error::Extraction {
                stage: "diagnostic",
                message: "skipped client".into(),
            });
        }
        let response = self.inner.execute(req).await?;
        if player {
            let json: serde_json::Value =
                serde_json::from_slice(&response.body).unwrap_or_default();
            eprintln!(
                "{}",
                serde_json::json!({"client":client,"http":response.status,"status":json["playabilityStatus"]["status"],"reason":json["playabilityStatus"]["reason"],"formats":json["streamingData"]["adaptiveFormats"].as_array().map(Vec::len)})
            );
        }
        Ok(response)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let http = reqwest_media::Client::builder()
        .use_native_tls()
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(15))
        .build()?;
    let ctx = ytdown::ExtractorContext::new(Arc::new(Trace {
        inner: ytdown::ReqwestClient::new(http.clone()),
        only: args.get(2).cloned(),
    }));
    let source = reqwest_media::Url::parse(args.get(1).expect("video page URL"))?;
    let result = ytdown::extractor::youtube::YoutubeExtractor::new()
        .extract(&ctx, &source)
        .await
        .map_err(|e| anyhow::anyhow!(ffdownload::media::safe_error(&e.to_string())))?;
    let ytdown::MediaInfo::Single(info) = result else {
        anyhow::bail!("not a single video");
    };
    for format in &info.formats {
        if !matches!(format.itag, Some(400 | 137 | 140 | 18)) {
            continue;
        }
        let parsed = reqwest_media::Url::parse(&format.url)?;
        let client = parsed
            .query_pairs()
            .find(|(k, _)| k == "c")
            .map(|(_, v)| v.into_owned());
        let tail = format.filesize.unwrap_or(1).saturating_sub(1);
        let head = http
            .head(&format.url)
            .send()
            .await
            .map(|r| r.status().as_u16())
            .unwrap_or(0);
        let response = http
            .get(&format.url)
            .header("Range", format!("bytes={tail}-{tail}"))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!(ffdownload::media::safe_error(&e.to_string())))?;
        println!(
            "{}",
            serde_json::json!({"itag":format.itag,"client":client,"size":format.filesize,"head":head,"tail":response.status().as_u16(),"content_range":response.headers().get("content-range").and_then(|s|s.to_str().ok())})
        );
    }
    Ok(())
}
