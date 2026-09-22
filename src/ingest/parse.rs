//! Parsing: JSON-first, fall back to syslog/free-text heuristics (architecture §2b.1).
//!
//! Hot-attribute promotion (telemetry optimization): a configurable list of
//! frequently-filtered JSON keys is extracted at parse time into typed top-level
//! columns. The matching keys are also popped out of the `attributes` JSON blob
//! so the residue stays small.

use chrono::Utc;
use serde_json::Value;
use syslog_loose::{Message, ProcId, SyslogSeverity};

use crate::hot::{coerce, resolve_json_path, HotAttribute, HotValue};
use crate::store::appender::LogRow;
use crate::{Protocol, RawRecord};

/// Owns the configured hot-attribute list and parses [`RawRecord`]s into
/// [`LogRow`]s. Cheap to clone; share freely across worker tasks.
#[derive(Debug, Clone)]
pub struct Parser {
    hot: Vec<HotAttribute>,
    drop_keys: Vec<String>,
    unwrap_message_json: bool,
}

impl Parser {
    pub fn new(hot: Vec<HotAttribute>) -> Self {
        Self {
            hot,
            drop_keys: Vec::new(),
            unwrap_message_json: true,
        }
    }

    /// Parser with hot attributes AND drop-attribute keys (stripped at parse
    /// time before persistence — "remove fields early").
    pub fn with_drop(hot: Vec<HotAttribute>, drop_keys: Vec<String>) -> Self {
        Self {
            hot,
            drop_keys,
            unwrap_message_json: true,
        }
    }

    /// Fully-specified parser: hot attrs + drop keys + message-JSON lifting.
    pub fn with_options(
        hot: Vec<HotAttribute>,
        drop_keys: Vec<String>,
        unwrap_message_json: bool,
    ) -> Self {
        Self {
            hot,
            drop_keys,
            unwrap_message_json,
        }
    }

    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    pub fn hot(&self) -> &[HotAttribute] {
        &self.hot
    }

    pub fn drop_keys(&self) -> &[String] {
        &self.drop_keys
    }

    /// Parse a [`RawRecord`] into a [`LogRow`].
    ///
    /// Strategy:
    /// 1. JSON envelope (object) — pull known fields, then promote hot attrs.
    /// 2. Syslog (RFC 3164/5424).
    /// 3. Free-text fallback.
    ///
    /// Finally, any configured drop-attribute keys are stripped from the
    /// `attributes` JSON regardless of which path produced it.
    pub fn parse(&self, rec: &RawRecord) -> LogRow {
        let mut row = self.parse_inner(rec);
        if !self.drop_keys.is_empty() {
            if let Some(obj) = row.attributes.as_object_mut() {
                for k in &self.drop_keys {
                    obj.remove(k);
                }
            }
        }
        // Error tracking: carry an explicit fingerprint (Sentry events arrive
        // with one), or derive a message-based one for plain error logs so
        // they group too (docs/ERROR_TRACKING.md).
        if row.fingerprint.is_empty() && matches!(row.level.as_str(), "error" | "fatal") {
            row.fingerprint = crate::errors::message_fingerprint(&row.service, &row.message);
        }
        row
    }

    fn parse_inner(&self, rec: &RawRecord) -> LogRow {
        let insert_ts = rec.receive_ts;
        let mut row = LogRow::empty_at(insert_ts);
        row.protocol = rec.protocol.to_string();
        row.raw_len = rec.raw_len() as i32;
        row.source_host = rec.source_addr.clone();
        // Pre-allocate the hot-values vec so its length always matches the
        // configured list (even when parsing fails entirely).
        row.hot = vec![HotValue::Null; self.hot.len()];

        let body = match std::str::from_utf8(&rec.raw) {
            Ok(s) => s,
            Err(_) => {
                row.message = format!("<{} non-utf8 bytes>", rec.raw.len());
                row.ts = insert_ts;
                row.level = "info".into();
                return row;
            }
        };

        // 1. JSON path
        if let Ok(value) = serde_json::from_str::<Value>(body) {
            if let Some(obj) = value.as_object() {
                apply_json_object(
                    &mut row,
                    obj,
                    body,
                    insert_ts,
                    &self.hot,
                    self.unwrap_message_json,
                );
                return row;
            }
        }

        // 2. Syslog path
        if matches!(rec.protocol, Protocol::SyslogUdp | Protocol::SyslogTcp) {
            if let Some(parsed) = parse_syslog(body) {
                apply_syslog(&mut row, &parsed, body, insert_ts);
                // Syslog structured_data rarely overlaps with hot attributes —
                // skip hot extraction here for v1. Easy to add if a config asks
                // for `$syslog_procid` etc.
                return row;
            }
        }

        // 3. Free-text fallback
        apply_free_text(&mut row, body, insert_ts);
        row
    }
}

