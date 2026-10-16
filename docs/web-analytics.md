# Website analytics

First-party, cookieless web analytics riding the same
**insert → WAL → ingest → DuckDB → rollup** pipeline as your logs.
Pageviews, custom events, outbound/download clicks, engagement, Web Vitals
and JS errors land in a separate `web_events` table and can be joined to
backend logs via `trace_id`.

Beacons are sent **same-origin**, so ad-blockers rarely interfere and no CORS
preflight is needed. There are no cookies and no stored IPs: each visitor is
a SHA-256 hash of (daily-rotating salt, project, IP, user-agent).

## Quick start

No server change is needed per project. Pick an id, embed the snippet, point
a same-origin proxy at the collector — the **first beacon carrying a new id
creates the project automatically** (trust on first use; its origin becomes
the first allowed origin).

**1. Name your project.** One id per site/app, e.g. `shop`. Letters, digits,
`._-`, max 64 chars. The id travels in every beacon (`{"s": "shop", …}`).

**2. Embed one line in every page:**

```html
<script defer data-site="shop" data-endpoint="/_/e" src="/_/s.js"></script>
```

For single-page apps nothing extra is needed — `pushState`/`replaceState`/
`popstate` are wrapped and each URL change sends a pageview. Custom events:

```js
window.cl.track("signup", { plan: "pro" });
```

