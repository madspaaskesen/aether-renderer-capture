# Changelog

All notable changes to this project are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added
- ZIP output: `-o anim.zip` writes the frames as an uncompressed ZIP in the layout
  `aether-renderer-core` reads (`frame_00000.png` … at the top level, in frame order).
  A fixed timestamp makes the ZIP itself byte-identical between runs, and it is
  written as `anim.zip.partial` first, so a failed capture never leaves a ZIP that
  looks complete.

## [0.1.1] - 2026-10-28

### Added
- `--no-sandbox` / `AETHER_NO_SANDBOX=1` to run Chrome without its sandbox.

### Fixed
- On Ubuntu 23.10+ (AppArmor restricts unprivileged user namespaces) Chrome's sandbox
  can't start and 0.1.0 failed with a cryptic websocket error. It now explains the two
  ways out: allow user namespaces with `sysctl`, or pass `--no-sandbox`.
- Windows: local files were turned into an invalid `file://\\?\C:\...` URL, so the
  page never loaded. File paths now become proper file URLs (spaces and non-ASCII too).
- Windows: `--mode auto` now uses `screenshot` (BeginFrame returns no image for the
  first frame there). `begin-frame` stays the default on Linux only.
- Page loading has a 60 s wall-clock timeout, and a failed navigation reports the
  real reason (e.g. `net::ERR_...`) together with the URL.

## [0.1.0] - 2026-10-28

### Added
- Proof of concept: capture an HTML page as a deterministic, transparent PNG sequence
  using chrome-headless-shell with BeginFrame control and virtual time.
- Output to a folder of `frame_00000.png` files (works with `aether-renderer-core` as-is)
  or as a PNG stream on stdout for `ffmpeg -f image2pipe`.
- Frame-clock shim that pins `performance.now()`, rAF timestamps, `Date` and all
  CSS/WAAPI animations to the exact frame time, removing Chrome's timer jitter.
- `examples/orbit.html` demo (CSS animation, canvas + rAF, timers, Date).
- `--mode screenshot` (default on macOS, where Chrome has no BeginFrame control):
  the frame clock runs rAF callbacks itself and frames are grabbed with
  `Page.captureScreenshot`. Output is bit-identical to `--mode begin-frame`.
- Windows support: the BeginFrame clock uses QueryPerformanceCounter (the same
  clock as Chrome's TimeTicks) instead of `clock_gettime`, which Windows lacks.
- CI on Linux, macOS and Windows: captures the example twice and fails unless the
  frames are byte-identical; on Linux it also checks both modes agree.
- Release workflow that attaches binaries for Linux, macOS (arm64 + x86_64) and Windows.
- Chrome's sandbox stays on; it is only disabled when running as root (Docker/CI).
