//! Server-side LLM calls: natural-language → filter DSL, and the AI dashboard
//! builder.
//!
//! Two flows share one provider layer (OpenAI / Anthropic / 9inference /
//! Off):
//!
//! 1. **Filter translation** — the user types something like "show me errors
//!    from the last hour for user 42 in production" and gets back a filter
//!    DSL string the UI applies directly.
//! 2. **Dashboard builder** — the user describes a dashboard in prose; the
//!    model either asks clarifying questions (multiple-choice when the answer
//!    set is enumerable, free text otherwise) or emits a concrete dashboard
//!    panel spec that is validated server-side before it reaches the UI.
//!
//! The LLM never sees actual log data — only the schema, aggregate service
//! names, and the user's text. Returned DSL is parsed by
//! [`crate::query::parse_filter`] before being applied, so even a misbehaving
//! model can't produce dangerous SQL.

use serde::{Deserialize, Serialize};

use crate::hot::HotAttribute;
use crate::query::ColumnWhitelist;

/// LLM provider configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum LlmConfig {
    Openai {
        api_key: String,
        model: String,
        /// Override for OpenAI-compatible gateways (default: api.openai.com).
        #[serde(default)]
        base_url: Option<String>,
    },
    Anthropic {
        api_key: String,
        model: String,
        /// Override for Anthropic-protocol gateways (MiniMax coding plan,
        /// Zhipu, LiteLLM, …). Default: https://api.anthropic.com.
        #[serde(default)]
        base_url: Option<String>,
    },
    /// 9inference.cloud — OpenAI-compatible API. Behind Cloudflare, so we send
    /// a browser User-Agent (Cloudflare blocks rustls/reqwest's default UA).
    /// Always uses `stream: true` per the provider's recommended usage.
    #[serde(rename = "9inference")]
    NineInference { api_key: String, model: String },
    /// Skip the LLM call entirely; return a canned DSL hint. Useful for tests.
    Off,
}

