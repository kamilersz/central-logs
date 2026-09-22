//! Error-groups API (error tracking, `docs/ERROR_TRACKING.md`): list, detail
//! (with stacktrace samples + sparkline), lifecycle mutations, and the AI
//! explain endpoint.
//!
//! Scopes (see `web::auth::required_scope`): reads are `read`; resolve /
//! unresolve / ignore are `write`; explain is `read` (no mutation).

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::errors::{self, ListParams};

use super::api::ApiState;

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/api/error-groups", get(list_error_groups))
        .route(
            "/api/error-groups/{fingerprint}",
            get(get_error_group).delete(delete_error_group),
        )
        .route(
            "/api/error-groups/{fingerprint}/resolve",
            post(resolve_group),
        )
        .route(
            "/api/error-groups/{fingerprint}/unresolve",
            post(unresolve_group),
        )
        .route("/api/error-groups/{fingerprint}/ignore", post(ignore_group))
        .route(
            "/api/error-groups/{fingerprint}/explain",
            post(explain_group),
        )
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(state)
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    /// Relative window for "active in" — `1h`, `24h` (default), `7d`, `30d`.
    #[serde(default = "default_window")]
    window: String,
    /// unresolved (default) | resolved | ignored | all
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    service: Option<String>,
    /// Free-text search across title + exception_type.
    #[serde(default)]
    q: Option<String>,
    /// recent (default) | count
    #[serde(default = "default_sort")]
    sort: String,
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
}

fn default_window() -> String {
    "24h".into()
}
fn default_sort() -> String {
    "recent".into()
}
fn default_limit() -> i64 {
    100
}

/// Resolve a window shorthand using the shared parser (falls back to 24h).
fn window_secs(s: &str) -> i64 {
    super::api::parse_window_secs(s).unwrap_or(24 * 3600)
}

async fn list_error_groups(State(st): State<ApiState>, Query(q): Query<ListQuery>) -> Response {
    let conn = st.store.lock();
    let params = ListParams {
        window_secs: window_secs(&q.window),
        status: q.status,
        service: q.service,
        q: q.q,
        sort: q.sort,
        limit: q.limit.clamp(1, 500),
        offset: q.offset.max(0),
    };
    match errors::list_groups(&conn, &params) {
        Ok((groups, total)) => {
            Json(serde_json::json!({ "groups": groups, "total": total })).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn get_error_group(State(st): State<ApiState>, Path(fingerprint): Path<String>) -> Response {
    let conn = st.store.lock();
    // Sparkline covers the last 24h of per-minute counts.
    match errors::get_group(&conn, &fingerprint, 24 * 3600) {
        Ok(Some(detail)) => Json(detail).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no such error group" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Deleting a group row is a maintenance action (its events remain). Admin
/// scope is enforced here in addition to the middleware's write check —
/// deleting history has more blast radius than resolving.
async fn delete_error_group(
    State(st): State<ApiState>,
    Path(fingerprint): Path<String>,
) -> Response {
    let conn = st.store.lock();
    match conn.execute(
        "DELETE FROM error_groups WHERE fingerprint = ?",
        duckdb::params![fingerprint],
    ) {
        Ok(n) if n > 0 => {
            Json(serde_json::json!({ "fingerprint": fingerprint, "deleted": true })).into_response()
        }
        Ok(_) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no such error group" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn set_group_status(st: &ApiState, fingerprint: &str, status: &str) -> Response {
    let conn = st.store.lock();
    match errors::set_status(&conn, fingerprint, status) {
        Ok(true) => Json(serde_json::json!({ "fingerprint": fingerprint, "status": status }))
            .into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no such error group" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn resolve_group(State(st): State<ApiState>, Path(fp): Path<String>) -> Response {
    set_group_status(&st, &fp, "resolved").await
}

async fn unresolve_group(State(st): State<ApiState>, Path(fp): Path<String>) -> Response {
    set_group_status(&st, &fp, "unresolved").await
}

async fn ignore_group(State(st): State<ApiState>, Path(fp): Path<String>) -> Response {
    set_group_status(&st, &fp, "ignored").await
}

#[derive(Debug, Deserialize)]
struct ExplainBody {
    /// Optional free-text context from the user ("it started after deploy X").
    #[serde(default)]
    context: Option<String>,
}

/// AI explain: sends the group's title/exception/sample stack (NOT raw log
/// data beyond this group's own trace) to the configured LLM and returns a
/// plain-language explanation + likely cause + suggested fix. With `[llm]`
/// unconfigured we return a helpful hint instead of an error so the button
/// degrades gracefully.
async fn explain_group(
    State(st): State<ApiState>,
    Path(fingerprint): Path<String>,
    body: Option<Json<ExplainBody>>,
) -> Response {
    let detail = {
        let conn = st.store.lock();
        match errors::get_group(&conn, &fingerprint, 24 * 3600) {
            Ok(Some(d)) => d,
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": "no such error group" })),
                )
                    .into_response()
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": e.to_string() })),
                )
                    .into_response()
            }
        }
    };

    let llm = st.llm.as_ref();
    if let crate::ai::LlmConfig::Off = llm {
        return Json(serde_json::json!({
            "provider": "off",
            "explanation": null,
            "hint": "No LLM provider is configured. Set [llm] in the TOML config \
                     (provider + api_key + model) or CENTRAL_LOGS_LLM_API_KEY / \
                     CENTRAL_LOGS_LLM_MODEL to enable AI explanations.",
        }))
        .into_response();
    }

    // Compose the prompt from the group's own metadata + best sample stack.
    let sample_stack = detail
        .samples
        .as_array()
        .and_then(|a| a.last())
        .and_then(|s| s.get("stack"))
        .and_then(|v| v.as_str())
        .unwrap_or("(no stacktrace captured — message-only group)")
        .chars()
        .take(6000)
        .collect::<String>();
    let mut user_msg = format!(
        "Service: {}\nLevel: {}\nError: {}\nException type: {}\nOccurrences (retained): {}\n\
         First seen: {}\nLast seen: {}\n\nStacktrace (newest frame first):\n{}\n",
        detail.group.service.as_deref().unwrap_or("unknown"),
        detail.group.level.as_deref().unwrap_or("error"),
        detail.group.title.as_deref().unwrap_or("(untitled)"),
        detail.group.exception_type.as_deref().unwrap_or("(none)"),
        detail.group.total_count,
        detail.group.first_seen.as_deref().unwrap_or("?"),
        detail.group.last_seen.as_deref().unwrap_or("?"),
        sample_stack,
    );
    if let Some(Json(b)) = &body {
        if let Some(ctx) = b.context.as_deref() {
            if !ctx.trim().is_empty() {
                user_msg.push_str(&format!("\nOperator context: {}\n", ctx.trim()));
            }
        }
    }

    const SYSTEM: &str = "You are a senior on-call engineer. A centralized logging \
         platform shows the error group below. Explain it for the on-call human in \
         plain language: (1) what is happening, (2) the most likely root cause(s), \
         (3) concrete next steps or fixes, ranked. Be specific to the stacktrace; \
         if information is missing, say what to check. Keep it under 250 words. \
         Plain text, no markdown fences.";

    match crate::ai::complete(llm, SYSTEM, &user_msg, 700).await {
        Ok((text, provider)) => Json(serde_json::json!({
            "provider": provider,
            "explanation": text,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": format!("LLM call failed: {e}"),
                "provider": crate::ai::provider_name(llm),
            })),
        )
            .into_response(),
    }
}
