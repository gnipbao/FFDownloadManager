//! Run with `cargo run -p ffdm-resolver --example resolve -- <url>`.
use anyhow::{Context, Result};
use ffdm_resolver::ResourceResolver;

#[tokio::main]
async fn main() -> Result<()> {
    let url = std::env::args().nth(1).context("请传入资源链接")?;
    let resource = ResourceResolver::with_ffmpeg(None)?.extract(&url).await?;
    println!("{} · {}", resource.platform, resource.title);
    for format in resource.formats {
        println!(
            "{}\t{}\t{}",
            format.plan.format_id,
            format.plan.label,
            format.plan.filename()
        );
    }
    Ok(())
}
