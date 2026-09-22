# AI features

central-logs ships two LLM-powered surfaces:

1. **Natural-language → filter DSL** — humans type free text in the
   SPA's **Ask AI** button; the server calls an LLM provider and
   re-validates the result through the same filter-DSL parser every
   other query path uses.
2. **MCP tools for AI agents** — `query_logs`,
   `get_dashboard_summary`, `detect_anomalies`, `forecast`,
   `create_alert_rule`. See [MCP server](mcp.md) for the tool
   reference; this page is the human-side overview.

## Ask AI (natural-language → DSL)

The SPA's **Logs Explorer** has an **Ask AI** button that posts to
`POST /api/ai/query`:

```bash
curl -sf -X POST http://localhost:8080/api/ai/query \
  -H "Authorization: Bearer $ADMIN_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"query":"show me errors for user 42 in the last hour"}'
```

Response:

```json
{
  "filter": "service:api level:error user_id:42",
  "provider": "anthropic",
  "raw": "...",
  "filter_error": null
}
```

The `filter` field is **always** validated through `query::parse_filter`
before it's returned. A misbehaving or adversarial model output that
isn't valid filter-DSL syntax, or that references an unknown column, is
rejected by the same whitelist-based parser every other query path
goes through — it can never become raw SQL.

**The LLM never sees log data**, only the current hot-attribute schema
(the names of typed columns you've promoted) and the user's query
text.

## Error-group explain

Each error group has an **Explain with AI** button:

```bash
curl -sf -X POST http://localhost:8080/api/error-groups/<fp>/explain \
  -H "Authorization: Bearer $ADMIN_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"context": "deployment 1.42 just shipped"}'
```

Response:

```json
{ "provider": "anthropic", "explanation": "..." }
```

With `[llm]` off, the endpoint returns a configuration hint instead
of a model output, so the UI doesn't break — it just doesn't get an
explanation.

## Supported providers

| Provider | `[llm].provider` / `CENTRAL_LOGS_LLM_PROVIDER` | Notes |
|---|---|---|
| OpenAI | `openai` | `gpt-4o-mini` is a good default. `base_url` overrides for OpenAI-compatible relays. |
| Anthropic | `anthropic` | `claude-3-5-sonnet-20241022`. `base_url` overrides for Anthropic-protocol gateways (e.g. MiniMax coding plan). Sends both `x-api-key` and `Authorization: Bearer`. |
| 9inference | `9inference` | OpenAI-compatible. `nemotron-3-ultra`. Behind Cloudflare — sends a browser User-Agent. Always uses `stream: true` and parses SSE chunks. |
| Off (default) | `off` | Local keyword-extract fallback (`level:error`, `service:foo`, etc.) — the endpoint is still useful with zero configuration. |

Set up via TOML ([llm] block) or flat env vars (`.env`-friendly):

```bash
CENTRAL_LOGS_LLM_PROVIDER=anthropic
CENTRAL_LOGS_LLM_API_KEY=sk-ant-...
CENTRAL_LOGS_LLM_MODEL=claude-3-5-sonnet-20241022
# optional:
CENTRAL_LOGS_LLM_BASE_URL=https://api.minimax.io/anthropic
```

TOML `[llm]` wins when both are set. **Never hardcode keys** — keep
them in `.env` (gitignored) or your secrets manager. See
[Configuration → LLM providers](CONFIGURATION.md) for the full
table.

## Audit

Every call is audited (`ai.query` event, actor + source IP + the
query text) before the LLM call fires. "Who asked what" is exactly
the audit trail's job. Query the audit log with the same DSL:

```
service:central-logs action:ai.query
```

Promote `action` / `outcome` to typed columns at startup to make
this zonemap-pruned:

```bash
--hot-attribute 'action:varchar'
--hot-attribute 'outcome:varchar'
```

## Safety

- **LLM output never reaches DuckDB raw.** It is re-parsed as filter
  DSL, which compiles to bound-parameter SQL with a column whitelist.
- **Filter parser rejects unknown columns and wrong-type operators.**
  An agent that emits `evil_col:1` gets `filter_error` back, not a SQL
  injection.
- **Self-audit is opt-out impossible.** Every call to `/api/ai/query`
  is recorded before the model is invoked.

## Next

- [MCP server →](mcp.md)
- [Configuration → LLM providers](CONFIGURATION.md)
- [HTTP API → AI auto-filter](API.md#ai-auto-filter)