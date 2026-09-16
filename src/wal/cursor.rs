//! Tail cursor: sequential reader over WAL segments starting from a checkpoint.
//!
//! Ingest workers use a [`TailCursor`] to read frames from where they last
//! stopped, advancing through the segment files in order.

use std::path::PathBuf;

use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::wal::frame::{decode_frame, parse_header, FRAME_HEADER_LEN, MAX_PAYLOAD_LEN};
use crate::wal::meta::Checkpoint;
use crate::wal::segment::{segment_filename, SegmentRotator};
use crate::{Error, RawRecord, Result};

/// One read step's outcome.
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
    /// Open file for the current segment, plus its id (to detect rotation).
    open: Option<(u64, tokio::fs::File)>,
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

    /// Attempt to read the next available record. Returns [`TailEntry::CaughtUp`]
    /// when there is nothing to read right now.
    pub async fn next(&mut self) -> Result<TailEntry> {
        loop {
            // Make sure the file for `pos.segment_id` is open.
            let need_reopen = match &self.open {
                None => true,
                Some((id, _)) => *id != self.pos.segment_id,
            };
            if need_reopen {
                let path = self.base_dir.join(segment_filename(self.pos.segment_id));
                if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
                    // Segment doesn't exist yet.
                    return Ok(TailEntry::CaughtUp { next: self.pos });
                }
                let mut file = tokio::fs::OpenOptions::new().read(true).open(&path).await?;
                file.seek(std::io::SeekFrom::Start(self.pos.byte_offset)).await?;
                self.open = Some((self.pos.segment_id, file));
            }

            let (_, file) = self.open.as_mut().expect("file open");
            // Read header.
            let mut header_buf = [0u8; FRAME_HEADER_LEN];
            match file.read(&mut header_buf[..]).await? {
                0 => {
                    // End of current segment file. Are there more segments?
                    let writer_seg = self.writer_segment_id();
                    if self.pos.segment_id < writer_seg {
                        // Advance to next segment.
                        self.pos = Checkpoint {
                            segment_id: self.pos.segment_id + 1,
                            byte_offset: 0,
                        };
                        self.open = None;
                        continue;
                    }
                    return Ok(TailEntry::CaughtUp { next: self.pos });
                }
                n if n < FRAME_HEADER_LEN => {
                    // Partial header — treat as caught up; the writer may be
                    // mid-write. (Group-commit writes whole frames in one call,
                    // so a partial header means a torn read at the boundary.)
                    let (_, file) = self.open.as_mut().unwrap();
                    file.seek(std::io::SeekFrom::Current(-(n as i64))).await?;
                    return Ok(TailEntry::CaughtUp { next: self.pos });
                }
                _ => {}
            }

            let header = parse_header(&header_buf);
            let payload_len = header.payload_len as usize;
            if payload_len > MAX_PAYLOAD_LEN {
                // Skip past this frame to avoid infinite loop.
                let next = Checkpoint {
                    segment_id: self.pos.segment_id,
                    byte_offset: self.pos.byte_offset + FRAME_HEADER_LEN as u64 + payload_len as u64,
                };
                self.pos = next;
                return Ok(TailEntry::BadFrame {
                    error: Error::invalid_input(format!(
                        "frame payload_len {payload_len} exceeds max"
                    )),
                    next,
                });
            }

            let mut payload = vec![0u8; payload_len];
            let read = file.read(&mut payload[..]).await?;
            if read < payload_len {
                // Torn read — rewind so we re-read it next time.
                let rewind = read + FRAME_HEADER_LEN;
                let (_, file) = self.open.as_mut().unwrap();
                file.seek(std::io::SeekFrom::Current(-(rewind as i64))).await?;
                return Ok(TailEntry::CaughtUp { next: self.pos });
            }

            // Stitch header + payload for decode_frame.
            let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + payload_len);
            frame.extend_from_slice(&header_buf);
            frame.extend_from_slice(&payload);
            let advance = frame.len() as u64;
            let next = Checkpoint {
                segment_id: self.pos.segment_id,
                byte_offset: self.pos.byte_offset + advance,
            };
            self.pos = next;
            return Ok(match decode_frame(&frame) {
                Ok(rec) => TailEntry::Record { rec, next },
                Err(e) => TailEntry::BadFrame { error: e, next },
            });
        }
    }

    /// Skip to a new starting position (used when recovering or to bound catch-up).
    pub fn seek(&mut self, pos: Checkpoint) {
        self.pos = pos;
        self.open = None;
    }
}
