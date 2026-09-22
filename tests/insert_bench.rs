//! Throwaway micro-benchmark: how fast can we push rows into DuckDB?
//! Run with: cargo test --release --test insert_bench -- --nocapture

use central_logs::ingest::parse::Parser;
use central_logs::store::appender::{insert_batch, LogRow};
use central_logs::store::schema::apply_schema;
use central_logs::{Protocol, RawRecord};
use chrono::Utc;
use duckdb::Connection;
use std::time::Instant;
use tempfile::tempdir;

fn make_record(body: &str) -> RawRecord {
    RawRecord {
        receive_ts: Utc::now(),
        source_addr: "127.0.0.1:1234".into(),
        protocol: Protocol::HttpJson,
        raw: bytes::Bytes::copy_from_slice(body.as_bytes()),
    }
}

fn make_rows(parser: &Parser, n: usize) -> Vec<LogRow> {
    (0..n)
        .map(|i| {
            parser.parse(&make_record(&format!(
                r#"{{"ts":"2026-09-22T06:00:{:02}.{:03}Z","service":"svc-{}","level":"info","msg":"request completed route=/v1/items/{} dur_ms={}","request_id":"req_{i}","user_id":{},"method":"GET","status":200,"host":"node-{}"}}"#,
                i % 60, i % 1000, i % 8, i, i % 500, i % 100000, i % 40
            )))
        })
        .collect()
}

fn bench(name: &str, f: impl FnOnce(&[LogRow]) -> duckdb::Result<usize>) {
    let tmp = tempdir().unwrap();
    let conn = Connection::open(tmp.path().join("bench.duckdb")).unwrap();
    apply_schema(&conn, &tmp.path().join("parquet"), &[]).unwrap();
    let parser = Parser::new(vec![]);
    let rows = make_rows(&parser, 50_000);
    let t0 = Instant::now();
    let n = f(&rows).unwrap_or(0);
    let dt = t0.elapsed();
    if n > 0 {
        println!(
            "{name:>28}: {n} rows in {dt:.2?} -> {:.0} rows/s ({:.2} us/row)",
            n as f64 / dt.as_secs_f64(),
            dt.as_micros() as f64 / n as f64
        );
    }
}

