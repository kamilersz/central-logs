# Docker Engine & Kubernetes ingest

Containers and orchestrators have their own log plumbing. central-logs
speaks their native push protocols **and** ships optional built-in
collectors that pull log streams over the Docker Engine API and the
Kubernetes API — no third-party agent required in either mode.

| Path | Best for | Tag |
|---|---|---|
| [GELF](#gelf-docker-gelf-log-driver) | Docker `--log-driver=gelf`, any GELF shipper | `protocol:gelf` |
| [Fluentd forward](#fluentd-forward-docker-fluentd-log-driver) | Docker `--log-driver=fluentd`, fluentd/fluent-bit forward outputs | `protocol:fluentd` |
| [Splunk HEC](#splunk-hec-docker-splunk-log-driver) | Docker `--log-driver=splunk`, any HEC client | `protocol:splunk_hec` |
| [Kubernetes audit webhook](#kubernetes-audit-webhook) | `kube-apiserver --audit-webhook-config-file` | `protocol:k8s_audit` |
| [Docker collector](#docker-engine-api-collector-pull) | Hosts where changing each container's log driver is impractical | `protocol:docker_api` |
| [Kubernetes collector](#kubernetes-api-collector-pull) | In-cluster pod logs without an agent | `protocol:k8s_api` |

All six feed the same durable WAL → parse → DuckDB pipeline as
[HTTP ingest](http.md): rows are queryable with the normal
[filter DSL](../query.md) within a second of arrival.

---

## GELF (Docker `gelf` log driver)

Enable the listener (UDP, TCP, or both):

```toml
[ingest.gelf]
enabled = true
udp_bind = "0.0.0.0:12201"   # "" disables UDP
tcp_bind = "0.0.0.0:12201"   # "" disables TCP
chunk_timeout_secs = 5
```

Point Docker at it:

```bash
docker run --log-driver=gelf \
  --log-opt gelf-address=udp://logs.example.com:12201 \
  --log-opt tag=web-1 \
  nginx
```

What central-logs handles:

- **UDP payloads** gzip- or zlib-compressed (the driver's default), or plain.
- **GELF chunking** (`\x1e\x0f` + 8-byte id + seq/total) for messages larger
  than one datagram — up to 128 chunks, reassembled out of order, expired
  after `chunk_timeout_secs`.
- **TCP framing**: JSON + NUL per message.
- **Field mapping**: `short_message` → `message`, `level` (syslog 0-7) →
  level, `timestamp` → `ts`, `host` → `source_host`, `_container_name` →
  `service`. All `_`-extras (`_container_id`, `_image_name`, `_tag`,
  label/env extras, …) land in `attributes`.

Docker maps `stderr` lines to GELF level 3 (error); everything else is 6
(info) — central-logs preserves that.

## Fluentd forward (Docker `fluentd` log driver)

```toml
[ingest.fluentd]
enabled = true
tcp_bind = "0.0.0.0:24224"
ack = true   # reply {"ack": id} when the driver requests acknowledgement
```

```bash
docker run --log-driver=fluentd \
  --log-opt fluentd-address=tcp://logs.example.com:24224 \
  --log-opt fluentd-request-ack=true \
  nginx
```

Details:

- MessagePack **message mode** `[tag, time, record, option]` (forward /
  packed-forward arrays are also decoded).
- `time` is unix seconds or Fluentd's EventTime ext (type 0) when the driver
  uses `fluentd-sub-second-precision=true`.
- Docker's record: `container_id`, `container_name` (leading `/` trimmed),
  `source` (`stdout`→info, `stderr`→error), `log` → `message`; label/env
  extras and `partial_*` fields land in `attributes`; `service` = container
  name (fallback: tag).
- With `fluentd-request-ack=true` the driver retries until it sees
  `{"ack":"<chunk id>"}` — keep `ack = true` for lossless delivery.

## Splunk HEC (Docker `splunk` log driver)

```toml
[ingest.splunk_hec]
enabled = true
token = ""   # optional static HEC token; API keys always work too
```

The driver must be given *some* token (`--log-opt splunk-token=…`); use
either an insert-scoped API key or the static token above. central-logs
accepts `Authorization: Splunk <token>` on these endpoints:

```
POST /services/collector/event/1.0     # driver's target
POST /services/collector/event
POST /services/collector/raw
OPTIONS /services/collector/event/1.0  # driver connection verification
GET  /services/collector/health/1.0
```

```bash
docker run --log-driver=splunk \
  --log-opt splunk-url=https://logs.example.com \
  --log-opt splunk-token=clk_... \
  --log-opt splunk-sourcetype=my-app \
  nginx
```

Details: batches of concatenated event JSON (the driver flushes up to 1000
events per request), `time` as a float-epoch *string*, gzip request bodies
(handled by the standard decompression layer). Responses use HEC codes:
`{"text":"Success","code":0}` on 200, code 9/503 when the WAL cap pauses
ingest, code 6/400 on malformed JSON. Mapping: `event.line` → `message`,
`event.source == "stderr"` → error level, `sourcetype` → `service`,
`host` → `source_host`, `event.attrs` + HEC `fields` → `attributes`.

## Kubernetes audit webhook

```toml
[ingest.k8s_audit]
enabled = true
omit_stages = ["RequestReceived"]   # dedup multi-stage events (optional)
```

Point the API server at central-logs (`/etc/kubernetes/audit-webhook.yaml`):

```yaml
apiVersion: v1
kind: Config
clusters:
- name: central-logs
  cluster:
    server: https://logs.example.com:8443/ingest/kubernetes/audit
    # or insecure-skip-tls-verify in dev
contexts:
- name: default
  context: {cluster: central-logs, user: default}
current-context: default
users:
- name: default
  user:
    token: <insert-scoped API key>
```

```bash
kube-apiserver \
  --audit-webhook-config-file=/etc/kubernetes/audit-webhook.yaml \
  --audit-policy-file=/etc/kubernetes/audit-policy.yaml
```

central-logs accepts `audit.k8s.io/v1` `EventList` batches (and single
`Event`s). Per event:

- `message` = `create pods default/nginx → 201` style rendering;
- severity: Panic stage → `fatal`, 5xx → `error`, 4xx → `warn`, else `info`;
- `auditID` → `trace_id`, last `sourceIPs` entry → `source_host`,
  `stageTimestamp` → `ts`;
- the full event (user, objectRef, responseStatus, requestURI, userAgent,
  annotations, audit level, and — at `RequestResponse` policy level —
  request/response objects) lands in `attributes`; use
  `--drop-attribute requestObject` to strip big bodies.

The endpoint answers with a k8s `Status` object and only 200s after the WAL
fsync — the apiserver's exponential-backoff retry semantics stay correct.

## Docker Engine API collector (pull)

For hosts where you can't (or don't want to) set `--log-driver` per
container:

```toml
[collector.docker]
enabled = true
socket = "unix:///var/run/docker.sock"   # or tcp://host:2375
api_version = "v1.41"
include = []                # container-name globs, e.g. ["web-*"]; [] = all
exclude = []                # wins over include
refresh_secs = 30           # re-list containers, attach to new ones
tail_lines = 0              # history on first attach (0 = new lines only)
```

The collector lists containers each cycle, opens a
`GET /containers/{id}/logs?follow=true&timestamps=true` stream per container
(demultiplexing the 8-byte stdcopy frames, TTY streams handled too), and
releases the stream when a container stops — a restarted container is
re-attached on the next cycle. Rows: `service` = container name,
`attributes.container_id` / `image` / `stream` (`stdout`/`stderr`), stderr
lines tagged `error`.

Requires read access to the socket (run central-logs in the `docker` group
or mount the socket read-only in a container deployment).

## Kubernetes API collector (pull)

In-cluster pod-log collection without a DaemonSet agent:

```toml
[collector.kubernetes]
enabled = true
api_url = "https://kubernetes.default.svc"
token_file = "/var/run/secrets/kubernetes.io/serviceaccount/token"
ca_file = "/var/run/secrets/kubernetes.io/serviceaccount/ca.crt"
# token = "literal-token"      # alternative to token_file
# insecure_tls = false
namespaces = []                # [] = all namespaces
label_selector = ""            # e.g. "app=web"
refresh_secs = 30
tail_lines = 0
```

RBAC needed (ClusterRole):

```yaml
rules:
- apiGroups: [""]
  resources: ["pods", "pods/log"]
  verbs: ["get", "list", "watch"]
```

The collector lists pods each cycle (honoring `namespaces` +
`label_selector`), follows each container's log stream, and re-attaches
after pod restarts. SA tokens rotate — the token file is re-read every
cycle. Rows: `service` = pod name, `attributes.namespace` / `pod` /
`container` / `node`, `source_host` = node name, ts from the kubelet's
RFC3339Nano line prefix.

## Config reference

Full option list with defaults:
[Configuration → ingest / collector](../reference/config.md).
