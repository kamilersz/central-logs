//! OTLP/HTTP ingest (OpenTelemetry).
//!
//! Implements the receiver half of the OTLP/HTTP protocol so any OpenTelemetry
//! SDK, in any language, can export to central-logs without a collector:
//!
//! - `POST /v1/traces`  → [`ExportTraceServiceRequest`]
//! - `POST /v1/logs`    → [`ExportLogsServiceRequest`] (in addition to the
//!                        plain-JSON/NDJSON contract that already existed)
//! - `POST /v1/metrics` → [`ExportMetricsServiceRequest`]
//!
//! Both wire encodings are accepted, selected by `Content-Type`:
//! `application/x-protobuf` (the default for most SDKs) and
//! `application/json` (OTLP/JSON). gzip request bodies are handled by the
//! global request-decompression layer before this code runs.
//!
//! The receiver is deliberately *shape-preserving but storage-native*: each
//! span / log record / metric point is normalized into the JSON envelope the
//! existing WAL→parser pipeline already understands, tagged with an
//! `otlp_*` [`Protocol`], and pushed through the same durable WAL as
//! everything else. Traces therefore get `trace_id`/`span_id`/`duration_ms`
//! for free (they show up in the Logs Explorer and the latency dashboards),
//! and metrics land as queryable rows with the numeric value under
//! `attributes.value`.

use chrono::{DateTime, Utc};
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{
    any_value::Value as AnyValue, AnyValue as AnyValueMsg, InstrumentationScope, KeyValue,
};
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;
use opentelemetry_proto::tonic::metrics::v1::{
    metric::Data, number_data_point::Value as NumberValue, AggregationTemporality,
    ExponentialHistogramDataPoint, HistogramDataPoint, Metric, NumberDataPoint, SummaryDataPoint,
};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{span::SpanKind, status::StatusCode};
use prost::Message;
use serde_json::{Map, Value as Json};

use crate::{Protocol, RawRecord};

/// Wire encoding of an OTLP/HTTP request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    Protobuf,
    Json,
}

/// Choose the decoder from the request `Content-Type`. Anything that isn't
/// JSON is treated as protobuf (the OTLP default).
pub fn detect_wire(content_type: &str) -> Wire {
    if content_type.to_ascii_lowercase().contains("json") {
        Wire::Json
    } else {
        Wire::Protobuf
    }
}

/// Heuristic: does a JSON body look like an OTLP export request? Used to let
/// `/v1/logs` keep accepting plain JSON/NDJSON logs while also accepting
/// OTLP/JSON (`{"resourceLogs":[...]}`) on the same path.
pub fn json_body_looks_like_otlp(body: &[u8]) -> bool {
    match std::str::from_utf8(body) {
        Ok(s) => {
            s.contains("\"resourceLogs\"")
                || s.contains("\"resourceSpans\"")
                || s.contains("\"resourceMetrics\"")
        }
        Err(_) => false,
    }
}

/// The OTLP success response body. An empty protobuf message is a valid
/// `Export*ServiceResponse` (no partial success), and `{}` for OTLP/JSON.
/// Returning the right content type matters: SDK protobuf parsers choke on a
/// JSON/HTML body even when the status code is 200.
pub fn success_body(wire: Wire) -> (&'static str, &'static [u8]) {
    match wire {
        Wire::Protobuf => ("application/x-protobuf", &[]),
        Wire::Json => ("application/json", b"{}"),
    }
}

pub fn content_type_for(wire: Wire) -> &'static str {
    match wire {
        Wire::Protobuf => "application/x-protobuf",
        Wire::Json => "application/json",
    }
}

// =====================================================================
// Decoding
// =====================================================================

pub fn decode_traces(wire: Wire, body: &[u8]) -> Result<ExportTraceServiceRequest, String> {
    match wire {
        Wire::Protobuf => ExportTraceServiceRequest::decode(body)
            .map_err(|e| format!("otlp/traces protobuf decode failed: {e}")),
        Wire::Json => otlp_json::<ExportTraceServiceRequest>(body, "traces"),
    }
}

pub fn decode_logs(wire: Wire, body: &[u8]) -> Result<ExportLogsServiceRequest, String> {
    match wire {
        Wire::Protobuf => ExportLogsServiceRequest::decode(body)
            .map_err(|e| format!("otlp/logs protobuf decode failed: {e}")),
        Wire::Json => otlp_json::<ExportLogsServiceRequest>(body, "logs"),
    }
}