impl Default for LlmConfig {
    fn default() -> Self {
        // Always Off here: env-based setup goes through the flat contract in
        // `config::load()` (CENTRAL_LOGS_LLM_PROVIDER / _API_KEY / _MODEL /
        // _BASE_URL), which supports anthropic, openai, and 9inference and
        // runs when [llm] is unset (i.e. when this default is used).
        Self::Off
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AiQueryRequest {
    /// Natural-language query from the user.
    pub query: String,
    /// Optional time-range hint in the same DSL the LLM should produce
    /// (e.g. `from:-1h`). The UI may pre-fill this from the current picker.
    #[serde(default)]
    pub time_hint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AiQueryResponse {
    /// The DSL string the UI should apply to its filter input.
    pub filter: String,
    /// The provider that produced the answer (for transparency).
    pub provider: String,
    /// Raw model output before parsing (for debugging display in the UI).
    pub raw: String,
}

const SYSTEM_PROMPT: &str = r#"You are a log query assistant for the central-logs platform.

Convert the user's natural-language request into a structured filter DSL.

# DSL syntax

- A filter is a space-separated list of clauses. Adjacent clauses combine with
  implicit AND; explicit `AND` / `OR` keywords (case-insensitive) are also
  accepted. AND binds tighter than OR.
- A leading `-` negates a clause (exclusion).
- Each clause is either:
  - `key:value` — equality on a known column.
  - `key>value`, `key>=value`, `key<value`, `key<=value`, `key!=value` — comparisons.
  - `key~value` — substring match (case-insensitive).
  - `key:"value with spaces"` — quoted value containing whitespace.
  - bare `text` — implicit `message ILIKE '%text%'` substring search.
  - any of the above prefixed with `-` — e.g. `-service:central-logs` excludes it.

# Available columns

You will be given a list of column names and types. Only filter on those columns. Common ones:

- `service`, `level`, `source_host`, `protocol`, `trace_id`, `geo_country`, `message`
- `raw_len` (bytes) — numeric
- Plus configured hot attributes (high-cardinality IDs, env tags, etc.)

# Output format

Respond with ONLY the filter DSL string, nothing else. No prose, no markdown fences, no explanation. Just the DSL.

# Examples

- "errors in the api service" → `service:api level:error`
- "user 42 errors" → `user_id:42 level:error`
- "high-latency requests" → `duration_ms>1000`
- "connection refused in production" → `env:prod "connection refused"`
- "errors from api or web" → `service:api OR service:web level:error`
- "errors excluding internal logs" → `level:error -service:central-logs`
"#;

/// Call the configured LLM and translate the user's NL query into a filter DSL.
pub async fn translate(
    cfg: &LlmConfig,
    request: &AiQueryRequest,
    hot: &[HotAttribute],
) -> Result<AiQueryResponse, crate::Error> {
    let columns = ColumnWhitelist::standard(hot);
    let mut col_list = String::new();
    for k in columns.keys() {
        col_list.push_str(&format!("- `{k}`\n"));
    }
    let user_msg = format!(
        "Available columns:\n{col_list}\nUser query: {q}",
        q = request.query.trim()
    );

    let (filter, provider, raw) = match cfg {
        LlmConfig::Off => {
            // No LLM configured: return a hint that surfaces the schema so the
            // UI can still demo the input. The UI should also show a warning.
            (
                suggest_from_keywords(&request.query, &columns),
                "off".to_string(),
                "LLM disabled in config".to_string(),
            )
        }
        LlmConfig::Openai {
            api_key,
            model,
            base_url,
        } => {
            let resp =
                call_openai(api_key, model, base_url.as_deref(), SYSTEM_PROMPT, &user_msg, 200)
                    .await?;
            (parse_model_output(&resp), "openai".to_string(), resp)
        }
        LlmConfig::Anthropic {
            api_key,
            model,
            base_url,
        } => {
            let resp =
                call_anthropic(api_key, model, base_url.as_deref(), SYSTEM_PROMPT, &user_msg, 200)
                    .await?;
            (parse_model_output(&resp), "anthropic".to_string(), resp)
        }
        LlmConfig::NineInference { api_key, model } => {
            let resp = call_9inference(api_key, model, SYSTEM_PROMPT, &user_msg, 200).await?;
            (
                parse_model_output(&resp),
                "9inference".to_string(),
                resp,
            )
        }
    };

    // Validate the LLM's output against our parser — if it doesn't parse,
    // return an error rather than surfacing garbage to the UI.
    crate::query::parse_filter(&filter)
        .map_err(|e| crate::Error::config(format!("LLM produced invalid DSL: {e}")))?;

    Ok(AiQueryResponse {
        filter,
        provider,
        raw,
    })
}

// =====================================================================
// AI dashboard builder
// =====================================================================

/// One Q/A pair sent back by the SPA when it has collected the user's
/// clarifying answers. The server is stateless — the UI holds the
/// questions and echoes them with answers.
#[derive(Debug, Clone, Deserialize)]
pub struct AiDashboardAnswer {
    pub question: String,
    pub answer: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AiDashboardRequest {
    /// The user's free-form description of the dashboard they want.
    pub description: String,
    /// Answers to a previous "clarify" round (may be empty on the first call).
    #[serde(default)]
    pub answers: Vec<AiDashboardAnswer>,
    /// Revision mode: the dashboard to modify (from a previous build).
    #[serde(default)]
    pub current: Option<AiDashboardProposal>,
    /// Revision mode: what to change about `current`.
    #[serde(default)]
    pub revision: Option<String>,
}

/// A clarifying question. `choices` is present when the answer set is
/// enumerable (services, levels, window sizes…); absent for open-ended
/// questions. The SPA renders choices as a picker and free-text otherwise.
#[derive(Debug, Clone, Serialize)]
pub struct AiDashboardQuestion {
    pub id: usize,
    pub question: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub choices: Option<Vec<String>>,
}

/// A validated panel proposal. Mirrors the SPA's panel model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiPanel {
    #[serde(rename = "type")]
    pub panel_type: String,
    pub title: String,
    #[serde(default = "default_window")]
    pub window: String,
    #[serde(default)]
    pub filter: String,
    #[serde(default = "default_viz")]
    pub viz: String,
    /// Width on a 12-column grid (1..=12). None = viewer default (6).
    #[serde(default)]
    pub w: Option<u8>,
}

fn default_window() -> String {
    "1h".into()
}
fn default_viz() -> String {
    "chart".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiDashboardProposal {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub panels: Vec<AiPanel>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AiDashboardResponse {
    /// "clarify" (ask the questions) or "build" (here is the dashboard).
    pub stage: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<AiDashboardQuestion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dashboard: Option<AiDashboardProposal>,
    /// Provider that produced the answer (for transparency / "off" notice).
    pub provider: String,
    /// Raw model output (debugging display in the UI).
    pub raw: String,
}

/// Data context the handler gathers so the model stays grounded: which
/// columns the filter DSL accepts and which services actually exist.
pub struct DashboardContext {
    pub columns: Vec<String>,
    pub services: Vec<String>,
}

/// The panel vocabulary the builder (LLM or heuristic) must draw from.
pub const DASHBOARD_PANEL_TYPES: &[&str] = &[
    "volume",
    "error-rate",
    "latency",
    "top-services",
    "log-count",
    "anomalies",
];
pub const DASHBOARD_VIZ_TYPES: &[&str] = &["chart", "number"];
pub const DASHBOARD_WINDOWS: &[&str] = &["5m", "15m", "1h", "6h", "24h", "7d"];

const DASHBOARD_SYSTEM_PROMPT: &str = r#"You are a dashboard design assistant for the central-logs platform.
Turn the user's loose description into a concrete dashboard, asking sharp
clarifying questions first when needed.

# Panel model

Each panel has:
- `type`: one of "volume" (records over time), "error-rate" (error count +
  percentage over time), "latency" (p50/p95/p99 per service, from the
  `duration_ms` attribute), "top-services" (ranked table), "log-count"
  (total matching records), "anomalies" (flagged volume anomalies with
  severity).
- `viz`: "chart" (timeline) or "number" (a single big aggregate over the
  period, e.g. total errors, avg rate, max p95). Use "number" for KPI-style
  panels, "chart" for trends.
- `window`: one of "5m", "15m", "1h", "6h", "24h", "7d".
- `filter`: optional filter DSL (syntax below), e.g. "service:api level:error".
- `w`: width on a 12-column grid — one of 3, 4, 6, 8, 12. Suggest 3 for
  "number" KPIs, 4 for tables, 6 for charts, 12 only for hero charts.
- `title`: short human label.

# Filter DSL syntax

- Space-separated clauses, implicit AND: `service:api level:error`.
- `key:value` equality; `key!=value`; `key~text` substring;
  `key>=n` / `key>n` / `key<=n` / `key<n` comparisons (numeric columns only).
- Quoted values: `message:"connection refused"`.
- Bare text searches the message: `timeout`.
- Only filter on the columns provided in the context. Never invent columns.

# Interaction protocol

Respond with EXACTLY ONE JSON object. No markdown fences, no prose.

If the description is ambiguous or missing key details, ask up to 3
clarifying questions:
{"stage":"clarify","questions":[{"id":1,"question":"Which service should the panels focus on?","choices":["api","web","all services"]},{"id":2,"question":"Any specific error message or keyword to track?","choices":null},{"id":3,"question":"Chart over time or a single big number?","choices":["chart","number"]}]}

- Include `choices` (2-5 concrete options) whenever the answer set is
  enumerable — services seen in the data, standard windows, chart-vs-number.
- Omit `choices` (or use null) for open-ended questions (free-text keywords,
  thresholds).
- Never ask what the user already answered or stated.
- If the description is already fully concrete, go straight to build.

When you have enough information (first call, or after answers), emit:
{"stage":"build","dashboard":{"name":"Payments ops","description":"Errors + latency for payment services","panels":[{"type":"error-rate","title":"Payment errors","window":"24h","viz":"chart","filter":"service:payment level:error"},{"type":"log-count","title":"Failed checkouts (24h)","window":"24h","viz":"number","filter":"service:payment \"checkout failed\""},{"type":"latency","title":"Latency p95","window":"1h","viz":"chart","filter":"service:payment"}]}}

Rules:
- Typically 2-6 panels. Prefer "chart" for trends, "number" for KPIs; a mix
  is good.
- Only reference services that appear in the context (or leave the filter
  empty for all services).
- Keep filters valid per the DSL syntax; combine clauses with spaces.

# Revision mode

When the message contains a # Current dashboard section you are REVISING
an existing dashboard. The # Revision request says what to change (add /
remove / reorder panels, switch viz, resize widths, tweak filters or
windows, rename). Apply the changes, keep everything else exactly as-is,
and return the COMPLETE updated dashboard in the build format above. Only
ask a clarifying question if the request is truly uninterpretable. Never
drop or reorder panels unless the request asks for it.
"#;

/// Entry point for the AI dashboard builder. Returns either clarifying
/// questions or a validated dashboard proposal. In revision mode (current +
/// revision set) the previous proposal is echoed back with the requested
/// changes applied.
pub async fn build_dashboard(
    cfg: &LlmConfig,
    request: &AiDashboardRequest,
    ctx: &DashboardContext,
    hot: &[HotAttribute],
) -> Result<AiDashboardResponse, crate::Error> {
    let description = request.description.trim();
    let revision_mode = request.current.is_some()
        && request
            .revision
            .as_deref()
            .map(|r| !r.trim().is_empty())
            .unwrap_or(false);
    if description.is_empty() && !revision_mode {
        return Err(crate::Error::invalid_input(
            "description must not be empty",
        ));
    }

    let user_msg = if revision_mode {
        format_revision_context(
            description,
            request.current.as_ref().expect("checked above"),
            request.revision.as_deref().unwrap_or_default().trim(),
            &request.answers,
            ctx,
        )
    } else {
        format_user_context(description, &request.answers, ctx)
    };

    let (provider, raw) = match cfg {
        LlmConfig::Off => ("off".to_string(), String::new()),
        LlmConfig::Openai {
            api_key,
            model,
            base_url,
        } => (
            "openai".to_string(),
            call_openai(
                api_key,
                model,
                base_url.as_deref(),
                DASHBOARD_SYSTEM_PROMPT,
                &user_msg,
                1200,
            )
            .await?,
        ),
        LlmConfig::Anthropic {
            api_key,
            model,
            base_url,
        } => (
            "anthropic".to_string(),
            call_anthropic(
                api_key,
                model,
                base_url.as_deref(),
                DASHBOARD_SYSTEM_PROMPT,
                &user_msg,
                1200,
            )
            .await?,
        ),
        LlmConfig::NineInference { api_key, model } => (
            "9inference".to_string(),
            call_9inference(api_key, model, DASHBOARD_SYSTEM_PROMPT, &user_msg, 1200).await?,
        ),
    };

    if matches!(cfg, LlmConfig::Off) {
        // No LLM: deterministic best-effort dashboard from keywords, no
        // clarify round — the UI flags the "off" provider.
        let (proposal, note) = if revision_mode {
            let current = request.current.as_ref().expect("checked above");
            let revision = request.revision.as_deref().unwrap_or_default();
            let (p, note) = heuristic_revision(current, revision, ctx);
            (p, format!("LLM disabled in config; {note}"))
        } else {
            (
                heuristic_dashboard(description, ctx),
                "LLM disabled in config; generated a starter dashboard from keywords".to_string(),
            )
        };
        return Ok(AiDashboardResponse {
            stage: "build".into(),
            questions: Vec::new(),
            dashboard: Some(proposal),
            provider,
            raw: note,
        });
    }

    let parsed = parse_dashboard_reply(&raw)
        .map_err(|e| crate::Error::config(format!("LLM reply not parseable: {e}")))?;
    match parsed.stage.as_str() {
        "clarify" => {
            if parsed.questions.is_empty() {
                return Err(crate::Error::config(
                    "LLM returned clarify stage with no questions",
                ));
            }
            Ok(AiDashboardResponse {
                stage: "clarify".into(),
                questions: parsed
                    .questions
                    .into_iter()
                    .enumerate()
                    .map(|(i, q)| AiDashboardQuestion {
                        id: i + 1,
                        question: q.question,
                        choices: q.choices.filter(|c| !c.is_empty()),
                    })
                    .collect(),
                dashboard: None,
                provider,
                raw,
            })
        }
        _ => {
            let Some(dash) = parsed.dashboard else {
                return Err(crate::Error::config(
                    "LLM returned build stage with no dashboard",
                ));
            };
            let proposal = validate_proposal(dash, hot)?;
            Ok(AiDashboardResponse {
                stage: "build".into(),
                questions: Vec::new(),
                dashboard: Some(proposal),
                provider,
                raw,
            })
        }
    }
}

/// Assemble the user-side context message: columns, known services, the
/// description, and any answers from a previous clarify round.
fn format_user_context(
    description: &str,
    answers: &[AiDashboardAnswer],
    ctx: &DashboardContext,
) -> String {
    let mut s = String::new();
    s.push_str("# Context\n\nFilterable columns:\n");
    for c in &ctx.columns {
        s.push_str(&format!("- `{c}`\n"));
    }
    if ctx.services.is_empty() {
        s.push_str("\nServices seen in data: (none yet)\n");
    } else {
        s.push_str("\nServices seen in data (top by volume):\n");
        for svc in &ctx.services {
            s.push_str(&format!("- `{svc}`\n"));
        }
    }
    s.push_str(&format!("\n# User request\n\n{description}\n"));
    if !answers.is_empty() {
        s.push_str("\n# Answers to previous clarifying questions\n\n");
        for a in answers {
            s.push_str(&format!("Q: {}\nA: {}\n\n", a.question, a.answer));
        }
    }
    s
}

/// Revision-mode context: the current dashboard as JSON + what to change.
fn format_revision_context(
    description: &str,
    current: &AiDashboardProposal,
    revision: &str,
    answers: &[AiDashboardAnswer],
    ctx: &DashboardContext,
) -> String {
    let mut s = format_user_context(description, answers, ctx);
    let cur = serde_json::to_string_pretty(current).unwrap_or_default();
    s.push_str("# Current dashboard\n\n```json\n");
    s.push_str(&cur);
    s.push_str("\n```\n\n");
    s.push_str(&format!("# Revision request\n\n{revision}\n"));
    s
}

/// Deterministic revision fallback for `LlmConfig::Off`: understands simple
/// add/remove requests by panel-type keyword ("add a latency panel",
/// "remove anomalies"). Anything subtler returns the dashboard unchanged
/// with an honest note.
fn heuristic_revision(
    current: &AiDashboardProposal,
    revision: &str,
    ctx: &DashboardContext,
) -> (AiDashboardProposal, String) {
    let lower = revision.to_ascii_lowercase();
    let mut panels = current.panels.clone();
    let mut note = String::from("applied simple keyword revisions");

    // Reuse the most common existing filter for newly added panels; prefer a
    // service the request mentions if it exists in the data.
    let mut filter = panels
        .iter()
        .filter(|p| !p.filter.is_empty())
        .next()
        .map(|p| p.filter.clone())
        .unwrap_or_default();
    for svc in &ctx.services {
        if lower.contains(&svc.to_ascii_lowercase()) {
            filter = format!("service:{svc}");
            break;
        }
    }

    let removing = lower.contains("remove") || lower.contains("delete") || lower.contains("drop");
    for (kw, panel_type, title) in [
        ("latency", "latency", "Latency p95"),
        ("anomal", "anomalies", "Anomalies"),
        ("volume", "volume", "Volume"),
        ("error", "error-rate", "Error rate"),
        ("top service", "top-services", "Top services"),
        ("top-service", "top-services", "Top services"),
    ] {
        if !lower.contains(kw) {
            continue;
        }
        if removing {
            let before = panels.len();
            panels.retain(|p| p.panel_type != panel_type);
            if panels.len() < before {
                note.push_str(&format!(" (removed {panel_type})"));
            }
        } else if !panels.iter().any(|p| p.panel_type == panel_type) {
            panels.push(AiPanel {
                panel_type: panel_type.into(),
                title: title.into(),
                window: "1h".into(),
                filter: filter.clone(),
                viz: "chart".into(),
                w: Some(6),
            });
            note.push_str(&format!(" (added {panel_type})"));
        }
    }
    if panels.is_empty() {
        return (
            current.clone(),
            "revision would remove every panel; returned the dashboard unchanged".into(),
        );
    }
    let mut out = current.clone();
    out.panels = panels;
    (out, note)
}

/// Internal mirror of the model's JSON reply (lenient: unknown fields
/// ignored, missing stage defaults to build).
#[derive(Debug, Clone, Deserialize)]
struct ModelReply {
    #[serde(default)]
    stage: String,
    #[serde(default)]
    questions: Vec<ModelQuestion>,
    #[serde(default)]
    dashboard: Option<ModelDashboard>,
}

#[derive(Debug, Clone, Deserialize)]
struct ModelQuestion {
    #[serde(default)]
    question: String,
    #[serde(default)]
    choices: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
struct ModelDashboard {
    #[serde(default = "default_name")]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    panels: Vec<AiPanel>,
}

fn default_name() -> String {
    "AI dashboard".into()
}

/// Extract the JSON object from the model output. Tolerates markdown fences
/// and surrounding prose: takes the first `{` to the last `}` and parses.
fn parse_dashboard_reply(raw: &str) -> Result<ModelReply, String> {
    let trimmed = raw.trim();
    let inner = trimmed
        .strip_prefix("```")
        .and_then(|s| s.strip_prefix("json").or_else(|| Some(s)))
        .unwrap_or(trimmed);
    let inner = inner.strip_suffix("```").unwrap_or(inner);
    let candidate = match (inner.find('{'), inner.rfind('}')) {
        (Some(a), Some(b)) if b > a => &inner[a..=b],
        _ => return Err("no JSON object found in output".into()),
    };
    serde_json::from_str(candidate).map_err(|e| e.to_string())
}

/// Validate + sanitize the proposal: unknown panel types / viz kinds,
/// unparsable filters, or bogus windows drop that panel (rather than
/// failing the whole build). Empty result is an error.
fn validate_proposal(dash: ModelDashboard, hot: &[HotAttribute]) -> Result<AiDashboardProposal, crate::Error> {
    let whitelist = ColumnWhitelist::standard(hot);
    let mut panels = Vec::new();
    for mut p in dash.panels {
        if !DASHBOARD_PANEL_TYPES.contains(&p.panel_type.as_str()) {
            continue;
        }
        if !DASHBOARD_VIZ_TYPES.contains(&p.viz.as_str()) {
            p.viz = default_viz();
        }
        if !valid_window(&p.window) {
            p.window = default_window();
        }
        if !p.filter.is_empty() {
            let ok = crate::query::parse_filter(&p.filter)
                .ok()
                .and_then(|c| c.to_sql(&whitelist).ok())
                .is_some();
            if !ok {
                p.filter = String::new();
            }
        }
        // Width lives on the 12-column grid; clamp anything else into range.
        p.w = p.w.map(|w| w.clamp(1, 12));
        if p.title.trim().is_empty() {
            p.title = p.panel_type.clone();
        }
        panels.push(p);
    }
    if panels.is_empty() {
        return Err(crate::Error::config(
            "LLM proposal contained no valid panels",
        ));
    }
    let name = if dash.name.trim().is_empty() {
        default_name()
    } else {
        dash.name
    };
    Ok(AiDashboardProposal {
        name,
        description: dash.description,
        panels,
    })
}

/// Windows like `5m`, `1h`, `7d` (number + single unit char s/m/h/d/w).
fn valid_window(w: &str) -> bool {
    if w.len() < 2 {
        return false;
    }
    let split = w
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(w.len());
    let (num, unit) = w.split_at(split);
    !num.is_empty()
        && num.bytes().all(|b| b.is_ascii_digit())
        && matches!(unit, "s" | "m" | "h" | "d" | "w")
}

/// Deterministic fallback for `LlmConfig::Off`: scans the description for
/// known services and intent keywords, then emits a sensible 3-panel
/// starter dashboard.
fn heuristic_dashboard(description: &str, ctx: &DashboardContext) -> AiDashboardProposal {
    let lower = description.to_ascii_lowercase();
    let mut filter = String::new();
    // Prefer a service the user mentioned that actually exists.
    for svc in &ctx.services {
        if lower.contains(&svc.to_ascii_lowercase()) {
            filter = format!("service:{svc}");
            break;
        }
    }
    let f = filter.as_str();
    let mut panels = vec![
        AiPanel {
            panel_type: "volume".into(),
            title: if f.is_empty() { "Volume".into() } else { format!("Volume · {f}") },
            window: "1h".into(),
            filter: f.to_string(),
            viz: "chart".into(),
            w: Some(6),
        },
        AiPanel {
            panel_type: "error-rate".into(),
            title: "Error rate".into(),
            window: "24h".into(),
            filter: f.to_string(),
            viz: "chart".into(),
            w: Some(6),
        },
    ];
    if !f.is_empty() || lower.contains("latency") || lower.contains("slow") {
        panels.push(AiPanel {
            panel_type: "latency".into(),
            title: "Latency p95".into(),
            window: "1h".into(),
            filter: f.to_string(),
            viz: "chart".into(),
            w: Some(6),
        });
    }
    if lower.contains("anomal") {
        panels.push(AiPanel {
            panel_type: "anomalies".into(),
            title: "Anomalies".into(),
            window: "24h".into(),
            filter: f.to_string(),
            viz: "chart".into(),
            w: Some(12),
        });
    }
    AiDashboardProposal {
        name: if f.is_empty() {
            "Starter dashboard".into()
        } else {
            format!("{} dashboard", filter.split(':').nth(1).unwrap_or("service"))
        },
        description: format!("Generated from: {description}"),
        panels,
    }
}


/// Strip markdown fences / leading prose from model output. The system prompt
/// asks for "ONLY the DSL" but models don't always comply.
fn parse_model_output(raw: &str) -> String {
    // Strip ```fences``` if present.
    let trimmed = raw.trim();
    let inner = trimmed
        .strip_prefix("```")
        .and_then(|s| s.strip_prefix('\n').or_else(|| Some(s)))
        .unwrap_or(trimmed)
        .trim();
    let inner = inner.strip_suffix("```").unwrap_or(inner).trim();
    // If there are multiple lines, the DSL is usually the last non-empty one.
    let lines: Vec<&str> = inner.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    lines.last().copied().unwrap_or("").to_string()
}

/// Cheap local fallback: pull tokens out of the user's text that happen to
/// match known column names. e.g. "errors from api" → `service:api level:error`.
fn suggest_from_keywords(query: &str, columns: &ColumnWhitelist) -> String {
    let lower = query.to_ascii_lowercase();
    let mut parts: Vec<String> = Vec::new();
    if lower.contains("error") || lower.contains("fail") {
        parts.push("level:error".into());
    }
    if lower.contains("warn") {
        parts.push("level:warn".into());
    }
    // Match known column names mentioned by the user, e.g. "user_id" or "service".
    for key in columns.keys() {
        if lower.contains(key) {
            parts.push(format!("{key}:"));
        }
    }
    parts.join(" ")
}

/// Display name of the configured provider (for API transparency fields).
pub fn provider_name(cfg: &LlmConfig) -> String {
    match cfg {
        LlmConfig::Openai { .. } => "openai".into(),
        LlmConfig::Anthropic { .. } => "anthropic".into(),
        LlmConfig::NineInference { .. } => "9inference".into(),
        LlmConfig::Off => "off".into(),
    }
}

/// Generic single-turn completion used by additional AI surfaces (e.g. the
/// error-group "Explain with AI" endpoint). Returns (text, provider).
pub async fn complete(
    cfg: &LlmConfig,
    system: &str,
    user_msg: &str,
    max_tokens: u32,
) -> Result<(String, String), crate::Error> {
    let (text, provider) = match cfg {
        LlmConfig::Off => {
            return Err(crate::Error::config(
                "LLM provider is 'off'; configure [llm] to use this feature",
            ))
        }
        LlmConfig::Openai {
            api_key,
            model,
            base_url,
        } => (
            call_openai(api_key, model, base_url.as_deref(), system, user_msg, max_tokens).await?,
            "openai",
        ),
        LlmConfig::Anthropic {
            api_key,
            model,
            base_url,
        } => (
            call_anthropic(api_key, model, base_url.as_deref(), system, user_msg, max_tokens)
                .await?,
            "anthropic",
        ),
        LlmConfig::NineInference { api_key, model } => (
            call_9inference(api_key, model, system, user_msg, max_tokens).await?,
            "9inference",
        ),
    };
    Ok((text, provider.to_string()))
}

async fn call_openai(
    api_key: &str,
    model: &str,
    base_url: Option<&str>,
    system: &str,
    user_msg: &str,
    max_tokens: u32,
) -> Result<String, crate::Error> {
    let body = serde_json::json!({
        "model": model,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user_msg },
        ],
        "temperature": 0.0,
        "max_tokens": max_tokens,
    });
    let url = format!(
        "{}/chat/completions",
        base_url
            .map(|b| b.trim_end_matches('/'))
            .unwrap_or("https://api.openai.com/v1")
    );
    let resp: serde_json::Value = reqwest::Client::new()
        .post(url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| crate::Error::config(format!("openai request: {e}")))?
        .json()
        .await
        .map_err(|e| crate::Error::config(format!("openai decode: {e}")))?;
    let text = resp
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| crate::Error::config(format!("openai: unexpected response {resp}")))?
        .to_string();
    Ok(text)
}

async fn call_anthropic(
    api_key: &str,
    model: &str,
    base_url: Option<&str>,
    system: &str,
    user_msg: &str,
    max_tokens: u32,
) -> Result<String, crate::Error> {
    let body = serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "system": system,
        "messages": [
            { "role": "user", "content": user_msg },
        ],
    });
    let url = format!(
        "{}/v1/messages",
        base_url
            .map(|b| b.trim_end_matches('/'))
            .unwrap_or("https://api.anthropic.com")
    );
    let resp: serde_json::Value = reqwest::Client::new()
        .post(url)
        // x-api-key for Anthropic proper; Authorization Bearer for
        // Anthropic-protocol gateways (MiniMax coding plan, LiteLLM, …).
        .header("x-api-key", api_key)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .map_err(|e| crate::Error::config(format!("anthropic request: {e}")))?
        .json()
        .await
        .map_err(|e| crate::Error::config(format!("anthropic decode: {e}")))?;
    let text = resp
        .get("content")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("text"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| crate::Error::config(format!("anthropic: unexpected response {resp}")))?
        .to_string();
    Ok(text)
}

