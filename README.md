# 🪼 aether-renderer-capture

> The HTML finger of Aether Renderer: turn any HTML/CSS/JS animation into a
> **deterministic, transparent PNG sequence** — ready for `aether-renderer-core`.

**Status:** proof of concept (v0.1.0). Same input → bit-identical frames, every run,
on any machine speed.

## Install

```bash
cargo install aether-renderer-capture

# It drives chrome-headless-shell (BeginFrame doesn't exist in normal Chrome's --headless=new)
npx @puppeteer/browsers install chrome-headless-shell@stable
export AETHER_CHROME=/path/printed/above/chrome-headless-shell
```

FFmpeg is only needed to turn the frames into video (directly, or through
[`aether-renderer-core`](https://crates.io/crates/aether-renderer-core)).

## Usage

```bash
export AETHER_CHROME=/path/to/chrome-headless-shell

# 1. PNG folder -> works with aether-renderer-core today, no changes needed
aether-renderer-capture -i anim.html -o frames --fps 30 -d 4 -s 1080x1080
aether-renderer-core --input frames --output anim.webm --fps 30 --format webm

# 2. Stream straight into ffmpeg (no files on disk)
aether-renderer-capture -i anim.html --stdout --fps 30 -d 4 \
  | ffmpeg -f image2pipe -framerate 30 -c:v png -i - \
           -c:v libvpx-vp9 -pix_fmt yuva420p -b:v 0 -crf 30 anim.webm
```

| Flag | Default | |
|---|---|---|
| `-i, --input` | — | HTML file or URL |
| `-o, --out` | `frames` | Output folder (`frame_00000.png` …) |
| `--stdout` | off | Concatenated PNG stream on stdout |
| `--fps` | 30 | |
| `-d, --duration` | 2.0 | Seconds |
| `-s, --size` | 512x512 | `WIDTHxHEIGHT` |
| `--chrome` / `AETHER_CHROME` | — | Path to **chrome-headless-shell** |
| `--mode` | `auto` | `begin-frame`, `screenshot`, or `auto` (screenshot on macOS, begin-frame elsewhere) |

Transparent pages (`html, body { background: transparent }`) give real alpha.

## How it works

Every frame is stepped, never sampled from a running clock. There are two
ways to draw and grab a frame, and both give bit-identical output:

| Mode | Platforms | How a frame is drawn |
|---|---|---|
| `begin-frame` | Linux, Windows | Chrome only draws when we send `HeadlessExperimental.beginFrame`, which returns the PNG |
| `screenshot` | everywhere (default on macOS) | The frame-clock shim runs rAF callbacks itself, then `Page.captureScreenshot` |

`auto` picks `screenshot` on macOS, because Chrome doesn't support BeginFrame
control there. The steps below describe `begin-frame`; `screenshot` mode is the
same minus the BeginFrame flags.

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
- With virtual time fully paused, the `load` event never fires.
- chromiumoxide adds the `--` itself: `.arg("enable-begin-frame-control")`.

## Verified

- 10/10 runs bit-identical (60 frames, sha256 of all PNGs), also under 4× CPU load
- 30 fps frame 15 == 60 fps frame 30 (same instant, bit-identical)
- `screenshot` mode produces the exact same frames as `begin-frame` mode
- Alpha survives into VP9 WebM (`alpha_mode=1`)
- 512×512: ~1 s for 60 frames; 1920×1080: ~3.5 s (`begin-frame`) / ~6.5 s
  (`screenshot`) for 60 frames (software GL, Linux)

## Known limits

- `<video>`, `<audio>`, WebGL/Canvas driven by their own clocks, and Web
  Workers' `performance.now()` are not pinned.
- Pages measuring real durations with `performance.now()` will see 0 inside a frame.
- Timers run on virtual time, which lands ~10 ms ahead of the frame clock after
  the first step (harmless for drawing, but noted).

## Roadmap

Follows the capture plan that lives next to the other Aether repos.

- [x] Page mode: deterministic, transparent viewport capture (this PoC)
- [ ] Streaming input in `aether-renderer-core` (`--input -`, raw RGBA) so
      capture encodes straight to WebM/MOV/GIF with no temp files
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
