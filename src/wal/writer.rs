//! The WAL writer task: drains a bounded channel and appends records to the
//! current segment, performing group-commit fsync (architecture §2a).

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::wal::frame::{encode_record_into, FRAME_HEADER_LEN};
use crate::wal::meta::{Checkpoint, WalMeta};
use crate::wal::segment::SegmentRotator;
use crate::{Protocol, RawRecord, Result};

/// Shared WAL health gauges (LESSON_LEARNED: make backpressure visible
/// *before* drops start — their librdkafka OOM was invisible until crash).
///
/// - `channel_depth`: entries currently sitting in the bounded insert→WAL
///   channel. Trending toward `channel_capacity` means the writer (disk
///   fsync) can't keep up with insert rate.
/// - `channel_capacity`: the configured bound.
/// - `bytes_written_total`: monotonically increasing WAL throughput counter.
#[derive(Debug, Default)]
pub struct WalGauges {
    pub channel_depth: AtomicI64,
    pub channel_capacity: AtomicU64,
    pub bytes_written_total: AtomicU64,
}

impl WalGauges {
    pub fn depth(&self) -> i64 {
        self.channel_depth.load(Ordering::Relaxed)
    }
    pub fn capacity(&self) -> u64 {
        self.channel_capacity.load(Ordering::Relaxed)
    }
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written_total.load(Ordering::Relaxed)
    }
}

/// One entry flowing through the WAL channel: the record plus an optional ack
/// oneshot. UDP/syslog inserts that don't require durability ack leave `ack = None`.
pub struct WalEntry {
    pub record: RawRecord,
    pub ack: Option<oneshot::Sender<()>>,
}

impl WalEntry {
    pub fn fire_and_forget(record: RawRecord) -> Self {
        Self { record, ack: None }
    }

    pub fn with_ack(record: RawRecord) -> (Self, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                record,
                ack: Some(tx),
            },
            rx,
        )
    }
}

/// Handle held by insert endpoints to push records into the WAL.
#[derive(Clone)]
pub struct InsertHandle {
    tx: mpsc::Sender<WalEntry>,
    gauges: Arc<WalGauges>,
}

impl InsertHandle {
    pub fn new(tx: mpsc::Sender<WalEntry>) -> Self {
        Self {
            tx,
            gauges: Arc::new(WalGauges::default()),
        }
    }

    pub fn with_gauges(tx: mpsc::Sender<WalEntry>, gauges: Arc<WalGauges>) -> Self {
        Self { tx, gauges }
    }

    /// Append `record` and wait until the batch containing it has been fsynced.
    /// Use for HTTP/TCP paths that support backpressure.
    pub async fn append_acked(&self, record: RawRecord) -> Result<()> {
        let (entry, rx) = WalEntry::with_ack(record);
        self.tx
            .send(entry)
            .await
            .map_err(|_| crate::Error::ChannelClosed)?;
        self.gauges.channel_depth.fetch_add(1, Ordering::Relaxed);
        rx.await.map_err(|_| crate::Error::ChannelClosed)
    }

    /// Append `records` and wait ONCE for the whole batch to be durable.
    ///
    /// The channel is FIFO and the writer commits (write+fsync+ack) batches in
    /// order, so when the last entry's ack fires, every earlier entry is
    /// already fsynced — awaiting a single oneshot gives identical durability
    /// semantics to per-record `append_acked` at a fraction of the cost
    /// (one group-commit cycle per request instead of one per record).
    ///
    /// Returns the number of records durably queued and acked. If the channel
    /// closes mid-batch, the already-queued count is returned as an error path
    /// via `Err(ChannelClosed)` after waiting for their acks when possible.
    pub async fn append_batch_acked(&self, records: Vec<RawRecord>) -> Result<usize> {
        let total = records.len();
        let mut last_rx: Option<oneshot::Receiver<()>> = None;
        let mut queued = 0usize;
        for record in records {
            let (entry, rx) = WalEntry::with_ack(record);
            match self.tx.send(entry).await {
                Ok(()) => {
                    self.gauges.channel_depth.fetch_add(1, Ordering::Relaxed);
                    queued += 1;
                    last_rx = Some(rx);
                }
                Err(_) => break,
            }
        }
        if queued == 0 {
            return Err(crate::Error::ChannelClosed);
        }
        // Wait for the last ack — covers every entry queued by this call.
        match last_rx {
            Some(rx) => match rx.await {
                Ok(()) => Ok(queued),
                Err(_) => Err(crate::Error::ChannelClosed),
            },
            None => Err(crate::Error::ChannelClosed),
        }
        .map(|n| {
            debug_assert!(n <= total);
            n
        })
    }

