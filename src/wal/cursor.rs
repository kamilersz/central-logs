//! Tail cursor: sequential reader over WAL segments starting from a checkpoint.
//!
//! Ingest workers use a [`TailCursor`] to read frames from where they last
//! stopped, advancing through the segment files in order.
//!
//! The reader is buffered (`BufReader`, 512KB): unbuffered per-frame
//! `tokio::fs::File` reads dispatch through `spawn_blocking`, and two
//! round-trips per ~250-byte frame made ingest CPU-bound on read scheduling
//! instead of parse/insert. Frames are decoded straight out of the internal
//! buffer; only frames that straddle a buffer refill take the slow path.

use std::future::Future;
use std::path::PathBuf;

use tokio::io::{AsyncBufReadExt, AsyncSeekExt};

use crate::wal::frame::{decode_frame, parse_header, FRAME_HEADER_LEN, MAX_PAYLOAD_LEN};
use crate::wal::meta::Checkpoint;
use crate::wal::segment::{segment_filename, SegmentRotator};
use crate::{Error, RawRecord, Result};

/// Internal read-buffer capacity. Frames larger than this take a slower
/// chunked path (rare: typical frames are a few hundred bytes).
const READ_BUF: usize = 512 * 1024;

/// One read step's outcome.
#[derive(Debug)]
pub enum TailEntry {
    /// A decoded record. The new cursor position (post-record) is included.
    Record { rec: RawRecord, next: Checkpoint },
    /// End of currently-available data; caller should sleep and poll again.
    CaughtUp { next: Checkpoint },
    /// The current segment has no more data and the segment id is below the
    /// writer's current segment, but the file was truncated/corrupted. Skip.
    BadFrame { error: Error, next: Checkpoint },
}

pub struct TailCursor {
    base_dir: PathBuf,
    pos: Checkpoint,
    /// Buffered reader for the current segment, plus its id (rotation detect).
    open: Option<(u64, tokio::io::BufReader<tokio::fs::File>)>,
}

impl TailCursor {
    pub fn new(base_dir: PathBuf, start: Checkpoint) -> Self {
        Self {
            base_dir,
            pos: start,
            open: None,
        }
    }

    pub fn position(&self) -> Checkpoint {
        self.pos
    }

    /// Current writer segment id (highest in the directory) or 0 if none.
    pub fn writer_segment_id(&self) -> u64 {
        SegmentRotator::open(self.base_dir.clone(), u64::MAX)
            .map(|r| r.current_id())
            .unwrap_or(0)
    }