**3. Proxy two neutral paths at your domain** (ad-blockers don't match `/_/`):

```caddy
# Caddy
your-site.example {
    handle_path /_/s.js { reverse_proxy 127.0.0.1:8080/v1/web/t.js }
    handle_path /_/e    { reverse_proxy 127.0.0.1:8080/v1/web/collect }
    handle { reverse_proxy 127.0.0.1:3000 }
}
```

```nginx
# nginx
location = /_/s.js {
    proxy_set_header X-CL-User "";
    proxy_pass http://127.0.0.1:8080/v1/web/t.js;
}
location = /_/e {
    proxy_set_header X-Forwarded-For $remote_addr;
    proxy_set_header X-CL-User "";
    proxy_pass http://127.0.0.1:8080/v1/web/collect;
}
```

`/_/s.js` serves the ~2 KB tracker, `/_/e` receives beacons. Only these two
paths are public; everything under `/api/web/*` stays authenticated.

**4. Verify:**

```bash
curl -s -o /dev/null -w '%{http_code}\n' \
  -H 'Origin: https://your-site.example' -H 'Content-Type: text/plain' \
  --data '{"s":"shop","ev":[{"e":"pageview","u":"/test","dt":0}]}' \
  http://127.0.0.1:8080/v1/web/collect
# → 202 (first beacon for a new id also creates the project)
```

Events appear on the **Web** page within seconds. The in-app
**Connect → Web analytics** tab renders these same snippets with your host
filled in.

## Projects

Each project is an isolated scope: own dashboard slice, own allowed origins.

| Way to create | Effect |
|---|---|
| First beacon (default) | Auto-created, beacon's origin allowlisted. No restart. |
| Web page → Projects (admin) | Pre-creates/reserves an id with chosen origins. No restart. |
| `POST /api/web/sites` (admin) | Same as above, scriptable. `409` on duplicate, `400` on invalid. |
| `[[web.site]]` in TOML | Seed for infra-managed ids. Needs a restart; listed as `source: "config"` and cannot be deleted via the API. |

`DELETE /api/web/sites/{id}` (admin) removes a runtime project: history is
kept, new beacons for it are rejected. First write wins on an id, so
pre-creating reserves it against squatting.

## What is collected

| Event | Source |
|---|---|
| `pageview` | Load + SPA navigations (URL changes only; deduped per URL) |
| `custom` | `window.cl.track(name, props)` |
| `outbound` / `download` | Clicks on external links / file-extension links |
| `engagement` | Active time (paused when tab hidden) + max scroll depth |
| `vital` | LCP, CLS, INP via `PerformanceObserver` |
| `error` | `window.onerror`, unhandled rejections (message only, truncated) |

The tracker honors `DNT: 1` and `Sec-GPC: 1` by disabling itself, and stays
idle during prerendering. The dashboard itself dogfoods this: every central-logs
UI additionally sends `ui.<page>.<action>` custom events (filter runs, alert
approvals, key lifecycle…), so the `central-logs` project shows real usage.

## Privacy model

- No cookies, no `localStorage`, no client-side identifiers.
- Raw client IPs are **never stored** — they only feed the daily visitor hash.
  Hashes change every UTC day; salts are deleted after 2 days, making past
  hashes unreconstructable.
- Consequence: unique visitors are exact **per day**; multi-day ranges **sum**
  daily uniques (shown as a note wherever uniques appear).
- `DNT`/`Sec-GPC` beacons are answered `202` and discarded (and never create
  projects). Authenticated users appear only as keyed per-site hashes.

## Viewing data

- **Web** page: KPI row (visitors, sessions, pageviews, bounce rate, avg
  duration — each with change vs previous period and a sparkline), trend with
  previous-period overlay, tabbed Top/Entry/Exit pages, **Traffic Sources**,
  **Geography**, **Devices**, **Web Vitals** (thresholds + per-page/device),
  **Donation funnel** (configurable goal), **Session explorer**, System, Exit
  Links & Downloads, UTM, Custom Tags, JS errors; **Web Realtime**: last
  5 minutes. The toolbar (sticky) holds project, range, compare toggle, page
  filter chip, exclude-my-visits toggle, timezone, and CSV export.
- **Ranges**: every endpoint accepts `window=1h|24h|7d…` (or explicit
  `from`/`to`); the UI's compare mode simply queries the previous window.
- **Definitions**: a session starts on a pageview and closes after 30 idle
  minutes; beacons without a pageview attach to the open session or are
  dropped. Bounce = exactly one pageview and no deliberate action (outbound
  click / download / custom tag) — passive vitals and heartbeats don't count.
  Engaged time never exceeds the session's wall-clock length.
- **Filter chips**: clicking any value (page, country, region, city, device,
  browser, OS, source) filters the whole view; the chips live in the URL, so
  a filtered view reloads as-is. Server-side keys: `path`, `country`,
  `region`, `city`, `device`, `browser`, `os`, `source`, `medium`.
- **Projects & goals**: managed under Manage → Web projects (origins,
  excluded IPs, and the donation-funnel goal — steps may be paths,
  `event:name` custom events or `outbound:host` exit links).
- **API** (read scope): `GET /api/web/stats|breakdown|realtime|vitals|errors|
  sites|sources|pages|links|custom|funnel|sessions|sessions/{id}|export|
  summary` and `GET /api/web/trace/{trace_id}` (browser events + backend
  logs side by side). `?include_internal=true` re-includes operator visits
  (excluded by default); `?path=/donasi` scopes traffic views to one page.
- **MCP**: `web_stats`, `web_top` return aggregates only — never raw rows.
- **Alerts**: threshold rules accept `web_pageviews:<site>` (e.g.
  `web_pageviews:ajakan`), so traffic-drop alerts reuse the normal Alerts
  tab — pick "web_pageviews (per site)" in the rule editor.

## Geography (country / region / city)

Every event carries `country`, `region` (state/province ISO code) and `city`:

- **Country works with zero setup behind Cloudflare**: the collector reads
  the edge-stamped `CF-IPCountry` header into each row.
- **Region/city need a GeoLite2-City database**: no key required —
  `./scripts/fetch-geolite.sh` pulls the auto-updated
  [wp-statistics mirror](https://github.com/wp-statistics/GeoLite2-City)
  into `./geoip/` (git-ignored; `--maxmind` flag + free license key uses the
  official download instead). Then uncomment `geoip_db_path` in the TOML and
  restart. Requires a geoip-enabled build — `scripts/deploy.sh` builds with
  `--features geoip`; without the file the server logs one warning at boot
  and continues country-only. A Country-only DB also works (country field
  only).

## Correlating with backend logs

Have your backend render `<meta name="traceparent"
content="00-<trace_id>-<span_id>-01">`. The tracker picks it up, every
`web_events` row carries `trace_id`, and `web_events JOIN logs USING
(trace_id)` (or the trace endpoint) shows the click and its server-side
handling together.

## Operator reference

One-time server setup — the collector itself must be enabled (restart):

```toml
[web]
enabled = true
trusted_proxies = ["127.0.0.1/32", "::1/128"]
max_beacon_bytes = 16384
max_events_per_beacon = 20
rate_limit_per_ip_per_min = 120
shed_when_wal_fill_pct = 80   # shed analytics before logs feel WAL pressure
honor_dnt = true
raw_retention_days = 90       # web_events TTL; sessions + rollups kept longer
session_gap_minutes = 30
auto_create_projects = true   # TOFU (default on); set false for allowlist-only
auto_create_max_projects = 100
```

`allowed_origins = ["*"]` (config, API, or UI) accepts beacons from any
present origin — useful behind reverse proxies with churning hostnames. A
missing `Origin`/`Referer` is still rejected, and rate limits still apply;
prefer explicit origins for projects facing untrusted browsers, since anyone
can then submit under the id.

| Endpoint | Auth | Purpose |
|---|---|---|
| `GET /v1/web/t.js` | public | tracker script (serve via neutral path) |
| `POST /v1/web/collect` | public | beacon collector (202 empty on bad input — never confirms/denies ids) || `GET /api/web/*` | read | stats, breakdowns, realtime, vitals, errors, sites, trace |
| `POST /api/web/sites`, `DELETE /api/web/sites/{id}` | admin | project CRUD |

Rejections are counted per reason in `/api/counters` → `web_rejected_total`
(`dnt`, `rate_limited`, `shed`, `unknown_site`, `origin_denied`, …).

### Behind Cloudflare

The collector honors `CF-Connecting-IP` ahead of `X-Forwarded-For`.
Cloudflare stamps it with the real eyeball IP at the edge and your proxy
forwards it untouched — while an XFF rewritten to `$remote_addr` would carry
a *rotating Cloudflare edge IP* per connection, fragmenting every visitor
into one session per beacon. Don't strip the header; no config change is
needed.

### Troubleshooting

- **202 but nothing stored**: the 202 is intentionally opaque. Check
  `web_rejected_total` — `origin_denied` means the Origin isn't allowlisted
  for that project; `unknown_site` with auto-create off means the id doesn't
  exist yet.
- **Sessions missing**: the sessionizer only processes settled events
  (`ts < now − 30s`) and skips bots; wait a minute and re-check.
- **Multi-day uniques look high**: expected — daily uniques are summed, never
  deduplicated across days (see Privacy model).