pub fn decode_metrics(wire: Wire, body: &[u8]) -> Result<ExportMetricsServiceRequest, String> {
    match wire {
        Wire::Protobuf => ExportMetricsServiceRequest::decode(body)
            .map_err(|e| format!("otlp/metrics protobuf decode failed: {e}")),
        Wire::Json => otlp_json::<ExportMetricsServiceRequest>(body, "metrics"),
    }
}

/// Decode an OTLP/JSON request. The OTLP spec allows enums to be either the
/// protobuf JSON *string* name (`"SPAN_KIND_SERVER"`, `"STATUS_CODE_OK"`,
/// `"SEVERITY_NUMBER_INFO"`) or an integer; the generated Rust types only
/// understand integers, so string enum values for the known enum fields are
/// rewritten to their numeric value first.
fn otlp_json<T: serde::de::DeserializeOwned>(body: &[u8], signal: &str) -> Result<T, String> {
    let mut v: Json = serde_json::from_slice(body)
        .map_err(|e| format!("otlp/{signal} json decode failed: {e}"))?;
    normalize_otlp_json(&mut v);
    serde_json::from_value(v).map_err(|e| format!("otlp/{signal} json decode failed: {e}"))
}

fn normalize_otlp_json(v: &mut Json) {
    match v {
        Json::Object(map) => {
            for (k, val) in map.iter_mut() {
                if let Json::String(s) = val {
                    if let Some(n) = enum_string_to_number(k, s) {
                        *val = Json::Number(n.into());
                        continue;
                    }
                    // The OTLP/JSON spec encodes int64 values as decimal
                    // strings; the `asInt` oneof branch of NumberDataPoint is
                    // the one field prost's serde only accepts as a JSON
                    // number. Java SDKs rely on this for JVM/gauge metrics.
                    if k == "asInt" {
                        if let Ok(i) = s.parse::<i64>() {
                            *val = Json::Number(i.into());
                            continue;
                        }
                    }
                }
                normalize_otlp_json(val);
            }
        }
        Json::Array(arr) => {
            for e in arr.iter_mut() {
                normalize_otlp_json(e);
            }
        }
        _ => {}
    }
}

fn enum_string_to_number(key: &str, s: &str) -> Option<i64> {
    match key {
        "kind" => SpanKind::from_str_name(s).map(|x| x as i64),
        "code" => StatusCode::from_str_name(s).map(|x| x as i64),
        "severityNumber" => SeverityNumber::from_str_name(s).map(|x| x as i64),
        "aggregationTemporality" => AggregationTemporality::from_str_name(s).map(|x| x as i64),
        _ => None,
    }
}

// =====================================================================
// Traces → RawRecords
// =====================================================================