#[test]
fn bench_variants_detailed() {
    let parser = Parser::new(vec![]);

    // -- variant: multirow-128 baseline (current code) --
    {
        let tmp = tempdir().unwrap();
        let conn = Connection::open(tmp.path().join("b.duckdb")).unwrap();
        apply_schema(&conn, &tmp.path().join("parquet"), &[]).unwrap();
        let rows = make_rows(&parser, 50_000);
        let t0 = Instant::now();
        for c in rows.chunks(8192) {
            insert_batch(&conn, &[], c).unwrap();
        }
        let dt = t0.elapsed();
        println!(
            "multirow-128 (current) : {} rows in {dt:.2?} -> {:.0} rows/s",
            rows.len(),
            rows.len() as f64 / dt.as_secs_f64()
        );
    }

    // -- variant: multirow-128 on threads=8 --
    {
        let tmp = tempdir().unwrap();
        let conn = Connection::open(tmp.path().join("b.duckdb")).unwrap();
        apply_schema(&conn, &tmp.path().join("parquet"), &[]).unwrap();
        conn.execute_batch("SET threads TO 8").unwrap();
        let rows = make_rows(&parser, 50_000);
        let t0 = Instant::now();
        for c in rows.chunks(8192) {
            insert_batch(&conn, &[], c).unwrap();
        }
        let dt = t0.elapsed();
        println!(
            "multirow-128 threads=8 : {} rows in {dt:.2?} -> {:.0} rows/s",
            rows.len(),
            rows.len() as f64 / dt.as_secs_f64()
        );
    }

    // -- variant: multirow-1024 --
    {
        let tmp = tempdir().unwrap();
        let conn = Connection::open(tmp.path().join("b.duckdb")).unwrap();
        apply_schema(&conn, &tmp.path().join("parquet"), &[]).unwrap();
        let rows = make_rows(&parser, 50_000);
        let t0 = Instant::now();
        for c in rows.chunks(1024) {
            insert_chunk_n(&conn, c, 128).unwrap();
        }
        let dt = t0.elapsed();
        println!(
            "multirow via 1024-txn  : {} rows in {dt:.2?} -> {:.0} rows/s",
            rows.len(),
            rows.len() as f64 / dt.as_secs_f64()
        );
    }

    // -- variant: raw SQL multirow WITHOUT LogRow conversion (isolate conversion cost) --
    {
        let tmp = tempdir().unwrap();
        let conn = Connection::open(tmp.path().join("b.duckdb")).unwrap();
        conn.execute_batch("CREATE TABLE t(a TIMESTAMP, b VARCHAR, c JSON);")
            .unwrap();
        let rows = make_rows(&parser, 50_000);
        let t0 = Instant::now();
        let tx = conn.unchecked_transaction().unwrap();
        for c in rows.chunks(128) {
            let sql = format!(
                "INSERT INTO t VALUES {}",
                vec!["(?,?,?)"; c.len()].join(",")
            );
            let mut stmt = tx.prepare(&sql).unwrap();
            let mut params: Vec<duckdb::types::Value> = Vec::with_capacity(c.len() * 3);
            for r in c {
                params.push(duckdb::types::Value::Timestamp(
                    duckdb::types::TimeUnit::Microsecond,
                    r.ts.timestamp_micros(),
                ));
                params.push(duckdb::types::Value::Text(r.service.clone()));
                params.push(duckdb::types::Value::Text("{}".into()));
            }
            let refs: Vec<&dyn duckdb::ToSql> =
                params.iter().map(|p| p as &dyn duckdb::ToSql).collect();
            stmt.execute(refs.as_slice()).unwrap();
        }
        tx.commit().unwrap();
        let dt = t0.elapsed();
        println!(
            "raw 3-col multirow     : {} rows in {dt:.2?} -> {:.0} rows/s",
            rows.len(),
            rows.len() as f64 / dt.as_secs_f64()
        );
    }

    // -- variant: DuckDB Appender --
    {
        let tmp = tempdir().unwrap();
        let conn = Connection::open(tmp.path().join("b.duckdb")).unwrap();
        apply_schema(&conn, &tmp.path().join("parquet"), &[]).unwrap();
        let rows = make_rows(&parser, 50_000);
        let t0 = Instant::now();
        {
            let mut app = conn.appender("logs").unwrap();
            for r in &rows {
                use duckdb::types::{TimeUnit, Value as V};
                let cells: Vec<V> = vec![
                    V::Timestamp(TimeUnit::Microsecond, r.ts.timestamp_micros()),
                    V::Timestamp(TimeUnit::Microsecond, r.insert_ts.timestamp_micros()),
                    nullable(&r.source_host),
                    nullable(&r.service),
                    nullable(&r.level),
                    nullable(&r.message),
                    nullable(&r.fingerprint),
                    nullable(&r.trace_id),
                    nullable(&r.span_id),
                    V::Text(serde_json::to_string(&r.attributes).unwrap_or_default()),
                    nullable(&r.geo_country),
                    V::Int(r.raw_len),
                    nullable(&r.protocol),
                ];
                let refs: Vec<&dyn duckdb::ToSql> =
                    cells.iter().map(|c| c as &dyn duckdb::ToSql).collect();
                app.append_row(refs.as_slice()).unwrap();
            }
            app.flush().unwrap();
        }
        let dt = t0.elapsed();
        println!(
            "duckdb Appender        : {} rows in {dt:.2?} -> {:.0} rows/s",
            rows.len(),
            rows.len() as f64 / dt.as_secs_f64()
        );
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM logs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count as usize, rows.len());
    }
}

