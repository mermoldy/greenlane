//! On-disk recording format for captured timelines (`.glr` files), v2 (chunked).
//!
//! Layout:
//!
//! ```text
//!   8 bytes   magic  b"GREENLNE"
//!   postcard  Header { format_version, greenlane_version }
//!   postcard  Index  { pid, epoch_ms, gc, chunk_lens }
//!   bytes     chunk 0 (postcard Vec<Slice>)
//!   bytes     chunk 1 …
//! ```
//!
//! Slices are split into fixed-size chunks so [`ingest_file`] can stream the file
//! into the DB **one chunk at a time** — a multi-million-slice recording never has
//! to be fully decoded into memory at once (that decode, not the compact file, is
//! what used to OOM on open). `FORMAT_VERSION` gates compatibility: bump it on any
//! change to the serialized layout, so older binaries refuse to misread newer files.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::store::{GcEvent, Slice};

/// Fixed file signature — present in every `.glr`, independent of version.
const MAGIC: &[u8; 8] = b"GREENLNE";

/// On-disk data format version. **Bump on any change** to the serialized layout
/// of [`Header`], [`Index`], `Slice`, or `GcEvent`.
const FORMAT_VERSION: u16 = 2;

/// Slices per chunk — bounds the working set when streaming a file into the DB.
pub const CHUNK: usize = 16_384;

#[derive(Serialize, Deserialize)]
struct Header {
    format_version: u16,
    greenlane_version: String,
}

#[derive(Serialize, Deserialize)]
struct Index {
    pid: i32,
    epoch_ms: Option<u64>,
    gc: Vec<GcEvent>,
    /// Byte length of each slice chunk, in order; offsets are the running sum.
    chunk_lens: Vec<u32>,
}

/// Postcard-encode one slice chunk.
pub fn encode_chunk(slices: &[Slice]) -> Result<Vec<u8>> {
    postcard::to_allocvec(slices).context("encoding slice chunk")
}

/// Write a complete recording: magic + header + index + concatenated chunk bytes.
/// Returns the on-disk size. `chunk_data` is the chunks already encoded (compact),
/// `chunk_lens` their individual lengths — so the caller can stream-encode chunks
/// without ever holding all decoded slices in memory.
pub fn write_file(
    path: &Path,
    pid: i32,
    epoch_ms: Option<u64>,
    gc: Vec<GcEvent>,
    chunk_lens: Vec<u32>,
    chunk_data: &[u8],
) -> Result<u64> {
    let header = Header {
        format_version: FORMAT_VERSION,
        greenlane_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let index = Index {
        pid,
        epoch_ms,
        gc,
        chunk_lens,
    };
    let mut buf = Vec::with_capacity(MAGIC.len() + chunk_data.len() + 256);
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&postcard::to_allocvec(&header).context("encoding header")?);
    buf.extend_from_slice(&postcard::to_allocvec(&index).context("encoding index")?);
    buf.extend_from_slice(chunk_data);
    std::fs::write(path, &buf).with_context(|| format!("writing recording to {}", path.display()))?;
    Ok(buf.len() as u64)
}

/// Stream a `.glr` into `db`, decoding one chunk at a time (bounded memory).
/// Verifies magic + format version before touching the body. Returns the PID the
/// recording was captured from.
pub fn ingest_file(path: &Path, db: &Db) -> Result<i32> {
    let data = std::fs::read(path)
        .with_context(|| format!("reading recording from {}", path.display()))?;

    let body = data.strip_prefix(MAGIC.as_slice()).ok_or_else(|| {
        anyhow::anyhow!(
            "{} is not a greenlane recording (bad magic) — expected a .glr file written by `greenlane attach`",
            path.display()
        )
    })?;

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

    let (index, mut chunks): (Index, &[u8]) =
        postcard::take_from_bytes(rest).context("decoding recording index")?;

    let pid = index.pid;
    if let Some(ms) = index.epoch_ms {
        db.set_epoch(ms);
    }
    db.ingest_gc(index.gc);

    for len in index.chunk_lens {
        let len = len as usize;
        if chunks.len() < len {
            bail!("{}: truncated recording (chunk overruns file)", path.display());
        }
        let (head, tail) = chunks.split_at(len);
        let slices: Vec<Slice> =
            postcard::from_bytes(head).context("decoding slice chunk")?;
        db.ingest_slices(slices);
        chunks = tail;
    }
    Ok(pid)
}
