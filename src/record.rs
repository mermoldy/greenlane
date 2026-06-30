//! On-disk recording format for captured timelines (`.glr` files).
//!
//! A `.glr` is a sequence of **sealed, DEFLATE-compressed segments** (R7) over the
//! framed binary trace format (see [`crate::trace_format`] and ADR 0001) — the same
//! encoding used on the live wire. Each segment is a self-contained trace stream
//! (its own schemas, `execution`/`gc` events, and — in the first segment — a `meta`
//! frame); identical funcs/stacks are interned within a segment, so a recording is
//! far smaller than the old tab-delimited dump, and compression shrinks it further.
//!
//! [`SegmentWriter`] appends one sealed segment per periodic flush and fsyncs it, so
//! a hard kill loses at most the not-yet-sealed tail — every prior segment stays
//! intact. [`ingest_file`] streams a recording back into the DB one segment at a
//! time (bounded memory). The container header (`b"GLRS"` + version) selects the
//! format; a pre-R7 single uncompressed stream (`b"GLR\0"`) is still read, and the
//! original text/postcard format (`b"GREENLNE"`) is detected and rejected clearly.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use tracing::warn;

use crate::db::Db;
use crate::store::{Execution, GcEvent, SysSample};
use crate::trace_format::{Decoder, Encoder, Item, Step};

/// Executions buffered before an ingest batch when reading a file back.
pub const CHUNK: usize = 16_384;

/// Upper bound on a single segment's compressed/raw size the reader will accept.
/// Real per-flush segments are tiny; this exists only so a corrupt length field
/// can't make the reader allocate/buffer toward a bogus size (OOM).
const MAX_SEGMENT_LEN: usize = 512 << 20; // 512 MiB
/// Soft cap the writer keeps each segment's RAW size under (half the reader cap,
/// leaving headroom for the execution that trips it). A larger delta is split into
/// several segments so the writer never produces one the reader would reject.
const SEG_SOFT_RAW: usize = MAX_SEGMENT_LEN / 2; // 256 MiB

/// Container magic for the **segmented** `.glr` (R7): an append-only sequence of
/// independently DEFLATE-compressed segments, each a self-contained trace stream.
const SEG_MAGIC: &[u8; 4] = b"GLRS";
/// Container version (bumped independently of the inner trace-format VERSION).
const SEG_VERSION: u8 = 1;
/// Pre-R7 single uncompressed trace stream (every `Encoder` stream starts with it).
/// Still read for back-compat with recordings made between R1 and R7.
const STREAM_MAGIC: &[u8; 4] = b"GLR\0";
/// Legacy signature: the original text/postcard `.glr`. Recognized only to give a
/// clear "re-record" error instead of an opaque binary decode failure.
const LEGACY_MAGIC: &[u8; 8] = b"GREENLNE";

/// I/O read buffer for streaming a recording back in.
const READ_BUF: usize = 1 << 16;

// ── Compression ───────────────────────────────────────────────────────────────

/// DEFLATE a buffer (pure-Rust miniz_oxide backend; no C dep, musl-clean).
fn deflate(raw: &[u8]) -> Result<Vec<u8>> {
    use flate2::{Compression, write::DeflateEncoder};
    let mut e = DeflateEncoder::new(Vec::new(), Compression::default());
    e.write_all(raw).context("deflating segment")?;
    e.finish().context("finishing deflate")
}

/// Inverse of [`deflate`].
fn inflate(comp: &[u8]) -> Result<Vec<u8>> {
    use flate2::read::DeflateDecoder;
    let mut out = Vec::new();
    DeflateDecoder::new(comp)
        .read_to_end(&mut out)
        .context("inflating segment")?;
    Ok(out)
}

// ── Writer ──────────────────────────────────────────────────────────────────