    /// Append `record` without waiting for fsync (UDP syslog). Returns Err only
    /// if the channel is closed.
    pub async fn append_unacked(&self, record: RawRecord) -> Result<()> {
        self.tx
            .send(WalEntry::fire_and_forget(record))
            .await
            .map(|()| {
                self.gauges.channel_depth.fetch_add(1, Ordering::Relaxed);
            })
            .map_err(|_| crate::Error::ChannelClosed)
    }

    /// Try to append without blocking at all. Returns `Err(ChannelClosed)` if the
    /// WAL writer has exited, or `Err(InvalidInput("full"))` if the channel is full.
    pub fn try_append_unacked(&self, record: RawRecord) -> Result<()> {
        match self.tx.try_send(WalEntry::fire_and_forget(record)) {
            Ok(()) => {
                self.gauges.channel_depth.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                Err(crate::Error::invalid_input("channel full"))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(crate::Error::ChannelClosed),
        }
    }
}

/// The WAL writer. Owns the open segment file and the group-commit loop.
pub struct WalWriter {
    rotator: SegmentRotator,
    meta: Arc<WalMeta>,
    rx: mpsc::Receiver<WalEntry>,
    batch_max_records: usize,
    batch_max_delay: Duration,
    file: Option<tokio::fs::File>,
    writer_pos: u64,
    records_written: u64,
    bytes_written: u64,
    gauges: Arc<WalGauges>,
}

impl WalWriter {
    pub fn new(
        base_dir: PathBuf,
        segment_max_bytes: u64,
        meta: Arc<WalMeta>,
        channel_depth: usize,
        batch_max_records: usize,
        batch_max_delay: Duration,
    ) -> Result<(Self, InsertHandle)> {
        let rotator = SegmentRotator::open(base_dir, segment_max_bytes)?;
        let (tx, rx) = mpsc::channel(channel_depth);
        let gauges = Arc::new(WalGauges {
            channel_capacity: AtomicU64::new(channel_depth as u64),
            ..WalGauges::default()
        });
        let handle = InsertHandle::with_gauges(tx, gauges.clone());
        let writer = Self {
            rotator,
            meta,
            rx,
            batch_max_records,
            batch_max_delay,
            file: None,
            writer_pos: 0,
            records_written: 0,
            bytes_written: 0,
            gauges,
        };
        Ok((writer, handle))
    }

    /// Shared health gauges (clone before `run` consumes the writer).
    pub fn gauges(&self) -> &Arc<WalGauges> {
        &self.gauges
    }

    pub fn stats(&self) -> (u64, u64, u64) {
        (
            self.rotator.current_id(),
            self.records_written,
            self.bytes_written,
        )
    }

    /// Run the group-commit loop until the channel closes or `shutdown` fires.
    pub async fn run(mut self) -> Result<()> {
        tracing::info!(
            "wal writer starting at segment {} (current_bytes={})",
            self.rotator.current_id(),
            self.rotator.current_bytes()
        );

        loop {
            let batch = match self.collect_batch().await {
                Ok(Some(b)) => b,
                Ok(None) => break,
                Err(e) => {
                    tracing::error!(?e, "wal collect_batch failed");
                    return Err(e);
                }
            };
            if let Err(e) = self.commit_batch(batch).await {
                tracing::error!(?e, "wal commit_batch failed");
                return Err(e);
            }
        }
        tracing::info!("wal writer channel closed, exiting");
        Ok(())
    }

    async fn collect_batch(&mut self) -> Result<Option<Vec<WalEntry>>> {
        // Block on the first message of the next batch.
        match self.rx.recv().await {
            Some(first) => {
                let mut batch = Vec::with_capacity(self.batch_max_records.min(1024));
                batch.push(first);
                // Drain anything else immediately available up to the cap.
                while batch.len() < self.batch_max_records {
                    match self.rx.try_recv() {
                        Ok(e) => batch.push(e),
                        Err(_) => break,
                    }
                }
                // If still under cap, wait briefly for more (group-commit window).
                if batch.len() < self.batch_max_records {
                    let _ = tokio::time::timeout(self.batch_max_delay, async {
                        while batch.len() < self.batch_max_records {
                            match self.rx.recv().await {
                                Some(e) => batch.push(e),
                                None => break,
                            }
                        }
                    })
                    .await;
                }
                // Entries have left the channel — reflect that in the gauge.
                self.gauges
                    .channel_depth
                    .fetch_sub(batch.len() as i64, Ordering::Relaxed);
                Ok(Some(batch))
            }
            None => Ok(None),
        }
    }

