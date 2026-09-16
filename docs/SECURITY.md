# Security

The HTTP surface (insert, query, dashboards, alert approval, AI query, the SPA
shell itself) is **behind an authentication wall** (OWASP A01 / A07).

## How auth works

API keys are **CRUD-able**, stored as **SHA-256 hashes** in the `api_keys`
DuckDB table — raw values are returned **exactly once** at creation. Each key
carries a comma-separated scope list:

| Scope | Grants |
|---|---|
| `insert` | `POST /v1/logs` |
| `read` | every `GET /api/*` and `/metrics` |
| `write` | `read` + dashboard create/edit/delete |
| `admin` | everything + `/v1/api-keys` CRUD + alert approve/reject |

Three credential forms are accepted on every request, in order:

1. **Cookie** — `cl_session=<sid>`, set by `POST /api/auth/login`. Used by the
   SPA. `HttpOnly`, `SameSite=Strict`, dropped when the server restarts.
2. **Bearer header** — `Authorization: Bearer clk_...`. Used by API clients.
3. **`X-API-Key` header** — same value as Bearer, for client convenience.

Comparison is constant-time via `subtle::ConstantTimeEq` (legacy static key)
or SHA-256 hash lookup (CRUD keys) — neither leaks per-byte timing.

## First-run bootstrap

If `api_keys` is empty on startup and no `--http-api-key` is configured, the
server mints a one-time admin token and prints it to stderr:

```
========================================
central-logs bootstrap admin token (shown ONCE):
clk_rnZ-t_0PKaurwGtAnSkouue4FZxUgiGzW1q8lWiShh0
Log in at /login with this token.
========================================
```

Browse to `http://localhost:8080/login`, paste it, and you're in. From the
**API Keys** page (admin-scoped only) you can issue scoped per-source keys,
then revoke the bootstrap token.

If you set `CENTRAL_LOGS_HTTP_API_KEY` (legacy static-key mode below), it
becomes an admin token accepted by every endpoint — no throwaway bootstrap
token is minted, so the dashboard is reachable immediately and you don't lose
access if `api_keys` ever gets wiped.

## Unauthenticated browser access

Navigating to `/` without a session cookie **redirects to `/login`**
(server-rendered HTML form). API clients without credentials get a JSON
`401`. Public paths are limited to `/login`, `/api/auth/login`, and
`/health` — everything else (including the SPA shell) is gated.

## Legacy single-static-key mode

For simple deployments that don't want CRUD management, set
`--http-api-key` / `CENTRAL_LOGS_HTTP_API_KEY`. The configured string is
accepted as an implicit **admin** credential. This is honoured alongside
CRUD keys, so you can mix both.

```bash
CENTRAL_LOGS_HTTP_API_KEY=$(openssl rand -hex 32) ./target/release/central-logs
```

## Other OWASP-aligned defaults

- **A03 Injection** — filter DSL compiles to parameterized SQL with a strict
  column whitelist; values are bound, never interpolated (see `src/query.rs`,
  including the `sql_injection_via_value_is_safe` test).
- **A04 Insecure Design** — `create_alert_rule` always lands in
  `pending_approval`; a human must approve via the dashboard before any
  AI-created rule can fire (architecture §7).
- **A05 Security Misconfiguration** — request bodies capped
  (`http_max_body_bytes`, default 16 MiB) to prevent unbounded-memory DoS on
  `/v1/logs`. The server logs a loud warning at startup if HTTP is bound to
  a non-loopback interface with auth unset.
- **A07 Auth Failures** — minimum-privilege scopes; tokens are hashed, never
  stored plaintext; revocation is instant (in-memory cache drop + soft
  `revoked_at` flag for audit).

## Self-audit (dogfood)

The server emits its own audit events (login, logout, dashboard create /
update / delete, alert approve / reject, api-key create / revoke, AI query,
log search) into the same `logs` table as user data. They carry
`service = "central-logs"` and a structured `attributes` JSON with `action`,
`outcome`, `actor_key_id`, `actor_key_name`, `source_ip`, etc.

Query them with the existing DSL:

```
service:central-logs auth.login.failed        # all failed logins
service:central-logs action:apikey.create     # token issuances
service:central-logs action:dashboard.delete  # destructive ops
```

For tighter filtering on `action`, promote it to a typed column at startup:

```
--hot-attribute 'action:varchar'
--hot-attribute 'outcome:varchar'
--hot-attribute 'actor_key_id:bigint'
```

Then `action:auth.login.failed outcome:failure` becomes a zonemap-pruned
column scan instead of a JSON parse.

## Client IP resolution

Audit events record the calling client's IP, resolved from the proxy chain
in this precedence (first non-empty match wins):

1. **`CF-Connecting-IP`** — set by Cloudflare's edge
2. **`X-Real-IP`** — set by the closest reverse proxy (nginx, traefik)
3. **`Forwarded`** — RFC 7239 (`for=192.0.2.60;proto=https`)
4. **`X-Forwarded-For`** — leftmost entry of the comma-separated chain
5. **TCP socket address** — the literal peer (your reverse proxy's address
   when one is in front; an attacker can't fake this)

**Spoofing caveat**: `X-Forwarded-For` is append-only and the leftmost entry
is client-controlled unless your front-most proxy overwrites it. If you're
not behind Cloudflare or a trusted proxy that strips inbound XFF,
`CF-Connecting-IP` and `X-Real-IP` are safer signals. The TCP socket is the
only value an attacker cannot fake.
