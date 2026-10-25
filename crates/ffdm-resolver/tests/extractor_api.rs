use anyhow::Result;
use async_trait::async_trait;
use ffdm_resolver::{
    Assembly, ExtractedFormat, ExtractedResource, MediaPlan, MediaStream, ResourceExtractor,
    ResourceResolver, StreamTransform, Url,
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
                            transform: None,
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

#[test]
fn old_saved_plans_remain_equal_and_suno_processing_is_scoped_to_its_song() {
    let song_id = "11111111-2222-4333-8444-555555555555";
    let saved = serde_json::json!({
        "source_url": format!("https://suno.com/song/{song_id}"),
        "source_id": format!("suno-{song_id}"),
        "platform": "Suno", "title": "Song", "format_id": "audio",
        "label": "M4A", "extension": "m4a", "assembly": "direct",
        "streams": [{"url": "https://cdn.invalid/audio.m4a", "headers": [],
            "size": 123, "decrypt_key": null}],
        "extracted_at": 1
    });
    let mut plan: MediaPlan = serde_json::from_value(saved.clone()).unwrap();
    // Resume compares serialized plans; the new optional field must not change
    // checkpoints written before stream transforms existed.
    assert!(plan.streams[0].transform.is_none());
    assert_eq!(serde_json::to_value(&plan).unwrap(), saved);
    plan.streams[0].transform = Some(StreamTransform::SunoAudio {
        content_id: song_id.into(),
    });
    assert!(plan.validate().is_ok());
    plan.source_id = "suno-different-song".into();
    assert!(plan.validate().is_err());
    plan.source_id = format!("suno-{song_id}");
    plan.platform = "Host".into();
    assert!(plan.validate().is_err());
    plan.platform = "Suno".into();
    let invalid_id = format!("{song_id}?other-song");
    plan.source_id = format!("suno-{invalid_id}");
    plan.streams[0].transform = Some(StreamTransform::SunoAudio {
        content_id: invalid_id,
    });
    assert!(plan.validate().is_err());
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
