//! Integration test: hot-attribute columns are queryable end-to-end.
//!
//! Verifies the "telemetry optimization" path: configure hot attributes, apply
//! schema, parse a JSON envelope that includes the hot keys, insert via the
//! appender, then filter by the promoted column. The filter should hit a real
//! column (zonemap-prunable) rather than `attributes->>'$.user_id'`.

use central_logs::hot::HotAttribute;
use central_logs::ingest::parse::Parser;
use central_logs::store::appender::insert_batch;
use central_logs::store::schema::apply_schema;
use central_logs::{Protocol, RawRecord};
use chrono::Utc;
use duckdb::Connection;
use tempfile::tempdir;

fn make_record(body: &str) -> RawRecord {
    RawRecord {
        receive_ts: Utc::now(),
        source_addr: "127.0.0.1:1234".into(),
        protocol: Protocol::HttpJson,
        raw: bytes::Bytes::copy_from_slice(body.as_bytes()),
    }
}

#[test]
fn hot_columns_are_filterable_end_to_end() {
    let tmp = tempdir().unwrap();
    let db_path = tmp.path().join("test.duckdb");
    let parquet_dir = tmp.path().join("parquet");
    std::fs::create_dir_all(&parquet_dir).unwrap();

    let conn = Connection::open(&db_path).unwrap();
    let hot = vec![
        HotAttribute::parse_shorthand("user_id:bigint").unwrap(),
        HotAttribute::parse_shorthand("env:varchar").unwrap(),
        HotAttribute::parse_shorthand("is_canary:boolean").unwrap(),
    ];
    apply_schema(&conn, &parquet_dir, &hot).unwrap();

    let parser = Parser::new(hot.clone());

    let bodies = [
        r#"{"service":"api","msg":"a","user_id":42,"env":"prod","is_canary":false}"#,
        r#"{"service":"api","msg":"b","user_id":7,"env":"prod","is_canary":true}"#,
        r#"{"service":"api","msg":"c","user_id":42,"env":"staging","is_canary":false}"#,
        // Missing user_id → column should be NULL, not 0.
        r#"{"service":"api","msg":"d","env":"prod"}"#,
    ];
    let rows: Vec<_> = bodies.iter().map(|b| parser.parse(&make_record(b))).collect();
    let inserted = insert_batch(&conn, &hot, &rows).unwrap();
    assert_eq!(inserted, 4);

    // Filter by BIGINT hot column — exact-match filter goes straight to the column.
    let count_user_42: i64 = conn
        .query_row("SELECT COUNT(*) FROM logs WHERE user_id = 42", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count_user_42, 2);

    // Filter by VARCHAR hot column.
    let count_staging: i64 = conn
        .query_row("SELECT COUNT(*) FROM logs WHERE env = 'staging'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count_staging, 1);

    // Filter by BOOLEAN hot column.
    let count_canary: i64 = conn
        .query_row("SELECT COUNT(*) FROM logs WHERE is_canary = true", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count_canary, 1);

    // NULL handling: missing user_id should not match `user_id = 0`.
    let count_zero: i64 = conn
        .query_row("SELECT COUNT(*) FROM logs WHERE user_id = 0", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count_zero, 0);
    let count_null: i64 = conn
        .query_row("SELECT COUNT(*) FROM logs WHERE user_id IS NULL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count_null, 1);

    // The promoted keys should NOT be present in the JSON residue — that's the
    // whole point of popping them out (smaller attributes column, faster scans
    // over arbitrary-key queries).
    let residue_keys: Vec<String> = conn
        .prepare("SELECT attributes FROM logs WHERE user_id = 42 ORDER BY message")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(residue_keys.len(), 2);
    for json in &residue_keys {
        // Original payload also had `service` and `msg` — those are RESERVED so
        // they're not in attributes either. So attributes should be empty (or
        // close to it).
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(
            v.get("user_id").is_none(),
            "user_id should be popped out of residue, got {json}"
        );
        assert!(
            v.get("env").is_none(),
            "env should be popped out of residue, got {json}"
        );
    }
}

#[test]
fn hot_columns_survive_restart_and_alter() {
    // Start with no hot attrs, insert a row, then "restart" with hot attrs
    // configured. ALTER TABLE ADD COLUMN IF NOT EXISTS should make the new
    // column queryable without losing existing rows.
    let tmp = tempdir().unwrap();
    let db_path = tmp.path().join("test.duckdb");
    let parquet_dir = tmp.path().join("parquet");
    std::fs::create_dir_all(&parquet_dir).unwrap();

    let conn = Connection::open(&db_path).unwrap();
    apply_schema(&conn, &parquet_dir, &[]).unwrap();
    let parser_v1 = Parser::new(vec![]);
    let rows = vec![parser_v1.parse(&make_record(
        r#"{"service":"api","msg":"first","user_id":99}"#,
    ))];
    insert_batch(&conn, &[], &rows).unwrap();

    // Restart: open a fresh connection with hot attrs configured.
    drop(conn);
    let conn = Connection::open(&db_path).unwrap();
    let hot = vec![HotAttribute::parse_shorthand("user_id:bigint").unwrap()];
    apply_schema(&conn, &parquet_dir, &hot).unwrap();

    // Old row is still there; its user_id column is NULL (added late).
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM logs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total, 1);
    let null_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM logs WHERE user_id IS NULL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(null_count, 1);

    // New rows get the column populated correctly.
    let parser_v2 = Parser::new(hot.clone());
    let rows = vec![parser_v2.parse(&make_record(
        r#"{"service":"api","msg":"second","user_id":100}"#,
    ))];
    insert_batch(&conn, &hot, &rows).unwrap();
    let count_100: i64 = conn
        .query_row("SELECT COUNT(*) FROM logs WHERE user_id = 100", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count_100, 1);
}

#[test]
fn filter_on_hot_column_uses_column_not_json() {
    // Sanity: filtering by the hot column should return rows where the *value*
    // matches even if the JSON residue no longer contains the key. This proves
    // the column is the source of truth, not `attributes->>'$.user_id'`.
    let tmp = tempdir().unwrap();
    let db_path = tmp.path().join("test.duckdb");
    let parquet_dir = tmp.path().join("parquet");
    std::fs::create_dir_all(&parquet_dir).unwrap();

    let conn = Connection::open(&db_path).unwrap();
    let hot = vec![HotAttribute::parse_shorthand("user_id:bigint").unwrap()];
    apply_schema(&conn, &parquet_dir, &hot).unwrap();

    let parser = Parser::new(hot.clone());
    let rows = vec![parser.parse(&make_record(
        r#"{"msg":"x","user_id":123}"#,
    ))];
    insert_batch(&conn, &hot, &rows).unwrap();

    // user_id is popped out of residue; verify the column-only filter still works.
    let via_col: i64 = conn
        .query_row("SELECT user_id FROM logs WHERE user_id = 123", [], |r| r.get(0))
        .unwrap();
    assert_eq!(via_col, 123);

    // And the JSON path should fail (returns NULL/empty) because the key was popped.
    let via_json: Option<String> = conn
        .query_row(
            "SELECT attributes->>'$.user_id' FROM logs WHERE user_id = 123",
            [],
            |r| r.get(0),
        )
        .ok()
        .flatten();
    assert!(
        via_json.is_none() || via_json.as_deref() == Some(""),
        "user_id should be absent from attributes residue, got: {via_json:?}"
    );
}