/// Append-only writer for a **segmented, compressed** `.glr` (R7).
///
/// The container header is written once on [`SegmentWriter::create`]; thereafter
/// every [`SegmentWriter::seal_segment`] encodes its batch as a self-contained
/// trace stream, DEFLATEs it, appends `[u32 raw_len][u32 comp_len][bytes]`, and
/// **fsyncs** — so each segment is durable the instant the call returns. Because
/// segments are only ever *appended* (never the whole-file rewrite the old writer
/// did on every flush), a hard kill mid-recording loses at most the not-yet-sealed
/// tail: every prior sealed segment stays intact and readable. Each segment is also
/// independently decodable (it carries its own schemas), so the reader handles one
/// at a time and a multi-GB recording never lands in memory at once.
pub struct SegmentWriter {
    file: BufWriter<File>,
    written: u64,
    /// The `meta` frame is written once, in the first sealed segment.
    meta_done: bool,
}

impl SegmentWriter {
    /// Create (truncating) the recording and write + fsync the container header.
    pub fn create(path: &Path) -> Result<Self> {
        let mut file = BufWriter::new(
            File::create(path).with_context(|| format!("creating recording {}", path.display()))?,
        );
        file.write_all(SEG_MAGIC).context("writing .glr header")?;
        file.write_all(&[SEG_VERSION])
            .context("writing .glr header")?;
        file.flush().ok();
        file.get_ref().sync_all().ok();
        Ok(SegmentWriter {
            file,
            written: (SEG_MAGIC.len() + 1) as u64,
            meta_done: false,
        })
    }

    /// Seal the new executions + GC since the last seal. The first sealed segment also
    /// carries the `meta` frame. A fully-empty seal (no meta, no data) is skipped so
    /// idle flushes don't grow the file. Returns once the bytes are fsynced — every
    /// written segment is then crash-durable.
    ///
    /// A large delta is split into multiple sub-segments, each kept under the
    /// reader's `MAX_SEGMENT_LEN` by encoded size, so the writer can never produce a
    /// segment the reader would refuse to open (the first sub-segment carries the
    /// meta + GC).
    pub fn seal_segment(
        &mut self,
        executions: &[Execution],
        gc: &[GcEvent],
        samples: &[SysSample],
        pid: i32,
        epoch_ms: Option<u64>,
    ) -> Result<()> {
        let include_meta = !self.meta_done;
        if !include_meta && executions.is_empty() && gc.is_empty() && samples.is_empty() {
            return Ok(());
        }

        let mut idx = 0;
        let mut wrote_first = false;
        // Seal while executions remain — and at least once if there's meta/GC/samples to
        // record even with no executions.
        while idx < executions.len()
            || (!wrote_first && (include_meta || !gc.is_empty() || !samples.is_empty()))
        {
            // A self-contained trace stream: header + schemas (+ meta/GC/samples on the
            // first sub-segment) + as many executions as fit under the soft size cap.
            let mut enc = Encoder::new();
            enc.write_file_schemas();
            if !wrote_first {
                if include_meta {
                    // tid is wire-only (live lag sampling); recordings carry 0;
                    // epoch 0 = unknown.
                    enc.meta(epoch_ms.unwrap_or(0), 0, pid as i64, "");
                }
                for g in gc {
                    enc.gc(g);
                }
                for s in samples {
                    enc.sample(s);
                }
            }
            while idx < executions.len() {
                enc.execution(&executions[idx]);
                idx += 1;
                if enc.bytes().len() >= SEG_SOFT_RAW {
                    break; // start a fresh sub-segment for the rest
                }
            }
            self.seal_raw(enc.bytes())?;
            wrote_first = true;
        }
        self.meta_done = true;
        Ok(())
    }

    /// Deflate one self-contained stream and append `[u32 raw_len][u32 comp_len]
    /// [bytes]`, then fsync. `raw` is bounded by `SEG_SOFT_RAW` (+ one execution), so it
    /// always fits the u32 fields and the reader's `MAX_SEGMENT_LEN`.
    fn seal_raw(&mut self, raw: &[u8]) -> Result<()> {
        let comp = deflate(raw)?;
        let raw_len = u32::try_from(raw.len())
            .with_context(|| format!("segment too large to encode ({} bytes)", raw.len()))?;
        let comp_len = u32::try_from(comp.len())
            .with_context(|| format!("compressed segment too large ({} bytes)", comp.len()))?;
        self.file
            .write_all(&raw_len.to_le_bytes())
            .context("writing segment header")?;
        self.file
            .write_all(&comp_len.to_le_bytes())
            .context("writing segment header")?;
        self.file.write_all(&comp).context("writing segment body")?;
        self.file.flush().context("flushing recording")?;
        self.file.get_ref().sync_all().context("fsync recording")?;
        self.written += 8 + comp.len() as u64;
        Ok(())
    }