/// Back-compat shim: parse without any hot attributes (used in tests that don't
/// care about promotion).
pub fn parse(rec: &RawRecord) -> LogRow {
    Parser::empty().parse(rec)
}

/// First string value found under any of `keys` in `m`.
fn pick_str_any<'v>(m: &'v serde_json::Map<String, Value>, keys: &[&str]) -> Option<&'v str> {
    for key in keys {
        if let Some(s) = m.get(*key).and_then(Value::as_str) {
            return Some(s);
        }
    }
    None
}

fn apply_json_object(
    row: &mut LogRow,
    obj: &serde_json::Map<String, Value>,
    body: &str,
    insert_ts: chrono::DateTime<Utc>,
    hot: &[HotAttribute],
    unwrap_message_json: bool,
) {
    // "message_json" lifting (LESSON_LEARNED): apps frequently embed a JSON
    // object as a *string* inside msg/message. Without lifting, the inner
    // fields are opaque and unfilterable. When enabled, the inner object's
    // fields are merged: reserved fields backfill missing top-level columns,
    // other keys land in `attributes`, and hot-attr extraction sees both.
    // Only JSON *objects* are lifted (depth 1 — no recursive unwrapping).
    let message_str = obj
        .get("msg")
        .and_then(Value::as_str)
        .or_else(|| obj.get("message").and_then(Value::as_str))
        .or_else(|| obj.get("Message").and_then(Value::as_str));
    let inner: Option<serde_json::Map<String, Value>> = if unwrap_message_json {
        message_str
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .and_then(|v| v.as_object().cloned())
    } else {
        None
    };

    // ts: outer first, then lifted inner, then receive time.
    row.ts = pick_ts(obj)
        .or_else(|| inner.as_ref().and_then(|i| pick_ts(i)))
        .unwrap_or(insert_ts);

    if let Some(host) = obj.get("host").and_then(Value::as_str) {
        row.source_host = host.to_string();
    } else if let Some(host) = inner
        .as_ref()
        .and_then(|i| i.get("host").or_else(|| i.get("hostname")))
        .and_then(Value::as_str)
    {
        row.source_host = host.to_string();
    }

    if let Some(svc) = pick_str_any(obj, &["service", "logger"]) {
        row.service = svc.to_string();
    } else if let Some(svc) = inner
        .as_ref()
        .and_then(|i| pick_str_any(i, &["service", "logger"]))
    {
        row.service = svc.to_string();
    }

    row.level = pick_level(obj)
        .or_else(|| inner.as_ref().and_then(|i| pick_level(i)))
        .unwrap_or_else(|| "info".into());

    // The message column keeps the ORIGINAL string (JSON or not) — display
    // fidelity; queryability comes from the lifted fields.
    if let Some(msg) = message_str {
        row.message = msg.to_string();
    } else {
        // Use the whole body (truncated) as the message.
        row.message = body.chars().take(2048).collect();
    }

    if let Some(fp) = obj.get("fingerprint").and_then(Value::as_str) {
        row.fingerprint = fp.to_string();
    } else if let Some(fp) = inner
        .as_ref()
        .and_then(|i| i.get("fingerprint"))
        .and_then(Value::as_str)
    {
        row.fingerprint = fp.to_string();
    }

    if let Some(t) = pick_str_any(obj, &["trace_id", "traceId"]) {
        row.trace_id = t.to_string();
    } else if let Some(t) = inner
        .as_ref()
        .and_then(|i| pick_str_any(i, &["trace_id", "traceId"]))
    {
        row.trace_id = t.to_string();
    }
    if let Some(s) = pick_str_any(obj, &["span_id", "spanId"]) {
        row.span_id = s.to_string();
    } else if let Some(s) = inner
        .as_ref()
        .and_then(|i| pick_str_any(i, &["span_id", "spanId"]))
    {
        row.span_id = s.to_string();
    }

    // Schema inference: anything that isn't a known field becomes an attribute.
    // Outer keys are inserted first; lifted inner keys merge on top only when
    // the outer object doesn't already carry the key (outer wins collisions).
    const RESERVED: &[&str] = &[
        "ts",
        "timestamp",
        "time",
        "@timestamp",
        "host",
        "hostname",
        "service",
        "logger",
        "level",
        "severity",
        "msg",
        "message",
        "Message",
        "fingerprint",
        "trace_id",
        "traceId",
        "span_id",
        "spanId",
    ];
    let mut attrs = serde_json::Map::new();
    if let Some(inner) = &inner {
        for (k, v) in inner.iter() {
            if RESERVED.contains(&k.as_str()) {
                continue;
            }
            attrs.insert(k.clone(), v.clone());
        }
    }
    for (k, v) in obj.iter() {
        if RESERVED.contains(&k.as_str()) {
            continue;
        }
        attrs.insert(k.clone(), v.clone());
    }

    // Hot-attribute promotion: extract typed values for the configured keys,
    // and remove the matching keys from the residue so the JSON column stays
    // small. Resolution runs against outer ∪ inner (outer wins), so hot
    // columns see lifted message-JSON fields too.
    let mut merged: serde_json::Map<String, Value> = inner.clone().unwrap_or_default();
    for (k, v) in obj.iter() {
        merged.insert(k.clone(), v.clone());
    }
    for (i, attr) in hot.iter().enumerate() {
        let leaf = attr.json_path.strip_prefix("$.").unwrap_or(&attr.json_path);
        if let Some(val) = resolve_json_path(&Value::Object(merged.clone()), &attr.json_path) {
            row.hot[i] = coerce(val, attr.duckdb_type);
            // Remove from residue only if the path is a simple top-level key.
            if !leaf.contains('.') {
                attrs.remove(leaf);
            }
        }
    }

    row.attributes = Value::Object(attrs);
}

