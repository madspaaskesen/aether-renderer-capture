# 🪼 aether-renderer-capture

> The HTML finger of Aether Renderer: turn any HTML/CSS/JS animation into a
> **deterministic, transparent PNG sequence** — ready for `aether-renderer-core`.

**Status:** early but working (v0.2.0), as a CLI and a library. Same input → bit-identical frames, every run,
on any machine speed.

## Install

```bash
cargo install aether-renderer-capture

# It drives chrome-headless-shell (BeginFrame doesn't exist in normal Chrome's --headless=new)
npx @puppeteer/browsers install chrome-headless-shell@stable
export AETHER_CHROME=/path/printed/above/chrome-headless-shell
```

On **Ubuntu 23.10+** Chrome's sandbox is blocked by AppArmor. Either allow it
(`sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`) or pass
`--no-sandbox` (trusted pages only).

FFmpeg is only needed to turn the frames into video (directly, or through
[`aether-renderer-core`](https://crates.io/crates/aether-renderer-core)).

## Usage

```bash
export AETHER_CHROME=/path/to/chrome-headless-shell

# 1. PNG folder or ZIP -> both are aether-renderer-core inputs
aether-renderer-capture -i anim.html -o frames --fps 30 -d 4 -s 1080x1080
aether-renderer-core --input frames --output anim.webm --fps 30 --format webm

aether-renderer-capture -i anim.html -o anim.zip --fps 30 -d 4 -s 1080x1080
aether-renderer-core --input anim.zip --output anim.webm --fps 30 --format webm

# 2. Stream straight into aether-renderer-core 0.7+ (no files on disk)
aether-renderer-capture -i anim.html --stdout --fps 30 -d 4 \
  | aether-renderer-core -i - -o anim.webm --fps 30 --frames 120

# ...or into plain ffmpeg
aether-renderer-capture -i anim.html --stdout --fps 30 -d 4 \
  | ffmpeg -f image2pipe -framerate 30 -c:v png -i - \
           -c:v libvpx-vp9 -pix_fmt yuva420p -b:v 0 -crf 30 anim.webm
```

| Flag | Default | |
|---|---|---|
| `-i, --input` | — | HTML file or URL |
| `-o, --out` | `frames` | Output folder, or a `.zip`, of `frame_00000.png` … |
| `--stdout` | off | Concatenated PNG stream on stdout |
| `--fps` | 30 | |
| `-d, --duration` | 2.0 | Seconds |
| `-s, --size` | 512x512 | `WIDTHxHEIGHT` |
| `--chrome` / `AETHER_CHROME` | — | Path to **chrome-headless-shell** |
| `--no-sandbox` / `AETHER_NO_SANDBOX` | off | Run Chrome without its sandbox (Ubuntu 23.10+, trusted pages only) |
| `--mode` | `auto` | `begin-frame`, `screenshot`, or `auto` (begin-frame on Linux, screenshot on macOS and Windows) |

Transparent pages (`html, body { background: transparent }`) give real alpha.

## As a library

```toml
[dependencies]
aether-renderer-capture = "0.2"
```

`capture` hands you each frame (async). `png_stream` runs a capture on its own
thread and gives you a `Read` of PNG files back to back, which is exactly what
`aether_renderer_core::render_stream` takes, so a capture goes straight into a
video with no files in between:

```rust
use aether_renderer_capture::{png_stream, CaptureOptions};

let opts = CaptureOptions::new("anim.html", "/path/to/chrome-headless-shell")
    .size(1080, 1080)
    .fps(30)
    .duration_secs(4.0);
let frames = opts.frame_count() as u32;

let (stream, capture) = png_stream(opts)?; // options are checked here
aether_renderer_core::render_stream(cfg, stream, Some(frames), |p| {
    println!("{:.0}%", p.percent);
})?;
capture.wait()?; // the capture's report, or why it failed
```

If the capture fails, the stream's next read returns the error, so the render
fails with it. Dropping the stream stops the capture and closes Chrome. Frames
are handed over through a small buffer, so a slow encoder slows the capture
down instead of piling frames up in memory.

```rust
// Or frame by frame, inside your own tokio runtime:
let report = aether_renderer_capture::capture(&opts, |index, png| {
    std::fs::write(format!("frame_{index:05}.png"), png)?;
    Ok(())
})
.await?;
```

## How it works

Every frame is stepped, never sampled from a running clock. There are two
ways to draw and grab a frame, and both give bit-identical output:

| Mode | Platforms | How a frame is drawn |
|---|---|---|
| `begin-frame` | Linux | Chrome only draws when we send `HeadlessExperimental.beginFrame`, which returns the PNG |
| `screenshot` | everywhere (default on macOS and Windows) | The frame-clock shim runs rAF callbacks itself, then `Page.captureScreenshot` |

`auto` picks `screenshot` on macOS (Chrome doesn't support BeginFrame control
there) and on Windows (BeginFrame returns no image for the first frame). The
steps below describe `begin-frame`; `screenshot` mode is the same minus the
BeginFrame flags.

1. **chrome-headless-shell** is launched with `--deterministic-mode`,
   `--enable-begin-frame-control`, `--run-all-compositor-stages-before-draw`.
   (BeginFrame only exists in the *old* headless — i.e. chrome-headless-shell,
   not `--headless=new`.)
2. The target is created with `enableBeginFrameControl: true`, so Chrome never
   draws on its own.
3. `Emulation.setVirtualTimePolicy` starts **paused**. During page load it
   advances in 1 µs steps with `pauseIfNetworkFetchesPending`, until
   `readyState === 'complete'` and fonts are loaded.
4. Per frame:
   - virtual time advances exactly one interval (`advance` + wait for
     `virtualTimeBudgetExpired`) → timers fire deterministically
   - `window.__aether.step(t)` pins the page clock to the exact frame time
   - `HeadlessExperimental.beginFrame` draws once and returns the PNG
   - no damage → no image from Chrome → previous frame is reused

### The frame-clock shim (why it's needed)

Even under virtual time, Chrome coarsens `performance.now()`, rAF timestamps
and the animation timeline to 100 µs **with random jitter** (Spectre
mitigation). That alone made ~10k pixels differ between runs. A tiny script
injected before any page code replaces them with one exact clock:

- `performance.now()`, rAF timestamps → exact frame time
- `Date` / `Date.now()` → fixed epoch (2023-11-14T22:13:20Z) + frame time
- CSS animations, transitions, WAAPI → paused and seeked to the frame time

### Gotchas learned the hard way

- BeginFrame control is not supported on macOS at all
  ("BeginFrameControl is not supported on MacOS yet"), hence `screenshot` mode.
- `frameTimeTicks` must be in Chrome's TimeTicks domain (host monotonic clock).
  Small values like `33.3` are silently ignored and rAF/CSS freeze.
- On Windows, `canonicalize()` returns `\\?\C:\...`, which isn't a valid
  `file://` URL. Build file URLs with `Url::from_file_path`.
- Ubuntu 23.10+ blocks the user namespaces Chrome's sandbox needs.
- With virtual time fully paused, the `load` event never fires.
- chromiumoxide adds the `--` itself: `.arg("enable-begin-frame-control")`.

## Verified

- 10/10 runs bit-identical (60 frames, sha256 of all PNGs), also under 4× CPU load
- 30 fps frame 15 == 60 fps frame 30 (same instant, bit-identical)
- `screenshot` mode produces the exact same frames as `begin-frame` mode
- macOS (Apple Silicon, Chrome 156): repeated runs bit-identical in `screenshot` mode
- CI captures twice on Linux, macOS and Windows and requires identical frames
- Alpha survives into VP9 WebM (`alpha_mode=1`)
- 512×512: ~1 s for 60 frames; 1920×1080: ~3.5 s (`begin-frame`) / ~6.5 s
  (`screenshot`) for 60 frames (software GL, Linux)

Determinism holds per machine and Chrome version. Across operating systems or
Chrome versions, fonts and anti-aliasing are rasterized slightly differently,
so frames look the same but are not byte-identical. Pin the
chrome-headless-shell version if you need identical output on several machines.

## Known limits

- `<video>`, `<audio>`, WebGL/Canvas driven by their own clocks, and Web
  Workers' `performance.now()` are not pinned.
- Pages measuring real durations with `performance.now()` will see 0 inside a frame.
- Timers run on virtual time, which lands ~10 ms ahead of the frame clock after
  the first step (harmless for drawing, but noted).

## Roadmap

Follows the capture plan that lives next to the other Aether repos.

- [x] Page mode: deterministic, transparent viewport capture (this PoC)
- [x] ZIP output, in the layout `aether-renderer-core` reads
- [x] Streaming into `aether-renderer-core` 0.7 (`--stdout | aether-renderer-core -i -`)
- [x] Library API: `capture` and `png_stream` (feeds `render_stream` directly)
- [ ] Canvas mode: auto-pick the largest `<canvas>`, grab exact pixels
      (`preserveDrawingBuffer` patch for WebGL)
- [ ] Start/stop handshake: `window.aether?.ready()` / `window.aether?.done()`,
      plus `--wait-for`, `--click`, `--start`
- [ ] Workers / OffscreenCanvas clocks
- [ ] Find or download chrome-headless-shell automatically
- [ ] `--scale` for retina output
- [ ] URL/HTML input in `aether-renderer-gui`

---

Made by Mads Paaskesen, [Hardcore Websmith](https://hardcorewebsmith.com). MIT licensed.
