//! Library API tests. They need chrome-headless-shell: set AETHER_CHROME to its
//! path, or they are skipped. The core round trip also needs ffmpeg.

use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use aether_renderer_capture::{CaptureOptions, capture, png_stream};

fn chrome() -> Option<PathBuf> {
    std::env::var_os("AETHER_CHROME")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
}

macro_rules! require_chrome {
    () => {
        match chrome() {
            Some(c) => c,
            None => {
                eprintln!("skipping: set AETHER_CHROME to chrome-headless-shell");
                return;
            }
        }
    };
}

fn example() -> String {
    format!("{}/examples/orbit.html", env!("CARGO_MANIFEST_DIR"))
}

/// 15 frames: half a second at 30 fps.
fn opts(chrome: PathBuf) -> CaptureOptions {
    CaptureOptions::new(example(), chrome)
        .size(320, 240)
        .duration_secs(0.5)
}

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Split a PNG stream into files by walking the chunks up to each IEND.
fn split_pngs(mut bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while !bytes.is_empty() {
        assert_eq!(
            &bytes[..8],
            b"\x89PNG\r\n\x1a\n",
            "stream starts with a PNG"
        );
        let mut pos = 8;
        loop {
            let len = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
            let kind = &bytes[pos + 4..pos + 8];
            pos += 12 + len;
            if kind == b"IEND" {
                break;
            }
        }
        out.push(bytes[..pos].to_vec());
        bytes = &bytes[pos..];
    }
    out
}

#[tokio::test]
async fn capture_calls_back_once_per_frame_in_order() {
    let chrome = require_chrome!();
    let o = opts(chrome);
    let mut seen = Vec::new();
    let report = capture(&o, |i, png| {
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        seen.push(i);
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(report.frames, 15);
    assert_eq!(seen, (0..15).collect::<Vec<_>>());
    assert_eq!((report.width, report.height), (320, 240));
}

#[test]
fn png_stream_matches_capture_byte_for_byte() {
    let chrome = require_chrome!();
    let o = opts(chrome);

    let mut direct = Vec::new();
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(capture(&o, |_, png| {
            direct.push(png);
            Ok(())
        }))
        .unwrap();

    let (mut stream, handle) = png_stream(o).unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    assert_eq!(handle.wait().unwrap().frames, 15);
    assert_eq!(split_pngs(&bytes), direct, "same frames, same order");
}

#[test]
fn png_stream_rejects_bad_options_before_starting() {
    let chrome = require_chrome!();
    let missing = CaptureOptions::new("does/not/exist.html", chrome.clone());
    assert!(png_stream(missing).is_err());
    let no_chrome = CaptureOptions::new(example(), "/no/such/chrome");
    let err = png_stream(no_chrome).err().unwrap().to_string();
    assert!(err.contains("chrome-headless-shell"), "{err}");
    let no_frames = opts(chrome).duration_secs(0.0);
    assert!(png_stream(no_frames).is_err());
}

#[test]
fn capture_errors_reach_the_reader() {
    let chrome = require_chrome!();
    // Port 9 is one Chrome refuses outright, so navigation fails fast.
    let o = CaptureOptions::new("http://127.0.0.1:9/x.html", chrome).duration_secs(0.5);
    let (mut stream, handle) = png_stream(o).unwrap();
    let err = stream.read_to_end(&mut Vec::new()).unwrap_err();
    assert!(err.to_string().contains("ERR_"), "{err}");
    assert!(handle.wait().is_err());
}

#[test]
fn dropping_the_stream_stops_the_capture() {
    let chrome = require_chrome!();
    // Long enough that finishing it would take a while.
    let o = opts(chrome).duration_secs(30.0);
    let (mut stream, handle) = png_stream(o).unwrap();
    let mut first = [0u8; 8];
    stream.read_exact(&mut first).unwrap();
    assert_eq!(&first, b"\x89PNG\r\n\x1a\n");
    let started = Instant::now();
    drop(stream);
    let err = handle.wait().unwrap_err().to_string();
    assert!(err.contains("closed"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(20), "stopped early");
}

/// The path the GUI uses: capture straight into aether-renderer-core, no files.
#[test]
fn png_stream_into_core_render_stream() {
    let chrome = require_chrome!();
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.webm");
    let cfg = aether_renderer_core::config::PartialConfig {
        input: Some("-".into()),
        output: Some(out.to_string_lossy().into_owned()),
        ..Default::default()
    }
    .into_config()
    .unwrap();

    let o = opts(chrome);
    let frames = o.frame_count() as u32;
    let (stream, handle) = png_stream(o).unwrap();
    let mut last = None;
    let report =
        aether_renderer_core::render_stream(cfg, stream, Some(frames), |p| last = Some(p)).unwrap();
    handle.wait().unwrap();

    assert_eq!(report.frames_rendered, Some(15));
    assert_eq!(last.unwrap().percent, 100.0);
    let probe = Command::new("ffprobe")
        .args(["-v", "error", "-count_frames", "-show_entries"])
        .args(["stream=codec_name,width,height,nb_read_frames:stream_tags=alpha_mode"])
        .args(["-of", "csv=p=0"])
        .arg(&out)
        .output()
        .unwrap();
    let info = String::from_utf8_lossy(&probe.stdout);
    assert!(info.starts_with("vp9,320,240,15"), "{info}");
    assert!(info.trim_end().ends_with(",1"), "alpha kept: {info}");
}
