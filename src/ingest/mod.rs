//! Background ingest workers (architecture §2b).

pub mod enrich;
pub mod parse;
pub mod web;
pub mod worker;

pub use web::WebParser;
pub use worker::{spawn_ingest_workers, IngestStats};
