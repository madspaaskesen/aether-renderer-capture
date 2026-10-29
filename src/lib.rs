//! Capture HTML/CSS/JS animations as a deterministic, transparent PNG sequence.
//!
//! Renders a page frame by frame in chrome-headless-shell with virtual time and
//! an injected frame clock, so every run gives byte-identical frames.
//!
//! Two capture modes:
//! - `begin-frame` (Linux): Chrome only draws when we issue
//!   `HeadlessExperimental.beginFrame`, which also returns the screenshot.
//! - `screenshot` (macOS and Windows, where BeginFrame is unsupported or
//!   unreliable): the frame clock shim runs rAF callbacks itself, then
//!   `Page.captureScreenshot`.
//!
//! # As a library
//!
//! [`capture`] calls back with each frame (async). [`png_stream`] runs a capture
//! on its own thread and returns a [`std::io::Read`] of PNG files back to back,
//! which is exactly what `aether_renderer_core::render_stream` reads:
//!
//! ```no_run
//! use aether_renderer_capture::{png_stream, CaptureOptions};
//!
//! let opts = CaptureOptions::new("anim.html", "/path/to/chrome-headless-shell")
//!     .size(1080, 1080)
//!     .fps(30)
//!     .duration_secs(4.0);
//! let frames = opts.frame_count();
//! let (stream, capture) = png_stream(opts)?;
//! // aether_renderer_core::render_stream(cfg, stream, Some(frames as u32), |p| { ... })?;
//! # drop(stream);
//! let report = capture.wait()?;
//! # Ok::<(), anyhow::Error>(())
//! ```

use std::io::Read;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
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
use futures::StreamExt;

pub mod sink;

/// Virtual-time step while the page loads. Tiny, so the clock the page sees at
/// frame 0 is effectively fixed no matter how many steps loading needs.
const WARMUP_STEP_MS: f64 = 0.001;

/// Real (wall-clock) time a page gets to load before we give up.
const LOAD_TIMEOUT: Duration = Duration::from_secs(60);

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
        // Screenshot mode waits for a new compositor frame, and Chrome draws
        // none when nothing on the page changed (the request then times out).
        // Change something invisible: black at 0.1% opacity rounds to nothing
        // in 8-bit pixels, so the frames stay identical to begin-frame mode.
        let tick = document.getElementById("__aether_tick");
        if (!tick) {
          tick = document.createElement("div");
          tick.id = "__aether_tick";
          tick.style.cssText = "position:fixed;right:0;bottom:0;width:1px;height:1px;" +
            "pointer-events:none;background:#000;z-index:2147483647";
          document.documentElement.appendChild(tick);
        }
        tick.style.opacity = tick.style.opacity === "0.001" ? "0.0011" : "0.001";
      }
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

/// How frames are drawn and grabbed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    /// begin-frame on Linux, screenshot on macOS and Windows
    Auto,
    /// HeadlessExperimental.beginFrame (Linux; unsupported on macOS)
    BeginFrame,
    /// Shim-driven rAF + Page.captureScreenshot (works everywhere)
    Screenshot,
}

impl Mode {
    /// The mode `Auto` stands for on this platform. BeginFrame is only
    /// dependable on Linux: unsupported on macOS, and on Windows the first
    /// frame comes back without a screenshot.
    pub fn resolve(self) -> Self {
        match self {
            Mode::Auto if cfg!(target_os = "linux") => Mode::BeginFrame,
            Mode::Auto => Mode::Screenshot,
            m => m,
        }
    }
}

/// What to capture and how.
#[derive(Clone, Debug)]
pub struct CaptureOptions {
    /// HTML file, or an http(s)/file URL.
    pub input: String,
    /// Path to chrome-headless-shell.
    pub chrome: PathBuf,
    /// Viewport width in CSS pixels.
    pub width: u32,
    /// Viewport height in CSS pixels.
    pub height: u32,
    /// Frames per second.
    pub fps: u32,
    /// Length of the capture in seconds.
    pub duration_secs: f64,
    /// How frames are drawn and grabbed.
    pub mode: Mode,
    /// Run Chrome without its sandbox (Ubuntu 23.10+; trusted pages only).
    /// Running as root always disables it, since Chrome refuses otherwise.
    pub no_sandbox: bool,
}