    async fn commit_batch(&mut self, batch: Vec<WalEntry>) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        // Encode all records into a single buffer — one write() call amortizes
        // kernel-entry cost, one fsync covers the whole batch.
        let mut buf = Vec::with_capacity(batch.len() * 256);
        for entry in &batch {
            encode_record_into(&mut buf, &entry.record);
        }

        let mut acks: Vec<oneshot::Sender<()>> = batch.into_iter().filter_map(|e| e.ack).collect();

        if buf.is_empty() {
            for ack in acks {
                let _ = ack.send(());
            }
            return Ok(());
        }

        // Make sure we have an open segment file, rotating if needed.
        self.ensure_open_file_for(buf.len() as u64).await?;

        // Write + fsync. tokio::fs methods dispatch to spawn_blocking internally.
        let file = self.file.as_mut().expect("file open");
        use tokio::io::AsyncWriteExt;
        file.write_all(&buf).await?;
        file.sync_all().await?;

        self.writer_pos += buf.len() as u64;
        self.records_written += acks.len() as u64;
        self.bytes_written += buf.len() as u64;
        self.rotator.record_written(buf.len() as u64);
        self.gauges
            .bytes_written_total
            .fetch_add(buf.len() as u64, Ordering::Relaxed);

        // Update meta so a crash restarts at the right segment id.
        let cur_id = self.rotator.current_id();
        self.meta.set_meta("last_segment_id", &cur_id.to_string())?;

        // Resolve acks — every record in this batch is now durable.
        for ack in acks.drain(..) {
            let _ = ack.send(());
        }

        Ok(())
    }

    async fn ensure_open_file_for(&mut self, next_write_bytes: u64) -> Result<()> {
        let cap = self.rotator.max_bytes();
        if self.file.is_none() {
            self.open_current().await?;
        } else if self.writer_pos + next_write_bytes > cap {
            self.rotate_now().await?;
        }
        Ok(())
    }

    async fn open_current(&mut self) -> Result<()> {
        let path = self.rotator.current_path();
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        self.writer_pos = file.metadata().await?.len();
        self.file = Some(file);
        tracing::debug!(
            segment = self.rotator.current_id(),
            pos = self.writer_pos,
            "opened segment"
        );
        Ok(())
    }

    async fn rotate_now(&mut self) -> Result<()> {
        // Flush + close current.
        if let Some(mut f) = self.file.take() {
            use tokio::io::AsyncWriteExt;
            f.flush().await?;
            f.sync_all().await?;
        }
        self.rotator.rotate()?;
        self.writer_pos = 0;
        self.open_current().await?;
        tracing::info!(new_segment = self.rotator.current_id(), "wal rotated");
        Ok(())
    }

    /// Background task that prunes segments below the min ingest checkpoint.
    pub fn spawn_pruner(rotator_dir: PathBuf, meta: Arc<WalMeta>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                match meta.min_checkpoint() {
                    Ok(Some(min)) => {
                        let rotator = match SegmentRotator::open(rotator_dir.clone(), u64::MAX) {
                            Ok(r) => r,
                            Err(e) => {
                                tracing::warn!(?e, "pruner: open rotator failed");
                                continue;
                            }
                        };
                        for id in rotator
                            .closed_segment_ids()
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|&id| id < min.segment_id)
                        {
                            tracing::debug!(segment = id, "pruning fully-ingested segment");
                            let _ = rotator.delete_segment(id);
                        }
                    }
                    Ok(None) => {
                        // No checkpoints yet — nothing to prune.
                    }
                    Err(e) => {
                        tracing::warn!(?e, "pruner: min_checkpoint failed");
                    }
                }
            }
        })
    }

    /// Expose the rotator's `current_id` for `WalMeta` initialization.
    pub fn current_segment_id(&self) -> u64 {
        self.rotator.current_id()
    }

    /// Used by tests: synthesize a checkpoint from current writer position.
    pub fn synthetic_checkpoint(&self) -> Checkpoint {
        Checkpoint {
            segment_id: self.rotator.current_id(),
            byte_offset: self.writer_pos,
        }
    }
}

/// Convenience: write a single record immediately (used by tests only).
pub fn _encode_one(record: &RawRecord, protocol: Protocol) -> Vec<u8> {
    let _ = protocol;
    let mut buf = Vec::new();
    encode_record_into(&mut buf, record);
    assert!(buf.len() >= FRAME_HEADER_LEN);
    buf
}
