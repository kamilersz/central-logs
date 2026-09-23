//! Kubernetes API pod-log collector — pull pod container logs via the API
//! server (`GET /api/v1/namespaces/{ns}/pods/{pod}/log?follow=true`).
//!
//! API facts (k8s v1 core API):
//! - Auth: bearer token (literal or service-account token file — SA tokens
//!   rotate, so the file is re-read every discovery refresh) + cluster CA.
//! - Discovery: `GET /api/v1/pods[?labelSelector=…&fieldSelector=…]` or
//!   `GET /api/v1/namespaces/{ns}/pods` → `items[]` with
//!   `metadata.name/namespace`, `spec.nodeName`, `spec.containers[].name`.
//! - Follow: `GET /api/v1/namespaces/{ns}/pods/{pod}/log?container={c}
//!   &follow=true&timestamps=true&tailLines=N` → plain text stream, one
//!   `<RFC3339Nano> <line>` per line (stream ends when the pod dies).
//! - Pods have no stream multiplexing; stdout/stderr are merged by the
//!   kubelet, so severity defaults to info (parse-time text inference does
//!   not run for JSON envelopes — plain text lines are scanned for error
//!   keywords in the parser's free-text path only).
//!
//! Rows are tagged `protocol:k8s_api`; `service` = pod name, attributes
//! carry `namespace`/`pod`/`container`/`node`; `host` = node name.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::collector::{log_envelope, push_record, split_timestamp_prefix};
use crate::config::K8sCollectorConfig;
use crate::insert::counters::InsertCountersRef;
use crate::wal::InsertHandle;
use crate::Protocol;

pub fn spawn(
    cfg: K8sCollectorConfig,
    handle: InsertHandle,
    counters: InsertCountersRef,
    shutdown: CancellationToken,
) -> Option<JoinHandle<()>> {
    if !cfg.enabled {
        return None;
    }
    let client = match build_http_client(&cfg) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            tracing::error!(?e, "k8s collector: HTTP client build failed");
            return None;
        }
    };
    Some(tokio::spawn(async move {
        let attached: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let refresh = Duration::from_secs(cfg.refresh_secs.max(1));
        let namespaces = if cfg.namespaces.is_empty() {
            None
        } else {
            Some(cfg.namespaces.clone())
        };
        tracing::info!(
            api_url = %cfg.api_url,
            namespaces = ?cfg.namespaces,
            refresh_secs = cfg.refresh_secs,
            tail_lines = cfg.tail_lines,
            "kubernetes collector started"
        );
        let mut tick = tokio::time::interval(refresh);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                _ = tick.tick() => {
                    let token = read_token(&cfg);
                    if let Err(e) = discover(
                        &client, &cfg, &namespaces, &token, &handle, &counters,
                        &attached, &shutdown,
                    )
                    .await
                    {
                        tracing::debug!(?e, "k8s collector: discovery failed");
                    }
                }
            }
        }
        tracing::info!("kubernetes collector stopped");
    }))
}