/// Browser User-Agent string. 9inference.cloud sits behind Cloudflare, which
/// returns HTTP 1010 to reqwest's default UA. Reusing the same string across
/// all providers is harmless.
const BROWSER_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/124.0 Safari/537.36";

async fn call_9inference(
    api_key: &str,
    model: &str,
    system: &str,
    user_msg: &str,
    max_tokens: u32,
) -> Result<String, crate::Error> {
    let body = serde_json::json!({
        "model": model,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user_msg },
        ],
        "temperature": 0.0,
        "max_tokens": max_tokens,
        "stream": true,
    });

    // Build a client with a browser UA — the default reqwest UA gets blocked.
    let client = reqwest::Client::builder()
        .user_agent(BROWSER_UA)
        .build()
        .map_err(|e| crate::Error::config(format!("reqwest build: {e}")))?;

    let resp = client
        .post("https://9inference.cloud/v1/chat/completions")
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| crate::Error::config(format!("9inference request: {e}")))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| crate::Error::config(format!("9inference body: {e}")))?;
    if !status.is_success() {
        return Err(crate::Error::config(format!(
            "9inference HTTP {status}: {}",
            text.chars().take(500).collect::<String>()
        )));
    }

    // With `stream: true` the response is SSE: a series of `data: {...}` lines
    // ending with `data: [DONE]`. Each chunk's `choices[0].delta.content` is
    // an incremental piece of the message — accumulate them.
    Ok(parse_sse_stream(&text))
}

