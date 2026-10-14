//! Minimal host: reusable extraction plus FFDownload's Rust executor.
use anyhow::{Context, Result};
use ffdownload::{
    download::{DownloadControl, DownloadSnapshot},
    media::find_ffmpeg,
    media_download,
    resolver::ResourceResolver,
};
use std::path::PathBuf;
use tokio::sync::watch;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .context("用法：resource_download <链接> <输出文件> [格式ID]")?;
    let output = PathBuf::from(args.next().context("请指定输出文件")?);
    let format_id = args.next();
    let ffmpeg = find_ffmpeg();
    let resource = ResourceResolver::with_ffmpeg(ffmpeg.clone())?
        .extract(&url)
        .await?;
    let format = resource
        .formats
        .into_iter()
        .find(|f| format_id.as_ref().is_none_or(|id| id == &f.plan.format_id))
        .context("没有找到所选格式，请先运行 resolver 的 resolve 示例列出格式")?;
    println!("{} · {}", resource.title, format.plan.label);
    let (_pause, pause) = watch::channel(false);
    let (progress, _) = watch::channel(DownloadSnapshot::default());
    let report = media_download::run(
        format.plan,
        output,
        4,
        None,
        DownloadControl { pause, progress },
        ffmpeg,
    )
    .await?;
    println!(
        "{} · {} bytes · SHA-256 {}",
        report.outcome,
        report.bytes,
        report.sha256.as_deref().unwrap_or("未完成")
    );
    Ok(())
}
