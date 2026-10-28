//! aether-renderer-capture — proof of concept
//!
//! Renders an HTML animation frame-by-frame in chrome-headless-shell using
//! deterministic BeginFrame control + virtual time, and writes a transparent
//! PNG sequence (or a PNG stream on stdout for `ffmpeg -f image2pipe`).

use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::dom::Rgba;
use chromiumoxide::cdp::browser_protocol::emulation::{
    EventVirtualTimeBudgetExpired, SetDefaultBackgroundColorOverrideParams,
    SetDeviceMetricsOverrideParams, SetVirtualTimePolicyParams, VirtualTimePolicy,
};
use chromiumoxide::cdp::browser_protocol::headless_experimental::{
    BeginFrameParams, ScreenshotParams, ScreenshotParamsFormat,
};
use chromiumoxide::cdp::browser_protocol::target::CreateTargetParams;
use chromiumoxide::page::Page;
use clap::Parser;
use futures::StreamExt;

/// Virtual-time step while the page loads. Tiny, so the clock the page sees at
/// frame 0 is effectively fixed no matter how many steps loading needs.
const WARMUP_STEP_MS: f64 = 0.001;

/// Injected before any page script runs. Chrome coarsens performance.now(),
/// rAF timestamps and the animation timeline to 100µs *with random jitter*
/// (Spectre mitigation), so even under virtual time two runs differ by a
/// sub-pixel. This shim gives the page one exact clock per frame instead:
///  - performance.now() and rAF timestamps = exact frame time (0 while loading)
///  - CSS animations / transitions / WAAPI are paused and seeked to the exact
///    frame time each frame, so the compositor never uses its own clock.
const FRAME_CLOCK_SHIM: &str = r#"(() => {
  let now = 0;
  const started = new WeakMap();
  const raf = window.requestAnimationFrame.bind(window);
  performance.now = () => now;
  window.requestAnimationFrame = (cb) => raf(() => cb(now));

  // Date / Date.now() follow the same frame clock from a fixed epoch.
  const RealDate = Date, EPOCH = 1700000000000;
  function FrameDate(...args) {
    const d = args.length ? new RealDate(...args) : new RealDate(EPOCH + Math.floor(now));
    return new.target ? d : d.toString();
  }
  FrameDate.prototype = RealDate.prototype;
  FrameDate.now = () => EPOCH + Math.floor(now);
  FrameDate.parse = RealDate.parse;
  FrameDate.UTC = RealDate.UTC;
  window.Date = FrameDate;
  Object.defineProperty(window, '__aether', { value: Object.freeze({
    step(t) {
      now = t;
      for (const a of document.getAnimations()) {
        if (!started.has(a)) started.set(a, t);
        a.pause();
        a.currentTime = t - started.get(a);
      }
    },
  }) });
})();"#;

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

    /// Output folder for frame_00000.png ... (ignored with --stdout)
    #[arg(short, long, default_value = "frames")]
    out: PathBuf,

    /// Write a concatenated PNG stream to stdout (pipe into `ffmpeg -f image2pipe -i -`)
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
}

fn parse_size(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s
        .split_once('x')
        .ok_or_else(|| anyhow!("size must be WIDTHxHEIGHT"))?;
    Ok((w.parse()?, h.parse()?))
}

