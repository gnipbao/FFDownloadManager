use anyhow::Result;
use async_trait::async_trait;
use ffdm_resolver::{
    Assembly, ExtractedFormat, ExtractedResource, MediaPlan, MediaStream, ResourceExtractor,
    ResourceResolver, Url,
};

struct HostExtractor;
#[async_trait]
impl ResourceExtractor for HostExtractor {
    fn supports(&self, url: &Url) -> bool {
        url.host_str() == Some("host-adapter.invalid")
    }
    async fn extract(&self, url: &Url) -> Result<ExtractedResource> {
        Ok(ExtractedResource {
            source_url: url.to_string(),
            title: "Host video".into(),
            platform: "Host".into(),
            duration_seconds: Some(2),
            formats: [360, 1080, 1080]
                .into_iter()
                .map(|height| ExtractedFormat {
                    height,
                    audio_only: false,
                    plan: MediaPlan {
                        source_url: url.to_string(),
                        source_id: "item".into(),
                        platform: "Host".into(),
                        title: "Host video".into(),
                        format_id: height.to_string(),
                        label: format!("{height}p"),
                        extension: "mp4".into(),
                        assembly: Assembly::Direct,
                        streams: vec![MediaStream {
                            url: format!("https://cdn.invalid/file-{height}?secret=private"),
                            headers: vec![(
                                "Referer".into(),
                                "https://host-adapter.invalid/".into(),
                            )],
                            size: Some(123),
                            decrypt_key: None,
                        }],
                        extracted_at: 1,
                    },
                })
                .collect(),
        })
    }
}

#[tokio::test]
async fn a_host_adapter_uses_the_public_contract_without_app_or_network_dependencies() {
    let resolver = ResourceResolver::with_ffmpeg(None)
        .unwrap()
        .with_extractor(HostExtractor);
    let raw = resolver
        .extract("https://host-adapter.invalid/item")
        .await
        .unwrap();
    assert_eq!(raw.formats.len(), 2);
    assert_eq!(raw.formats[0].height, 1080);
    let preview = resolver
        .resolve("https://host-adapter.invalid/item")
        .await
        .unwrap();
    let display = serde_json::to_string(&preview).unwrap();
    assert!(!display.contains("private"));
    assert!(!display.contains("Referer"));
    assert_eq!(
        resolver.select(&preview.id, "1080").unwrap().streams.len(),
        1
    );
    assert!(resolver
        .extract_with_cookie("https://host-adapter.invalid/item", Some("cookie=private"))
        .await
        .is_err());
}
