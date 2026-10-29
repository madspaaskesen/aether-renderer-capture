//! Where captured frames go: a folder, a ZIP, or stdout.
//!
//! Folder and ZIP use the layout `aether-renderer-core` reads as input:
//! `frame_00000.png`, `frame_00001.png`, … at the top level, in frame order.

use std::fs::File;
use std::io::{BufWriter, StdoutLock, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipWriter};

pub enum FrameSink {
    Dir(PathBuf),
    Zip {
        writer: Box<ZipWriter<BufWriter<File>>>,
        partial: PathBuf,
        dest: PathBuf,
    },
    Stdout(StdoutLock<'static>),
}

impl FrameSink {
    /// `--stdout` wins; otherwise a path ending in `.zip` becomes a ZIP and
    /// anything else a folder.
    pub fn open(out: &Path, stdout: bool) -> Result<Self> {
        if stdout {
            return Ok(Self::Stdout(std::io::stdout().lock()));
        }
        if is_zip(out) {
            if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)?;
            }
            // Written next to the target and renamed on success, so a failed
            // capture never leaves a half-written ZIP that looks complete.
            let mut partial = out.as_os_str().to_owned();
            partial.push(".partial");
            let partial = PathBuf::from(partial);
            let file = File::create(&partial)
                .with_context(|| format!("creating {}", partial.display()))?;
            return Ok(Self::Zip {
                writer: Box::new(ZipWriter::new(BufWriter::new(file))),
                partial,
                dest: out.to_path_buf(),
            });
        }
        std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
        Ok(Self::Dir(out.to_path_buf()))
    }

    pub fn write(&mut self, index: u64, png: &[u8]) -> Result<()> {
        match self {
            Self::Dir(dir) => std::fs::write(dir.join(frame_name(index)), png)?,
            Self::Zip { writer, .. } => {
                // PNG is already compressed, so deflate would only cost time.
                // A fixed timestamp keeps the ZIP itself byte-identical per run.
                let options = SimpleFileOptions::default()
                    .compression_method(CompressionMethod::Stored)
                    .last_modified_time(DateTime::default());
                writer.start_file(frame_name(index), options)?;
                writer.write_all(png)?;
            }
            Self::Stdout(out) => out.write_all(png)?,
        }
        Ok(())
    }

    /// Flush and, for a ZIP, write the central directory and move it into place.
    pub fn finish(self) -> Result<()> {
        match self {
            Self::Dir(_) => {}
            Self::Zip {
                writer,
                partial,
                dest,
            } => {
                writer.finish()?.flush()?;
                std::fs::rename(&partial, &dest)
                    .with_context(|| format!("moving {} into place", dest.display()))?;
            }
            Self::Stdout(mut out) => out.flush()?,
        }
        Ok(())
    }

    pub fn is_stdout(&self) -> bool {
        matches!(self, Self::Stdout(_))
    }
}

fn is_zip(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
}

fn frame_name(index: u64) -> String {
    format!("frame_{index:05}.png")
}