fn to_url(input: &str) -> Result<String> {
    if input.contains("://") {
        return Ok(input.to_string());
    }
    let p = std::fs::canonicalize(input).with_context(|| format!("cannot find {input}"))?;
    Ok(format!("file://{}", p.display()))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let (width, height) = parse_size(&args.size)?;
    let url = to_url(&args.input)?;
    let interval_ms = 1000.0 / args.fps as f64;
    let total_frames = (args.duration * args.fps as f64).round() as u64;

    // --- 1. Launch chrome-headless-shell with deterministic flags -----------
    let config = BrowserConfig::builder()
        .chrome_executable(&args.chrome)
        .no_sandbox()
        .window_size(width, height)
        .arg("deterministic-mode")
        .arg("enable-begin-frame-control")
        .arg("run-all-compositor-stages-before-draw")
        .arg("disable-threaded-animation")
        .arg("disable-threaded-scrolling")
        .arg("disable-checker-imaging")
        .arg("disable-new-content-rendering-timeout")
        .arg("disable-gpu")
        .arg("hide-scrollbars")
        .arg("force-color-profile=srgb")
        .arg("font-render-hinting=none")
        .build()
        .map_err(|e| anyhow!(e))?;

    let (mut browser, mut handler) = Browser::launch(config).await.context("launching chrome")?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    // --- 2. Target with BeginFrame control ------------------------------------
    let target = CreateTargetParams::builder()
        .url("about:blank")
        .enable_begin_frame_control(true)
        .width(width as i64)
        .height(height as i64)
        .build()
        .map_err(|e| anyhow!(e))?;
    let page = browser.new_page(target).await.context("creating page")?;

    page.execute(
        SetDeviceMetricsOverrideParams::builder()
            .width(width as i64)
            .height(height as i64)
            .device_scale_factor(1.0)
            .mobile(false)
            .build()
            .map_err(|e| anyhow!(e))?,
    )
    .await?;

    // Transparent default background -> alpha in the PNGs
    page.execute(
        SetDefaultBackgroundColorOverrideParams::builder()
            .color(Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: Some(0.0),
            })
            .build(),
    )
    .await?;

    // Freeze virtual time before any page script runs. Virtual time drives
    // setTimeout/setInterval (and network-idle waiting); everything that is
    // drawn per frame is pinned to an exact frame clock by FRAME_CLOCK_SHIM.
    page.execute(
        SetVirtualTimePolicyParams::builder()
            .policy(VirtualTimePolicy::Pause)
            .initial_virtual_time(
                chromiumoxide::cdp::browser_protocol::network::TimeSinceEpoch::new(1_700_000_000.0),
            )
            .build()
            .map_err(|e| anyhow!(e))?,
    )
    .await?;

    // Give the page one exact, jitter-free clock (see FRAME_CLOCK_SHIM).
    page.evaluate_on_new_document(FRAME_CLOCK_SHIM).await?;

    // --- 3. Load the page ----------------------------------------------------
    // While virtual time is fully paused, the `load` task never runs. So during
    // loading we let virtual time move only when no network fetch is pending,
    // in tiny steps, and without issuing any BeginFrames (so rAF and CSS
    // animations haven't started yet when frame 0 is drawn).
    let mut budget_events = page
        .event_listener::<EventVirtualTimeBudgetExpired>()
        .await?;
    let nav = {
        let page = page.clone();
        let url = url.clone();
        tokio::spawn(async move { page.goto(url).await.map(|_| ()) })
    };
    let mut ready = false;
    let mut warmup_steps = 0u32;
    for _ in 0..2000 {
        if nav.is_finished() {
            ready = page
                .evaluate(
                    "document.readyState === 'complete' && document.fonts.status === 'loaded'",
                )
                .await
                .ok()
                .and_then(|v| v.into_value::<bool>().ok())
                .unwrap_or(false);
            if ready {
                break;
            }
        }
        warmup_steps += 1;
        advance_virtual_time(
            &page,
            &mut budget_events,
            VirtualTimePolicy::PauseIfNetworkFetchesPending,
            WARMUP_STEP_MS,
        )
        .await?;
    }
    if !ready {
        bail!("page did not finish loading (readyState/fonts) within the warm-up budget");
    }
    nav.await??;
    if std::env::var("AETHER_DEBUG").is_ok() {
        eprintln!("page ready after {warmup_steps} warm-up steps");
    }
    // frameTimeTicks is in Chrome's TimeTicks domain (ms of host monotonic
    // uptime). Values below "now" are ignored by the renderer, which freezes
    // rAF and CSS animations. So anchor frame 0 at the current monotonic clock
    // and step exactly one interval per frame from there.
    let base_ticks = monotonic_ms();

    // --- 4. Capture loop -----------------------------------------------------
    if !args.stdout {
        std::fs::create_dir_all(&args.out)?;
    }
    let pb = if args.stdout {
        indicatif::ProgressBar::hidden()
    } else {
        indicatif::ProgressBar::new(total_frames)
    };
    let mut stdout = std::io::stdout().lock();
    let mut last_png: Option<Vec<u8>> = None;
    let mut reused = 0u64;

    for i in 0..total_frames {
        // 4a. Advance virtual time by exactly one frame interval.
        if i > 0 {
            advance_virtual_time(
                &page,
                &mut budget_events,
                VirtualTimePolicy::Advance,
                interval_ms,
            )
            .await?;
        }

        // 4b. Pin the page clock + all CSS/WAAPI animations to this frame.
        let frame_ms = i as f64 * interval_ms;
        page.evaluate(format!(
            "window.__aether && window.__aether.step({frame_ms})"
        ))
        .await
        .context("stepping page clock")?;

        // 4c. Draw exactly one frame and grab it.
        let ticks = base_ticks + i as f64 * interval_ms;
        let res = page
            .execute(
                BeginFrameParams::builder()
                    .frame_time_ticks(ticks)
                    .interval(interval_ms)
                    .screenshot(
                        ScreenshotParams::builder()
                            .format(ScreenshotParamsFormat::Png)
                            .build(),
                    )
                    .build(),
            )
            .await?;

        let png = match res.result.screenshot_data {
            Some(b64) => {
                let bytes =
                    base64::engine::general_purpose::STANDARD.decode(AsRef::<str>::as_ref(&b64))?;
                last_png = Some(bytes.clone());
                bytes
            }
            // No damage => Chrome sends no image. Reuse the previous frame so
            // the sequence never has holes.
            None => {
                reused += 1;
                last_png
                    .clone()
                    .ok_or_else(|| anyhow!("first frame produced no screenshot"))?
            }
        };

        if args.stdout {
            stdout.write_all(&png)?;
        } else {
            std::fs::write(args.out.join(format!("frame_{i:05}.png")), &png)?;
        }
        pb.inc(1);
    }
    pb.finish_and_clear();
    stdout.flush()?;

    eprintln!(
        "captured {total_frames} frames @ {} fps ({width}x{height}), {reused} reused (no damage)",
        args.fps
    );

    browser.close().await.ok();
    handler_task.abort();
    Ok(())
}

