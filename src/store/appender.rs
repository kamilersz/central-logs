//! Appender-based bulk insert into the DuckDB hot `logs` table (architecture §2b.3).

use chrono::Utc;
use duckdb::{types::TimeUnit, types::Value as DuckValue, Connection};
use serde_json::Value;

use crate::hot::{HotAttribute, HotValue};
use crate::Result;

/// A normalized, enriched log row ready for DuckDB.
#[derive(Debug, Clone, Default)]
pub struct LogRow {
    pub ts: chrono::DateTime<chrono::Utc>,
    pub insert_ts: chrono::DateTime<chrono::Utc>,
    pub source_host: String,
    pub service: String,
    pub level: String,
    pub message: String,
    /// Error-group fingerprint (error tracking; empty for non-error rows).
    pub fingerprint: String,
    pub trace_id: String,
    pub span_id: String,
    pub attributes: Value,
    pub geo_country: String,
    pub raw_len: i32,
    pub protocol: String,
    /// Promoted attribute values, parallel to the configured `hot_attributes`
    /// list. `hot[i]` corresponds to attribute `i`.
    pub hot: Vec<HotValue>,
}

impl LogRow {
    pub fn empty_at(now: chrono::DateTime<chrono::Utc>) -> Self {
        Self {
            ts: now,
            insert_ts: now,
            attributes: Value::Object(Default::default()),
            ..Default::default()
        }
    }
}

/// Insert `rows` using a transaction. The column list is built dynamically from
/// the configured hot-attribute set, so the prepared statement matches the
/// table schema exactly.
pub fn insert_batch(
    conn: &Connection,
    hot: &[HotAttribute],
    rows: &[LogRow],
) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }

    let base_cols = "(ts, insert_ts, source_host, service, level, message, fingerprint, \
                      trace_id, span_id, attributes, geo_country, raw_len, protocol";
    let hot_cols: String = if hot.is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = hot.iter().map(|h| h.name.as_str()).collect();
        format!(", {}", names.join(", "))
    };
    let placeholders = (0..(13 + hot.len()))
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("INSERT INTO logs {base_cols}{hot_cols}) VALUES ({placeholders})");

    let mut inserted = 0usize;
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare(&sql)?;
        for r in rows {
            let attrs = if r.attributes.is_null() {
                Value::Object(Default::default())
            } else {
                r.attributes.clone()
            };
            let attrs_str = serde_json::to_string(&attrs).unwrap_or_else(|_| "{}".into());

            // Build a heterogeneous list of owned DuckDB values. `Value`
            // implements `ToSql`, so this is the simplest way to mix types at
            // runtime when the column set is dynamic.
            let mut cells: Vec<DuckValue> = Vec::with_capacity(13 + hot.len());
            // TIMESTAMP columns: stored as microseconds since epoch (DuckDB's
            // preferred resolution). DateTime<Tz> implements ToSql but doesn't
            // have a Value::From impl, so we go through Timestamp directly.
            cells.push(DuckValue::Timestamp(
                TimeUnit::Microsecond,
                r.ts.timestamp_micros(),
            ));
            cells.push(DuckValue::Timestamp(
                TimeUnit::Microsecond,
                r.insert_ts.timestamp_micros(),
            ));
            cells.push(nullable_str(&r.source_host));   // VARCHAR
            cells.push(nullable_str(&r.service));
            cells.push(nullable_str(&r.level));
            cells.push(nullable_str(&r.message));
            cells.push(nullable_str(&r.fingerprint));
            cells.push(nullable_str(&r.trace_id));
            cells.push(nullable_str(&r.span_id));
            // DuckDB has no dedicated `Json` value variant; a TEXT string is
            // implicitly cast to JSON on insert into a JSON-typed column.
            cells.push(DuckValue::Text(attrs_str));
            cells.push(nullable_str(&r.geo_country));
            cells.push(DuckValue::Int(r.raw_len));
            cells.push(nullable_str(&r.protocol));

            // Promoted hot-attribute values, parallel to the configured list.
            for i in 0..hot.len() {
                let v = r.hot.get(i).unwrap_or(&HotValue::Null);
                cells.push(hot_value_to_duck(v));
            }

            let refs: Vec<&dyn duckdb::ToSql> = cells.iter().map(|c| c as &dyn duckdb::ToSql).collect();
            stmt.execute(refs.as_slice())?;
            inserted += 1;
        }
    }
    tx.commit()?;
    Ok(inserted)
}

#[inline]
fn nullable_str(s: &str) -> DuckValue {
    if s.is_empty() {
        DuckValue::Null
    } else {
        DuckValue::Text(s.to_string())
    }
}

fn hot_value_to_duck(v: &HotValue) -> DuckValue {
    match v {
        HotValue::Null => DuckValue::Null,
        HotValue::Bigint(i) => (*i).into(),
        HotValue::Double(f) => (*f).into(),
        HotValue::Varchar(s) => DuckValue::Text(s.clone()),
        HotValue::Boolean(b) => (*b).into(),
    }
}

/// Wrapper for batched appender-based inserts. We use a thin convenience struct
/// to keep the call sites readable.
pub struct LogAppender;

impl LogAppender {
    /// Bulk-insert via a single transaction. (The DuckDB Appender API would be
    /// marginally faster, but it doesn't accept arbitrary JSON values without
    /// a per-row type dance — using prepared statements is plenty fast for v1.)
    pub fn flush(conn: &Connection, hot: &[HotAttribute], rows: &[LogRow]) -> Result<usize> {
        insert_batch(conn, hot, rows)
    }
}

/// Quick health-check used by dashboards: most-recent insert.
pub fn latest_insert_ts(conn: &Connection) -> Option<chrono::DateTime<Utc>> {
    conn.query_row("SELECT MAX(insert_ts) FROM logs", [], |row| {
        row.get::<_, Option<chrono::DateTime<Utc>>>(0)
    })
    .ok()
    .flatten()
}
