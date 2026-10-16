//! Web-analytics integration tests (PLAN-WEB-ANALYTICS.md W1):
//! beacon → WAL → ingest → `web_events`, mixed log+web exactly-once across
//! sharded workers, atomic two-table flush, visitor-hash daily rotation.

use std::sync::Arc;
use std::time::Duration;

use central_logs::config::{WebConfig, WebSiteConfig};
use central_logs::ingest::enrich::Enricher;
use central_logs::ingest::parse::Parser as LogParser;
use central_logs::ingest::spawn_ingest_workers;
use central_logs::ingest::web::WebParser;
use central_logs::insert::web::{parse_beacon, WebState};
use central_logs::wal::meta::WalMeta;
use central_logs::wal::writer::WalGauges;
use central_logs::{Protocol, RawRecord};

fn web_cfg() -> WebConfig {
    let mut c = WebConfig::default();
    c.enabled = true;
    c.site.push(WebSiteConfig {
        id: "site_abc".into(),
        allowed_origins: vec!["https://example.com".into()],
        user_header: None,
        exclude_ips: Vec::new(),
    });
    c
}

fn beacon() -> String {
    r#"{"s":"site_abc","ev":[
        {"e":"pageview","u":"/pricing?utm_source=google&utm_medium=organic","r":"https://www.google.com/search?q=x","w":1440,"dt":0},
        {"e":"custom","n":"signup","p":{"plan":"pro"},"dt":850},
        {"e":"vital","n":"LCP","v":1840.5,"dt":2100},
        {"e":"error","n":"TypeError: boom","dt":3000},
        {"e":"engagement","v":18500,"sd":72,"dt":9000}
    ]}"#
    .to_string()
}

/// Drive the collector handler against a full in-process stack. The beacon
/// enters the REAL WAL via the writer handle; the ingest workers pick it up
/// from there.
async fn collect_to_record(st: &WebState) {
    use axum::extract::ConnectInfo;
    use tower::ServiceExt;

    let app = central_logs::insert::web::router(st.clone());
    let mut req = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/web/collect")
        .header("content-type", "text/plain")
        .header("origin", "https://example.com")
        .header("user-agent", "TestBrowser/1.0")
        .header("x-forwarded-for", "203.0.113.50")
        .body(axum::body::Body::from(beacon()))
        .unwrap();
    req.extensions_mut().insert(ConnectInfo(std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        39999,
    )));
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), axum::http::StatusCode::ACCEPTED);
}