/// Host monotonic clock in ms — the same clock Chrome's TimeTicks use
/// (CLOCK_MONOTONIC on Linux, mach_absolute_time on macOS).
fn monotonic_ms() -> f64 {
    #[cfg(target_os = "macos")]
    const CLOCK: libc::clockid_t = libc::CLOCK_UPTIME_RAW;
    #[cfg(not(target_os = "macos"))]
    const CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC;
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes into a valid, owned timespec.
    unsafe { libc::clock_gettime(CLOCK, &mut ts) };
    ts.tv_sec as f64 * 1000.0 + ts.tv_nsec as f64 / 1_000_000.0
}

/// Let virtual time run for `budget_ms` and block until Chrome reports the
/// budget as spent (Emulation.virtualTimeBudgetExpired). Virtual time then
/// pauses again by itself.
async fn advance_virtual_time(
    page: &Page,
    events: &mut chromiumoxide::listeners::EventStream<EventVirtualTimeBudgetExpired>,
    policy: VirtualTimePolicy,
    budget_ms: f64,
) -> Result<()> {
    page.execute(
        SetVirtualTimePolicyParams::builder()
            .policy(policy)
            .budget(budget_ms)
            .build()
            .map_err(|e| anyhow!(e))?,
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(30), events.next())
        .await
        .context("timed out waiting for virtualTimeBudgetExpired")?
        .ok_or_else(|| anyhow!("event stream closed"))?;
    Ok(())
}
