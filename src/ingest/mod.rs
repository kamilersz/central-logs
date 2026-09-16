//! Background ingest workers (architecture §2b).

pub mod enrich;
pub mod parse;
pub mod worker;

pub use worker::{spawn_ingest_workers, IngestStats};
