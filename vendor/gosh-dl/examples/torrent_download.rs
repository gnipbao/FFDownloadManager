//! Torrent download example
//!
//! Loads a .torrent file and starts downloading.
//!
//! Usage: cargo run --example torrent_download --features torrent -- path/to/file.torrent

use gosh_dl::{DownloadEngine, DownloadOptions, EngineConfig};
use std::env;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let torrent_path = env::args()
        .nth(1)
        .expect("Usage: torrent_download <path-to-torrent-file>");

    let torrent_data = std::fs::read(&torrent_path)?;
    println!(
        "Loaded torrent: {torrent_path} ({} bytes)",
        torrent_data.len()
    );

    // Create engine
    let config = EngineConfig::default();
    let engine = DownloadEngine::new(config).await?;

    // Add torrent with sequential mode for streaming-friendly order
    let options = DownloadOptions::new().sequential(true);
    let id = engine.add_torrent(&torrent_data, options).await?;
    println!("Torrent added: {id}");

    // Print file listing if available
    if let Some(status) = engine.status(id) {
        if let Some(ref info) = status.torrent_info {
            println!("Files:");
            for file in &info.files {
                println!("  {} ({} bytes)", file.path.display(), file.size);
            }
        }
    }

    // Event loop
    let mut events = engine.subscribe();
    loop {
        match events.recv().await {
            Ok(event) => match &event {
                gosh_dl::DownloadEvent::Progress { id: eid, progress } if *eid == id => {
                    let pct = progress.total_size.map_or(0.0, |total| {
                        if total == 0 {
                            0.0
                        } else {
                            progress.completed_size as f64 / total as f64 * 100.0
                        }
                    });
                    println!(
                        "Progress: {pct:.1}% — {}/s down, {} peers",
                        format_bytes(progress.download_speed),
                        progress.peers,
                    );
                }
                gosh_dl::DownloadEvent::Completed { id: eid, .. } if *eid == id => {
                    println!("Download complete!");
                    break;
                }
                gosh_dl::DownloadEvent::Failed { id: eid, error, .. } if *eid == id => {
                    eprintln!("Error: {error}");
                    break;
                }
                _ => {}
            },
            Err(e) => {
                eprintln!("Event error: {e}");
                break;
            }
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