/// Parse an OpenAI-compatible SSE streaming response and concatenate the
/// `delta.content` chunks into a single string. Lines that aren't valid JSON
/// (e.g. the trailing `[DONE]` sentinel, comments, or malformed events) are
/// skipped — the model's output should still come through cleanly.
fn parse_sse_stream(body: &str) -> String {
    let mut content = String::new();
    for line in body.lines() {
        let payload = match line
            .strip_prefix("data:")
            .or_else(|| line.strip_prefix("data: "))
        {
            Some(p) => p.trim(),
            None => continue,
        };
        if payload == "[DONE]" || payload.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
            continue;
        };
        if let Some(delta) = v
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("delta"))
            .and_then(|d| d.get("content"))
            .and_then(|c| c.as_str())
        {
            content.push_str(delta);
        }
    }
    content
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_output_strips_fences() {
        let got = parse_model_output("```\nservice:api level:error\n```");
        assert_eq!(got, "service:api level:error");
    }

    #[test]
    fn parse_output_takes_last_line() {
        let got = parse_model_output("Here's the DSL:\n\nservice:api level:error");
        assert_eq!(got, "service:api level:error");
    }

    #[test]
    fn suggest_pulls_level_error() {
        let cols = ColumnWhitelist::standard(&[]);
        let got = suggest_from_keywords("show me errors from api", &cols);
        assert!(got.contains("level:error"));
    }

    #[test]
    fn suggest_pulls_known_column() {
        let cols = ColumnWhitelist::standard(&[]);
        let got = suggest_from_keywords("filter by service", &cols);
        assert!(got.contains("service:"), "got: {got}");
    }

    #[test]
    fn sse_stream_concatenates_delta_content() {
        let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"service\"}}]}\n\
data: {\"choices\":[{\"delta\":{\"content\":\":\"}}]}\n\
data: {\"choices\":[{\"delta\":{\"content\":\"api\"}}]}\n\
data: {\"choices\":[{\"delta\":{\"content\":\" level:error\"}}]}\n\
data: [DONE]\n";
        let got = parse_sse_stream(body);
        assert_eq!(got, "service:api level:error");
    }

    #[test]
    fn sse_stream_tolerates_garbage_lines() {
        let body = "\
: comment line\n\
data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\
\n\
not an SSE line at all\n\
data: malformed-json\n\
data: [DONE]\n";
        assert_eq!(parse_sse_stream(body), "x");
    }

    #[test]
    fn sse_stream_empty_input_returns_empty_string() {
        assert_eq!(parse_sse_stream(""), "");
        assert_eq!(parse_sse_stream("data: [DONE]\n"), "");
    }

    // --- dashboard builder ---

    fn ctx() -> DashboardContext {
        DashboardContext {
            columns: vec!["service".into(), "level".into(), "message".into()],
            services: vec!["telegram-bot".into(), "api".into()],
        }
    }

    #[test]
    fn dashboard_reply_parses_clarify_with_fences() {
        let raw = "```json\n{\"stage\":\"clarify\",\"questions\":[{\"id\":1,\"question\":\"Which service?\",\"choices\":[\"api\",\"web\"]},{\"question\":\"Keyword?\"}]}\n```";
        let r = parse_dashboard_reply(raw).unwrap();
        assert_eq!(r.stage, "clarify");
        assert_eq!(r.questions.len(), 2);
        assert_eq!(r.questions[0].choices.as_deref().unwrap(), ["api", "web"]);
        assert!(r.questions[1].choices.is_none());
    }

    #[test]
    fn dashboard_reply_parses_build_with_prose_around_json() {
        let raw = "Here is your dashboard:\n{\"stage\":\"build\",\"dashboard\":{\"name\":\"Ops\",\"panels\":[{\"type\":\"error-rate\",\"title\":\"Errors\",\"window\":\"24h\",\"viz\":\"chart\",\"filter\":\"service:api level:error\"}]}}\nHope that helps!";
        let r = parse_dashboard_reply(raw).unwrap();
        assert_eq!(r.stage, "build");
        assert_eq!(r.dashboard.as_ref().unwrap().panels.len(), 1);
    }

    #[test]
    fn dashboard_reply_rejects_non_json() {
        assert!(parse_dashboard_reply("no json here").is_err());
    }

    #[test]
    fn proposal_validation_drops_bad_panels_and_repairs_fields() {
        let dash = ModelDashboard {
            name: "  ".into(),
            description: String::new(),
            panels: vec![
                AiPanel {
                    panel_type: "bogus-type".into(),
                    title: "x".into(),
                    window: "1h".into(),
                    filter: String::new(),
                    viz: "chart".into(),
                    w: None,
                },
                AiPanel {
                    panel_type: "error-rate".into(),
                    title: "".into(),
                    window: "yesterday".into(),
                    filter: "service:api evil_col:1".into(),
                    viz: "hologram".into(),
                    w: None,
                },
            ],
        };
        let p = validate_proposal(dash, &[]).unwrap();
        assert_eq!(p.panels.len(), 1);
        assert_eq!(p.panels[0].window, "1h", "bad window repaired to default");
        assert_eq!(p.panels[0].viz, "chart", "bad viz repaired to default");
        assert!(p.panels[0].filter.is_empty(), "filter with unknown column dropped");
        assert_eq!(p.panels[0].title, "error-rate", "empty title backfilled");
        assert_eq!(p.name, "AI dashboard", "empty name backfilled");
    }

    #[test]
    fn proposal_validation_all_bad_panels_errors() {
        let dash = ModelDashboard {
            name: "n".into(),
            description: String::new(),
            panels: vec![AiPanel {
                panel_type: "nope".into(),
                title: "x".into(),
                window: "1h".into(),
                filter: String::new(),
                viz: "chart".into(),
                w: None,
            }],
        };
        assert!(validate_proposal(dash, &[]).is_err());
    }

    #[test]
    fn window_validator() {
        for good in ["5m", "15m", "1h", "6h", "24h", "7d", "30s", "2w"] {
            assert!(valid_window(good), "{good} should be valid");
        }
        for bad in ["", "h", "1", "1x", "1hh", "abc", "1m30s"] {
            assert!(!valid_window(bad), "{bad} should be invalid");
        }
    }

    #[test]
    fn heuristic_builder_matches_known_service() {
        let p = heuristic_dashboard("dashboard for the telegram-bot with latency focus", &ctx());
        assert_eq!(p.panels.len(), 3);
        assert!(p.panels.iter().all(|x| x.filter == "service:telegram-bot"));
        assert!(p.panels.iter().any(|x| x.panel_type == "latency"));
        assert!(p.name.contains("telegram-bot"));
    }

    #[test]
    fn heuristic_builder_no_service_match_is_unfiltered() {
        let p = heuristic_dashboard("general overview", &ctx());
        assert!(p.panels.iter().all(|x| x.filter.is_empty()));
    }

    // --- revision mode ---

    fn sample_proposal() -> AiDashboardProposal {
        AiDashboardProposal {
            name: "Ops".into(),
            description: "d".into(),
            panels: vec![AiPanel {
                panel_type: "volume".into(),
                title: "Volume".into(),
                window: "1h".into(),
                filter: "service:api".into(),
                viz: "chart".into(),
                w: Some(6),
            }],
        }
    }

    #[tokio::test]
    async fn revision_off_mode_adds_requested_panel() {
        let req = AiDashboardRequest {
            description: String::new(),
            answers: Vec::new(),
            current: Some(sample_proposal()),
            revision: Some("add a latency panel".into()),
        };
        let resp = build_dashboard(&LlmConfig::Off, &req, &ctx(), &[])
            .await
            .unwrap();
        assert_eq!(resp.stage, "build");
        let dash = resp.dashboard.unwrap();
        assert_eq!(dash.panels.len(), 2);
        assert!(dash.panels.iter().any(|p| p.panel_type == "latency"));
        // Existing panel untouched (filter reused for the new one).
        assert_eq!(dash.panels[0].filter, "service:api");
        assert_eq!(dash.panels[1].filter, "service:api");
    }

    #[tokio::test]
    async fn revision_off_mode_removes_requested_panel() {
        let req = AiDashboardRequest {
            description: String::new(),
            answers: Vec::new(),
            current: Some(sample_proposal()),
            revision: Some("remove the volume panel".into()),
        };
        let resp = build_dashboard(&LlmConfig::Off, &req, &ctx(), &[])
            .await
            .unwrap();
        // Would leave zero panels → returned unchanged instead.
        assert_eq!(resp.dashboard.unwrap().panels.len(), 1);
    }

    #[test]
    fn revision_context_embeds_current_and_request() {
        let s = format_revision_context(
            "ops dashboard",
            &sample_proposal(),
            "make it wider",
            &[],
            &ctx(),
        );
        assert!(s.contains("# Current dashboard"));
        assert!(s.contains("\"name\": \"Ops\""));
        assert!(s.contains("# Revision request"));
        assert!(s.contains("make it wider"));
    }

    #[test]
    fn proposal_validation_clamps_width() {
        let dash = ModelDashboard {
            name: "n".into(),
            description: String::new(),
            panels: vec![AiPanel {
                panel_type: "volume".into(),
                title: "v".into(),
                window: "1h".into(),
                filter: String::new(),
                viz: "chart".into(),
                w: Some(99),
            }],
        };
        let p = validate_proposal(dash, &[]).unwrap();
        assert_eq!(p.panels[0].w, Some(12));
    }
}
