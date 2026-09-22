//! Crate-wide error type.

use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("wal io error: {0}")]
    WalIo(#[from] std::io::Error),

    #[error(
        "wal frame corrupted: crc mismatch (expected {expected}, got {got}) at offset {offset}"
    )]
    WalCrc {
        expected: u32,
        got: u32,
        offset: u64,
    },

    #[error("wal frame truncated at offset {0}")]
    WalTruncated(u64),

    #[error("redb error: {0}")]
    Redb(#[from] redb::DatabaseError),

    #[error("redb transaction error: {0}")]
    RedbTx(#[from] redb::TransactionError),

    #[error("redb storage error: {0}")]
    RedbStorage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    RedbTable(#[from] redb::TableError),

    #[error("redb commit error: {0}")]
    RedbCommit(#[from] redb::CommitError),

    #[error("duckdb error: {0}")]
    Duckdb(#[from] duckdb::Error),

    #[error("serde json error: {0}")]
    SerdeJson(#[from] serde_json::Error),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("channel send failed (insert layer shutting down)")]
    ChannelClosed,

    #[error("config error: {0}")]
    Config(String),

    #[cfg(feature = "object-storage")]
    #[error("object storage error: {0}")]
    ObjectStorage(#[from] object_store::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl Error {
    pub fn invalid_input(msg: impl Into<String>) -> Self {
        Error::InvalidInput(msg.into())
    }

    pub fn config(msg: impl Into<String>) -> Self {
        Error::Config(msg.into())
    }
}
