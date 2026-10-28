# Changelog

All notable changes to this project are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - Unreleased

### Added
- Proof of concept: capture an HTML page as a deterministic, transparent PNG sequence
  using chrome-headless-shell with BeginFrame control and virtual time.
- Output to a folder of `frame_00000.png` files (works with `aether-renderer-core` as-is)
  or as a PNG stream on stdout for `ffmpeg -f image2pipe`.
- Frame-clock shim that pins `performance.now()`, rAF timestamps, `Date` and all
  CSS/WAAPI animations to the exact frame time, removing Chrome's timer jitter.
- `examples/orbit.html` demo (CSS animation, canvas + rAF, timers, Date).