impl CaptureOptions {
    /// 512×512 at 30 fps for 2 seconds, `Mode::Auto`, sandbox on.
    pub fn new(input: impl Into<String>, chrome: impl Into<PathBuf>) -> Self {
        Self {
            input: input.into(),
            chrome: chrome.into(),
            width: 512,
            height: 512,
            fps: 30,
            duration_secs: 2.0,
            mode: Mode::Auto,
            no_sandbox: false,
        }
    }

    pub fn size(mut self, width: u32, height: u32) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    pub fn fps(mut self, fps: u32) -> Self {
        self.fps = fps;
        self
    }

    pub fn duration_secs(mut self, secs: f64) -> Self {
        self.duration_secs = secs;
        self
    }

    pub fn mode(mut self, mode: Mode) -> Self {
        self.mode = mode;
        self
    }

    pub fn no_sandbox(mut self, no_sandbox: bool) -> Self {
        self.no_sandbox = no_sandbox;
        self
    }

    /// How many frames the capture produces: `duration × fps`, rounded.
    pub fn frame_count(&self) -> u64 {
        (self.duration_secs * self.fps as f64).round() as u64
    }

    /// Catch bad options before a browser is started.
    pub fn validate(&self) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            bail!("size must be at least 1x1");
        }
        if self.fps == 0 {
            bail!("fps must be greater than 0");
        }
        if !self.duration_secs.is_finite() || self.frame_count() == 0 {
            bail!("duration must give at least one frame at {} fps", self.fps);
        }
        if !self.chrome.is_file() {
            bail!(
                "chrome-headless-shell not found at {} (install it with \
                 `npx @puppeteer/browsers install chrome-headless-shell@stable`)",
                self.chrome.display()
            );
        }
        to_url(&self.input)?;
        Ok(())
    }
}

/// What a finished capture did.
#[derive(Clone, Debug)]
pub struct CaptureReport {
    pub frames: u64,
    /// Frames where Chrome drew nothing new, so the previous frame was reused.
    pub reused: u64,
    /// The mode that actually ran (`Auto` resolved).
    pub mode: Mode,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// Capture `opts`, calling `on_frame(index, png)` for every frame in order.
///
/// If `on_frame` returns an error, the capture stops and returns it. Chrome is
/// closed in every case.
pub async fn capture<F>(opts: &CaptureOptions, on_frame: F) -> Result<CaptureReport>
where
    F: FnMut(u64, Vec<u8>) -> Result<()>,
{
    opts.validate()?;
    let mode = opts.mode.resolve();
    let begin_frame = mode == Mode::BeginFrame;
    let (width, height) = (opts.width, opts.height);

    // --- 1. Launch chrome-headless-shell with deterministic flags -----------
    let mut builder = BrowserConfig::builder().chrome_executable(&opts.chrome);
    // Chrome refuses to start sandboxed as root (typical in Docker/CI), so
    // only drop the sandbox there or when asked.
    if opts.no_sandbox || running_as_root() {
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

    let result = run(&browser, opts, mode, on_frame).await;

    browser.close().await.ok();
    browser.wait().await.ok();
    handler_task.abort();
    result
}

/// Everything after Chrome is up, so `capture` can always close it.
async fn run<F>(
    browser: &Browser,
    opts: &CaptureOptions,
    mode: Mode,
    mut on_frame: F,
) -> Result<CaptureReport>
where
    F: FnMut(u64, Vec<u8>) -> Result<()>,
{
    let begin_frame = mode == Mode::BeginFrame;
    let (width, height) = (opts.width, opts.height);
    let url = to_url(&opts.input)?;
    let interval_ms = 1000.0 / opts.fps as f64;
    let total_frames = opts.frame_count();

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
    let deadline = std::time::Instant::now() + LOAD_TIMEOUT;
    while std::time::Instant::now() < deadline {
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
        // Report why navigation failed (bad URL, net error) when we know.
        if nav.is_finished() {
            nav.await?.with_context(|| format!("loading {url}"))?;
        }
        bail!(
            "page did not finish loading (readyState/fonts) within {}s: {url}",
            LOAD_TIMEOUT.as_secs()
        );
    }
    nav.await?.with_context(|| format!("loading {url}"))?;
    if std::env::var("AETHER_DEBUG").is_ok() {
        eprintln!("page ready after {warmup_steps} warm-up steps");
    }
    // frameTimeTicks is in Chrome's TimeTicks domain (ms of host monotonic
    // uptime). Values below "now" are ignored by the renderer, which freezes
    // rAF and CSS animations. So anchor frame 0 at the current monotonic clock
    // and step exactly one interval per frame from there.
    let base_ticks = monotonic_ms();

    // --- 4. Capture loop -----------------------------------------------------
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
                last_png.clone().ok_or_else(|| {
                    anyhow!(
                        "Chrome returned no image for the first frame in begin-frame \
                         mode (known on Windows). Use --mode screenshot."
                    )
                })?
            }
        };

