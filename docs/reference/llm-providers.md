# LLM providers

central-logs calls an LLM for two things: **natural-language → filter
DSL** translation (the **Ask AI** button), and **error-group
explanations** (the **Explain with AI** button). No log data is ever
sent to the model — only the current hot-attribute schema and the
user's text.

Four modes:

| Provider | Config value | Notes |
|---|---|---|
| OpenAI | `openai` | Default model: `gpt-4o-mini`. `base_url` overrides for OpenAI-compatible relays. |
| Anthropic | `anthropic` | Default model: `claude-3-5-sonnet-20241022`. `base_url` overrides for Anthropic-protocol gateways. |
| 9inference | `9inference` | OpenAI-compatible. Default model: `nemotron-3-ultra`. Behind Cloudflare — sends a browser User-Agent. |
| Off (default) | `off` | Local keyword-extract fallback. `level:error`, `service:foo`, etc. The endpoint is still useful with zero configuration. |

## Setting up via TOML

```toml
[llm]
provider = "anthropic"
api_key  = "sk-ant-..."
model    = "claude-3-5-sonnet-20241022"
# optional gateway override (Anthropic-protocol)
base_url = "https://api.minimax.io/anthropic"
```

## Setting up via env (no TOML needed)

When `[llm]` is unset, four flat env vars configure the provider —
this is what the systemd instances use:

```bash
CENTRAL_LOGS_LLM_PROVIDER=anthropic       # anthropic (default) | openai | 9inference
CENTRAL_LOGS_LLM_API_KEY=sk-ant-...       # required to enable LLM features
CENTRAL_LOGS_LLM_MODEL=claude-3-5-sonnet-20241022
CENTRAL_LOGS_LLM_BASE_URL=https://api.minimax.io/anthropic   # optional gateway
```

TOML `[llm]` takes precedence when both are set.

## Provider-specific notes

### OpenAI

- Protocol: OpenAI Chat Completions (`/v1/chat/completions`).
- Default `base_url`: `https://api.openai.com/v1`.
- Sends `Authorization: Bearer <api_key>`.
- Set `base_url` for OpenAI-compatible relays (Together, Groq,
  LM Studio, etc.).

### Anthropic

- Protocol: Anthropic Messages API (`/v1/messages`).
- Default `base_url`: `https://api.anthropic.com`.
- Sends both `x-api-key: <api_key>` and
  `Authorization: Bearer <api_key>` (some gateways require the latter).
- Use `base_url` to point at Anthropic-protocol gateways — e.g. MiniMax
  coding plan: `base_url = "https://api.minimax.io/anthropic"`,
  `model = "MiniMax-M3"`.

### 9inference

- Protocol: OpenAI Chat Completions.
- Always uses `stream: true` and parses SSE chunks.
- Behind Cloudflare — the client sends a browser User-Agent so the
  response isn't blocked.

### Off

`provider = "off"` (the default) — `/api/ai/query` falls back to a
local keyword-extract heuristic. The endpoint is still useful: a
user-typed "errors" maps to `level:error`, "for api" maps to
`service:api`, etc. The `provider` field in the response is `off` and
the `raw` field is the extracted terms.

## System prompts (what the model sees)

For Ask AI:

"You are a filter-DSL compiler for central-logs. The user describes
what they want in natural language; respond with ONLY a single
filter-DSL expression, no markdown, no commentary. Whitelisted keys:
[hot-attributes + built-ins]. If the request is impossible with
the whitelist, respond with the single token INVALID."

For Error-group explain:

"You are an SRE assistant. Summarize the following exception for an
on-call operator, the likely cause, and 2-3 concrete next steps."

The model never sees the log data itself.

## Safety rails

- **Output is always re-parsed through `query::parse_filter`** before
  it reaches SQL. An `evil_col:1` or any other DSL-incompatible output
  is rejected with `filter_error`, never reaching DuckDB.
- **Audit event** is emitted for every call before the model is
  invoked (`service:central-logs action:ai.query`).
- **No log data leaves the box.** Only the schema and the user's
  query text go out.

## Cost

Both endpoints make one LLM call per request. The natural-language
endpoint is bounded by user interaction in the SPA. The error-group
explain is on-demand from a button click. Neither is on a hot path.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `Ask AI` returns a configuration hint | `[llm]` is `off` or the API key is empty | Set `CENTRAL_LOGS_LLM_API_KEY` (or `[llm].api_key`) |
| `filter_error` in the response | Model returned an invalid filter | Retry; the schema hint in the system prompt is supposed to prevent this |
| `429` / `5xx` from the LLM provider | Rate-limit or transient | Server logs the underlying error; the UI shows a "service unavailable" hint |
| Model returns SQL or commentary | Provider leaks prompt | Switch providers; the re-parse step still catches bad filter output |

## Next

- [AI features](../ai.md) — overview of the two surfaces
- [Configuration](../CONFIGURATION.md) — full config table
- [For AI agents](../llms.md) — recommended agent usage patterns