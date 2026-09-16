<?php
/**
 * central-logs integration client — batched NDJSON inserts (PHP 7.4+).
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
 *   require_once __DIR__ . '/central_logs.php';
 *   central_logs_log('info', 'app started', ['version' => '1.2.3']);
 *   central_logs_log('warn', 'queue lag 12s', ['queue' => 'events', 'duration_ms' => 12000]);
 *   // registered_shutdown_function flushes automatically — nothing to call.
 *
 * PHP has no background timer in classic SAPIs, so batches flush when
 * MAX_BATCH is reached or at request shutdown. For long-running CLI daemons
 * call central_logs_flush() on your own loop cadence.
 */

declare(strict_types=1);

final class CentralLogsClient
{
    private string $url;
    private string $apiKey;
    private string $service;
    private int $maxBatch;
    private int $maxQueue;
    /** @var list<array<string,mixed>> */
    private array $queue = [];
    private int $dropped = 0;

    public function __construct(
        ?string $url = null,
        ?string $apiKey = null,
        ?string $service = null
    ) {
        $this->url      = rtrim($url      ?? (getenv('CENTRAL_LOGS_URL')      ?: 'http://localhost:8080'), '/');
        $this->apiKey   = $apiKey         ?? (getenv('CENTRAL_LOGS_API_KEY')  ?: '');
        $this->service  = $service        ?? (getenv('CENTRAL_LOGS_SERVICE')  ?: 'default-app');
        $this->maxBatch = (int) (getenv('CENTRAL_LOGS_MAX_BATCH') ?: '100');
        $this->maxQueue = (int) (getenv('CENTRAL_LOGS_MAX_QUEUE') ?: '5000');

        if (function_exists('register_shutdown_function')) {
            register_shutdown_function(fn () => $this->flush());
        }
    }

    /** Queue one record. Extra key/value pairs become queryable attributes. */
    public function log(string $level, string $msg, array $fields = []): void
    {
        $record = array_merge([
            'service' => $this->service,
            'level'   => strtolower($level),
            'msg'     => $msg,
            'ts'      => gmdate('Y-m-d\TH:i:s.v\Z'),
        ], $fields);

        if (count($this->queue) >= $this->maxQueue) {
            array_shift($this->queue); // bounded: drop oldest
        }
        $this->queue[] = $record;

        if (count($this->queue) >= $this->maxBatch) {
            $this->flush();
        }
    }

    /** Send the queued batch now. */
    public function flush(): void
    {
        if (!$this->queue) {
            return;
        }
        $batch = $this->queue;
        $this->queue = [];

        if ($this->apiKey === '') {
            $this->drop($batch, 'CENTRAL_LOGS_API_KEY not set');
            return;
        }

        $body = '';
        foreach ($batch as $record) {
            $body .= json_encode($record, JSON_UNESCAPED_SLASHES) . "\n";
        }

        $ch = curl_init($this->url . '/v1/logs');
        curl_setopt_array($ch, [
            CURLOPT_POST           => true,
            CURLOPT_POSTFIELDS     => $body,
            CURLOPT_RETURNTRANSFER => true,
            CURLOPT_TIMEOUT        => 5,
            CURLOPT_HTTPHEADER     => [
                'Content-Type: application/x-ndjson',
                'Authorization: Bearer ' . $this->apiKey,
            ],
        ]);
        $resp = curl_exec($ch);
        if ($resp === false) {
            $this->drop($batch, 'curl: ' . curl_error($ch));
        } else {
            $status = (int) curl_getinfo($ch, CURLINFO_RESPONSE_CODE);
            if ($status !== 200) {
                $this->drop($batch, "HTTP $status: " . substr((string) $resp, 0, 200));
            } else {
                $out = json_decode((string) $resp, true);
                if (is_array($out) && ($out['rejected'] ?? 0) > 0) {
                    error_log(sprintf(
                        '[central-logs] rejected %d/%d records',
                        (int) $out['rejected'],
                        count($batch)
                    ));
                }
            }
        }
        curl_close($ch);
    }

    /** @param list<array<string,mixed>> $batch */
    private function drop(array $batch, string $reason): void
    {
        $this->dropped += count($batch);
        error_log("[central-logs] flush failed ({$this->dropped} dropped total): $reason");
        foreach ($batch as $record) {
            error_log(json_encode($record, JSON_UNESCAPED_SLASHES));
        }
    }
}

/** Shared default instance. */
$GLOBALS['__central_logs'] = $GLOBALS['__central_logs'] ?? new CentralLogsClient();

function central_logs_log(string $level, string $msg, array $fields = []): void
{
    $GLOBALS['__central_logs']->log($level, $msg, $fields);
}

function central_logs_flush(): void
{
    $GLOBALS['__central_logs']->flush();
}

// ── smoke test ─────────────────────────────────────────────────────────────
//   CENTRAL_LOGS_API_KEY=clk_... php central_logs.php
if (realpath($argv[0] ?? '') === __FILE__) {
    central_logs_log('info', 'central-logs php client smoke test', ['env' => 'test']);
    central_logs_flush();
    echo "flushed; verify with: curl '.../api/logs?filter=service:default-app&window=5m'\n";
}