fn pick_ts(obj: &serde_json::Map<String, Value>) -> Option<chrono::DateTime<Utc>> {
    for key in ["ts", "timestamp", "time", "@timestamp"] {
        if let Some(v) = obj.get(key) {
            if let Some(s) = v.as_str() {
                if let Some(ts) = parse_flexible_ts(s) {
                    return Some(ts);
                }
            } else if let Some(n) = v.as_i64() {
                if let Some(dt) = chrono::DateTime::from_timestamp(n, 0) {
                    return Some(dt);
                }
            } else if let Some(n) = v.as_f64() {
                let secs = n.trunc() as i64;
                let nsec = ((n.fract().abs()) * 1e9) as u32;
                if let Some(dt) = chrono::DateTime::from_timestamp(secs, nsec) {
                    return Some(dt);
                }
            }
        }
    }
    None
}

fn parse_flexible_ts(s: &str) -> Option<chrono::DateTime<Utc>> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    for fmt in &[
        "%Y-%m-%dT%H:%M:%S%.3fZ",
        "%Y-%m-%dT%H:%M:%SZ",
        "%Y-%m-%d %H:%M:%S",
        "%Y/%m/%d %H:%M:%S",
    ] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(dt.and_utc());
        }
    }
    None
}

fn pick_level(obj: &serde_json::Map<String, Value>) -> Option<String> {
    for key in ["level", "severity", "loglevel", "lvl"] {
        if let Some(v) = obj.get(key) {
            let s = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => {
                    let raw = n.as_i64().unwrap_or(0);
                    int_severity_to_level(raw).to_string()
                }
                _ => continue,
            };
            return Some(normalize_level(&s));
        }
    }
    None
}

/// Public wrapper used by other ingest paths (OTLP severity fallback).
pub fn normalize_level_public(raw: &str) -> String {
    normalize_level(raw)
}

fn normalize_level(raw: &str) -> String {
    let upper = raw.trim().to_ascii_uppercase();
    match upper.as_str() {
        "TRACE" | "FINEST" | "VERBOSE" | "DBG" | "DEBUG" => "debug".into(),
        "INFO" | "INFORMATIONAL" | "NOTICE" | "LOG" | "I" => "info".into(),
        "WARN" | "WARNING" | "WRN" | "W" => "warn".into(),
        "ERR" | "ERROR" | "ERRORS" | "E" | "FAULT" => "error".into(),
        "CRIT" | "CRITICAL" | "FATAL" | "ALERT" | "EMERG" | "EMERGENCY" | "F" => "fatal".into(),
        other if other.len() <= 32 => other.to_ascii_lowercase(),
        _ => "info".into(),
    }
}

