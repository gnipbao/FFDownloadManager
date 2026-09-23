use anyhow::{ensure, Result};
use clap::{Parser, Subcommand};
use ffdownload::{
    benchmark::{default_report_path, local_benchmark, summarize},
    download::{download, DownloadRequest},
};
use std::{path::PathBuf, time::Duration};

#[derive(Parser)]
#[command(
    name = "ffdm",
    version,
    about = "Rust HTTP download MVP and benchmark runner"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the local browser UI (loopback only); Ctrl-C saves active downloads.
    Serve {
        #[arg(long, default_value_t = 17890)]
        port: u16,
        #[arg(long, default_value = "./downloads")]
        download_dir: PathBuf,
        #[arg(long, default_value = "./.ffdm-web")]
        state_dir: PathBuf,
    },
    /// Inspect a URL using the same Rust HTTP/TLS stack; no payload download.
    Probe { url: String },
    /// Parse a supported video page into available download formats.
    Extract { url: String },
    /// Download a URL; Ctrl-C checkpoints the session for `resume`.
    Download {
        url: String,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(short, long, default_value_t = 8)]
        connections: usize,
        #[arg(long)]
        sha256: Option<String>,
        #[arg(long)]
        pause_after: Option<f64>,
        #[arg(long, default_value_t = 900)]
        timeout: u64,
        #[arg(long)]
        quiet: bool,
    },
    /// Continue a saved session using the same output filename.
    Resume {
        output: PathBuf,
        #[arg(long, default_value_t = 900)]
        timeout: u64,
        #[arg(long)]
        quiet: bool,
    },
    /// Controlled localhost tests. Does not measure Internet bandwidth.
    Bench {
        #[arg(long, default_value_t = 64)]
        mib: usize,
        #[arg(long, default_value_t = 3)]
        rounds: usize,
        #[arg(long, value_delimiter = ',', default_value = "1,4,8")]
        connections: Vec<usize>,
        #[arg(long, default_value_os_t = default_report_path())]
        output: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "error".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    match Cli::parse().command {
        Command::Serve {
            port,
            download_dir,
            state_dir,
        } => {
            ffdownload::web::serve(port, download_dir, state_dir).await?;
        }
        Command::Probe { url } => {
            println!(
                "{}",
                serde_json::to_string(&ffdownload::download::probe(&url).await?)?
            );
        }
        Command::Extract { url } => {
            let resolver = ffdownload::media::MediaResolver::new()?;
            println!(
                "{}",
                serde_json::to_string_pretty(&resolver.resolve(&url).await?)?
            );
        }
        Command::Download {
            url,
            output,
            connections,
            sha256,
            pause_after,
            timeout,
            quiet,
        } => {
            if let Some(seconds) = pause_after {
                ensure!(
                    seconds.is_finite() && seconds > 0.0 && seconds < 86400.0,
                    "pause-after must be between 0 and 86400 seconds"
                );
            }
            let report = download(DownloadRequest {
                url: Some(url),
                output,
                connections,
                resume: false,
                expected_sha256: sha256,
                pause_after: pause_after.map(Duration::from_secs_f64),
                timeout: Duration::from_secs(timeout),
                interactive: !quiet,
            })
            .await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::Resume {
            output,
            timeout,
            quiet,
        } => {
            let report = download(DownloadRequest {
                url: None,
                output,
                connections: 8,
                resume: true,
                expected_sha256: None,
                pause_after: None,
                timeout: Duration::from_secs(timeout),
                interactive: !quiet,
            })
            .await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::Bench {
            mib,
            rounds,
            connections,
            output,
        } => {
            let report = local_benchmark(mib, rounds, connections, &output).await?;
            println!("{}\nRaw results: {}", summarize(&report), output.display());
        }
    }
    Ok(())
}