        on_frame(i, png)?;
    }

    Ok(CaptureReport {
        frames: total_frames,
        reused,
        mode,
        width,
        height,
        fps: opts.fps,
    })
}

/// Frames buffered between the capture thread and the reader. Small, so a slow
/// encoder slows the capture down instead of piling frames up in memory.
const STREAM_BUFFER_FRAMES: usize = 4;

/// Run a capture on its own thread and read it as a PNG stream (PNG files back
/// to back), e.g. for `aether_renderer_core::render_stream`.
///
/// Options are checked before anything starts. If the capture fails, the next
/// read returns that error. Dropping the stream stops the capture.
pub fn png_stream(opts: CaptureOptions) -> Result<(PngStream, CaptureHandle)> {
    opts.validate()?;
    let (tx, rx) = sync_channel::<std::result::Result<Vec<u8>, String>>(STREAM_BUFFER_FRAMES);
    let thread = std::thread::Builder::new()
        .name("aether-capture".into())
        .spawn(move || run_stream(opts, tx))
        .context("starting the capture thread")?;
    Ok((
        PngStream {
            rx,
            frame: Vec::new(),
            pos: 0,
            received: 0,
            on_frame: None,
        },
        CaptureHandle { thread },
    ))
}

fn run_stream(
    opts: CaptureOptions,
    tx: SyncSender<std::result::Result<Vec<u8>, String>>,
) -> Result<CaptureReport> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("starting the async runtime")?;
    let result = runtime.block_on(capture(&opts, |_, png| {
        tx.send(Ok(png))
            .map_err(|_| anyhow!("the PNG stream was closed before the capture finished"))
    }));
    if let Err(e) = &result {
        // Hand the reason to the reader (ignored if it has gone away).
        let _ = tx.send(Err(format!("{e:#}")));
    }
    result
}

/// PNG files back to back, produced by a capture running on another thread.
/// See [`png_stream`].
pub struct PngStream {
    rx: Receiver<std::result::Result<Vec<u8>, String>>,
    frame: Vec<u8>,
    pos: usize,
    received: u64,
    on_frame: Option<Box<dyn FnMut(u64) + Send>>,
}

impl PngStream {
    /// Call `f(n)` as frame `n` (1-based) of the capture reaches the reader,
    /// on the thread that reads the stream. Meant for progress bars: the
    /// capture is usually the slow part, and encoders report frames late.
    pub fn on_frame(mut self, f: impl FnMut(u64) + Send + 'static) -> Self {
        self.on_frame = Some(Box::new(f));
        self
    }
}

impl Read for PngStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.pos == self.frame.len() {
            match self.rx.recv() {
                Ok(Ok(frame)) => {
                    self.frame = frame;
                    self.pos = 0;
                    self.received += 1;
                    if let Some(f) = &mut self.on_frame {
                        f(self.received);
                    }
                }
                Ok(Err(e)) => return Err(std::io::Error::other(e)),
                // The capture thread is done and every frame has been read.
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.frame.len() - self.pos);
        buf[..n].copy_from_slice(&self.frame[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// The capture thread behind a [`PngStream`].
pub struct CaptureHandle {
    thread: std::thread::JoinHandle<Result<CaptureReport>>,
}

impl CaptureHandle {
    /// Wait for the capture to finish and return its report or error.
    pub fn wait(self) -> Result<CaptureReport> {
        self.thread
            .join()
            .map_err(|_| anyhow!("the capture thread panicked"))?
    }
}

pub(crate) fn to_url(input: &str) -> Result<String> {
    if input.contains("://") {
        return Ok(input.to_string());
    }
    let path = std::path::Path::new(input);
    if !path.is_file() {
        bail!("cannot find {input}");
    }
    // `path::absolute` (not `canonicalize`): on Windows canonicalize returns a
    // verbatim `\\?\C:\...` path, which is not a valid file:// URL. `Url`
    // does the per-platform escaping (drive letters, backslashes, spaces).
    let abs = std::path::absolute(path)?;
    let url = url::Url::from_file_path(&abs)
        .map_err(|_| anyhow!("cannot turn {} into a file:// URL", abs.display()))?;
    Ok(url.into())
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
