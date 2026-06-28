//! On-disk recording format for captured timelines (`.glr` files).
//!
//! A `.glr` is the **framed binary trace format** (see [`crate::trace_format`] and
//! ADR 0001) — the same encoding used on the live wire. The file stores closed
//! intervals as `slice` events (plus `gc` events and a `meta` frame) rather than
//! the wire's raw `switch` events, but it is the same self-describing, pooled,
//! versioned format: identical funcs/stacks are interned once, so a recording is
//! far smaller than the old tab-delimited dump.
//!
//! [`GlrWriter`] streams the DataFrame to disk a chunk at a time (bounded memory),
//! and [`ingest_file`] streams it back into the DB frame by frame. The binary
//! header (`b"GLR\0"` + version) gates compatibility; the previous text/postcard
//! format (`b"GREENLNE"`) is detected and rejected with a clear message.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::db::Db;
use crate::store::{GcEvent, Slice};
use crate::trace_format::{Decoder, Encoder, Item, Step};

/// Slices buffered before an ingest batch when reading a file back.
pub const CHUNK: usize = 16_384;

/// Legacy signature: the original text/postcard `.glr`. Recognized only to give a
/// clear "re-record" error instead of an opaque binary decode failure.
const LEGACY_MAGIC: &[u8; 8] = b"GREENLNE";

/// Streaming writer for a `.glr`. Emits the header, the `slice`/`gc` schemas, and
/// a `meta` frame on creation, then accepts slices/GC and flushes the encoder's
/// buffer to disk periodically so a multi-million-row recording never holds the
/// whole encoded stream in memory.
pub struct GlrWriter {
    file: BufWriter<File>,
    enc: Encoder,
    written: u64,
    /// Where we're actually writing (a sibling temp); renamed onto `final_path`
    /// only once fully written + fsynced, so a partial flush never truncates the
    /// last good recording.
    tmp_path: std::path::PathBuf,
    final_path: std::path::PathBuf,
}

impl GlrWriter {
    /// Flush bytes to disk once the encoder buffer crosses this size.
    const FLUSH_BYTES: usize = 1 << 16;

    /// Create a temp file alongside `path` and write header + schemas + meta + all
    /// GC events. The recorder calls this repeatedly (periodic partial flushes);
    /// each write goes to a fresh temp and is atomically renamed onto `path` by
    /// [`finish`], so a mid-write crash leaves the previous complete `.glr` intact.
    /// GC is bounded (one per collection) so it's written up front in full.
    pub fn create(path: &Path, pid: i32, epoch_ms: Option<u64>, gc: &[GcEvent]) -> Result<Self> {
        let mut enc = Encoder::new();
        enc.write_file_schemas();
        // tid is a wire-only concept (live scheduler-lag sampling); recordings
        // carry 0. epoch 0 = unknown.
        enc.meta(epoch_ms.unwrap_or(0), 0, pid as i64, "");
        for g in gc {
            enc.gc(g);
        }
        // Unique temp name in the same directory (rename is atomic within a
        // filesystem). pid+nanos avoids collisions between successive flushes.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp_name = format!(
            ".{}.{}.{}.tmp",
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("greenlane"),
            std::process::id(),
            nanos
        );
        let tmp_path = path.with_file_name(tmp_name);
        let file = BufWriter::new(
            File::create(&tmp_path)
                .with_context(|| format!("creating recording temp {}", tmp_path.display()))?,
        );
        let mut w = GlrWriter {
            file,
            enc,
            written: 0,
            tmp_path,
            final_path: path.to_path_buf(),
        };
        w.flush_enc()?;
        Ok(w)
    }

    fn flush_enc(&mut self) -> Result<()> {
        let bytes = self.enc.bytes();
        if bytes.is_empty() {
            return Ok(());
        }
        self.file
            .write_all(bytes)
            .context("writing recording bytes")?;
        self.written += bytes.len() as u64;
        self.enc.clear_out();
        Ok(())
    }

    /// Append one closed interval.
    pub fn push_slice(&mut self, s: &Slice) -> Result<()> {
        self.enc.slice(s);
        if self.enc.bytes().len() >= Self::FLUSH_BYTES {
            self.flush_enc()?;
        }
        Ok(())
    }

    /// Finish writing: flush the buffer to the fd, fsync, then atomically rename
    /// the temp onto the final path. Returns the on-disk size in bytes.
    pub fn finish(mut self) -> Result<u64> {
        self.flush_enc()?;
        self.file.flush().context("flushing recording")?;
        self.file.get_ref().sync_all().context("fsync recording")?;
        std::fs::rename(&self.tmp_path, &self.final_path).with_context(|| {
            format!(
                "renaming {} onto {}",
                self.tmp_path.display(),
                self.final_path.display()
            )
        })?;
        // `self` drops here; the temp is already renamed away, so Drop's cleanup
        // is a harmless no-op.
        Ok(self.written)
    }
}