    /// On-disk size so far, in bytes.
    pub fn size(&self) -> u64 {
        self.written
    }
}

// ── Reader ──────────────────────────────────────────────────────────────────

/// Accumulates decoded executions/GC and flushes them into the DB in CHUNK batches.
struct Ingest<'a> {
    db: &'a Db,
    pid: i32,
    executions: Vec<Execution>,
    gc: Vec<GcEvent>,
    samples: Vec<SysSample>,
}

impl<'a> Ingest<'a> {
    fn new(db: &'a Db) -> Self {
        Ingest {
            db,
            pid: 0,
            executions: Vec::with_capacity(CHUNK),
            gc: Vec::new(),
            samples: Vec::new(),
        }
    }

    fn item(&mut self, item: Option<Item>) {
        match item {
            Some(Item::Meta(m)) => {
                self.pid = m.pid as i32;
                if m.epoch_ms > 0 {
                    self.db.set_epoch(m.epoch_ms);
                }
            }
            Some(Item::Execution(s)) => {
                self.executions.push(s);
                if self.executions.len() >= CHUNK {
                    self.db
                        .ingest_executions(std::mem::take(&mut self.executions));
                }
            }
            Some(Item::Gc(g)) => self.gc.push(g),
            Some(Item::Sample(s)) => self.samples.push(s),
            // `switch` is wire-only; a file shouldn't contain one — ignore if it does.
            Some(Item::Switch(_)) | None => {}
        }
    }

    fn finish(mut self) -> i32 {
        if !self.executions.is_empty() {
            self.db
                .ingest_executions(std::mem::take(&mut self.executions));
        }
        if !self.gc.is_empty() {
            self.db.ingest_gc(std::mem::take(&mut self.gc));
        }
        for s in self.samples.drain(..) {
            self.db.ingest_sample(s);
        }
        self.pid
    }
}

/// Stream a `.glr` into `db`. Sniffs the container format from the first bytes and
/// dispatches: segmented+compressed (`GLRS`, the R7 default), a pre-R7 single
/// uncompressed stream (`GLR\0`), or the rejected legacy text format. Returns the
/// PID the recording was captured from.
pub fn ingest_file(path: &Path, db: &Db) -> Result<i32> {
    let mut file =
        File::open(path).with_context(|| format!("opening recording {}", path.display()))?;
    let mut sig = [0u8; 8];
    let n = read_up_to(&mut file, &mut sig)
        .with_context(|| format!("reading recording {}", path.display()))?;

    if n >= LEGACY_MAGIC.len() && &sig == LEGACY_MAGIC {
        bail!(
            "{}: this is a legacy text-format .glr from an older greenlane. The recording \
             format is now binary (ADR 0001) and the old files can't be read — re-record \
             with this version.",
            path.display()
        );
    }
    if n >= 5 && &sig[..4] == SEG_MAGIC {
        if sig[4] != SEG_VERSION {
            bail!(
                "{}: unsupported .glr container version {} (expected {})",
                path.display(),
                sig[4],
                SEG_VERSION
            );
        }
        let mut reader = BufReader::new(file);
        return read_segments(sig[5..n].to_vec(), &mut reader, db, path);
    }
    if n >= 4 && &sig[..4] == STREAM_MAGIC {
        // Pre-R7 single uncompressed stream: feed the bytes already read, then the rest.
        let mut reader = BufReader::new(file);
        return read_stream(&sig[..n], &mut reader, db, path);
    }
    bail!(
        "{}: bad magic (not a greenlane .glr recording)",
        path.display()
    );
}

/// Read up to `buf.len()` bytes, returning how many were filled (fewer = short file).
fn read_up_to<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<usize> {
    let mut off = 0;
    while off < buf.len() {
        let k = r.read(&mut buf[off..])?;
        if k == 0 {
            break;
        }
        off += k;
    }
    Ok(off)
}

