//! Progress display example
//!
//! Shows how to poll download status for a human-readable progress display.
//!
//! Usage: cargo run --example progress_display --features http -- [url1] [url2] ...

use gosh_dl::{DownloadEngine, DownloadOptions, DownloadState, EngineConfig};
use std::env;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let urls: Vec<String> = env::args().skip(1).collect();
    let urls = if urls.is_empty() {
        vec![
            "https://httpbin.org/bytes/4096".to_string(),
            "https://httpbin.org/bytes/8192".to_string(),
        ]
    } else {
        urls
    };

    let config = EngineConfig::default();
    let engine = DownloadEngine::new(config).await?;

    // Add all downloads
    let mut ids = Vec::new();
    for url in &urls {
        let id = engine.add_http(url, DownloadOptions::default()).await?;
        println!("Added: {id} — {url}");
        ids.push(id);
    }

    // Poll progress until all complete
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let mut all_done = true;
        println!("\n--- Status ---");

        for &id in &ids {
            if let Some(status) = engine.status(id) {
                let pct = status.progress.total_size.map_or_else(
                    || "??%".to_string(),
                    |total| {
                        if total == 0 {
                            "100%".to_string()
                        } else {
                            format!(
                                "{:.0}%",
                                status.progress.completed_size as f64 / total as f64 * 100.0
                            )
                        }
                    },
                );

                let eta = status
                    .progress
                    .eta_seconds
                    .map_or_else(|| "--".to_string(), |s| format!("{s}s"));

                println!(
                    "  {} | {:12} | {} | {}/s | ETA {}",
                    &id.to_string()[..8],
                    format!("{:?}", status.state),
                    pct,
                    format_bytes(status.progress.download_speed),
                    eta,
                );

                if !matches!(
                    status.state,
                    DownloadState::Completed | DownloadState::Error { .. }
                ) {
                    all_done = false;
                }
            }
        }

        // Global stats
        let stats = engine.global_stats();
        println!(
            "  Total: {} active, {}/s down, {}/s up",
            stats.num_active,
            format_bytes(stats.download_speed),
            format_bytes(stats.upload_speed),
        );

        if all_done {
            println!("\nAll downloads finished.");
            break;
        }
    }

    engine.shutdown().await?;
    Ok(())
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}
