//! aether-renderer-capture CLI: capture an HTML animation to a folder, a ZIP or
//! stdout. All the work happens in the library (`src/lib.rs`).

use std::path::PathBuf;

use aether_renderer_capture::sink::FrameSink;
use aether_renderer_capture::{CaptureOptions, Mode, capture};
use anyhow::{Result, anyhow};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "aether-renderer-capture",
    version,
    about = "Capture HTML animations as a deterministic, transparent PNG sequence"
)]
struct Args {
    /// HTML file (or http(s)/file URL) to capture
    #[arg(short, long)]
    input: String,

    /// Output: a folder, or a path ending in .zip, of frame_00000.png ...
    /// Both are inputs aether-renderer-core reads directly. Ignored with --stdout.
    #[arg(short, long, default_value = "frames")]
    out: PathBuf,

    /// Write a concatenated PNG stream to stdout (pipe into
    /// `aether-renderer-core -i -` or `ffmpeg -f image2pipe -i -`)
    #[arg(long)]
    stdout: bool,

    #[arg(long, default_value_t = 30)]
    fps: u32,

    /// Duration in seconds
    #[arg(short, long, default_value_t = 2.0)]
    duration: f64,

    /// Viewport size, WIDTHxHEIGHT
    #[arg(short, long, default_value = "512x512")]
    size: String,

    /// Path to chrome-headless-shell (or set AETHER_CHROME)
    #[arg(long, env = "AETHER_CHROME")]
    chrome: PathBuf,

    /// How frames are drawn and grabbed
    #[arg(long, value_enum, default_value_t = Mode::Auto)]
    mode: Mode,

    /// Run Chrome without its sandbox. Needed on Ubuntu 23.10+ (AppArmor blocks
    /// the user namespaces the sandbox uses). Only for pages you trust.
    #[arg(long, env = "AETHER_NO_SANDBOX", value_parser = clap::builder::BoolishValueParser::new())]
    no_sandbox: bool,
}

fn parse_size(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s
        .split_once('x')
        .ok_or_else(|| anyhow!("size must be WIDTHxHEIGHT"))?;
    Ok((w.parse()?, h.parse()?))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let (width, height) = parse_size(&args.size)?;
    let opts = CaptureOptions::new(args.input, args.chrome)
        .size(width, height)
        .fps(args.fps)
        .duration_secs(args.duration)
        .mode(args.mode)
        .no_sandbox(args.no_sandbox);
    opts.validate()?;

    let mut sink = FrameSink::open(&args.out, args.stdout)?;
    let pb = if sink.is_stdout() {
        indicatif::ProgressBar::hidden()
    } else {
        indicatif::ProgressBar::new(opts.frame_count())
    };

    let report = capture(&opts, |i, png| {
        sink.write(i, &png)?;
        pb.inc(1);
        Ok(())
    })
    .await?;
    pb.finish_and_clear();
    sink.finish()?;

    eprintln!(
        "captured {} frames @ {} fps ({}x{}, {:?} mode), {} reused (no damage)",
        report.frames, report.fps, report.width, report.height, report.mode, report.reused
    );
    Ok(())
}