impl Drop for GlrWriter {
    fn drop(&mut self) {
        // If we never reached finish() (an error mid-write), don't leave the temp
        // littering the directory. The previous complete recording is untouched.
        let _ = std::fs::remove_file(&self.tmp_path);
    }
}

/// Stream a `.glr` into `db`, reading the file in chunks and decoding one frame at
/// a time so a multi-GB recording never lands in memory at once (only a ~64 KB
/// read buffer plus the largest partial frame). Verifies the binary header before
/// touching the body. Returns the PID the recording was captured from.
pub fn ingest_file(path: &Path, db: &Db) -> Result<i32> {
    let file = File::open(path).with_context(|| format!("opening recording {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut dec = Decoder::new();
    // Incremental decode buffer: accumulated bytes not yet consumed as a frame.
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 1 << 16];
    let mut legacy_checked = false;
    let mut pid = 0i32;
    let mut slices: Vec<Slice> = Vec::with_capacity(CHUNK);
    let mut gc: Vec<GcEvent> = Vec::new();

    loop {
        // Drain every complete frame currently buffered.
        let mut consumed = 0usize;
        loop {
            match dec
                .step(&buf[consumed..])
                .with_context(|| format!("decoding recording {}", path.display()))?
            {
                Step::NeedMore => break,
                Step::Done { item, consumed: n } => {
                    consumed += n;
                    match item {
                        Some(Item::Meta(m)) => {
                            pid = m.pid as i32;
                            if m.epoch_ms > 0 {
                                db.set_epoch(m.epoch_ms);
                            }
                        }
                        Some(Item::Slice(s)) => {
                            slices.push(s);
                            if slices.len() >= CHUNK {
                                db.ingest_slices(std::mem::take(&mut slices));
                            }
                        }
                        Some(Item::Gc(g)) => gc.push(g),
                        // `switch` is a wire-only event; a file shouldn't contain
                        // one, but ignore it rather than fail if it ever does.
                        Some(Item::Switch(_)) | None => {}
                    }
                    if consumed >= buf.len() {
                        break;
                    }
                }
            }
        }
        if consumed > 0 {
            buf.drain(0..consumed);
        }

        // Read the next chunk; EOF ends the stream (a non-empty leftover means a
        // truncated final frame).
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
        // Recognize the legacy text/postcard format once enough header bytes exist,
        // for a clear "re-record" message instead of an opaque bad-magic decode.
        if !legacy_checked && buf.len() >= LEGACY_MAGIC.len() {
            if buf.starts_with(LEGACY_MAGIC) {
                bail!(
                    "{}: this is a legacy text-format .glr from an older greenlane. The \
                     recording format is now binary (ADR 0001) and the old files can't be \
                     read — re-record with this version.",
                    path.display()
                );
            }
            legacy_checked = true;
        }
    }

    if !slices.is_empty() {
        db.ingest_slices(slices);
    }
    if !gc.is_empty() {
        db.ingest_gc(gc);
    }
    Ok(pid)
}

#[cfg(test)]
mod tests {
    //! Round-trips a timeline through a real `.glr` file (write → read) and
    //! asserts slices, GC, and metadata survive intact. Run with `cargo test`.
    use super::*;
    use crate::db::{Db, Query, Reply};
    use crate::store::{GcEvent, Slice};

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

    fn sl(gid: u64, start: u64, dur: u64) -> Slice {
        Slice {
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
        db.ingest_slices((1..=2000).map(|i| sl(i, i * 10, 5)).collect()); // > CHUNK
        db.ingest_gc(vec![GcEvent {
            start: 50,
            dur: 9,
            generation: 1,
            collected: 3,
        }]);

        let path = tmp_path();
        let bytes = db.flush_to_file(&path, 4321).unwrap();
        assert!(bytes > 0);

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
            Reply::Window { slices, gc, .. } => {
                assert_eq!(slices.len(), 2000);
                assert_eq!(gc.len(), 1);
                assert_eq!(gc[0].collected, 3);
            }
            _ => panic!("expected Window reply"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn glr_interns_repeated_strings() {
        // 1000 slices that all share func "hot.py:work:1" and the same stack must
        // produce a file far smaller than the raw text would, proving interning.
        let db = Db::spawn(None).unwrap();
        let slices: Vec<Slice> = (0..1000)
            .map(|i| Slice {
                gid: i,
                start: i * 5,
                dur: 1,
                name: "Greenlet".into(),
                func: "hot.py:work:1".into(),
                task: String::new(),
                stack: "hot.py:work:1 <- hot.py:main:9".into(),
            })
            .collect();
        db.ingest_slices(slices);
        let path = tmp_path();
        let bytes = db.flush_to_file(&path, 1).unwrap();
        // The shared func (13 chars) + stack (30 chars) alone would be ~43 KB of
        // raw text across 1000 slices; interned they're sent once, leaving only
        // the compact per-event frames (~14 B each).
        assert!(
            bytes < 20_000,
            "expected interned file to be small, got {bytes} bytes"
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
