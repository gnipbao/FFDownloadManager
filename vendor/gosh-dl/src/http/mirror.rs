//! Mirror URL management and failover logic
//!
//! Supports multiple URLs for the same file with automatic failover
//! when one URL fails. Can distribute segments across mirrors for
//! parallel downloads.

use parking_lot::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Mirror URL manager with failover support
pub struct MirrorManager {
    /// All URLs (primary + mirrors)
    urls: Vec<String>,
    /// Current primary URL index
    current_index: AtomicUsize,
    /// Failed URL indices
    failed: RwLock<Vec<usize>>,
    /// Failure counts per URL
    failure_counts: RwLock<Vec<u32>>,
    /// Max failures before marking URL as dead
    max_failures: u32,
}

impl MirrorManager {
    /// Create a new mirror manager with a primary URL and optional mirror URLs
    pub fn new(primary_url: String, mirrors: Vec<String>) -> Self {
        let mut urls = vec![primary_url];
        urls.extend(mirrors);
        let len = urls.len();

        Self {
            urls,
            current_index: AtomicUsize::new(0),
            failed: RwLock::new(Vec::new()),
            failure_counts: RwLock::new(vec![0; len]),
            max_failures: 3,
        }
    }

    /// Create from a single URL (no mirrors)
    pub fn single(url: String) -> Self {
        Self::new(url, Vec::new())
    }

    /// Get current active URL
    pub fn current_url(&self) -> &str {
        let idx = self.current_index.load(Ordering::Relaxed);
        &self.urls[idx]
    }

    /// Get URL for a specific segment (round-robin for parallel downloads)
    pub fn url_for_segment(&self, segment_idx: usize) -> &str {
        let available = self.available_urls();
        if available.is_empty() {
            return self.current_url(); // Fallback
        }
        let idx = segment_idx % available.len();
        available[idx]
    }

    /// Get list of available (non-failed) URLs
    pub fn available_urls(&self) -> Vec<&str> {
        let failed = self.failed.read();
        self.urls
            .iter()
            .enumerate()
            .filter(|(i, _)| !failed.contains(i))
            .map(|(_, url)| url.as_str())
            .collect()
    }

    /// Check if any URLs are still available
    pub fn has_available(&self) -> bool {
        self.failed.read().len() < self.urls.len()
    }

    /// Get the number of available URLs
    pub fn available_count(&self) -> usize {
        self.urls.len() - self.failed.read().len()
    }

    /// Report failure for current URL, switch to next available
    /// Returns the new URL if switch was successful, None if all URLs failed
    pub fn report_failure(&self) -> Option<&str> {
        let current = self.current_index.load(Ordering::Relaxed);

        // Increment failure count
        {
            let mut counts = self.failure_counts.write();
            counts[current] += 1;

            if counts[current] >= self.max_failures {
                let mut failed = self.failed.write();
                if !failed.contains(&current) {
                    failed.push(current);
                    tracing::warn!(
                        "Mirror {} marked as failed after {} failures",
                        &self.urls[current],
                        self.max_failures
                    );
                }
            }
        }

        // Find next available URL
        self.switch_to_next()
    }

    /// Report a specific URL as failed
    pub fn report_url_failure(&self, url: &str) -> Option<&str> {
        if let Some(idx) = self.urls.iter().position(|u| u == url) {
            let mut counts = self.failure_counts.write();
            counts[idx] += 1;

            if counts[idx] >= self.max_failures {
                let mut failed = self.failed.write();
                if !failed.contains(&idx) {
                    failed.push(idx);
                    tracing::warn!(
                        "Mirror {} marked as failed after {} failures",
                        url,
                        self.max_failures
                    );
                }
            }

            // Switch if the current URL was the one that failed
            if self.current_index.load(Ordering::Relaxed) == idx {
                return self.switch_to_next();
            }
        }
        Some(self.current_url())
    }

    /// Immediately fail over away from a specific URL.
    pub fn failover_from(&self, url: &str) -> Option<&str> {
        let idx = self.urls.iter().position(|candidate| candidate == url)?;
        {
            let mut failed = self.failed.write();
            if !failed.contains(&idx) {
                failed.push(idx);
            }
        }

        if self.current_index.load(Ordering::Relaxed) == idx {
            self.switch_to_next()
        } else {
            Some(self.current_url())
        }
    }