pub fn traces_to_records(
    req: ExportTraceServiceRequest,
    now: DateTime<Utc>,
    peer: &str,
) -> Vec<RawRecord> {
    let mut out = Vec::new();
    for rs in req.resource_spans {
        let rmap = resource_map(rs.resource.as_ref());
        let service = resource_service(rs.resource.as_ref());
        for ss in rs.scope_spans {
            let base = base_attrs(&rmap, ss.scope.as_ref());
            for span in ss.spans {
                let ts = nanos_to_dt(span.start_time_unix_nano).unwrap_or(now);
                let dur_ms = if span.end_time_unix_nano >= span.start_time_unix_nano
                    && span.end_time_unix_nano > 0
                {
                    (span.end_time_unix_nano - span.start_time_unix_nano) as f64 / 1_000_000.0
                } else {
                    0.0
                };
                let status_code = span.status.as_ref().map(|s| s.code).unwrap_or(0);
                let level = match StatusCode::try_from(status_code) {
                    Ok(StatusCode::Error) => "error",
                    _ => "info",
                };
                let mut obj = Map::new();
                obj.insert("service".into(), Json::String(service.clone()));
                obj.insert("level".into(), Json::String(level.into()));
                obj.insert("msg".into(), Json::String(span.name.clone()));
                obj.insert("ts".into(), Json::String(ts.to_rfc3339()));
                obj.insert("duration_ms".into(), json_f64(dur_ms));
                if !span.trace_id.is_empty() {
                    obj.insert("trace_id".into(), Json::String(hex(&span.trace_id)));
                }
                if !span.span_id.is_empty() {
                    obj.insert("span_id".into(), Json::String(hex(&span.span_id)));
                }
                let mut attrs = base.clone();
                for kv in &span.attributes {
                    attrs.insert(kv.key.clone(), any_value_to_json_opt(&kv.value));
                }
                if !span.parent_span_id.is_empty() {
                    attrs.insert(
                        "parent_span_id".into(),
                        Json::String(hex(&span.parent_span_id)),
                    );
                }
                if let Ok(kind) = SpanKind::try_from(span.kind) {
                    attrs.insert("span_kind".into(), Json::String(kind.as_str_name().into()));
                }
                if let Some(st) = span.status.as_ref() {
                    attrs.insert(
                        "status_code".into(),
                        Json::String(
                            StatusCode::try_from(st.code)
                                .map(|c| c.as_str_name().to_string())
                                .unwrap_or_else(|_| st.code.to_string()),
                        ),
                    );
                    if !st.message.is_empty() {
                        attrs.insert("status_message".into(), Json::String(st.message.clone()));
                    }
                }
                if !span.trace_state.is_empty() {
                    attrs.insert("trace_state".into(), Json::String(span.trace_state.clone()));
                }
                if !span.events.is_empty() {
                    let events: Vec<Json> = span
                        .events
                        .iter()
                        .map(|e| {
                            let mut em = Map::new();
                            em.insert("name".into(), Json::String(e.name.clone()));
                            em.insert(
                                "time_unix_nano".into(),
                                Json::String(e.time_unix_nano.to_string()),
                            );
                            let a = kv_to_map(&e.attributes);
                            em.insert("attributes".into(), Json::Object(a));
                            Json::Object(em)
                        })
                        .collect();
                    attrs.insert("events".into(), Json::Array(events));
                }
                for (k, v) in attrs {
                    obj.insert(k, v);
                }
                out.push(record(Protocol::OtlpSpan, &obj, now, peer));
            }
        }
    }
    out
}

// =====================================================================
// Logs → RawRecords
// =====================================================================

pub fn logs_to_records(
    req: ExportLogsServiceRequest,
    now: DateTime<Utc>,
    peer: &str,
) -> Vec<RawRecord> {
    let mut out = Vec::new();
    for rl in req.resource_logs {
        let rmap = resource_map(rl.resource.as_ref());
        let service = resource_service(rl.resource.as_ref());
        for sl in rl.scope_logs {
            let base = base_attrs(&rmap, sl.scope.as_ref());
            for lr in sl.log_records {
                let ts = nanos_to_dt(lr.time_unix_nano)
                    .or_else(|| nanos_to_dt(lr.observed_time_unix_nano))
                    .unwrap_or(now);
                let level = severity_to_level(lr.severity_number, &lr.severity_text);
                let msg = any_value_display(&lr.body);
                let mut obj = Map::new();
                obj.insert("service".into(), Json::String(service.clone()));
                obj.insert("level".into(), Json::String(level));
                obj.insert("msg".into(), Json::String(msg));
                obj.insert("ts".into(), Json::String(ts.to_rfc3339()));
                if !lr.trace_id.is_empty() {
                    obj.insert("trace_id".into(), Json::String(hex(&lr.trace_id)));
                }
                if !lr.span_id.is_empty() {
                    obj.insert("span_id".into(), Json::String(hex(&lr.span_id)));
                }
                let mut attrs = base.clone();
                for kv in &lr.attributes {
                    attrs.insert(kv.key.clone(), any_value_to_json_opt(&kv.value));
                }
                if !lr.severity_text.is_empty() {
                    attrs.insert(
                        "severity_text".into(),
                        Json::String(lr.severity_text.clone()),
                    );
                }
                if !lr.event_name.is_empty() {
                    attrs.insert("event_name".into(), Json::String(lr.event_name.clone()));
                }
                for (k, v) in attrs {
                    obj.insert(k, v);
                }
                out.push(record(Protocol::OtlpLog, &obj, now, peer));
            }
        }
    }
    out
}

// =====================================================================
// Metrics → RawRecords
// =====================================================================

pub fn metrics_to_records(
    req: ExportMetricsServiceRequest,
    now: DateTime<Utc>,
    peer: &str,
) -> Vec<RawRecord> {
    let mut out = Vec::new();
    for rm in req.resource_metrics {
        let rmap = resource_map(rm.resource.as_ref());
        let service = resource_service(rm.resource.as_ref());
        for sm in rm.scope_metrics {
            let base = base_attrs(&rmap, sm.scope.as_ref());
            for metric in &sm.metrics {
                metric_to_records(&service, &base, metric, now, peer, &mut out);
            }
        }
    }
    out
}

