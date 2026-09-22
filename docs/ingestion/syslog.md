# Syslog ingest

The simplest path for infrastructure devices, OS daemons, and anything
that already speaks syslog. UDP and TCP listeners on `:5140`, RFC 3164
/ RFC 5424 parsing via `syslog_loose`.

## Endpoints

| Transport | Bind | Default |
|---|---|---|
| UDP | `0.0.0.0:5140` | enabled |
| TCP | `0.0.0.0:5140` | enabled |

Disable individually:

```bash
--no-syslog-udp
--no-syslog-tcp
```

Or via TOML:

```toml
syslog_udp_enabled = false
```

## What lands in the `logs` table

Parsed syslog fields map onto the same columns as HTTP/JSON ingest:

| Syslog field | `logs` column |
|---|---|
| HOSTNAME | `source_host` |
| APP-NAME | `service` (fallback; if APP-NAME is missing, `"syslog"` is used) |
| MSGID / structured data | `message` |
| PROCID | `attributes.procid` |
| severity (RFC 5424) | `level` (`err` → `error`, etc.) |
| TIMESTAMP | `ts` (falls back to receive time) |
| (transport) | `protocol` = `syslog_udp` or `syslog_tcp` |

Unparseable lines are still ingested as a single `message` with the
raw payload preserved in `attributes.raw`. Better to keep the line
than to drop it.

## Pointing rsyslog / syslog-ng

`rsyslog` example forwarding all messages:

```
# /etc/rsyslog.d/central-logs.conf
*.* @central-logs.example.com:5140    # UDP
# or: *.* @@central-logs.example.com:5140   # TCP
```

`syslog-ng` example:

```
destination d_central_logs {
    udp("central-logs.example.com" port(5140));
};
log { source(s_src); destination(d_central_logs); };
```

## Caveats

- **No auth on the syslog transport.** Restrict via network ACL
  (firewall, security group, or bind to a private interface).
- **UDP has no backpressure concept.** When the insert layer is
  saturated, packets are dropped and counted (see
  `/api/pipeline` and `/metrics`). Prefer TCP when you can.
- **Large volumes:** syslog was designed for low-rate event streams.
  For high-volume application logs use HTTP/NDJSON or OTLP/HTTP.

## Next

- [Sentry SDK →](sentry.md)
- [HTTP / NDJSON →](http.md)