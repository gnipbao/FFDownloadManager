//! Rust resource extraction independent of FFDownload's UI, queue and task database.
//!
//! [`ResourceResolver::extract`] returns private download plans for a host's own
//! executor. [`ResourceResolver::resolve`] provides a redacted preview with cached
//! format selection for interactive clients. Ordinary file transfers remain the
//! responsibility of a downloader, not a website extractor.
mod browser;
mod chinese_video;
mod extractor;
mod generic;
pub mod hls;
mod manifest;
pub mod media;
mod platforms;
mod qishui;
mod resource;
pub mod routing;
pub mod source_policy;
pub mod suno_audio;
pub mod utils;
pub mod validation;
mod youtube;

pub use extractor::ResourceExtractor;
pub use media::{
    Assembly, ExtractedFormat, ExtractedResource, MediaFormat, MediaPlan, MediaPreview,
    MediaResolver as ResourceResolver, MediaStream, StreamTransform,
};
pub use reqwest::Url;