fn metric_to_records(
    service: &str,
    base: &Map<String, Json>,
    metric: &Metric,
    now: DateTime<Utc>,
    peer: &str,
    out: &mut Vec<RawRecord>,
) {
    let name = metric.name.clone();
    let common = |attrs: &mut Map<String, Json>| {
        if !metric.unit.is_empty() {
            attrs.insert("unit".into(), Json::String(metric.unit.clone()));
        }
        if !metric.description.is_empty() {
            attrs.insert(
                "description".into(),
                Json::String(metric.description.clone()),
            );
        }
        for kv in &metric.metadata {
            attrs.insert(kv.key.clone(), any_value_to_json_opt(&kv.value));
        }
    };
    match &metric.data {
        Some(Data::Gauge(g)) => {
            for p in &g.data_points {
                let mut attrs = base.clone();
                attrs.insert("metric_type".into(), Json::String("gauge".into()));
                common(&mut attrs);
                point_attrs(&mut attrs, &p.attributes);
                let v = number_point_value(p);
                attrs.insert("value".into(), json_f64(v));
                let ts = nanos_to_dt(p.time_unix_nano).unwrap_or(now);
                out.push(metric_record(service, &name, &attrs, ts, now, peer));
            }
        }
        Some(Data::Sum(s)) => {
            for p in &s.data_points {
                let mut attrs = base.clone();
                attrs.insert("metric_type".into(), Json::String("sum".into()));
                attrs.insert("is_monotonic".into(), Json::Bool(s.is_monotonic));
                attrs.insert(
                    "aggregation_temporality".into(),
                    Json::String(temporality_str(s.aggregation_temporality).into()),
                );
                common(&mut attrs);
                point_attrs(&mut attrs, &p.attributes);
                let v = number_point_value(p);
                attrs.insert("value".into(), json_f64(v));
                let ts = nanos_to_dt(p.time_unix_nano).unwrap_or(now);
                out.push(metric_record(service, &name, &attrs, ts, now, peer));
            }
        }
        Some(Data::Histogram(h)) => {
            for p in &h.data_points {
                let mut attrs = base.clone();
                attrs.insert("metric_type".into(), Json::String("histogram".into()));
                attrs.insert(
                    "aggregation_temporality".into(),
                    Json::String(temporality_str(h.aggregation_temporality).into()),
                );
                common(&mut attrs);
                histogram_point_attrs(&mut attrs, p);
                let ts = nanos_to_dt(p.time_unix_nano).unwrap_or(now);
                out.push(metric_record(service, &name, &attrs, ts, now, peer));
            }
        }
        Some(Data::ExponentialHistogram(h)) => {
            for p in &h.data_points {
                let mut attrs = base.clone();
                attrs.insert(
                    "metric_type".into(),
                    Json::String("exponential_histogram".into()),
                );
                attrs.insert(
                    "aggregation_temporality".into(),
                    Json::String(temporality_str(h.aggregation_temporality).into()),
                );
                common(&mut attrs);
                exp_histogram_point_attrs(&mut attrs, p);
                let ts = nanos_to_dt(p.time_unix_nano).unwrap_or(now);
                out.push(metric_record(service, &name, &attrs, ts, now, peer));
            }
        }
        Some(Data::Summary(s)) => {
            for p in &s.data_points {
                let mut attrs = base.clone();
                attrs.insert("metric_type".into(), Json::String("summary".into()));
                common(&mut attrs);
                summary_point_attrs(&mut attrs, p);
                let ts = nanos_to_dt(p.time_unix_nano).unwrap_or(now);
                out.push(metric_record(service, &name, &attrs, ts, now, peer));
            }
        }
        None => {}
    }
}

fn metric_record(
    service: &str,
    name: &str,
    attrs: &Map<String, Json>,
    ts: DateTime<Utc>,
    now: DateTime<Utc>,
    peer: &str,
) -> RawRecord {
    let mut obj = Map::new();
    obj.insert("service".into(), Json::String(service.to_string()));
    obj.insert("level".into(), Json::String("info".into()));
    obj.insert("msg".into(), Json::String(name.to_string()));
    obj.insert("ts".into(), Json::String(ts.to_rfc3339()));
    for (k, v) in attrs {
        obj.insert(k.clone(), v.clone());
    }
    let _ = now;
    record(Protocol::OtlpMetric, &obj, now, peer)
}

