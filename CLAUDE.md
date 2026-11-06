# aether-renderer-capture

Rust library + CLI: renders an HTML/CSS/JS animation to a deterministic, transparent PNG
sequence with chrome-headless-shell (frame by frame on an exact clock). Published on crates.io;
the GUI and web2vid-backend use it with `aether-renderer-core` (`png_stream()` →
`render_stream()`). How it works and the gotchas: README and `aether-renderer-capture.md` in the
parent folder / claude.ai project.

## Commands

```bash
cargo build
cargo test --release              # library tests need AETHER_CHROME (else skipped); the core round trip needs ffmpeg
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo run -- -i examples/orbit.html -o frames/
cargo run -- -i examples/orbit.html --stdout | aether-renderer-core -i - -o out.webm
```

CI (`.github/workflows/ci.yml`) runs fmt + clippy, then build and tests on Ubuntu, macOS and
Windows.

## Layout

| File | Role |
|---|---|
| `src/lib.rs` | `CaptureOptions`, `capture()`, `png_stream()`, the frame clock shim (`FRAME_CLOCK_SHIM`, injected with `evaluate_on_new_document`) |
| `src/main.rs` | CLI (clap) |
| `src/sink.rs` | Writes frames to a folder or stdout |
| `tests/library.rs` | Library API tests (need Chrome) |
| `examples/` | Runnable example pages |

## Branches (release branch flow)

Never commit straight to `main`: it only changes when a version is released.

- When work on a new version starts: `git switch main && git pull && git switch -c release/x.y.z`.
- Work is committed straight on `release/x.y.z` by default, so the history reads in one line.
- A separate branch only when it helps, with the version in the name: an experiment that may be
  thrown away (`try/x.y.z-<topic>`), something risky, or a fix for an already released version
  (`fix/x.y.z-<topic>` from `main`, released as a patch). Merge it back into the release branch.
- To release: version + CHANGELOG on the release branch, PR `release/x.y.z` → `main`, merge,
  then publish and tag (see Releasing).
- Before the first commit of a session: `git branch --show-current`. On `main`? Switch to the
  release branch first.

## Releasing

1. Bump `version` in `Cargo.toml`, move CHANGELOG `[Unreleased]` to `[x.y.z]` with the date,
   update the README.
2. PR `release/x.y.z` → `main`, wait for green CI, merge.
3. `cargo publish` from `main` by hand, then tag `vX.Y.Z` and publish the GitHub release
   (`release.yml` builds the binaries). Then bump the dependency in the GUI and web2vid-backend.
   Full steps (signing etc.): `release-checklist.md` in the parent folder / claude.ai project.

## Rules

- `cargo fmt` and `cargo clippy --all-targets -- -D warnings` must pass.
- Pages are loaded over http(s) or `file://`; a `file://` iframe inside a page hangs Chrome for the
  30 s load timeout (found in web2vid-backend, which serves pages over http instead).
- Keep output deterministic: anything the page reads that changes between runs (time, random,
  data) must be fixed before the page loads.