fn int_severity_to_level(n: i64) -> &'static str {
    match n {
        0 | 1 | 2 => "fatal",
        3 | 4 => "error",
        5 | 6 => "warn",
        7 | 8 | 9 | 10 => "info",
        _ => "debug",
    }
}

fn parse_syslog(body: &str) -> Option<Message<&str>> {
    // The 0.23 API takes an explicit `Variant`. Try RFC5424 first; if it parses
    // a recognisable PRI/hostname, accept it. Otherwise fall back to RFC3164.
    let parsed5424 = syslog_loose::parse_message(body, syslog_loose::Variant::RFC5424);
    let parsed3164 = syslog_loose::parse_message(body, syslog_loose::Variant::RFC3164);
    let chosen = if parsed5424.facility.is_some()
        || parsed5424.severity.is_some()
        || parsed5424.hostname.is_some()
    {
        parsed5424
    } else {
        parsed3164
    };
    if chosen.facility.is_some()
        || chosen.severity.is_some()
        || chosen.hostname.is_some()
        || body.starts_with('<')
    {
        Some(chosen)
    } else {
        None
    }
}

fn apply_syslog(
    row: &mut LogRow,
    msg: &Message<&str>,
    body: &str,
    insert_ts: chrono::DateTime<Utc>,
) {
    if let Some(ts) = msg.timestamp {
        row.ts = ts.with_timezone(&Utc);
    } else {
        row.ts = insert_ts;
    }
    if let Some(host) = msg.hostname {
        row.source_host = host.to_string();
    }
    if let Some(appname) = msg.appname {
        row.service = appname.to_string();
    }
    if let Some(sev) = msg.severity {
        row.level = syslog_severity_to_level(sev).to_string();
    } else {
        row.level = "info".into();
    }
    row.message = msg.msg.to_string();

    if let Some(facility) = msg.facility {
        row.attributes["syslog_facility"] = Value::String(format!("{facility:?}"));
    }
    if let Some(procid) = &msg.procid {
        let s = match procid {
            ProcId::PID(p) => p.to_string(),
            ProcId::Name(n) => n.to_string(),
        };
        row.attributes["syslog_procid"] = Value::String(s);
    }
    if let Some(msgid) = msg.msgid {
        row.attributes["syslog_msgid"] = Value::String(msgid.to_string());
    }
    for elem in &msg.structured_data {
        let id = elem.id.to_string();
        for (param, val) in &elem.params {
            let key = format!("syslog_sd_{id}_{param}");
            row.attributes[&key] = Value::String(val.to_string());
        }
    }
    let _ = body;
}

fn syslog_severity_to_level(s: SyslogSeverity) -> &'static str {
    use syslog_loose::SyslogSeverity::*;
    match s {
        SEV_EMERG | SEV_ALERT | SEV_CRIT => "fatal",
        SEV_ERR => "error",
        SEV_WARNING => "warn",
        SEV_NOTICE | SEV_INFO => "info",
        SEV_DEBUG => "debug",
    }
}

fn apply_free_text(row: &mut LogRow, body: &str, insert_ts: chrono::DateTime<Utc>) {
    row.ts = insert_ts;
    row.message = body.chars().take(2048).collect();
    row.level = infer_level_from_text(body).to_string();
}