fn number_point_value(p: &NumberDataPoint) -> f64 {
    match p.value {
        Some(NumberValue::AsDouble(d)) => d,
        Some(NumberValue::AsInt(i)) => i as f64,
        None => 0.0,
    }
}

fn histogram_point_attrs(attrs: &mut Map<String, Json>, p: &HistogramDataPoint) {
    point_attrs(attrs, &p.attributes);
    attrs.insert("hist_count".into(), Json::String(p.count.to_string()));
    if let Some(sum) = p.sum {
        attrs.insert("value".into(), json_f64(sum));
        attrs.insert("hist_sum".into(), json_f64(sum));
    } else {
        attrs.insert("value".into(), json_f64(p.count as f64));
    }
    if let Some(min) = p.min {
        attrs.insert("hist_min".into(), json_f64(min));
    }
    if let Some(max) = p.max {
        attrs.insert("hist_max".into(), json_f64(max));
    }
}

fn exp_histogram_point_attrs(attrs: &mut Map<String, Json>, p: &ExponentialHistogramDataPoint) {
    point_attrs(attrs, &p.attributes);
    attrs.insert("hist_count".into(), Json::String(p.count.to_string()));
    if let Some(sum) = p.sum {
        attrs.insert("value".into(), json_f64(sum));
        attrs.insert("hist_sum".into(), json_f64(sum));
    } else {
        attrs.insert("value".into(), json_f64(p.count as f64));
    }
    if let Some(min) = p.min {
        attrs.insert("hist_min".into(), json_f64(min));
    }
    if let Some(max) = p.max {
        attrs.insert("hist_max".into(), json_f64(max));
    }
}

fn summary_point_attrs(attrs: &mut Map<String, Json>, p: &SummaryDataPoint) {
    point_attrs(attrs, &p.attributes);
    attrs.insert("summary_count".into(), Json::String(p.count.to_string()));
    if p.sum != 0.0 {
        attrs.insert("value".into(), json_f64(p.sum));
        attrs.insert("summary_sum".into(), json_f64(p.sum));
    } else {
        attrs.insert("value".into(), json_f64(p.count as f64));
    }
}

fn point_attrs(attrs: &mut Map<String, Json>, kvs: &[KeyValue]) {
    for kv in kvs {
        attrs.insert(kv.key.clone(), any_value_to_json_opt(&kv.value));
    }
}

fn temporality_str(t: i32) -> &'static str {
    match AggregationTemporality::try_from(t) {
        Ok(AggregationTemporality::Delta) => "delta",
        Ok(AggregationTemporality::Cumulative) => "cumulative",
        _ => "unspecified",
    }
}

// =====================================================================
// Shared helpers
// =====================================================================

/// Resource attributes minus `service.name` (which becomes the `service`
/// column). These seed every record's attribute set.
fn resource_map(resource: Option<&Resource>) -> Map<String, Json> {
    let mut m = Map::new();
    if let Some(r) = resource {
        for kv in &r.attributes {
            if kv.key == "service.name" || kv.key.is_empty() {
                continue;
            }
            m.insert(kv.key.clone(), any_value_to_json_opt(&kv.value));
        }
    }
    m
}

fn resource_service(resource: Option<&Resource>) -> String {
    if let Some(r) = resource {
        for kv in &r.attributes {
            if kv.key == "service.name" {
                if let Some(AnyValueMsg {
                    value: Some(AnyValue::StringValue(s)),
                }) = &kv.value
                {
                    if !s.is_empty() {
                        return s.clone();
                    }
                }
            }
        }
    }
    "unknown_service".to_string()
}

fn base_attrs(
    resource: &Map<String, Json>,
    scope: Option<&InstrumentationScope>,
) -> Map<String, Json> {
    let mut m = resource.clone();
    if let Some(sc) = scope {
        if !sc.name.is_empty() {
            m.insert("otel_scope_name".into(), Json::String(sc.name.clone()));
        }
        if !sc.version.is_empty() {
            m.insert(
                "otel_scope_version".into(),
                Json::String(sc.version.clone()),
            );
        }
        for kv in &sc.attributes {
            m.insert(kv.key.clone(), any_value_to_json_opt(&kv.value));
        }
    }
    m
}

