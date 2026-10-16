//! Appender-based bulk insert into the DuckDB hot `logs` table (architecture §2b.3).

use chrono::Utc;
use duckdb::{types::TimeUnit, types::Value as DuckValue, Connection};
use serde_json::Value;

use crate::hot::{HotAttribute, HotValue};
use crate::Result;

/// A normalized, enriched website-analytics row ready for DuckDB
/// (PLAN-WEB-ANALYTICS.md W1, schema §3.3). The source IP is deliberately
/// absent — it is only ever an input to `visitor_id`.
#[derive(Debug, Clone, Default)]
pub struct WebRow {
    pub ts: chrono::DateTime<chrono::Utc>,
    pub insert_ts: chrono::DateTime<chrono::Utc>,
    pub site_id: String,
    /// pageview|custom|outbound|download|engagement|vital|error
    pub event: String,
    /// Custom-event / vital-metric / error name.
    pub name: String,
    pub value: Option<f64>,
    pub hostname: String,
    pub path: String,
    pub query: String,
    pub referrer: String,
    pub source: String,
    pub medium: String,
    pub utm: Value,
    /// Daily-rotating visitor hash (UBIGINT). 0 = unavailable salt.
    pub visitor_id: u64,
    /// Filled by the sessionizer job (W3), never at ingest.
    pub session_id: Option<u64>,
    /// Keyed per-site hash of the authenticated user header; None = anonymous.
    pub user_id: Option<u64>,
    /// Operator's own visit (matched `exclude_ips` at the edge). Excluded by
    /// default in every analytics query; re-includable for auditing.
    pub is_internal: bool,
    pub country: String,
    /// State/province ISO code (GeoIP subdivision, e.g. `JK`); empty when unknown.
    pub region: String,
    /// City name (GeoIP); empty when unknown.
    pub city: String,
    /// desktop|mobile|tablet|bot
    pub device: String,
    pub browser: String,
    pub os: String,
    pub screen_w: Option<u16>,
    pub is_bot: bool,
    pub props: Value,
    /// Same id space as `logs.trace_id` → join backend logs.
    pub trace_id: String,
    pub span_id: String,
}
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

/// Insert `rows` using the DuckDB Appender interface (column list built
/// dynamically from the configured hot-attribute set).
///
/// The Appender writes straight into column segments, skipping SQL parsing,
/// planning and per-statement execution overhead — measured ~370K rows/s vs
/// ~9K rows/s for 128-row multi-row VALUES statements (see
/// tests/insert_bench.rs). Note: unlike the previous transactional
/// multi-row-VALUES path, a failed append may leave a partially-appended
/// flush; the worker only advances its checkpoint on success, so retries
/// can duplicate rows after an insert error (crash-restart re-ingest
/// already had this property — at-least-once delivery).
pub fn insert_batch(conn: &Connection, hot: &[HotAttribute], rows: &[LogRow]) -> Result<usize> {
    insert_logs_in_tx(conn, hot, rows)
}

/// Insert log rows inside an existing transaction (PLAN-WEB-ANALYTICS W1:
/// the ingest worker commits `logs` and `web_events` ATOMICALLY, so a web
/// insert failure can no longer cause a logs re-ingest duplication on retry).
/// The DuckDB appender participates in an outer `unchecked_transaction`
/// (verified empirically: rollback undoes appended rows).
pub fn insert_logs_in_tx(
    tx: &Connection,
    hot: &[HotAttribute],
    rows: &[LogRow],
) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }

    let mut cols: Vec<&str> = BASE_COLUMNS.to_vec();
    cols.extend(hot.iter().map(|h| h.name.as_str()));

    let mut appender = tx.appender_with_columns("logs", &cols)?;
    for r in rows {
        let mut cells: Vec<DuckValue> = Vec::with_capacity(13 + hot.len());
        push_row_cells(&mut cells, r, hot);
        let refs: Vec<&dyn duckdb::ToSql> = cells.iter().map(|c| c as &dyn duckdb::ToSql).collect();
        appender.append_row(refs.as_slice())?;
    }
    appender.flush()?;
    Ok(rows.len())
}

/// Insert web-analytics rows inside the worker's shared transaction.
pub fn insert_web_in_tx(tx: &Connection, rows: &[WebRow]) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let mut appender = tx.appender_with_columns("web_events", WEB_COLUMNS)?;
    for r in rows {
        let cells = web_row_cells(r);
        let refs: Vec<&dyn duckdb::ToSql> = cells.iter().map(|c| c as &dyn duckdb::ToSql).collect();
        appender.append_row(refs.as_slice())?;
    }
    appender.flush()?;
    Ok(rows.len())
}

