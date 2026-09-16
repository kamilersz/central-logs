import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.time.Duration;
import java.time.Instant;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.TimeUnit;

/**
 * central-logs integration client — batched NDJSON inserts (JDK 11+).
 *
 * Setup (see INTEGRATION.md):
 *   1. Admin key pinned in the server's .env (CENTRAL_LOGS_HTTP_API_KEY).
 *   2. Mint an insert-only key (POST /v1/api-keys, scopes: "insert").
 *   3. Export for this app:
 *        CENTRAL_LOGS_URL      (default http://localhost:8080)
 *        CENTRAL_LOGS_API_KEY  (the raw clk_... value — required)
 *        CENTRAL_LOGS_SERVICE  (default "default-app")
 *
 * Usage:
 *   CentralLogs cl = new CentralLogs();              // config from env
 *   cl.log("info", "app started", Map.of("version", "1.2.3"));
 *   cl.log("warn", "queue lag 12s", Map.of("queue", "events", "duration_ms", 12000));
 *   cl.close();                                       // final flush on shutdown
 *
 * A daemon scheduler flushes every CENTRAL_LOGS_FLUSH_MS; addShutdownHook
 * covers normal JVM exit. On persistent failure the batch goes to stderr —
 * nothing silently vanishes — and the queue is bounded.
 */
public final class CentralLogs implements AutoCloseable {

    private static final int MAX_QUEUE =
        Integer.parseInt(System.getProperty("central.logs.max.queue",
            System.getenv().getOrDefault("CENTRAL_LOGS_MAX_QUEUE", "5000")));
    private static final int MAX_BATCH =
        Integer.parseInt(System.getProperty("central.logs.max.batch",
            System.getenv().getOrDefault("CENTRAL_LOGS_MAX_BATCH", "100")));
    private static final long FLUSH_MS =
        Long.parseLong(System.getProperty("central.logs.flush.ms",
            System.getenv().getOrDefault("CENTRAL_LOGS_FLUSH_MS", "1000")));

    private final String baseUrl;
    private final String apiKey;
    private final String service;
    private final HttpClient http;
    private final ScheduledExecutorService scheduler;
    private final Object lock = new Object();
    private final List<String> queue = new ArrayList<>();
    private int dropped = 0;
    private volatile boolean closed = false;

    public CentralLogs() {
        this(
            env("CENTRAL_LOGS_URL", "http://localhost:8080"),
            env("CENTRAL_LOGS_API_KEY", ""),
            env("CENTRAL_LOGS_SERVICE", "default-app")
        );
    }

    public CentralLogs(String baseUrl, String apiKey, String service) {
        this.baseUrl = baseUrl.replaceAll("/+$", "");
        this.apiKey = apiKey;
        this.service = service;
        this.http = HttpClient.newBuilder()
            .connectTimeout(Duration.ofSeconds(3))
            .build();
        this.scheduler = Executors.newSingleThreadScheduledExecutor(r -> {
            Thread t = new Thread(r, "central-logs");
            t.setDaemon(true);
            return t;
        });
        scheduler.scheduleWithFixedDelay(this::flushSafely, FLUSH_MS, FLUSH_MS, TimeUnit.MILLISECONDS);
        Runtime.getRuntime().addShutdownHook(new Thread(() -> close(), "central-logs-shutdown"));
    }

    /** Queue one record. Level: debug|info|warn|error|fatal. */
    public void log(String level, String msg, Map<String, Object> fields) {
        if (closed) return;
        StringBuilder sb = new StringBuilder(128);
        sb.append("{\"service\":\"").append(escape(service)).append('"');
        sb.append(",\"level\":\"").append(escape(level.toLowerCase())).append('"');
        sb.append(",\"msg\":\"").append(escape(msg)).append('"');
        sb.append(",\"ts\":\"").append(Instant.now()).append('"');
        if (fields != null) {
            for (Map.Entry<String, Object> e : fields.entrySet()) {
                sb.append(",\"").append(escape(e.getKey())).append("\":").append(jsonValue(e.getValue()));
            }
        }
        sb.append('}');

        boolean trigger = false;
        synchronized (lock) {
            if (queue.size() >= MAX_QUEUE) {
                queue.remove(0); // bounded: drop oldest
            }
            queue.add(sb.toString());
            trigger = queue.size() >= MAX_BATCH;
        }
        if (trigger) {
            flushSafely();
        }
    }