/// Ensure `buf` holds at least `need` bytes, reading more from `r`. Returns `false`
/// on EOF before `need` (with whatever was read left in `buf`).
fn fill<R: Read>(buf: &mut Vec<u8>, r: &mut R, need: usize, tmp: &mut [u8]) -> Result<bool> {
    while buf.len() < need {
        let k = r.read(tmp)?;
        if k == 0 {
            return Ok(false);
        }
        buf.extend_from_slice(&tmp[..k]);
    }
    Ok(true)
}

/// Read the `GLRS` segment container: each segment is `[u32 raw_len][u32 comp_len]
/// [deflated bytes]`, inflated and decoded as a self-contained trace stream.
fn read_segments<R: Read>(carry: Vec<u8>, reader: &mut R, db: &Db, path: &Path) -> Result<i32> {
    let mut buf = carry;
    let mut tmp = vec![0u8; READ_BUF];
    let mut ing = Ingest::new(db);
    loop {
        // Segment header, or a clean EOF on a segment boundary. A non-empty but
        // short buffer here means the process was killed mid-write of the next
        // segment's header: every prior segment was fsynced, so honor the R7
        // durability promise — warn and stop, keeping what we decoded.
        if !fill(&mut buf, reader, 8, &mut tmp)
            .with_context(|| format!("reading recording {}", path.display()))?
        {
            if !buf.is_empty() {
                warn!(
                    file = %path.display(),
                    bytes = buf.len(),
                    "recording ends mid-segment-header (likely a hard kill); \
                     dropping the unsealed tail"
                );
            }
            break;
        }
        let raw_len = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        let comp_len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
        // Guard against a corrupt length making us buffer toward a bogus size.
        if raw_len > MAX_SEGMENT_LEN || comp_len > MAX_SEGMENT_LEN {
            bail!(
                "{}: corrupt segment length (raw {raw_len}, comp {comp_len}; cap {MAX_SEGMENT_LEN})",
                path.display()
            );
        }
        if !fill(&mut buf, reader, 8 + comp_len, &mut tmp)
            .with_context(|| format!("reading recording {}", path.display()))?
        {
            // The final segment is incomplete — the not-yet-fully-written/fsynced
            // tail after a hard kill. Prior segments are intact; drop the tail.
            warn!(
                file = %path.display(),
                "recording ends mid-segment (likely a hard kill); dropping the unsealed tail"
            );
            break;
        }
        // Decode the segment. A failure here means the bytes are corrupt despite a
        // plausible length header — e.g. a hard kill that left a torn (but
        // length-prefixed) final segment, or genuine on-disk corruption. We can
        // only tell the two apart by what follows: a corrupt segment with nothing
        // after it is the unsealed tail (honor the durability promise: keep prior
        // segments), whereas corruption with more segments behind it is real and
        // must fail loudly.
        let outcome = (|| -> Result<()> {
            let raw = inflate(&buf[8..8 + comp_len])
                .with_context(|| format!("decompressing segment in {}", path.display()))?;
            if raw.len() != raw_len {
                bail!(
                    "{}: segment size mismatch (header {raw_len}, inflated {})",
                    path.display(),
                    raw.len()
                );
            }
            decode_segment(&raw, &mut ing, path)
        })();
        buf.drain(0..8 + comp_len);
        if let Err(e) = outcome {
            let more = fill(&mut buf, reader, 1, &mut tmp)
                .with_context(|| format!("reading recording {}", path.display()))?;
            if !more && buf.is_empty() {
                warn!(
                    file = %path.display(),
                    error = %e,
                    "final segment is corrupt (likely a hard kill); \
                     dropping the unsealed tail"
                );
                break;
            }
            return Err(e).with_context(|| {
                format!("corrupt segment in {} (not the final one)", path.display())
            });
        }
    }
    Ok(ing.finish())
}