fn insert_chunk_n(
    conn: &Connection,
    rows: &[LogRow],
    per_stmt: usize,
) -> central_logs::Result<usize> {
    // 1024-row transactions of 128-row statements to test txn granularity
    let mut inserted = 0;
    let tx = conn.unchecked_transaction()?;
    for chunk in rows.chunks(per_stmt) {
        let sql = format!(
            "INSERT INTO logs (ts, insert_ts, source_host, service, level, message, fingerprint, trace_id, span_id, attributes, geo_country, raw_len, protocol) VALUES {}",
            vec!["(?,?,?,?,?,?,?,?,?,?,?,?,?)"; chunk.len()].join(", ")
        );
        let mut stmt = tx.prepare(&sql)?;
        let mut params: Vec<duckdb::types::Value> = Vec::with_capacity(chunk.len() * 13);
        for r in chunk {
            params.push(duckdb::types::Value::Timestamp(
                duckdb::types::TimeUnit::Microsecond,
                r.ts.timestamp_micros(),
            ));
            params.push(duckdb::types::Value::Timestamp(
                duckdb::types::TimeUnit::Microsecond,
                r.insert_ts.timestamp_micros(),
            ));
            params.push(text(&r.source_host));
            params.push(text(&r.service));
            params.push(text(&r.level));
            params.push(text(&r.message));
            params.push(text(&r.fingerprint));
            params.push(text(&r.trace_id));
            params.push(text(&r.span_id));
            params.push(duckdb::types::Value::Text(
                serde_json::to_string(&r.attributes).unwrap_or_default(),
            ));
            params.push(text(&r.geo_country));
            params.push(duckdb::types::Value::Int(r.raw_len));
            params.push(text(&r.protocol));
        }
        let refs: Vec<&dyn duckdb::ToSql> =
            params.iter().map(|p| p as &dyn duckdb::ToSql).collect();
        stmt.execute(refs.as_slice())?;
        inserted += chunk.len();
    }
    tx.commit()?;
    Ok(inserted)
}

fn text(s: &str) -> duckdb::types::Value {
    if s.is_empty() {
        duckdb::types::Value::Null
    } else {
        duckdb::types::Value::Text(s.to_string())
    }
}

fn nullable(s: &str) -> duckdb::types::Value {
    text(s)
}

#[test]
fn bench_stall_hunt() {
    // 1M rows in 8192-row commits vs a file DB — print per-commit times to
    // expose checkpoint stalls (multi-second hitches between fast commits).
    let tmp = tempdir().unwrap();
    let conn = Connection::open(tmp.path().join("stall.duckdb")).unwrap();
    apply_schema(&conn, &tmp.path().join("parquet"), &[]).unwrap();
    let parser = Parser::new(vec![]);
    let n = 1_000_000;
    let rows: Vec<LogRow> = (0..n)
        .map(|i| {
            parser.parse(&make_record(&format!(
                r#"{{"ts":"2026-09-22T06:00:{:02}.{:03}Z","service":"svc-{}","level":"{}","msg":"request completed route=/v1/items/{} dur_ms={}","request_id":"req_{i}","user_id":{},"method":"GET","status":200,"host":"node-{}"}}"#,
                i % 60, i % 1000, i % 8, if i % 10 == 0 { "error" } else { "info" }, i, i % 500, i % 100000, i % 40
            )))
        })
        .collect();
    let t0 = Instant::now();
    let mut slowest = (0usize, 0u128);
    let mut total_slow = 0u128;
    for (ci, chunk) in rows.chunks(8192).enumerate() {
        let c0 = Instant::now();
        insert_batch(&conn, &[], chunk).unwrap();
        let ms = c0.elapsed().as_millis();
        if ms > slowest.1 {
            slowest = (ci, ms);
        }
        if ms > 500 {
            total_slow += ms;
            println!("commit #{ci}: {ms} ms  <-- slow");
        }
    }
    let dt = t0.elapsed();
    println!(
        "stall-hunt: {n} rows in {dt:.2?} -> {:.0} rows/s; slowest commit #{: >3} = {} ms; total slow-ms = {total_slow}",
        n as f64 / dt.as_secs_f64(),
        slowest.0,
        slowest.1
    );
}