/// `web_events` column order (must match the DDL in schema.rs).
pub const WEB_COLUMNS: &[&str] = &[
    "ts",
    "insert_ts",
    "site_id",
    "event",
    "name",
    "value",
    "hostname",
    "path",
    "query",
    "referrer",
    "source",
    "medium",
    "utm",
    "visitor_id",
    "session_id",
    "user_id",
    "is_internal",
    "country",
    "region",
    "city",
    "device",
    "browser",
    "os",
    "screen_w",
    "is_bot",
    "props",
    "trace_id",
    "span_id",
];

/// Build the DuckDB values for one `WebRow`, in `WEB_COLUMNS` order.
fn web_row_cells(r: &WebRow) -> Vec<DuckValue> {
    vec![
        DuckValue::Timestamp(TimeUnit::Microsecond, r.ts.timestamp_micros()),
        DuckValue::Timestamp(TimeUnit::Microsecond, r.insert_ts.timestamp_micros()),
        nullable_str(&r.site_id),
        nullable_str(&r.event),
        nullable_str(&r.name),
        match r.value {
            Some(v) => DuckValue::Double(v),
            None => DuckValue::Null,
        },
        nullable_str(&r.hostname),
        nullable_str(&r.path),
        nullable_str(&r.query),
        nullable_str(&r.referrer),
        nullable_str(&r.source),
        nullable_str(&r.medium),
        json_cell(&r.utm),
        DuckValue::UBigInt(r.visitor_id),
        match r.session_id {
            Some(v) => DuckValue::UBigInt(v),
            None => DuckValue::Null,
        },
        match r.user_id {
            Some(v) => DuckValue::UBigInt(v),
            None => DuckValue::Null,
        },
        DuckValue::Boolean(r.is_internal),
        nullable_str(&r.country),
        nullable_str(&r.region),
        nullable_str(&r.city),
        nullable_str(&r.device),
        nullable_str(&r.browser),
        nullable_str(&r.os),
        match r.screen_w {
            Some(v) => DuckValue::USmallInt(v),
            None => DuckValue::Null,
        },
        DuckValue::Boolean(r.is_bot),
        json_cell(&r.props),
        nullable_str(&r.trace_id),
        nullable_str(&r.span_id),
    ]
}

/// JSON column cell: DuckDB has no dedicated `Json` value variant; a TEXT
/// string is implicitly cast on insert into a JSON-typed column.
fn json_cell(v: &Value) -> DuckValue {
    if v.is_null() {
        DuckValue::Null
    } else {
        DuckValue::Text(serde_json::to_string(v).unwrap_or_else(|_| "{}".into()))
    }
}

const BASE_COLUMNS: &[&str] = &[
    "ts",
    "insert_ts",
    "source_host",
    "service",
    "level",
    "message",
    "fingerprint",
    "trace_id",
    "span_id",
    "attributes",
    "geo_country",
    "raw_len",
    "protocol",
];

/// Build the DuckDB values for one `LogRow`, in column order.
fn push_row_cells(cells: &mut Vec<DuckValue>, r: &LogRow, hot: &[HotAttribute]) {
    cells.push(DuckValue::Timestamp(
        TimeUnit::Microsecond,
        r.ts.timestamp_micros(),
    ));
    cells.push(DuckValue::Timestamp(
        TimeUnit::Microsecond,
        r.insert_ts.timestamp_micros(),
    ));
    cells.push(nullable_str(&r.source_host)); // VARCHAR
    cells.push(nullable_str(&r.service));
    cells.push(nullable_str(&r.level));
    cells.push(nullable_str(&r.message));
    cells.push(nullable_str(&r.fingerprint));
    cells.push(nullable_str(&r.trace_id));
    cells.push(nullable_str(&r.span_id));
    // DuckDB has no dedicated `Json` value variant; a TEXT string is
    // implicitly cast to JSON on insert into a JSON-typed column.
    let attrs_str = if r.attributes.is_null() {
        "{}".to_string()
    } else {
        serde_json::to_string(&r.attributes).unwrap_or_else(|_| "{}".into())
    };
    cells.push(DuckValue::Text(attrs_str));
    cells.push(nullable_str(&r.geo_country));
    cells.push(DuckValue::Int(r.raw_len));
    cells.push(nullable_str(&r.protocol));

    // Promoted hot-attribute values, parallel to the configured list.
    for i in 0..hot.len() {
        let v = r.hot.get(i).unwrap_or(&HotValue::Null);
        cells.push(hot_value_to_duck(v));
    }
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