/// Decode one fully-buffered, self-contained segment stream into `ing`.
fn decode_segment(raw: &[u8], ing: &mut Ingest, path: &Path) -> Result<()> {
    let mut dec = Decoder::new();
    let mut off = 0;
    while off < raw.len() {
        match dec
            .step(&raw[off..])
            .with_context(|| format!("decoding segment in {}", path.display()))?
        {
            Step::NeedMore => {
                bail!("{}: corrupt segment (incomplete frame)", path.display());
            }
            Step::Done { item, consumed } => {
                if consumed == 0 {
                    bail!("{}: corrupt segment (zero-length frame)", path.display());
                }
                off += consumed;
                ing.item(item);
            }
        }
    }
    Ok(())
}

/// Read a pre-R7 single uncompressed `GLR\0` stream, decoding a frame at a time over
/// a sliding buffer so a large legacy recording never lands in memory at once.
fn read_stream<R: Read>(prefix: &[u8], reader: &mut R, db: &Db, path: &Path) -> Result<i32> {
    let mut dec = Decoder::new();
    let mut buf: Vec<u8> = prefix.to_vec();
    let mut tmp = vec![0u8; READ_BUF];
    let mut ing = Ingest::new(db);
    loop {
        let mut consumed = 0usize;
        loop {
            match dec
                .step(&buf[consumed..])
                .with_context(|| format!("decoding recording {}", path.display()))?
            {
                Step::NeedMore => break,
                Step::Done { item, consumed: n } => {
                    consumed += n;
                    ing.item(item);
                    if consumed >= buf.len() {
                        break;
                    }
                }
            }
        }
        if consumed > 0 {
            buf.drain(0..consumed);
        }
        let n = reader
            .read(&mut tmp)
            .with_context(|| format!("reading recording {}", path.display()))?;
        if n == 0 {
            if !buf.is_empty() {
                bail!(
                    "{}: truncated recording (incomplete final frame)",
                    path.display()
                );
            }
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    Ok(ing.finish())
}

#[cfg(test)]
mod tests {
    //! Round-trips a timeline through a real `.glr` file (write → read) and asserts
    //! executions, GC, and metadata survive intact, including across multiple sealed
    //! segments. Run with `cargo test`.
    use super::*;
    use crate::db::{Db, Query, Reply};
    use crate::store::{Execution, GcEvent};

    fn tmp_path() -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "greenlane-test-{}-{}.glr",
            std::process::id(),
            nanos
        ))
    }

    fn sl(gid: u64, start: u64, dur: u64) -> Execution {
        Execution {
            gid,
            start,
            dur,
            name: format!("Greenlet-{gid}"),
            func: format!("app.py:f{gid}:1"),
            task: String::new(),
            stack: String::new(),
        }
    }

    #[tokio::test]
    async fn glr_roundtrip_preserves_timeline() {
        let db = Db::spawn(None).unwrap();
        db.set_epoch(1700);
        db.ingest_executions((1..=2000).map(|i| sl(i, i * 10, 5)).collect()); // > CHUNK
        db.ingest_gc(vec![GcEvent {
            start: 50,
            dur: 9,
            generation: 1,
            collected: 3,
        }]);

        let path = tmp_path();
        let bytes = db.flush_to_file(&path, 4321).unwrap();
        assert!(bytes > 0);
        // Container header is the segmented format.
        let head = std::fs::read(&path).unwrap();
        assert_eq!(&head[..4], SEG_MAGIC);

        let db2 = Db::spawn(None).unwrap();
        let pid = ingest_file(&path, &db2).unwrap();
        assert_eq!(pid, 4321);
        assert_eq!(db2.total(), 2000);
        assert_eq!(db2.epoch(), Some(1700));

        // The reloaded timeline answers queries like a live one.
        let r = db2
            .query(Query::Window {
                t0: 0,
                t1: u64::MAX >> 1,
                cap: 10_000,
            })
            .await
            .unwrap();
        match r {
            Reply::Window { gid, gc, .. } => {
                assert_eq!(gid.len(), 2000);
                assert_eq!(gc.len(), 1);
                assert_eq!(gc[0].collected, 3);
            }
            _ => panic!("expected Window reply"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn glr_multiple_sealed_segments_accumulate() {
        // Three flushes seal three segments; reloading must see every row exactly once
        // (the delta-sealing must not drop or double-count across segment boundaries).
        let db = Db::spawn(None).unwrap();
        let path = tmp_path();

        db.ingest_executions((1..=10).map(|i| sl(i, i * 10, 5)).collect());
        let b1 = db.flush_to_file(&path, 7).unwrap();
        db.ingest_executions((11..=25).map(|i| sl(i, i * 10, 5)).collect());
        let b2 = db.flush_to_file(&path, 7).unwrap();
        db.ingest_executions((26..=30).map(|i| sl(i, i * 10, 5)).collect());
        let b3 = db.flush_to_file(&path, 7).unwrap();
        // Append-only: the file grows with each seal, never shrinks/rewrites.
        assert!(
            b1 < b2 && b2 < b3,
            "segments should append: {b1} < {b2} < {b3}"
        );

        let db2 = Db::spawn(None).unwrap();
        let pid = ingest_file(&path, &db2).unwrap();
        assert_eq!(pid, 7);
        assert_eq!(db2.total(), 30);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn glr_tolerates_truncated_final_segment() {
        // Two sealed (fsynced) segments, then a partial third — a header promising a
        // body that was never fully written (a hard kill mid-seal). The reader must
        // keep both intact segments rather than failing the whole recording.
        use std::io::Write as _;
        let db = Db::spawn(None).unwrap();
        let path = tmp_path();
        db.ingest_executions((1..=10).map(|i| sl(i, i * 10, 5)).collect());
        db.flush_to_file(&path, 7).unwrap();
        db.ingest_executions((11..=20).map(|i| sl(i, i * 10, 5)).collect());
        db.flush_to_file(&path, 7).unwrap();
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(&100u32.to_le_bytes()).unwrap(); // raw_len
            f.write_all(&100u32.to_le_bytes()).unwrap(); // comp_len
            f.write_all(&[0u8; 10]).unwrap(); // ...but only 10 of the 100 body bytes
        }

        let db2 = Db::spawn(None).unwrap();
        let pid = ingest_file(&path, &db2).expect("truncated tail must not fail the load");
        assert_eq!(pid, 7);
        assert_eq!(
            db2.total(),
            20,
            "both sealed segments survive; partial tail dropped"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_corrupt_segment_length() {
        // A valid container header followed by a segment claiming an absurd length
        // must error, not try to buffer toward it.
        use std::io::Write as _;
        let path = tmp_path();
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(SEG_MAGIC).unwrap();
            f.write_all(&[SEG_VERSION]).unwrap();
            f.write_all(&(u32::MAX).to_le_bytes()).unwrap(); // raw_len ~4 GiB
            f.write_all(&(u32::MAX).to_le_bytes()).unwrap(); // comp_len ~4 GiB
        }
        let db = Db::spawn(None).unwrap();
        let err = ingest_file(&path, &db).unwrap_err();
        assert!(format!("{err:#}").contains("corrupt segment length"));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn glr_interns_repeated_strings() {
        // 1000 executions sharing one func + stack must produce a small file (interning +
        // compression), proving the repeated strings aren't stored per-execution.
        let db = Db::spawn(None).unwrap();
        let executions: Vec<Execution> = (0..1000)
            .map(|i| Execution {
                gid: i,
                start: i * 5,
                dur: 1,
                name: "Greenlet".into(),
                func: "hot.py:work:1".into(),
                task: String::new(),
                stack: "hot.py:work:1 <- hot.py:main:9".into(),
            })
            .collect();
        db.ingest_executions(executions);
        let path = tmp_path();
        let bytes = db.flush_to_file(&path, 1).unwrap();
        assert!(
            bytes < 20_000,
            "expected interned+compressed file to be small, got {bytes} bytes"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_non_glr_file() {
        let path = tmp_path();
        std::fs::write(&path, b"not a greenlane recording at all").unwrap();
        let db = Db::spawn(None).unwrap();
        let err = ingest_file(&path, &db).unwrap_err();
        assert!(format!("{err:#}").contains("bad magic"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_legacy_text_glr() {
        let path = tmp_path();
        std::fs::write(&path, b"GREENLNE\x01\x02 old recording body").unwrap();
        let db = Db::spawn(None).unwrap();
        let err = ingest_file(&path, &db).unwrap_err();
        assert!(err.to_string().contains("legacy"));
        let _ = std::fs::remove_file(&path);
    }
}
