//! Basic HTTP download example
//!
//! Downloads a file from a URL with custom options.
//!
//! Usage: cargo run --example http_download --features http

use gosh_dl::{DownloadEngine, DownloadOptions, DownloadPriority, EngineConfig};
use std::env;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    let url = env::args()
        .nth(1)
        .unwrap_or_else(|| "https://httpbin.org/bytes/1024".to_string());

    println!("Downloading: {url}");

    // Create engine with custom config
    let config = EngineConfig::default().max_concurrent_downloads(4);
    let engine = DownloadEngine::new(config).await?;

    // Add download with builder-style options
    let options = DownloadOptions::new()
        .priority(DownloadPriority::High)
        .max_connections(8);

    let id = engine.add_http(&url, options).await?;
    println!("Download added: {id}");

    // Subscribe to events
    let mut events = engine.subscribe();
    loop {
        match events.recv().await {
            Ok(event) => {
                println!("{event:?}");
                if let gosh_dl::DownloadEvent::Completed { id: eid, .. } = &event {
                    if *eid == id {
                        break;
                    }
                }
                if let gosh_dl::DownloadEvent::Failed { id: eid, .. } = &event {
                    if *eid == id {
                        break;
                    }
                }
            }
            Err(e) => {
                eprintln!("Event error: {e}");
                break;
            }
        }
    }

    // Print final status
    if let Some(status) = engine.status(id) {
        println!(
            "Final: {} — {:?} ({} bytes)",
            status.metadata.name, status.state, status.progress.completed_size
        );
    }

    engine.shutdown().await?;
    Ok(())
}