fn kv_to_map(kvs: &[KeyValue]) -> Map<String, Json> {
    let mut m = Map::new();
    for kv in kvs {
        if !kv.key.is_empty() {
            m.insert(kv.key.clone(), any_value_to_json_opt(&kv.value));
        }
    }
    m
}

fn any_value_to_json_opt(v: &Option<AnyValueMsg>) -> Json {
    match v {
        Some(a) => any_value_to_json(a),
        None => Json::Null,
    }
}

fn any_value_to_json(v: &AnyValueMsg) -> Json {
    match &v.value {
        Some(AnyValue::StringValue(s)) => Json::String(s.clone()),
        Some(AnyValue::BoolValue(b)) => Json::Bool(*b),
        Some(AnyValue::IntValue(i)) => Json::Number((*i).into()),
        Some(AnyValue::DoubleValue(d)) => json_f64(*d),
        Some(AnyValue::ArrayValue(a)) => {
            Json::Array(a.values.iter().map(any_value_to_json).collect())
        }
        Some(AnyValue::KvlistValue(kv)) => {
            let mut m = Map::new();
            for e in &kv.values {
                m.insert(e.key.clone(), any_value_to_json_opt(&e.value));
            }
            Json::Object(m)
        }
        Some(AnyValue::BytesValue(b)) => {
            use base64::Engine;
            Json::String(base64::engine::general_purpose::STANDARD.encode(b))
        }
        Some(AnyValue::StringValueStrindex(_)) | None => Json::Null,
    }
}

/// Render a log body for the `message` column: strings verbatim, everything
/// else as compact JSON.
fn any_value_display(v: &Option<AnyValueMsg>) -> String {
    match v {
        None => String::new(),
        Some(a) => match &a.value {
            Some(AnyValue::StringValue(s)) => s.clone(),
            _ => serde_json::to_string(&any_value_to_json(a)).unwrap_or_default(),
        },
    }
}

fn severity_to_level(number: i32, text: &str) -> String {
    match SeverityNumber::try_from(number) {
        Ok(SeverityNumber::Trace)
        | Ok(SeverityNumber::Trace2)
        | Ok(SeverityNumber::Trace3)
        | Ok(SeverityNumber::Trace4)
        | Ok(SeverityNumber::Debug)
        | Ok(SeverityNumber::Debug2)
        | Ok(SeverityNumber::Debug3)
        | Ok(SeverityNumber::Debug4) => "debug".into(),
        Ok(SeverityNumber::Info)
        | Ok(SeverityNumber::Info2)
        | Ok(SeverityNumber::Info3)
        | Ok(SeverityNumber::Info4) => "info".into(),
        Ok(SeverityNumber::Warn)
        | Ok(SeverityNumber::Warn2)
        | Ok(SeverityNumber::Warn3)
        | Ok(SeverityNumber::Warn4) => "warn".into(),
        Ok(SeverityNumber::Error)
        | Ok(SeverityNumber::Error2)
        | Ok(SeverityNumber::Error3)
        | Ok(SeverityNumber::Error4) => "error".into(),
        Ok(SeverityNumber::Fatal)
        | Ok(SeverityNumber::Fatal2)
        | Ok(SeverityNumber::Fatal3)
        | Ok(SeverityNumber::Fatal4) => "fatal".into(),
        _ => {
            // Unspecified / unknown number: fall back to the severity text.
            if text.is_empty() {
                "info".into()
            } else {
                crate::ingest::parse::normalize_level_public(text)
            }
        }
    }
}

fn nanos_to_dt(n: u64) -> Option<DateTime<Utc>> {
    if n == 0 {
        return None;
    }
    let secs = (n / 1_000_000_000) as i64;
    let nanos = (n % 1_000_000_000) as u32;
    DateTime::from_timestamp(secs, nanos)
}

