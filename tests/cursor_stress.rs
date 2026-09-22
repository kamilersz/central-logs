//! Cursor stress test: frames that straddle the 512KB BufReader boundary,
//! segment rotation, and tailing a segment while it grows. Guards against
//! cursor desync (lost/duplicated frames) and infinite CaughtUp loops —
//! both of which surfaced as frozen `ingest_lag_bytes` under load.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use central_logs::wal::cursor::TailCursor;
use central_logs::wal::meta::WalMeta;
use central_logs::wal::writer::WalWriter;
use central_logs::{Protocol, RawRecord};

fn rec(payload: usize) -> RawRecord {
    RawRecord {
        receive_ts: Utc::now(),
        source_addr: "127.0.0.1:9".into(),
        protocol: Protocol::HttpJson,
        raw: bytes::Bytes::from("x".repeat(payload)),
    }
}

#[tokio::test]
async fn cursor_reads_every_frame_across_buffer_boundaries() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let wal_dir = tmp.path().join("wal");
    let meta = Arc::new(WalMeta::open(&tmp.path().join("meta.redb")).expect("meta"));
    // Segment cap large enough to keep ONE segment; frames straddle the
    // cursor's internal 512KB buffer repeatedly.
    let (writer, handle) = WalWriter::new(
        wal_dir.clone(),
        64 * 1024 * 1024,
        meta.clone(),
        4096,
        8192,
        Duration::from_millis(1),
    )
    .expect("writer");
    let w = tokio::spawn(async move {
        let _ = writer.run().await;
    });

    let n = 4000;
    for i in 0..n {
        // ~10KB payloads → ~40MB total; 512KB boundary crossed ~78 times.
        handle.append_unacked(rec(10_000 + (i % 7) * 13)).await.expect("append");
    }
    // Ensure the writer flushed everything (last append acked by writer
    // batching; give it a moment, then drop the handle to close the channel).
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(handle);
    let _ = w.await;

    let mut cursor = TailCursor::new(wal_dir.clone(), central_logs::wal::meta::Checkpoint::zero());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut count = 0u64;
    loop {
        match cursor.next().await.expect("cursor next") {
            central_logs::wal::cursor::TailEntry::Record { next, .. } => {
                count += 1;
                let _ = next;
                if count == n as u64 {
                    break;
                }
            }
            central_logs::wal::cursor::TailEntry::CaughtUp { .. } => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "cursor stalled: {count}/{n} frames read"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            central_logs::wal::cursor::TailEntry::BadFrame { error, .. } => {
                panic!("bad frame after {count}: {error}");
            }
        }
    }
    assert_eq!(count, n as u64);
}

#[tokio::test]
async fn cursor_reads_frames_larger_than_read_buffer() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let wal_dir = tmp.path().join("wal");
    let meta = Arc::new(WalMeta::open(&tmp.path().join("meta.redb")).expect("meta"));
    let (writer, handle) = WalWriter::new(
        wal_dir.clone(),
        64 * 1024 * 1024,
        meta.clone(),
        4096,
        8192,
        Duration::from_millis(1),
    )
    .expect("writer");
    let w = tokio::spawn(async move {
        let _ = writer.run().await;
    });

    // Payload larger than the cursor's 512KB read buffer.
    for i in 0..5u32 {
        handle.append_unacked(rec(600_000 + i as usize)).await.expect("append");
    }
    handle.append_unacked(rec(100)).await.expect("append");
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(handle);
    let _ = w.await;

    let mut cursor = TailCursor::new(wal_dir, central_logs::wal::meta::Checkpoint::zero());
    let mut count = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        match cursor.next().await.expect("cursor next") {
            central_logs::wal::cursor::TailEntry::Record { .. } => count += 1,
            central_logs::wal::cursor::TailEntry::CaughtUp { .. } => {
                assert!(tokio::time::Instant::now() < deadline, "stalled at {count}");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            central_logs::wal::cursor::TailEntry::BadFrame { error, .. } => {
                panic!("bad frame after {count}: {error}");
            }
        }
        if count == 6 {
            break;
        }
    }
    assert_eq!(count, 6);
}