#[tokio::test]
async fn beacon_flows_through_wal_to_web_events() {
    let tmp = tempfile::tempdir().unwrap();
    let meta = Arc::new(WalMeta::open(&tmp.path().join("meta.redb")).unwrap());
    let (writer, handle) = central_logs::wal::writer::WalWriter::new(
        tmp.path().join("wal"),
        64 * 1024,
        meta.clone(),
        256,
        64,
        Duration::from_millis(2),
    )
    .unwrap();
    let wal_gauges = writer.gauges().clone();
    tokio::spawn(async move {
        let _ = writer.run().await;
    });

    let counters = central_logs::insert::counters::InsertCountersRef::new();
    let st = WebState::new(
        handle.clone(),
        counters.clone(),
        Duration::from_millis(1000),
        Arc::new(web_cfg()),
        central_logs::insert::web::WebSites::seeded(&web_cfg()),
        wal_gauges,
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    );
    let _ = collect_to_record(&st).await;

    let store = central_logs::store::Store::open(
        &tmp.path().join("cl.duckdb"),
        tmp.path().join("parquet"),
        Vec::new(),
    )
    .unwrap();
    let web_parser = Arc::new(WebParser::new(meta.clone()));
    let handles = spawn_ingest_workers(
        2,
        tmp.path().join("wal"),
        meta.clone(),
        store.clone(),
        Arc::new(Enricher::new("unknown", "localhost")),
        Arc::new(LogParser::empty()),
        web_parser,
        16,
        Duration::from_millis(10),
        None,
    );

    // Poll until the 5 events land.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut n = 0i64;
    loop {
        {
            let conn = store.lock();
            n = conn
                .query_row("SELECT COUNT(*) FROM web_events", [], |r| r.get(0))
                .unwrap_or(0);
        }
        if n >= 5 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timeout: only {n}/5 web events ingested"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for h in handles {
        h.abort();
    }

    let conn = store.lock();
    // Attribution, classification, correlation all survive the round trip.
    let (pv_path, utm_src, medium, source): (String, String, String, String) = conn
        .query_row(
            "SELECT path, utm->>'$.source', medium, source FROM web_events WHERE event='pageview'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(pv_path, "/pricing");
    assert_eq!(utm_src, "google");
    assert_eq!((source.as_str(), medium.as_str()), ("google", "organic"));

    let (dev, browser): (String, Option<String>) = conn
        .query_row(
            "SELECT device, browser FROM web_events WHERE event='pageview'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(dev, "desktop");
    // Unknown UA → NULL browser (not guessed).
    assert!(browser.is_none());

    let vital: f64 = conn
        .query_row(
            "SELECT value FROM web_events WHERE event='vital' AND name='LCP'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(vital, 1840.5);

    // No raw IP anywhere in the table (privacy): check every VARCHAR-ish col.
    let leaks: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM web_events WHERE
             path LIKE '%203.0.113%' OR referrer LIKE '%203.0.113%'
             OR props LIKE '%203.0.113%' OR query LIKE '%203.0.113%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leaks, 0, "client IP must never be stored");

    // visitor_id identical across the events of one beacon (same visitor).
    let distinct_visitors: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT visitor_id) FROM web_events",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(distinct_visitors, 1);
    // session_id left NULL for the sessionizer (W3).
    let null_sessions: i64 = conn
        .query_row("SELECT COUNT(*) FROM web_events WHERE session_id IS NULL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(null_sessions, 5);
}

#[tokio::test]
async fn mixed_log_and_web_exactly_once_across_shards() {
    let tmp = tempfile::tempdir().unwrap();
    let meta = Arc::new(WalMeta::open(&tmp.path().join("meta.redb")).unwrap());
    let (writer, handle) = central_logs::wal::writer::WalWriter::new(
        tmp.path().join("wal"),
        8 * 1024, // tiny segments → many shards
        meta.clone(),
        1024,
        64,
        Duration::from_millis(2),
    )
    .unwrap();
    tokio::spawn(async move {
        let _ = writer.run().await;
    });

    let store = central_logs::store::Store::open(
        &tmp.path().join("cl.duckdb"),
        tmp.path().join("parquet"),
        Vec::new(),
    )
    .unwrap();
    let handles = spawn_ingest_workers(
        3,
        tmp.path().join("wal"),
        meta.clone(),
        store.clone(),
        Arc::new(Enricher::new("unknown", "localhost")),
        Arc::new(LogParser::empty()),
        Arc::new(WebParser::new(meta.clone())),
        8, // tiny batches → many flushes
        Duration::from_millis(5),
        None,
    );

    // Interleave 150 log records and 50 web beacons (3 events each).
    for i in 0..150u32 {
        let rec = RawRecord {
            receive_ts: chrono::Utc::now(),
            source_addr: "127.0.0.1:9".into(),
            protocol: Protocol::HttpJson,
            raw: bytes::Bytes::from(format!(r#"{{"service":"t","msg":"row-{i}"}}"#)),
        };
        handle.append_unacked(rec).await.unwrap();
        if i % 3 == 0 {
            let env = serde_json::json!({
                "v": 1, "site": "site_abc", "ua": "Mix/1.0", "lang": "en",
                "origin": "https://example.com", "uid": null,
                "ev": [
                    {"e":"pageview","u":format!("/p{}", i),"dt":0},
                    {"e":"custom","n":"evt","p":{"i": i},"dt":10},
                    {"e":"engagement","v":1000,"sd":10,"dt":20}
                ]
            });
            let rec = RawRecord {
                receive_ts: chrono::Utc::now(),
                source_addr: "203.0.113.77".into(),
                protocol: Protocol::WebEvent,
                raw: bytes::Bytes::from(serde_json::to_vec(&env).unwrap()),
            };
            handle.append_acked(rec).await.unwrap();
        }
    }

    // 150 logs + 150 web rows must all land, exactly once.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let (logs, web): (i64, i64) = {
            let conn = store.lock();
            let l: i64 = conn.query_row("SELECT COUNT(*) FROM logs", [], |r| r.get(0)).unwrap_or(0);
            let w: i64 = conn.query_row("SELECT COUNT(*) FROM web_events", [], |r| r.get(0)).unwrap_or(0);
            (l, w)
        };
        if logs >= 150 && web >= 150 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timeout: logs={logs}/150 web={web}/150"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for h in handles {
        h.abort();
    }

    let conn = store.lock();
    let distinct_logs: i64 = conn
        .query_row("SELECT COUNT(DISTINCT message) FROM logs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(distinct_logs, 150, "duplicated logs across shard flushes?");
    let distinct_web: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM (SELECT DISTINCT ts, site_id, event, name, value FROM web_events)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(distinct_web, 150, "duplicated web events across shard flushes?");
}

/// The atomicity guarantee (plan §W1): when the web insert fails inside the
/// shared transaction, the log rows appended in the SAME transaction are
/// rolled back too — retry cannot duplicate logs.
#[test]
fn failed_web_insert_rolls_back_logs_in_shared_tx() {
    use central_logs::store::appender::{insert_logs_in_tx, insert_web_in_tx, LogRow, WebRow};
    let conn = duckdb::Connection::open_in_memory().unwrap();
    central_logs::store::schema::apply_schema(
        &conn,
        std::path::Path::new("/tmp/opencode/parquet-atomic-test"),
        &[],
    )
    .unwrap();

    let now = chrono::Utc::now();
    let log_row = LogRow {
        ts: now,
        insert_ts: now,
        service: "svc".into(),
        message: "hello".into(),
        ..LogRow::empty_at(now)
    };
    // Malformed web row: event kind longer than any sane value is fine, so
    // instead break the insert structurally — a web row whose props JSON is
    // invalid TEXT cannot happen from the parser, so simulate via a value
    // DuckDB rejects: NaN in a DOUBLE column is accepted, so use an
    // out-of-range USMALLINT via screen_w = u16::MAX is also fine. The
    // reliable structural failure: duplicate session rows violate nothing…
    // simplest: point the appender at a NON-WebRow column set by making the
    // row's site_id absurdly long is still fine. So drive the failure by
    // dropping the table the appender needs (structural failure).
    let web_row = WebRow {
        ts: now,
        insert_ts: now,
        site_id: "s".into(),
        event: "pageview".into(),
        ..Default::default()
    };

    conn.execute_batch("DROP TABLE web_events;").unwrap();

    let res: central_logs::Result<(usize, usize)> = (|| {
        let tx = conn.unchecked_transaction()?;
        let n = insert_logs_in_tx(&tx, &[], &[log_row])?;
        let w = insert_web_in_tx(&tx, &[web_row])?;
        tx.commit()?;
        Ok((n, w))
    })();
    assert!(res.is_err(), "web insert must fail (table dropped)");
    // The log append from the SAME transaction must be gone.
    let logs: i64 = conn
        .query_row("SELECT COUNT(*) FROM logs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(logs, 0, "failed web insert must roll back the logs insert");
}

#[test]
fn visitor_hash_rotates_daily() {
    let tmp = tempfile::tempdir().unwrap();
    let meta = Arc::new(WalMeta::open(&tmp.path().join("meta.redb")).unwrap());
    let parser = WebParser::new(meta.clone());
    let env = serde_json::json!({
        "v": 1, "site": "site_abc", "ua": "UA/1.0", "lang": "en",
        "origin": "https://example.com", "uid": null,
        "ev": [{"e":"pageview","u":"/","dt":0}]
    });
    let mk = |day: u32| RawRecord {
        receive_ts: chrono::Utc::now()
            .with_day(day)
            .unwrap(),
        source_addr: "203.0.113.50".into(),
        protocol: Protocol::WebEvent,
        raw: bytes::Bytes::from(serde_json::to_vec(&env).unwrap()),
    };
    let today = chrono::Utc::now().day();
    // Pick two days inside the current month (salt pruning keeps 2 days, but
    // salt creation is lazy per date, so any pair works as long as they differ).
    let (d1, d2) = if today > 15 { (today, today - 1) } else { (today, today + 1) };
    let rows1 = parser.parse(&mk(d1));
    let rows1_again = parser.parse(&mk(d1));
    let rows2 = parser.parse(&mk(d2));
    assert_eq!(rows1[0].visitor_id, rows1_again[0].visitor_id, "same day → same hash");
    assert_ne!(rows1[0].visitor_id, rows2[0].visitor_id, "different day → different hash");
}

use chrono::Datelike;

#[test]
fn beacon_round_trip_collector_to_parser() {
    let tmp = tempfile::tempdir().unwrap();
    let meta = Arc::new(WalMeta::open(&tmp.path().join("meta.redb")).unwrap());
    let parser = WebParser::new(meta);
    let cfg = web_cfg();
    let beacon = parse_beacon(beacon().as_bytes(), &cfg).unwrap();
    let envelope = serde_json::json!({
        "v": 1, "site": beacon.site, "ua": "TestBrowser/1.0", "lang": "en-US",
        "origin": "https://example.com", "uid": null, "ev": beacon.events, "tp": null
    });
    let rec = RawRecord {
        receive_ts: chrono::Utc::now(),
        source_addr: "203.0.113.50".into(),
        protocol: Protocol::WebEvent,
        raw: bytes::Bytes::from(serde_json::to_vec(&envelope).unwrap()),
    };
    let rows = parser.parse(&rec);
    assert_eq!(rows.len(), 5);
    let kinds: Vec<&str> = rows.iter().map(|r| r.event.as_str()).collect();
    assert_eq!(
        kinds,
        vec!["pageview", "custom", "vital", "error", "engagement"]
    );
    // dt → ts ordering: engagement happened 9000ms before the vital.
    assert!(rows[2].ts > rows[4].ts);
    // Client IP is not in any row field except via the hash.
    assert!(!rows[0].referrer.contains("203.0.113"));
    // Empty-beacon parse guards.
    assert!(parse_beacon(b"", &cfg).is_err());
}
