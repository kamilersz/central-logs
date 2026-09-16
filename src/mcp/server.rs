//! MCP server task launchers.

use std::sync::Arc;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::store::Store;
use crate::wal::meta::WalMeta;

/// Run MCP server over stdio (for local Claude Desktop / Code integration).
pub async fn spawn_mcp_stdio(
    store: Store,
    meta: Arc<WalMeta>,
    enable_alert_tool: bool,
    shutdown: CancellationToken,
) -> Result<()> {
    super::tools::run_stdio(store, meta, enable_alert_tool, shutdown).await
}

/// Run MCP server over SSE/HTTP (for remote agents). Not implemented — kept
/// as a stub only for defense-in-depth. `Config::validate` rejects
/// `mcp_mode = sse` before startup, so reaching this function at all
/// indicates that guard was bypassed (e.g. a config change after `load()`).
pub async fn spawn_mcp_sse(
    _bind: String,
    _api_key: String,
    _store: Store,
    _meta: Arc<WalMeta>,
    _enable_alert_tool: bool,
    _shutdown: CancellationToken,
) -> Result<()> {
    tracing::error!(
        "MCP SSE transport is not implemented; Config::validate should have rejected \
         mcp_mode=sse before this point. No MCP server is running."
    );
    Ok(())
}