    /// Switch to next available URL
    fn switch_to_next(&self) -> Option<&str> {
        let failed = self.failed.read();
        let current = self.current_index.load(Ordering::Relaxed);

        for offset in 1..self.urls.len() {
            let idx = (current + offset) % self.urls.len();
            if !failed.contains(&idx) {
                self.current_index.store(idx, Ordering::Relaxed);
                tracing::info!("Switched to mirror: {}", &self.urls[idx]);
                return Some(&self.urls[idx]);
            }
        }
        None // All URLs failed
    }

    /// Report success for current URL (reset failure count)
    pub fn report_success(&self) {
        let current = self.current_index.load(Ordering::Relaxed);
        self.failure_counts.write()[current] = 0;
    }

    /// Reset all failure counts and failed URLs
    pub fn reset(&self) {
        *self.failed.write() = Vec::new();
        *self.failure_counts.write() = vec![0; self.urls.len()];
        self.current_index.store(0, Ordering::Relaxed);
    }

    /// Get all URLs (for display/logging)
    pub fn all_urls(&self) -> &[String] {
        &self.urls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_url() {
        let mgr = MirrorManager::single("http://example.com/file.zip".to_string());
        assert_eq!(mgr.current_url(), "http://example.com/file.zip");
        assert_eq!(mgr.available_count(), 1);
    }

    #[test]
    fn test_multiple_mirrors() {
        let mgr = MirrorManager::new(
            "http://primary.com/file.zip".to_string(),
            vec![
                "http://mirror1.com/file.zip".to_string(),
                "http://mirror2.com/file.zip".to_string(),
            ],
        );
        assert_eq!(mgr.current_url(), "http://primary.com/file.zip");
        assert_eq!(mgr.available_count(), 3);
    }

    #[test]
    fn test_failover() {
        let mgr = MirrorManager::new(
            "http://primary.com/file.zip".to_string(),
            vec!["http://mirror.com/file.zip".to_string()],
        );

        // Fail the primary 3 times explicitly (using report_url_failure to target specific URL)
        for _ in 0..3 {
            mgr.report_url_failure("http://primary.com/file.zip");
        }

        // Should now use the mirror since primary is marked as failed
        assert_eq!(mgr.current_url(), "http://mirror.com/file.zip");
        assert_eq!(mgr.available_count(), 1);
    }

    #[test]
    fn test_round_robin_segments() {
        let mgr = MirrorManager::new(
            "http://primary.com/file.zip".to_string(),
            vec![
                "http://mirror1.com/file.zip".to_string(),
                "http://mirror2.com/file.zip".to_string(),
            ],
        );

        // Segments should round-robin across mirrors
        let url0 = mgr.url_for_segment(0);
        let url1 = mgr.url_for_segment(1);
        let url2 = mgr.url_for_segment(2);
        let url3 = mgr.url_for_segment(3);

        assert_eq!(url0, "http://primary.com/file.zip");
        assert_eq!(url1, "http://mirror1.com/file.zip");
        assert_eq!(url2, "http://mirror2.com/file.zip");
        assert_eq!(url3, "http://primary.com/file.zip"); // Wraps around
    }

    #[test]
    fn test_all_failed() {
        let mgr = MirrorManager::single("http://example.com/file.zip".to_string());

        // Fail it 3 times
        for _ in 0..3 {
            mgr.report_failure();
        }

        // No more URLs available
        assert!(!mgr.has_available());
        assert_eq!(mgr.available_count(), 0);
    }

    #[test]
    fn test_reset() {
        let mgr = MirrorManager::new(
            "http://primary.com/file.zip".to_string(),
            vec!["http://mirror.com/file.zip".to_string()],
        );

        // Fail everything
        for _ in 0..6 {
            mgr.report_failure();
        }
        assert!(!mgr.has_available());

        // Reset
        mgr.reset();
        assert!(mgr.has_available());
        assert_eq!(mgr.available_count(), 2);
        assert_eq!(mgr.current_url(), "http://primary.com/file.zip");
    }

    #[test]
    fn test_immediate_failover_switches_sources() {
        let mgr = MirrorManager::new(
            "http://primary.com/file.zip".to_string(),
            vec!["http://mirror.com/file.zip".to_string()],
        );

        let next = mgr.failover_from("http://primary.com/file.zip");
        assert_eq!(next, Some("http://mirror.com/file.zip"));
        assert_eq!(mgr.current_url(), "http://mirror.com/file.zip");
    }
}