/// Best-effort level extraction from free-text (e.g. Python logging default format).
fn infer_level_from_text(s: &str) -> &'static str {
    let lower = s.to_ascii_lowercase();
    for keyword in ["error", "exception", "traceback", "failed", "fatal"] {
        if lower.contains(keyword) {
            return "error";
        }
    }
    if lower.contains("warn") {
        return "warn";
    }
    if lower.contains("debug") {
        return "debug";
    }
    "info"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hot::HotAttribute;
    use crate::Protocol;

    fn raw(body: &str, proto: Protocol) -> RawRecord {
        RawRecord {
            receive_ts: Utc::now(),
            source_addr: "1.2.3.4:5".into(),
            protocol: proto,
            raw: bytes::Bytes::copy_from_slice(body.as_bytes()),
        }
    }

    #[test]
    fn parses_json_envelope() {
        let body = r#"{"@timestamp":"2026-08-11T10:00:00Z","service":"api","level":"error","msg":"boom","user_id":42}"#;
        let r = Parser::empty().parse(&raw(body, Protocol::HttpJson));
        assert_eq!(r.service, "api");
        assert_eq!(r.level, "error");
        assert_eq!(r.message, "boom");
        assert_eq!(r.attributes["user_id"], 42);
        assert_eq!(r.ts.to_rfc3339(), "2026-08-11T10:00:00+00:00");
    }

    #[test]
    fn parses_syslog() {
        let body = "<134>Aug 11 10:00:00 host1 app[4321]: hello world";
        let r = Parser::empty().parse(&raw(body, Protocol::SyslogUdp));
        assert!(r.message.contains("hello") || r.message.contains("Aug"));
        assert_eq!(r.source_host, "host1");
    }

    #[test]
    fn free_text_level_inference() {
        let body = "2026-08-11 something failed: connection refused";
        let r = Parser::empty().parse(&raw(body, Protocol::HttpJson));
        assert_eq!(r.level, "error");
    }

    #[test]
    fn hot_keys_extracted_and_removed_from_residue() {
        // Config: promote user_id (bigint), env (varchar), canary (varchar, not in payload).
        let hot = vec![
            HotAttribute::parse_shorthand("user_id:bigint").unwrap(),
            HotAttribute::parse_shorthand("env:varchar").unwrap(),
            HotAttribute::parse_shorthand("canary:varchar").unwrap(),
        ];
        let parser = Parser::new(hot.clone());

        let body = r#"{"service":"api","msg":"hi","user_id":42,"env":"prod","extra":"keep"}"#;
        let r = parser.parse(&raw(body, Protocol::HttpJson));

        // Hot values extracted with correct types.
        assert_eq!(r.hot[0], HotValue::Bigint(42));
        assert_eq!(r.hot[1], HotValue::Varchar("prod".into()));
        // Missing key → Null.
        assert_eq!(r.hot[2], HotValue::Null);

        // Promoted keys popped out of the JSON residue; unrelated keys remain.
        assert!(r.attributes.as_object().unwrap().get("user_id").is_none());
        assert!(r.attributes.as_object().unwrap().get("env").is_none());
        assert_eq!(r.attributes["extra"], "keep");
    }

    #[test]
    fn hot_keys_coerce_quoted_numbers() {
        // A common case: JSON like {"user_id":"42"} from loggers that string-ify everything.
        let hot = vec![HotAttribute::parse_shorthand("user_id:bigint").unwrap()];
        let parser = Parser::new(hot);
        let body = r#"{"msg":"x","user_id":"42"}"#;
        let r = parser.parse(&raw(body, Protocol::HttpJson));
        assert_eq!(r.hot[0], HotValue::Bigint(42));
    }

    #[test]
    fn hot_keys_dotted_path() {
        let hot = vec![HotAttribute::parse_shorthand("route:varchar:$.request.route").unwrap()];
        let parser = Parser::new(hot);
        let body = r#"{"msg":"x","request":{"route":"/health"}}"#;
        let r = parser.parse(&raw(body, Protocol::HttpJson));
        assert_eq!(r.hot[0], HotValue::Varchar("/health".into()));
        // Nested parent stays in the residue (we don't mutate nested objects).
        assert_eq!(r.attributes["request"]["route"], "/health");
    }

    #[test]
    fn hot_keys_type_mismatch_yields_null() {
        let hot = vec![HotAttribute::parse_shorthand("user_id:bigint").unwrap()];
        let parser = Parser::new(hot);
        // user_id is an array — can't be coerced to BIGINT.
        let body = r#"{"msg":"x","user_id":[1,2,3]}"#;
        let r = parser.parse(&raw(body, Protocol::HttpJson));
        assert_eq!(r.hot[0], HotValue::Null);
    }

    #[test]
    fn drop_keys_stripped_from_residue() {
        let parser = Parser::with_drop(
            Vec::new(),
            vec!["debug_payload".into(), "thread_local".into()],
        );
        let body = r#"{"service":"api","msg":"hi","debug_payload":{"big":"blob"},"thread_local":"x","keep":"me"}"#;
        let r = parser.parse(&raw(body, Protocol::HttpJson));
        let obj = r.attributes.as_object().unwrap();
        assert!(
            obj.get("debug_payload").is_none(),
            "dropped key must not persist"
        );
        assert!(
            obj.get("thread_local").is_none(),
            "dropped key must not persist"
        );
        assert_eq!(obj.get("keep"), Some(&serde_json::json!("me")));
    }

    #[test]
    fn drop_keys_never_touch_reserved_fields() {
        // Dropping "message" must not blank the message column — drop keys
        // only apply to the attributes JSON residue.
        let parser = Parser::with_drop(Vec::new(), vec!["message".into()]);
        let body = r#"{"service":"api","message":"hello"}"#;
        let r = parser.parse(&raw(body, Protocol::HttpJson));
        assert_eq!(r.message, "hello");
    }

    #[test]
    fn message_json_lifted_into_attributes() {
        // The "message_json" lesson: embedded JSON object becomes queryable.
        let parser = Parser::empty();
        let body = r#"{"service":"api","message":"{\"order_id\":\"ORD-123\",\"status\":\"delivered\",\"duration_ms\":42}"}"#;
        let r = parser.parse(&raw(body, Protocol::HttpJson));
        assert_eq!(r.service, "api");
        // Original string preserved for display.
        assert!(r.message.contains("ORD-123"));
        // Inner fields lifted into attributes.
        assert_eq!(r.attributes["order_id"], "ORD-123");
        assert_eq!(r.attributes["status"], "delivered");
        assert_eq!(r.attributes["duration_ms"], 42);
    }

    #[test]
    fn message_json_reserved_fields_backfill_columns() {
        let parser = Parser::empty();
        let body =
            r#"{"message":"{\"level\":\"error\",\"service\":\"worker\",\"trace_id\":\"abc\"}"}"#;
        let r = parser.parse(&raw(body, Protocol::HttpJson));
        assert_eq!(r.level, "error");
        assert_eq!(r.service, "worker");
        assert_eq!(r.trace_id, "abc");
        // Reserved keys never land in the attributes residue.
        assert!(r.attributes.as_object().unwrap().get("level").is_none());
    }

    #[test]
    fn message_json_hot_attr_extracted_from_inner() {
        let hot = vec![HotAttribute::parse_shorthand("order_id:varchar").unwrap()];
        let parser = Parser::new(hot);
        let body = r#"{"service":"api","message":"{\"order_id\":\"ORD-9\",\"status\":\"ok\"}"}"#;
        let r = parser.parse(&raw(body, Protocol::HttpJson));
        assert_eq!(r.hot[0], HotValue::Varchar("ORD-9".into()));
        // Promoted key popped out of the lifted residue; siblings remain.
        assert!(r.attributes.as_object().unwrap().get("order_id").is_none());
        assert_eq!(r.attributes["status"], "ok");
    }

    #[test]
    fn message_json_outer_wins_collision() {
        let parser = Parser::empty();
        let body = r#"{"user_id":42,"message":"{\"user_id\":7,\"status\":\"ok\"}"}"#;
        let r = parser.parse(&raw(body, Protocol::HttpJson));
        assert_eq!(r.attributes["user_id"], 42, "outer key must win");
    }

    #[test]
    fn message_json_disabled_via_config() {
        let parser = Parser::with_options(Vec::new(), Vec::new(), false);
        let body = r#"{"service":"api","message":"{\"order_id\":\"ORD-1\"}"}"#;
        let r = parser.parse(&raw(body, Protocol::HttpJson));
        assert!(r.attributes.as_object().unwrap().is_empty());
        assert!(r.message.contains("order_id"));
    }

    #[test]
    fn message_json_non_object_strings_untouched() {
        let parser = Parser::empty();
        // Plain text, JSON array, JSON scalar — none are lifted.
        for msg in ["hello world", "[1,2,3]", "42"] {
            let body = format!(
                r#"{{"service":"api","message":{}}}"#,
                serde_json::json!(msg)
            );
            let r = parser.parse(&raw(&body, Protocol::HttpJson));
            assert!(
                r.attributes.as_object().unwrap().is_empty(),
                "message {msg:?} must not be lifted"
            );
            assert_eq!(r.message, msg);
        }
    }
}
