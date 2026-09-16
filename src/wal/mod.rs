//! Append-only write-ahead log: CRC32-framed segments + group-commit fsync +
//! redb-backed metadata for ingest checkpoints.
//!
//! Frame layout (little-endian):
//! ```text
//!   +-------------------+----------------+-------------------+
//!   | payload_len: u32  | crc32: u32     | payload: [u8]     |
//!   +-------------------+----------------+-------------------+
//! ```
//! Payload layout:
//! ```text
//!   +-----------------------+-------------+--------------------+
//!   | receive_ts_nanos: i64 | protocol:u1 | source_addr:u16len |
//!   +-----------------------+-------------+--------------------+
//!   | raw_len: u32 + raw bytes                                 |
//!   +----------------------------------------------------------+
//! ```

pub mod cursor;
pub mod frame;
pub mod meta;
pub mod segment;
pub mod writer;

pub use cursor::{TailCursor, TailEntry};
pub use frame::{decode_frame, encode_record_into, FrameHeader, FrameIter, FRAME_HEADER_LEN};
pub use meta::{CheckpointStore, WalMeta};
pub use segment::{segment_filename, segment_id_from_name, SegmentRotator, SegmentRotatorStats};
pub use writer::{InsertHandle, WalEntry, WalGauges, WalWriter};

/// WAL bytes written but not yet consumed+checkpointed by ingest workers —
/// the local equivalent of a Kafka consumer-lag gauge. Sustained growth means
/// DuckDB inserts (or enrichment) can't keep up with the insert rate.
pub fn ingest_lag_bytes(wal_dir: &std::path::Path, meta: &WalMeta) -> u64 {
    let Ok(Some(cp)) = meta.min_checkpoint() else {
        return 0;
    };
    let mut lag = 0u64;
    let mut id = cp.segment_id;
    loop {
        let path = wal_dir.join(segment_filename(id));
        match std::fs::metadata(&path) {
            Ok(m) => {
                let len = m.len();
                lag += if id == cp.segment_id {
                    len.saturating_sub(cp.byte_offset)
                } else {
                    len
                };
                id += 1;
            }
            Err(_) => break,
        }
    }
    lag
}