    fn open_segment(&mut self) -> impl Future<Output = Result<bool>> + Send + '_ {
        async move {
            let path = self.base_dir.join(segment_filename(self.pos.segment_id));
            if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
                return Ok(false);
            }
            let file = tokio::fs::OpenOptions::new().read(true).open(&path).await?;
            // Guard against a stale checkpoint: pruned-then-recreated segment
            // ids can outlive a persisted offset (e.g. a cursor accumulated
            // before a crash/rotation bug). Seeking past EOF succeeds
            // silently and the cursor would then report CaughtUp forever —
            // the WAL keeps growing while nothing reaches DuckDB. When the
            // offset points beyond the file, the segment incarnation is
            // newer than the cursor: reset to 0 and re-read it (worst case a
            // few rows re-ingest; the alternative is silently dropping
            // everything buffered after the overrun).
            let file_len = file.metadata().await?.len();
            if self.pos.byte_offset > file_len {
                tracing::warn!(
                    segment_id = self.pos.segment_id,
                    checkpoint_offset = self.pos.byte_offset,
                    segment_bytes = file_len,
                    "ingest cursor beyond segment EOF (stale checkpoint for a recycled \
                     segment id); resetting cursor to 0 and re-reading the segment"
                );
                self.pos.byte_offset = 0;
            }
            let mut rdr = tokio::io::BufReader::with_capacity(READ_BUF, file);
            if self.pos.byte_offset > 0 {
                rdr.seek(std::io::SeekFrom::Start(self.pos.byte_offset))
                    .await?;
            }
            self.open = Some((self.pos.segment_id, rdr));
            Ok(true)
        }
    }

    /// Attempt to read the next available record. Returns [`TailEntry::CaughtUp`]
    /// when there is nothing to read right now.
    pub async fn next(&mut self) -> Result<TailEntry> {
        loop {
            // Make sure the file for `pos.segment_id` is open.
            let need_reopen = match &self.open {
                None => true,
                Some((id, _)) => *id != self.pos.segment_id,
            };
            if need_reopen && !self.open_segment().await? {
                // Segment doesn't exist yet.
                return Ok(TailEntry::CaughtUp { next: self.pos });
            }

            // --- Fast path: whole frame already inside the buffer. ---
            let fast = {
                let rdr = self.open.as_mut().expect("cursor reader open");
                let buffered = rdr.1.fill_buf().await?;
                if buffered.is_empty() {
                    None
                } else if buffered.len() >= FRAME_HEADER_LEN {
                    let arr = buffered
                        .first_chunk::<FRAME_HEADER_LEN>()
                        .expect("len checked");
                    let h = parse_header(arr);
                    let want = FRAME_HEADER_LEN + h.payload_len as usize;
                    if buffered.len() >= want {
                        Some((want, decode_frame(&buffered[..want])))
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            match fast {
                None => {}
                Some((want, Ok(rec))) => {
                    self.open.as_mut().unwrap().1.consume(want);
                    self.pos.byte_offset += want as u64;
                    return Ok(TailEntry::Record {
                        rec,
                        next: self.pos,
                    });
                }
                Some((want, Err(e))) => {
                    self.open.as_mut().unwrap().1.consume(want);
                    self.pos.byte_offset += want as u64;
                    return Ok(TailEntry::BadFrame {
                        error: e,
                        next: self.pos,
                    });
                }
            }

            let is_eof = {
                let rdr = self.open.as_mut().expect("cursor reader open");
                rdr.1.fill_buf().await?.is_empty()
            };
            if is_eof {
                // True EOF for this segment file: advance or wait.
                if self.pos.segment_id < self.writer_segment_id() {
                    self.pos = Checkpoint {
                        segment_id: self.pos.segment_id + 1,
                        byte_offset: 0,
                    };
                    self.open = None;
                    continue;
                }
                return Ok(TailEntry::CaughtUp { next: self.pos });
            }

            // --- Slow path: frame straddles the buffer boundary. ---
            // Assemble it across refills, consuming EXACTLY the frame's
            // bytes (over-consuming would drop the next frames' bytes and
            // desync the cursor). `pos` only advances once the full frame
            // is decoded; on a torn tail we drop the reader so the next
            // call reopens at `pos` and re-reads the partial frame.
            let mut frame: Vec<u8> = Vec::with_capacity(64 * 1024);
            let mut want: Option<usize> = None;
            let torn = loop {
                // Bytes still needed: 8 for the header, then the payload.
                let need = match want {
                    Some(w) => w - frame.len(),
                    None => FRAME_HEADER_LEN - frame.len(),
                };
                let got = {
                    let rdr = self.open.as_mut().expect("cursor reader open");
                    let available = rdr.1.fill_buf().await?;
                    if available.is_empty() {
                        break true; // EOF mid-frame: torn tail
                    }
                    let take = need.min(available.len());
                    frame.extend_from_slice(&available[..take]);
                    take
                };
                rdr_consume(self.open.as_mut().expect("open"), got);
                if want.is_none() && frame.len() >= FRAME_HEADER_LEN {
                    let arr = frame
                        .first_chunk::<FRAME_HEADER_LEN>()
                        .expect("len checked");
                    let h = parse_header(arr);
                    let payload = h.payload_len as usize;
                    if payload > MAX_PAYLOAD_LEN {
                        // Bogus frame — skip past it.
                        self.pos.byte_offset += FRAME_HEADER_LEN as u64 + payload as u64;
                        self.open = None;
                        return Ok(TailEntry::BadFrame {
                            error: Error::invalid_input(format!(
                                "frame payload_len {payload} exceeds max"
                            )),
                            next: self.pos,
                        });
                    }
                    want = Some(FRAME_HEADER_LEN + payload);
                }
                if let Some(w) = want {
                    if frame.len() >= w {
                        break false; // complete
                    }
                }
            };

            if !torn {
                let w = want.expect("complete implies want");
                return match decode_frame(&frame[..w]) {
                    Ok(rec) => {
                        self.pos.byte_offset += w as u64;
                        Ok(TailEntry::Record {
                            rec,
                            next: self.pos,
                        })
                    }
                    Err(e) => {
                        self.pos.byte_offset += w as u64;
                        Ok(TailEntry::BadFrame {
                            error: e,
                            next: self.pos,
                        })
                    }
                };
            }

            // Torn tail: writer is mid-write. Drop the reader so the next
            // call reopens at `pos` and re-reads the partial frame.
            self.open = None;
            return Ok(TailEntry::CaughtUp { next: self.pos });
        }
    }

    /// Skip to a new starting position (used when recovering or to bound catch-up).
    pub fn seek(&mut self, pos: Checkpoint) {
        self.pos = pos;
        self.open = None;
    }
}

fn rdr_consume(open: &mut (u64, tokio::io::BufReader<tokio::fs::File>), n: usize) {
    open.1.consume(n);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: u8) -> RawRecord {
        RawRecord {
            receive_ts: chrono::Utc::now(),
            source_addr: "10.0.0.1:1".into(),
            protocol: crate::Protocol::HttpJson,
            raw: bytes::Bytes::from(format!(r#"{{"msg":"rec-{id}"}}"#)),
        }
    }

    /// Regression: a persisted checkpoint with byte_offset beyond the
    /// segment's EOF (stale cursor for a recycled segment id) used to seek
    /// past EOF and report CaughtUp forever — the WAL grew while nothing
    /// reached DuckDB. The cursor must reset to 0 and re-read the segment.
    #[tokio::test]
    async fn stale_checkpoint_beyond_eof_resets_to_zero() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();

        // One 2-record segment.
        let mut buf = Vec::new();
        crate::wal::frame::encode_record_into(&mut buf, &sample(1));
        crate::wal::frame::encode_record_into(&mut buf, &sample(2));
        std::fs::write(base.join(segment_filename(0)), &buf).unwrap();

        // Cursor starting far beyond EOF (~1.9 GB, as observed in prod).
        let mut cur = TailCursor::new(
            base.clone(),
            Checkpoint {
                segment_id: 0,
                byte_offset: 1_947_781_504,
            },
        );

        let rec = match cur.next().await.unwrap() {
            TailEntry::Record { rec, next } => {
                assert!(next.byte_offset > 0);
                assert!(next.byte_offset as usize <= buf.len());
                rec
            }
            TailEntry::CaughtUp { .. } => panic!("stale cursor must reset, not report CaughtUp"),
            TailEntry::BadFrame { error, .. } => panic!("unexpected bad frame: {error}"),
        };
        assert!(String::from_utf8_lossy(&rec.raw).contains("rec-1"));

        // Second record decodes normally, then the cursor is caught up.
        match cur.next().await.unwrap() {
            TailEntry::Record { rec, .. } => {
                assert!(String::from_utf8_lossy(&rec.raw).contains("rec-2"))
            }
            other => panic!("expected second record, got {other:?}"),
        }
        assert!(matches!(cur.next().await.unwrap(), TailEntry::CaughtUp { .. }));
    }

    /// A checkpoint AT the segment size (legitimate caught-up state) must
    /// not trigger the reset path — it should report CaughtUp directly.
    #[tokio::test]
    async fn checkpoint_at_exact_eof_stays_caught_up() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let mut buf = Vec::new();
        crate::wal::frame::encode_record_into(&mut buf, &sample(1));
        std::fs::write(base.join(segment_filename(0)), &buf).unwrap();

        let mut cur = TailCursor::new(
            base,
            Checkpoint {
                segment_id: 0,
                byte_offset: buf.len() as u64,
            },
        );
        assert!(matches!(
            cur.next().await.unwrap(),
            TailEntry::CaughtUp { .. }
        ));
    }
}