fn json_f64(d: f64) -> Json {
    serde_json::Number::from_f64(d)
        .map(Json::Number)
        .unwrap_or(Json::Null)
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn record(
    proto: Protocol,
    obj: &Map<String, Json>,
    receive_ts: DateTime<Utc>,
    peer: &str,
) -> RawRecord {
    let bytes = serde_json::to_vec(&Json::Object(obj.clone())).unwrap_or_else(|_| b"{}".to_vec());
    RawRecord {
        receive_ts,
        source_addr: peer.to_string(),
        protocol: proto,
        raw: bytes::Bytes::from(bytes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_proto::tonic::common::v1::{any_value::Value as AV, KeyValue};
    use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
    use opentelemetry_proto::tonic::resource::v1::Resource;
    use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span, Status};

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }

    fn svc() -> Resource {
        Resource {
            attributes: vec![KeyValue {
                key: "service.name".into(),
                value: Some(AnyValueMsg {
                    value: Some(AV::StringValue("checkout".into())),
                }),
                key_strindex: 0,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn trace_envelope_has_core_fields() {
        let span = Span {
            trace_id: vec![0xab; 16],
            span_id: vec![0xcd; 8],
            parent_span_id: vec![0x11; 8],
            name: "GET /checkout".into(),
            kind: SpanKind::Server as i32,
            start_time_unix_nano: 1_700_000_000_000_000_000,
            end_time_unix_nano: 1_700_000_000_150_000_000,
            status: Some(Status {
                message: "db timeout".into(),
                code: StatusCode::Error as i32,
            }),
            attributes: vec![KeyValue {
                key: "http.status_code".into(),
                value: Some(AnyValueMsg {
                    value: Some(AV::IntValue(500)),
                }),
                key_strindex: 0,
            }],
            ..Default::default()
        };
        let req = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: Some(svc()),
                scope_spans: vec![ScopeSpans {
                    spans: vec![span],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        let recs = traces_to_records(req, now(), "1.2.3.4:5");
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].protocol, Protocol::OtlpSpan);
        let v: Json = serde_json::from_slice(&recs[0].raw).unwrap();
        assert_eq!(v["service"], "checkout");
        assert_eq!(v["level"], "error");
        assert_eq!(v["msg"], "GET /checkout");
        assert_eq!(v["trace_id"], "abababababababababababababababab");
        assert_eq!(v["span_id"], "cdcdcdcdcdcdcdcd");
        assert_eq!(v["parent_span_id"], "1111111111111111");
        assert_eq!(v["duration_ms"], 150.0);
        assert_eq!(v["http.status_code"], 500);
        assert_eq!(v["span_kind"], "SPAN_KIND_SERVER");
    }

    #[test]
    fn log_envelope_maps_severity_and_body() {
        let lr = LogRecord {
            time_unix_nano: 1_700_000_000_000_000_000,
            severity_number: SeverityNumber::Warn as i32,
            severity_text: "WARN".into(),
            body: Some(AnyValueMsg {
                value: Some(AV::StringValue("queue lag 12s".into())),
            }),
            trace_id: vec![0x01; 16],
            span_id: vec![0x02; 8],
            ..Default::default()
        };
        let req = ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(svc()),
                scope_logs: vec![ScopeLogs {
                    log_records: vec![lr],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        let recs = logs_to_records(req, now(), "");
        assert_eq!(recs[0].protocol, Protocol::OtlpLog);
        let v: Json = serde_json::from_slice(&recs[0].raw).unwrap();
        assert_eq!(v["service"], "checkout");
        assert_eq!(v["level"], "warn");
        assert_eq!(v["msg"], "queue lag 12s");
        assert_eq!(v["trace_id"], "01010101010101010101010101010101");
        assert_eq!(v["severity_text"], "WARN");
    }

    #[test]
    fn metric_envelope_value_and_labels() {
        use opentelemetry_proto::tonic::metrics::v1::{
            Gauge, Metric, ResourceMetrics, ScopeMetrics,
        };
        let metric = Metric {
            name: "http.server.duration".into(),
            unit: "ms".into(),
            data: Some(Data::Gauge(Gauge {
                data_points: vec![NumberDataPoint {
                    time_unix_nano: 1_700_000_000_000_000_000,
                    value: Some(NumberValue::AsDouble(42.5)),
                    attributes: vec![KeyValue {
                        key: "route".into(),
                        value: Some(AnyValueMsg {
                            value: Some(AV::StringValue("/health".into())),
                        }),
                        key_strindex: 0,
                    }],
                    ..Default::default()
                }],
            })),
            ..Default::default()
        };
        let req = ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                resource: Some(svc()),
                scope_metrics: vec![ScopeMetrics {
                    metrics: vec![metric],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        let recs = metrics_to_records(req, now(), "");
        assert_eq!(recs[0].protocol, Protocol::OtlpMetric);
        let v: Json = serde_json::from_slice(&recs[0].raw).unwrap();
        assert_eq!(v["msg"], "http.server.duration");
        assert_eq!(v["value"], 42.5);
        assert_eq!(v["route"], "/health");
        assert_eq!(v["metric_type"], "gauge");
        assert_eq!(v["unit"], "ms");
    }

    #[test]
    fn protobuf_and_json_requests_round_trip() {
        let req = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: Some(svc()),
                scope_spans: vec![ScopeSpans {
                    spans: vec![Span {
                        trace_id: vec![0x07; 16],
                        span_id: vec![0x08; 8],
                        name: "root".into(),
                        start_time_unix_nano: 1_700_000_000_000_000_000,
                        end_time_unix_nano: 1_700_000_000_010_000_000,
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };

        // Protobuf wire
        let mut buf = Vec::new();
        req.encode(&mut buf).unwrap();
        let decoded = decode_traces(Wire::Protobuf, &buf).expect("protobuf decode");
        let recs = traces_to_records(decoded, now(), "");
        assert_eq!(recs.len(), 1);
        let v: Json = serde_json::from_slice(&recs[0].raw).unwrap();
        assert_eq!(v["service"], "checkout");
        assert_eq!(v["trace_id"], "07070707070707070707070707070707");

        // OTLP/JSON wire (serde encoding from the same generated types).
        let json = serde_json::to_vec(&req).unwrap();
        let decoded = decode_traces(Wire::Json, &json).expect("json decode");
        let recs = traces_to_records(decoded, now(), "");
        assert_eq!(recs.len(), 1);
    }

    #[test]
    fn otlp_json_accepts_string_enum_names() {
        // The OTLP/JSON spec encodes enums as their protobuf string names.
        let json = br#"{
          "resourceSpans": [{
            "resource": {"attributes": [
              {"key": "service.name", "value": {"stringValue": "svc"}}
            ]},
            "scopeSpans": [{
              "spans": [{
                "traceId": "01010101010101010101010101010101",
                "spanId": "0202020202020202",
                "name": "root",
                "kind": "SPAN_KIND_SERVER",
                "startTimeUnixNano": "1700000000000000000",
                "endTimeUnixNano": "1700000000050000000",
                "status": {"code": "STATUS_CODE_ERROR", "message": "boom"}
              }]
            }]
          }]
        }"#;
        let req = decode_traces(Wire::Json, json).expect("decode string enums");
        let recs = traces_to_records(req, now(), "");
        let v: Json = serde_json::from_slice(&recs[0].raw).unwrap();
        assert_eq!(v["service"], "svc");
        assert_eq!(v["span_kind"], "SPAN_KIND_SERVER");
        assert_eq!(v["status_code"], "STATUS_CODE_ERROR");
        assert_eq!(v["level"], "error");
    }

    #[test]
    fn otlp_json_accepts_asint_as_decimal_string() {
        // The OTLP/JSON spec encodes int64 values (including the `asInt`
        // oneof branch of NumberDataPoint) as decimal strings. Java SDKs
        // rely on this for JVM/gauge metrics; the value must not be lost.
        let json = br#"{
          "resourceMetrics": [{
            "resource": {"attributes": [
              {"key": "service.name", "value": {"stringValue": "java-app"}}
            ]},
            "scopeMetrics": [{
              "metrics": [{
                "name": "jvm_memory_used_bytes",
                "gauge": {"dataPoints": [
                  {"timeUnixNano": "1700000000000000000", "asInt": "512000000",
                   "attributes": [{"key": "pool", "value": {"stringValue": "heap"}}]}
                ]}
              }]
            }]
          }]
        }"#;
        let req = decode_metrics(Wire::Json, json).expect("decode asInt string");
        let recs = metrics_to_records(req, now(), "");
        let v: Json = serde_json::from_slice(&recs[0].raw).unwrap();
        assert_eq!(v["value"], 512000000.0);
        assert_eq!(v["metric_type"], "gauge");
        assert_eq!(v["pool"], "heap");
    }

    #[test]
    fn detects_wire_and_otlp_json() {
        assert_eq!(detect_wire("application/x-protobuf"), Wire::Protobuf);
        assert_eq!(detect_wire("application/json"), Wire::Json);
        assert!(json_body_looks_like_otlp(br#"{"resourceLogs":[]}"#));
        assert!(!json_body_looks_like_otlp(
            br#"{"service":"x","msg":"plain"}"#
        ));
    }
}
