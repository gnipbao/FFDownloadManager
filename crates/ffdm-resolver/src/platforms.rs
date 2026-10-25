//! Site adapters resolve public page metadata into the same Rust media contract.
mod audio_batch4;
mod audio_sites;
mod clip_sites;
mod domestic;
mod domestic_batch3;
mod domestic_batch4;
mod domestic_extra;
mod federated;
mod international;
mod open_media;
mod social_batch3;
mod suno;
mod video_batch4;

use crate::chinese_video::NativeVideo;
use anyhow::{bail, Result};
use reqwest_media::Url;

pub struct PlatformResolver {
    domestic: domestic::DomesticResolver,
    international: international::InternationalResolver,
    domestic_extra: domestic_extra::DomesticExtraResolver,
    open_media: open_media::OpenMediaResolver,
    federated: federated::FederatedResolver,
    clip_sites: clip_sites::ClipSitesResolver,
    social_batch3: social_batch3::SocialBatch3Resolver,
    audio_sites: audio_sites::AudioSitesResolver,
    domestic_batch3: domestic_batch3::DomesticBatch3Resolver,
    audio_batch4: audio_batch4::AudioBatch4Resolver,
    domestic_batch4: domestic_batch4::DomesticBatch4Resolver,
    video_batch4: video_batch4::VideoBatch4Resolver,
}

impl PlatformResolver {
    pub(crate) async fn resolve_suno(&self, url: &Url) -> Result<crate::generic::GenericResource> {
        suno::resolve(url).await
    }

    pub fn new() -> Result<Self> {
        Ok(Self {
            domestic: domestic::DomesticResolver::new()?,
            international: international::InternationalResolver::new()?,
            domestic_extra: domestic_extra::DomesticExtraResolver::new()?,
            open_media: open_media::OpenMediaResolver::new()?,
            federated: federated::FederatedResolver::new()?,
            clip_sites: clip_sites::ClipSitesResolver::new()?,
            social_batch3: social_batch3::SocialBatch3Resolver::new()?,
            audio_sites: audio_sites::AudioSitesResolver::new()?,
            domestic_batch3: domestic_batch3::DomesticBatch3Resolver::new()?,
            audio_batch4: audio_batch4::AudioBatch4Resolver::new()?,
            domestic_batch4: domestic_batch4::DomesticBatch4Resolver::new()?,
            video_batch4: video_batch4::VideoBatch4Resolver::new()?,
        })
    }

    pub async fn resolve(&self, url: &Url, platform: &str) -> Result<NativeVideo> {
        match platform {
            "Kuaishou" | "Weibo" | "AcFun" => self.domestic.resolve(url, platform).await,
            "Vimeo" | "Dailymotion" | "Streamable" | "Imgur" => {
                self.international.resolve(url, platform).await
            }
            _ => bail!("暂未接入这个平台的公开单视频解析"),
        }
    }

    pub async fn resolve_resource(
        &self,
        url: &Url,
        platform: &str,
    ) -> Result<crate::generic::GenericResource> {
        match platform {
            "CCTV" | "Ixigua" | "PearVideo" => self.domestic_extra.resolve(url, platform).await,
            "Archive.org" | "TED" | "WikimediaCommons" => {
                self.open_media.resolve(url, platform).await
            }
            "Bluesky" | "PeerTube" => self.federated.resolve(url, platform).await,
            "Medal" | "9GAG" => self.clip_sites.resolve(url, platform).await,
            "Mastodon" | "Tumblr" => self.social_batch3.resolve(url, platform).await,
            "Bandcamp" | "SoundCloud" => self.audio_sites.resolve(url, platform).await,
            "Sohu" | "Haokan" => self.domestic_batch3.resolve(url, platform).await,
            "Clyp" | "AudioBoom" | "Audius" => self.audio_batch4.resolve(url, platform).await,
            "HuyaVideo" | "NetEaseNews" => self.domestic_batch4.resolve(url, platform).await,
            "Rumble" | "Odysee" | "OK.ru" => self.video_batch4.resolve(url, platform).await,
            _ => bail!("暂未接入这个平台的资源解析"),
        }
    }
}

pub(crate) use social_batch3::{mastodon_post, tumblr_post};
pub(crate) use suno::is_song_page as suno_song_page;
pub(crate) use video_batch4::supports as video_batch4_page;
