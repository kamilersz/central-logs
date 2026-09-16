//! MCP server via `rmcp` (architecture §7). Exposes query_logs, get_dashboard_summary,
//! detect_anomalies, forecast, and (optionally) create_alert_rule.

pub mod server;
pub mod tools;

pub use server::{spawn_mcp_stdio, spawn_mcp_sse};