    public void log(String level, String msg) {
        log(level, msg, null);
    }

    /** Send the queued batch now (safe to call concurrently). */
    public void flush() {
        List<String> batch;
        synchronized (lock) {
            if (queue.isEmpty()) return;
            batch = new ArrayList<>(queue);
            queue.clear();
        }
        if (apiKey.isEmpty()) {
            drop(batch, "CENTRAL_LOGS_API_KEY not set");
            return;
        }
        String body = String.join("\n", batch);
        try {
            HttpRequest req = HttpRequest.newBuilder()
                .uri(URI.create(baseUrl + "/v1/logs"))
                .timeout(Duration.ofSeconds(5))
                .header("Content-Type", "application/x-ndjson")
                .header("Authorization", "Bearer " + apiKey)
                .POST(HttpRequest.BodyPublishers.ofString(body))
                .build();
            HttpResponse<String> resp = http.send(req, HttpResponse.BodyHandlers.ofString());
            if (resp.statusCode() != 200) {
                drop(batch, "HTTP " + resp.statusCode() + ": " + truncate(resp.body()));
            } else if (resp.body().contains("\"rejected\":") && !resp.body().contains("\"rejected\":0")) {
                System.err.println("[central-logs] some records rejected: " + truncate(resp.body()));
            }
        } catch (Exception e) {
            drop(batch, e.getClass().getSimpleName() + ": " + e.getMessage());
        }
    }

    /** Stop the scheduler and send the final batch. */
    @Override
    public void close() {
        if (closed) return;
        closed = true;
        scheduler.shutdown();
        try {
            scheduler.awaitTermination(3, TimeUnit.SECONDS);
        } catch (InterruptedException ignored) {
            Thread.currentThread().interrupt();
        }
        flush();
    }

    // ── internals ────────────────────────────────────────────────────────
    private void flushSafely() {
        try {
            flush();
        } catch (RuntimeException e) {
            System.err.println("[central-logs] periodic flush error: " + e.getMessage());
        }
    }

    private void drop(List<String> batch, String reason) {
        dropped += batch.size();
        System.err.println("[central-logs] flush failed (" + dropped + " dropped total): " + reason);
        for (String line : batch) {
            System.err.println(line);
        }
    }

    /** Numbers/booleans emit as-is; everything else becomes a JSON string. */
    private static String jsonValue(Object v) {
        if (v == null) return "null";
        if (v instanceof Number || v instanceof Boolean) return v.toString();
        return "\"" + escape(String.valueOf(v)) + "\"";
    }

    private static String escape(String s) {
        StringBuilder out = new StringBuilder(s.length() + 8);
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            switch (c) {
                case '"'  -> out.append("\\\"");
                case '\\' -> out.append("\\\\");
                case '\n' -> out.append("\\n");
                case '\r' -> out.append("\\r");
                case '\t' -> out.append("\\t");
                default   -> {
                    if (c < 0x20) out.append(String.format("\\u%04x", (int) c));
                    else out.append(c);
                }
            }
        }
        return out.toString();
    }

    private static String truncate(String s) {
        return s == null ? "" : s.substring(0, Math.min(s.length(), 200));
    }

    private static String env(String key, String def) {
        String v = System.getenv(key);
        return (v == null || v.isEmpty()) ? def : v;
    }

    // ── smoke test ───────────────────────────────────────────────────────
    //   CENTRAL_LOGS_API_KEY=clk_... java CentralLogs.java
    public static void main(String[] args) {
        CentralLogs cl = new CentralLogs();
        cl.log("info", "central-logs java client smoke test", Map.of("env", "test"));
        cl.close();
        System.out.println("flushed; verify with: curl '.../api/logs?filter=service:default-app&window=5m'");
    }
}
