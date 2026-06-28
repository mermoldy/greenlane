//! On-disk recording format for captured timelines (`.glr` files).
//!
//! Layout:
//!
//! ```text
//!   8 bytes   magic  b"GREENLNE"
//!   postcard  Header { format_version, greenlane_version }
//!   postcard  Recording  (the columnar slice/GC timeline)
//! ```
//!
//! The header is decoded and verified *before* the (potentially large)
//! recording body, so an incompatible or non-greenlane file is rejected up
//! front with a clear message. `FORMAT_VERSION` gates compatibility: bump it
//! whenever the on-disk layout of [`Header`], [`Recording`], `Slice`, or
//! `GcEvent` changes, so older binaries refuse to misread newer files.
//!
//! `greenlane attach` (without `--serve`) writes one; `greenlane open` reads it
//! back into a [`Store`](crate::store::Store) and serves the unchanged web
//! viewer, so a recording opens exactly like a live session minus the streaming.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::store::Recording;

/// Fixed file signature — present in every `.glr`, independent of version.
const MAGIC: &[u8; 8] = b"GREENLNE";

/// On-disk data format version. **Bump this on any change to the serialized
/// layout** of [`Header`], [`Recording`], `Slice`, or `GcEvent`. A binary only
/// reads files whose `format_version` matches its own.
const FORMAT_VERSION: u16 = 1;

/// File header, postcard-encoded right after the magic.
#[derive(Serialize, Deserialize)]
struct Header {
    /// Data format version (see [`FORMAT_VERSION`]); gates compatibility.
    format_version: u16,
    /// greenlane crate version that wrote the file (informational, for errors).
    greenlane_version: String,
}

/// Serialize a recording to `path`: magic + header + recording body.
pub fn write(path: &Path, rec: &Recording) -> Result<()> {
    let header = Header {
        format_version: FORMAT_VERSION,
        greenlane_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let mut buf = Vec::new();
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&postcard::to_allocvec(&header).context("encoding recording header")?);
    buf.extend_from_slice(&postcard::to_allocvec(rec).context("encoding recording")?);
    std::fs::write(path, &buf)
        .with_context(|| format!("writing recording to {}", path.display()))?;
    Ok(())
}

/// Read and decode a recording from `path`, verifying the header first.
pub fn read(path: &Path) -> Result<Recording> {
    let data = std::fs::read(path)
        .with_context(|| format!("reading recording from {}", path.display()))?;

    let body = data.strip_prefix(MAGIC.as_slice()).ok_or_else(|| {
        anyhow::anyhow!(
            "{} is not a greenlane recording (bad magic) — expected a .glr file written by `greenlane attach`",
            path.display()
        )
    })?;

    // Decode + verify the header before touching the (large) recording body.
    let (header, rest): (Header, &[u8]) =
        postcard::take_from_bytes(body).context("decoding recording header")?;
    if header.format_version != FORMAT_VERSION {
        bail!(
            "{}: unsupported .glr format version {} (this build reads v{}). \
             The file was written by greenlane {}; use a matching greenlane version.",
            path.display(),
            header.format_version,
            FORMAT_VERSION,
            header.greenlane_version,
        );
    }

    postcard::from_bytes(rest).context("decoding recording")
}