/// One discovery pass: list pods, attach follow streams to new containers.
#[allow(clippy::too_many_arguments)]
async fn discover(
    client: &Arc<reqwest::Client>,
    cfg: &K8sCollectorConfig,
    namespaces: &Option<Vec<String>>,
    token: &str,
    handle: &InsertHandle,
    counters: &InsertCountersRef,
    attached: &Arc<Mutex<HashSet<String>>>,
    shutdown: &CancellationToken,
) -> Result<(), String> {
    let mut pods: Vec<serde_json::Value> = Vec::new();
    match namespaces {
        Some(nss) => {
            for ns in nss {
                let path = format!(
                    "/api/v1/namespaces/{ns}/pods{}",
                    selector_query(cfg, false)
                );
                let v = api_get_json(client, &cfg.api_url, &path, token).await?;
                if let Some(items) = v.get("items").and_then(|i| i.as_array()) {
                    pods.extend(items.iter().cloned());
                }
            }
        }
        None => {
            let v = api_get_json(
                client,
                &cfg.api_url,
                &format!("/api/v1/pods{}", selector_query(cfg, true)),
                token,
            )
            .await?;
            if let Some(items) = v.get("items").and_then(|i| i.as_array()) {
                pods = items.clone();
            }
        }
    }
    for pod in &pods {
        let meta = pod.get("metadata");
        let Some(pod_name) = meta
            .and_then(|m| m.get("name"))
            .and_then(|n| n.as_str())
        else {
            continue;
        };
        let ns = meta
            .and_then(|m| m.get("namespace"))
            .and_then(|n| n.as_str())
            .unwrap_or("default");
        let node = pod
            .get("spec")
            .and_then(|s| s.get("nodeName"))
            .and_then(|n| n.as_str())
            .unwrap_or_default()
            .to_string();
        let Some(containers) = pod
            .get("spec")
            .and_then(|s| s.get("containers"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for c in containers {
            let Some(cname) = c.get("name").and_then(|n| n.as_str()) else {
                continue;
            };
            let key = format!("{ns}/{pod_name}/{cname}");
            if attached.lock().contains(&key) {
                continue;
            }
            attached.lock().insert(key);
            let client = client.clone();
            let cfg = cfg.clone();
            let handle = handle.clone();
            let counters = counters.clone();
            let attached = attached.clone();
            let shutdown = shutdown.clone();
            let token = token.to_string();
            let pod_name = pod_name.to_string();
            let ns = ns.to_string();
            let cname = cname.to_string();
            let node = node.clone();
            tokio::spawn(async move {
                follow_pod(
                    client, cfg, handle, counters, attached, shutdown, token, ns,
                    pod_name, cname, node,
                )
                .await;
            });
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn follow_pod(
    client: Arc<reqwest::Client>,
    cfg: K8sCollectorConfig,
    handle: InsertHandle,
    counters: InsertCountersRef,
    attached: Arc<Mutex<HashSet<String>>>,
    shutdown: CancellationToken,
    token: String,
    ns: String,
    pod: String,
    container: String,
    node: String,
) {
    let path = format!(
        "/api/v1/namespaces/{ns}/pods/{pod}/log?container={container}&follow=true&timestamps=true&tailLines={}",
        cfg.tail_lines
    );
    let result = async {
        let resp = api_get_stream(&client, &cfg.api_url, &path, &token).await?;
        if !resp.status().is_success() {
            return Err(format!("pod log endpoint returned {}", resp.status()));
        }
        let mut resp = resp;
        let mut buf = Vec::with_capacity(4096);
        loop {
            // Cancellation-aware read: a quiet stream must not block shutdown.
            let chunk = tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                c = resp.chunk() => match c {
                    Ok(Some(c)) => c,
                    Ok(None) => break,
                    Err(e) => return Err(e.to_string()),
                },
            };
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let mut s = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
                if s.ends_with('\r') {
                    s.pop();
                }
                if !s.is_empty() {
                    push_line(&handle, &counters, &ns, &pod, &container, &node, s).await;
                }
            }
        }
        if !buf.is_empty() {
            let text = String::from_utf8_lossy(&buf).into_owned();
            push_line(&handle, &counters, &ns, &pod, &container, &node, text).await;
        }
        Ok::<(), String>(())
    };
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => {}
        res = result => {
            if let Err(e) = res {
                tracing::debug!(%ns, %pod, %container, %e, "k8s follow stream ended");
            }
        }
    }
    attached.lock().remove(&format!("{ns}/{pod}/{container}"));
}

async fn push_line(
    handle: &InsertHandle,
    counters: &InsertCountersRef,
    ns: &str,
    pod: &str,
    container: &str,
    node: &str,
    line: String,
) {
    let (ts, message) = split_timestamp_prefix(&line);
    let mut attrs = serde_json::Map::new();
    attrs.insert("namespace".into(), serde_json::json!(ns));
    attrs.insert("pod".into(), serde_json::json!(pod));
    attrs.insert("container".into(), serde_json::json!(container));
    if !node.is_empty() {
        attrs.insert("node".into(), serde_json::json!(node));
    }
    let env = log_envelope(message.to_string(), "info", pod.to_string(), ts, attrs);
    // host = node so source_host carries the node name.
    let mut env = env;
    if !node.is_empty() {
        env["host"] = serde_json::json!(node);
    }
    push_record(handle, counters, Protocol::K8sApi, env).await;
}

fn selector_query(cfg: &K8sCollectorConfig, all_namespaces: bool) -> String {
    let mut q = String::new();
    if !cfg.label_selector.trim().is_empty() {
        q = format!(
            "?labelSelector={}",
            urlencoding_lite(&cfg.label_selector)
        );
    }
    let _ = all_namespaces;
    q
}

/// Minimal percent-encoding for label selectors (`=`, `,`, `/`).
fn urlencoding_lite(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// =====================================================================
// HTTP plumbing (reqwest: TLS + bearer auth + streaming chunks)
// =====================================================================

fn build_http_client(cfg: &K8sCollectorConfig) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(0)) // no total timeout: follow streams are long-lived
        .connect_timeout(Duration::from_secs(10))
        .user_agent("central-logs-collector/1.0");
    if cfg.insecure_tls {
        builder = builder.danger_accept_invalid_certs(true);
    } else if !cfg.ca_file.trim().is_empty() {
        let pem = std::fs::read(&cfg.ca_file)
            .map_err(|e| format!("collector.kubernetes.ca_file: {e}"))?;
        let cert = reqwest::Certificate::from_pem(&pem)
            .map_err(|e| format!("collector.kubernetes.ca_file: {e}"))?;
        builder = builder.add_root_certificate(cert);
    }
    builder.build().map_err(|e| e.to_string())
}

/// Resolve the bearer token: literal config wins, else the token file
/// (re-read each call so rotated service-account tokens are picked up).
fn read_token(cfg: &K8sCollectorConfig) -> String {
    if !cfg.token.trim().is_empty() {
        return cfg.token.trim().to_string();
    }
    if !cfg.token_file.trim().is_empty() {
        if let Ok(t) = std::fs::read_to_string(&cfg.token_file) {
            return t.trim().to_string();
        }
    }
    String::new()
}

async fn api_get_json(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    token: &str,
) -> Result<serde_json::Value, String> {
    let url = format!("{}{}", base.trim_end_matches('/'), path);
    let mut req = client.get(&url);
    if !token.is_empty() {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp.bytes().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        let preview = String::from_utf8_lossy(&body);
        return Err(format!("GET {path} → {status}: {}", {
            let s: String = preview.chars().take(200).collect();
            s
        }));
    }
    serde_json::from_slice(&body).map_err(|e| format!("GET {path}: bad JSON: {e}"))
}

async fn api_get_stream(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    token: &str,
) -> Result<reqwest::Response, String> {
    let url = format!("{}{}", base.trim_end_matches('/'), path);
    let mut req = client.get(&url);
    if !token.is_empty() {
        req = req.bearer_auth(token);
    }
    req.send().await.map_err(|e| e.to_string())
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_selector_percent_encoding() {
        assert_eq!(urlencoding_lite("app=web,env=prod"), "app%3Dweb%2Cenv%3Dprod");
        assert_eq!(urlencoding_lite("app.kubernetes.io/name"), "app.kubernetes.io%2Fname");
        assert_eq!(urlencoding_lite("simple"), "simple");
    }

    #[test]
    fn selector_query_empty_when_unset() {
        let cfg = K8sCollectorConfig::default();
        assert_eq!(selector_query(&cfg, true), "");
        let mut cfg = cfg;
        cfg.label_selector = "app=web".into();
        assert_eq!(selector_query(&cfg, false), "?labelSelector=app%3Dweb");
    }

    #[test]
    fn token_resolution_prefers_literal_then_file() {
        let mut cfg = K8sCollectorConfig::default();
        cfg.token = " literal ".into();
        assert_eq!(read_token(&cfg), "literal");
        cfg.token = String::new();
        cfg.token_file = "/nonexistent/token".into();
        assert_eq!(read_token(&cfg), "");
    }
}
