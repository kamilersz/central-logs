//! End-to-end integration test: the architecture's core correctness claim —
//! a log line POSTed to the real HTTP insert endpoint survives the WAL,
//! gets picked up by an ingest worker, lands in DuckDB, is materialized into
//! a rollup, and comes back out through the filter-DSL query layer.
//!
//! Existing tests each cover one stage in isolation: `ingest/worker.rs`
//! drives the WAL/ingest path directly (bypassing HTTP), and
//! `hot_attributes.rs` drives the store directly (bypassing WAL/ingest
//! entirely). Nothing exercises the full chain through the actual axum
//! router, which is what a real client talks to.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use central_logs::ingest::enrich::Enricher;
use central_logs::ingest::parse::Parser as LogParser;
use central_logs::ingest::spawn_ingest_workers;
use central_logs::insert::counters::InsertCountersRef;
use central_logs::insert::http::{router as http_router, InsertState};
use central_logs::query::{parse_filter, ColumnWhitelist};
use central_logs::store::rollup::{rollup_once, RollupConfig, RollupState};
use central_logs::store::Store;
use central_logs::wal::meta::WalMeta;
use central_logs::wal::writer::WalWriter;
use tempfile::tempdir;
use tokio::time::Instant;
use tower::ServiceExt;

/// Mirrors `central_logs::insert::http::InsertResponse`'s JSON shape (that
/// type only derives `Serialize`, so the test decodes the wire format
/// itself rather than adding a test-only `Deserialize` to production code).
#[derive(serde::Deserialize, Debug)]
struct InsertResponseOut {
    accepted: usize,
    rejected: usize,
}

// `Enricher::new` takes an extra leading `geo` argument when the `geoip`
// feature is enabled (see `src/ingest/worker.rs`'s equivalent test) — this
// test targets the default feature set, which does not include it.
#[cfg(not(feature = "geoip"))]
#[tokio::test]
async fn insert_over_http_flows_through_ingest_to_a_filtered_query() {
    let tmp = tempdir().expect("tempdir");

    // --- Bring up the WAL writer + insert HTTP router, exactly as main.rs does.
    let meta = Arc::new(WalMeta::open(&tmp.path().join("meta.redb")).expect("wal meta"));
    let (writer, handle) = WalWriter::new(
        tmp.path().join("wal"),
        16 * 1024 * 1024,
        meta.clone(),
        1024,
        64,
        Duration::from_millis(5),
    )
    .expect("wal writer");
    tokio::spawn(async move {
        let _ = writer.run().await;
    });

    let counters = InsertCountersRef::new();
    let insert_state = InsertState {
        handle: handle.clone(),
        counters: counters.clone(),
        backpressure_timeout: Duration::from_secs(5),
        peer_header: None,
        ingest_paused: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let app = http_router(insert_state);

    // --- POST a batch of NDJSON records through the real HTTP handler
    // (validation → framing → WAL channel → fsync ack), not a shortcut.
    let body = [
        r#"{"service":"orders","level":"info","msg":"order placed","order_id":1}"#,
        r#"{"service":"orders","level":"error","msg":"payment declined","order_id":2}"#,
        r#"{"service":"billing","level":"info","msg":"invoice sent"}"#,
    ]
    .join("\n");

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/logs")
                .header("content-type", "application/x-ndjson")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .expect("http request");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let parsed: InsertResponseOut = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(parsed.accepted, 3, "all three records should be acked: {parsed:?}");
    assert_eq!(parsed.rejected, 0);

    // --- Ingest: parse/enrich/batch-insert into DuckDB, same as main.rs.
    let store = Store::open(&tmp.path().join("cl.duckdb"), tmp.path().join("parquet"), Vec::new())
        .expect("store");
    let enricher = Arc::new(Enricher::new("unknown", "localhost"));
    let parser = Arc::new(LogParser::empty());
    let ingest_handles = spawn_ingest_workers(
        1,
        tmp.path().join("wal"),
        meta.clone(),
        store.clone(),
        enricher,
        parser,
        32,
        Duration::from_millis(20),
        None, // error tracker
    );

    // Poll until all 3 rows have landed (or time out) — ingest is async.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let count: i64 = {
            let conn = store.lock();
            conn.query_row("SELECT COUNT(*) FROM logs", [], |r| r.get(0)).unwrap_or(0)
        };
        if count >= 3 {
            break;
        }
        assert!(Instant::now() < deadline, "timeout waiting for ingest: {count}/3 rows");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for h in ingest_handles {
        h.abort();
    }

    // --- Rollup: the dashboards/alerting read path, not raw `logs`.
    {
        let conn = store.lock();
        let mut state = RollupState::default();
        let n = rollup_once(&conn, &mut state, &RollupConfig::default()).expect("rollup");
        assert!(n >= 3, "rollup should have aggregated at least 3 rows, got {n}");
        let volume: i64 = conn
            .query_row("SELECT COALESCE(SUM(n), 0) FROM rollup_1m", [], |r| r.get(0))
            .unwrap();
        assert_eq!(volume, 3);
    }

    // --- Query: the same filter DSL the HTTP /api/logs and MCP query_logs
    // tool compile, run against the same logs_all view they read.
    let filter = parse_filter("service:orders level:error").expect("parse filter");
    let whitelist = ColumnWhitelist::standard(&[]);
    let (where_sql, params) = filter.to_sql(&whitelist).expect("compile filter");
    assert_eq!(where_sql, "service = ? AND level = ?");

    let conn = store.lock();
    let sql = format!("SELECT message FROM logs_all WHERE {where_sql}");
    let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(|s| s as &dyn duckdb::ToSql).collect();
    let mut stmt = conn.prepare(&sql).unwrap();
    let rows: Vec<String> = stmt
        .query_map(param_refs.as_slice(), |r| r.get(0))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(rows, vec!["payment declined".to_string()]);

    // Sanity: the non-matching records are still there under a broader filter.
    let all_orders = parse_filter("service:orders").expect("parse filter");
    let (where_sql, params) = all_orders.to_sql(&whitelist).unwrap();
    let sql = format!("SELECT COUNT(*) FROM logs_all WHERE {where_sql}");
    let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(|s| s as &dyn duckdb::ToSql).collect();
    let count: i64 = conn.query_row(&sql, param_refs.as_slice(), |r| r.get(0)).unwrap();
    assert_eq!(count, 2);
}
