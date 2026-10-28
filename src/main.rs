//! aether-renderer-capture — proof of concept
//!
//! Renders an HTML animation frame-by-frame in chrome-headless-shell with
//! virtual time and an injected frame clock, and writes a transparent PNG
//! sequence (or a PNG stream on stdout for `ffmpeg -f image2pipe`).
//!
//! Two capture modes:
//! - `begin-frame` (Linux/Windows): Chrome only draws when we issue
//!   `HeadlessExperimental.beginFrame`, which also returns the screenshot.
//! - `screenshot` (macOS, where BeginFrame control is unsupported): the frame
//!   clock shim runs rAF callbacks itself, then `Page.captureScreenshot`.

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
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, CaptureScreenshotParams,
};
use chromiumoxide::cdp::browser_protocol::target::CreateTargetParams;
use chromiumoxide::page::Page;
use clap::{Parser, ValueEnum};
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
///  - In screenshot mode (`__MANUAL_RAF__` = true) rAF callbacks are queued and
///    run exactly once per frame by `step()`, since Chrome's own frame
///    scheduling isn't under our control there.
const FRAME_CLOCK_SHIM: &str = r#"(() => {
  const manualRaf = __MANUAL_RAF__;
  let now = 0;
  const started = new WeakMap();
  performance.now = () => now;

  let rafQueue = new Map(), rafNextId = 1;
  if (manualRaf) {
    window.requestAnimationFrame = (cb) => { const id = rafNextId++; rafQueue.set(id, cb); return id; };
    window.cancelAnimationFrame = (id) => { rafQueue.delete(id); };
  } else {
    const raf = window.requestAnimationFrame.bind(window);
    window.requestAnimationFrame = (cb) => raf(() => cb(now));
  }

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
      if (manualRaf) {
        const due = rafQueue;
        rafQueue = new Map();
        for (const cb of due.values()) {
          try { cb(t); } catch (e) { console.error(e); }
        }
      }
      for (const a of document.getAnimations()) {
        if (!started.has(a)) started.set(a, t);
        a.pause();
        a.currentTime = t - started.get(a);
      }
    },
  }) });
})();"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Mode {
    /// begin-frame everywhere except macOS, which gets screenshot
    Auto,
    /// HeadlessExperimental.beginFrame (not available on macOS)
    BeginFrame,
    /// Shim-driven rAF + Page.captureScreenshot (works everywhere)
    Screenshot,
}

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
    let mode = match args.mode {
        Mode::Auto if cfg!(target_os = "macos") => Mode::Screenshot,
        Mode::Auto => Mode::BeginFrame,
        m => m,
    };
    let begin_frame = mode == Mode::BeginFrame;

    // --- 1. Launch chrome-headless-shell with deterministic flags -----------
    let mut builder = BrowserConfig::builder().chrome_executable(&args.chrome);
    // Chrome refuses to start sandboxed as root (typical in Docker/CI), so
    // only drop the sandbox there. Everyone else keeps it.
    if args.no_sandbox || running_as_root() {
        builder = builder.no_sandbox();
    }
    if begin_frame {
        builder = builder
            .arg("deterministic-mode")
            .arg("enable-begin-frame-control")
            .arg("run-all-compositor-stages-before-draw");
    }
    let config = builder
        .window_size(width, height)
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

    let (mut browser, mut handler) = match Browser::launch(config).await {
        Ok(v) => v,
        Err(e) if e.to_string().contains("No usable sandbox") => bail!(
            "Chrome's sandbox can't start on this system (Ubuntu 23.10+ restricts the user \
             namespaces it needs).\n\
             Either allow them:  sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0\n\
             or run without the sandbox (trusted pages only):  --no-sandbox  /  AETHER_NO_SANDBOX=1"
        ),
        Err(e) => return Err(anyhow::Error::new(e).context("launching chrome")),
    };
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    // --- 2. Target (with BeginFrame control in begin-frame mode) -------------
    let target = CreateTargetParams::builder()
        .url("about:blank")
        .enable_begin_frame_control(begin_frame)
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
    let shim =
        FRAME_CLOCK_SHIM.replace("__MANUAL_RAF__", if begin_frame { "false" } else { "true" });
    page.evaluate_on_new_document(shim).await?;

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
        let shot: Option<String> = if begin_frame {
            let ticks = base_ticks + i as f64 * interval_ms;
            page.execute(
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
            .await?
            .result
            .screenshot_data
            .map(|b| AsRef::<str>::as_ref(&b).to_string())
        } else {
            let res = page
                .execute(
                    CaptureScreenshotParams::builder()
                        .format(CaptureScreenshotFormat::Png)
                        .from_surface(true)
                        .build(),
                )
                .await?;
            Some(AsRef::<str>::as_ref(&res.result.data).to_string())
        };

        let png = match shot {
            Some(b64) => {
                let bytes = base64::engine::general_purpose::STANDARD.decode(b64)?;
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
        "captured {total_frames} frames @ {} fps ({width}x{height}, {mode:?} mode), {reused} reused (no damage)",
        args.fps
    );

    browser.close().await.ok();
    handler_task.abort();
    Ok(())
}

#[cfg(unix)]
fn running_as_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(unix))]
fn running_as_root() -> bool {
    false
}

/// Host monotonic clock in ms — the same clock Chrome's TimeTicks use
/// (CLOCK_MONOTONIC on Linux, mach_absolute_time on macOS, QPC on Windows).
#[cfg(unix)]
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

#[cfg(windows)]
fn monotonic_ms() -> f64 {
    use windows_sys::Win32::System::Performance::{
        QueryPerformanceCounter, QueryPerformanceFrequency,
    };
    let (mut count, mut freq) = (0i64, 0i64);
    // SAFETY: both calls write into valid, owned i64s and cannot fail on
    // Windows XP or later.
    unsafe {
        QueryPerformanceCounter(&mut count);
        QueryPerformanceFrequency(&mut freq);
    }
    count as f64 * 1000.0 / freq as f64
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
