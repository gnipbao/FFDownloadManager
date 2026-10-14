//! Host-provided adapters use the same download-plan contract as built-in sites.
use crate::ExtractedResource;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Url;

/// Register an adapter without changing FFDownload's UI or built-in parser code.
///
/// Implementations own their network requests and authentication. Return only
/// formats for the requested resource; private stream addresses are not UI data.
#[async_trait]
pub trait ResourceExtractor: Send + Sync {
    fn supports(&self, url: &Url) -> bool;
    async fn extract(&self, url: &Url) -> Result<ExtractedResource>;
}
