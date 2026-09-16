/*
 * central-logs integration client — batched NDJSON inserts (C11).
 *
 * Dependencies: libcurl, pthreads.
 *   Build:  cc -std=c11 app.c central_logs.c -o app -lcurl -lpthread
 *   Demo:   cc -std=c11 -DCL_DEMO central_logs.c -o cl-demo -lcurl -lpthread
 *
 * Setup (see INTEGRATION.md):
 *   1. Admin key pinned in the server's .env (CENTRAL_LOGS_HTTP_API_KEY).
 *   2. Mint an insert-only key (POST /v1/api-keys, scopes: "insert").
 *   3. Export for this app:
 *        CENTRAL_LOGS_URL      (default http://localhost:8080)
 *        CENTRAL_LOGS_API_KEY  (the raw clk_... value — required)
 *        CENTRAL_LOGS_SERVICE  (default "default-app")
 *
 * A background pthread flushes every CENTRAL_LOGS_FLUSH_MS (default 1000);
 * central_logs_shutdown() stops it and sends the final batch. On persistent
 * failure the batch goes to stderr and the queue is bounded, so outages
 * can't exhaust memory.
 */

#include "central_logs.h"

#include <curl/curl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define CL_MAX_QUEUE 5000
#define CL_MAX_BATCH 100

static char  cl_url[512];
static char  cl_key[512];
static char  cl_service[128];
static long  cl_flush_ms = 1000;

static char          **cl_queue;
static size_t          cl_len, cl_cap, cl_dropped;
static pthread_mutex_t cl_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_t       cl_thread;
static int             cl_closed;

static const char *cl_env(const char *k, const char *d) {
    const char *v = getenv(k);
    return (v && *v) ? v : d;
}

/* RFC3339 UTC timestamp with milliseconds, no external time lib. */
static void cl_timestamp(char out[32]) {
    struct timespec ts;
    timespec_get(&ts, TIME_UTC);
    struct tm tm;
    gmtime_r(&ts.tv_sec, &tm);
    snprintf(out, 32, "%04d-%02d-%02dT%02d:%02d:%02d.%03ldZ",
             tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday,
             tm.tm_hour, tm.tm_min, tm.tm_sec, ts.tv_nsec / 1000000);
}

/* Escape `src` as a JSON string (with quotes) into a malloc'd buffer. */
char *cl_json_escape(const char *src) {
    size_t cap = strlen(src) * 6 + 3;
    char *out = malloc(cap);
    if (!out) return NULL;
    char *o = out;
    *o++ = '"';
    for (const unsigned char *p = (const unsigned char *)src; *p; p++) {
        switch (*p) {
        case '"':  *o++ = '\\'; *o++ = '"';  break;
        case '\\': *o++ = '\\'; *o++ = '\\'; break;
        case '\n': *o++ = '\\'; *o++ = 'n';  break;
        case '\r': *o++ = '\\'; *o++ = 'r';  break;
        case '\t': *o++ = '\\'; *o++ = 't';  break;
        default:
            if (*p < 0x20) o += snprintf(o, 8, "\\u%04x", *p);
            else *o++ = (char)*p;
        }
    }
    *o++ = '"';
    *o = '\0';
    return out;
}

static void  central_logs_flush_impl(void);
static void *cl_flusher(void *arg);

void central_logs_init(const char *base_url) {
    snprintf(cl_url, sizeof cl_url, "%s",
             base_url ? base_url : cl_env("CENTRAL_LOGS_URL", "http://localhost:8080"));
    size_t n = strlen(cl_url);
    while (n > 0 && cl_url[n - 1] == '/') cl_url[--n] = '\0';
    snprintf(cl_key, sizeof cl_key, "%s", cl_env("CENTRAL_LOGS_API_KEY", ""));
    snprintf(cl_service, sizeof cl_service, "%s", cl_env("CENTRAL_LOGS_SERVICE", "default-app"));
    cl_flush_ms = atol(cl_env("CENTRAL_LOGS_FLUSH_MS", "1000"));
    if (cl_flush_ms < 100) cl_flush_ms = 100;

    cl_cap = 256;
    cl_queue = calloc(cl_cap, sizeof(char *));
    if (!cl_queue) abort();

    if (pthread_create(&cl_thread, NULL, cl_flusher, NULL) != 0) {
        fprintf(stderr, "[central-logs] failed to start flusher thread\n");
    }
}

static void *cl_flusher(void *arg) {
    (void)arg;
    while (!cl_closed) {
        usleep((useconds_t)cl_flush_ms * 1000);
        if (cl_closed) break;
        central_logs_flush_impl();
    }
    return NULL;
}

void central_logs_log(const char *level, const char *msg) {
    central_logs_logf(level, msg, NULL);
}

void central_logs_logf(const char *level, const char *msg, const char *fields) {
    if (cl_closed) return;
    char ts[32];
    cl_timestamp(ts);

    char *svc = cl_json_escape(cl_service);
    char *m   = cl_json_escape(msg);
    if (!svc || !m) { free(svc); free(m); return; }
    size_t cap = strlen(svc) + strlen(m) + strlen(ts) + strlen(level) + 96;
    char *line = malloc(cap);
    if (!line) { free(svc); free(m); return; }
    snprintf(line, cap, "{\"service\":%s,\"level\":\"%s\",\"msg\":%s,\"ts\":\"%s\"",
             svc, level, m, ts);
    free(svc);
    free(m);

    /* fields format: "key,S:string;count,I:42;ratio,F:1.5;ok,B:true" */
    if (fields && *fields) {
        char *dup = strdup(fields);
        if (!dup) { free(line); return; }
        for (char *pair = strtok(dup, ";"); pair; pair = strtok(NULL, ";")) {
            char *colon = strchr(pair, ':');
            if (!colon) continue;
            *colon = '\0';
            char *val = colon + 1;
            char type = 'S';
            if (val[0] && val[1] == ':') { type = val[0]; val += 2; }
            char *kesc = cl_json_escape(pair);
            if (!kesc) break;
            if (type == 'S') {
                char *vesc = cl_json_escape(val);
                if (!vesc) { free(kesc); break; }
                cap = strlen(line) + strlen(kesc) + strlen(vesc) + 4;
                char *tmp = realloc(line, cap);
                if (!tmp) { free(vesc); free(kesc); break; }
                line = tmp;
                sprintf(line + strlen(line), ",%s:%s", kesc, vesc);
                free(vesc);
            } else {
                /* I / F / B: value is emitted as raw JSON */
                cap = strlen(line) + strlen(kesc) + strlen(val) + 4;
                char *tmp = realloc(line, cap);
                if (!tmp) { free(kesc); break; }
                line = tmp;
                sprintf(line + strlen(line), ",%s:%s", kesc, val);
            }
            free(kesc);
        }
        free(dup);
    }
    strcat(line, "}");

    pthread_mutex_lock(&cl_lock);
    if (cl_len >= CL_MAX_QUEUE) {
        free(cl_queue[0]);
        memmove(&cl_queue[0], &cl_queue[1], (cl_len - 1) * sizeof(char *));
        cl_len--;
    }
    if (cl_len == cl_cap) {
        cl_cap *= 2;
        char **tmp = realloc(cl_queue, cl_cap * sizeof(char *));
        if (!tmp) { pthread_mutex_unlock(&cl_lock); free(line); return; }
        cl_queue = tmp;
    }
    cl_queue[cl_len++] = line;
    size_t count = cl_len;
    pthread_mutex_unlock(&cl_lock);

    if (count >= CL_MAX_BATCH) central_logs_flush_impl();
}

static size_t cl_curl_write(char *ptr, size_t size, size_t nmemb, void *ud) {
    (void)ptr; (void)ud;
    return size * nmemb; /* discard response body */
}

void central_logs_flush(void) { central_logs_flush_impl(); }

static void central_logs_flush_impl(void) {
    pthread_mutex_lock(&cl_lock);
    if (cl_len == 0) { pthread_mutex_unlock(&cl_lock); return; }
    char **batch = cl_queue;
    size_t blen = cl_len;
    cl_queue = calloc(cl_cap, sizeof(char *));
    if (!cl_queue) { cl_queue = batch; pthread_mutex_unlock(&cl_lock); return; }
    cl_len = 0;
    pthread_mutex_unlock(&cl_lock);

    if (!cl_key[0]) {
        cl_dropped += blen;
        fprintf(stderr, "[central-logs] flush failed (%zu dropped total): CENTRAL_LOGS_API_KEY not set\n",
                cl_dropped);
        for (size_t i = 0; i < blen; i++) { fprintf(stderr, "%s\n", batch[i]); free(batch[i]); }
        free(batch);
        return;
    }

    /* build NDJSON body */
    size_t cap = 4096, off = 0;
    char *body = malloc(cap);
    if (!body) { cl_dropped += blen; free(batch); return; }
    body[0] = '\0';
    for (size_t i = 0; i < blen; i++) {
        size_t need = strlen(batch[i]) + 2;
        if (off + need > cap) {
            cap = (off + need) * 2;
            char *tmp = realloc(body, cap);
            if (!tmp) { free(body); body = NULL; break; }
            body = tmp;
        }
        off += (size_t)snprintf(body + off, need, "%s\n", batch[i]);
    }

    if (!body) {
        cl_dropped += blen;
        for (size_t i = 0; i < blen; i++) free(batch[i]);
        free(batch);
        return;
    }

    char url[600], auth[600];
    snprintf(url, sizeof url, "%s/v1/logs", cl_url);
    snprintf(auth, sizeof auth, "Authorization: Bearer %s", cl_key);

    CURL *curl = curl_easy_init();
    if (!curl) {
        cl_dropped += blen;
        fprintf(stderr, "[central-logs] curl init failed\n");
        for (size_t i = 0; i < blen; i++) { fprintf(stderr, "%s\n", batch[i]); free(batch[i]); }
        free(batch); free(body);
        return;
    }
    struct curl_slist *headers = NULL;
    headers = curl_slist_append(headers, "Content-Type: application/x-ndjson");
    headers = curl_slist_append(headers, auth);
    curl_easy_setopt(curl, CURLOPT_URL, url);
    curl_easy_setopt(curl, CURLOPT_POSTFIELDS, body);
    curl_easy_setopt(curl, CURLOPT_HTTPHEADER, headers);
    curl_easy_setopt(curl, CURLOPT_TIMEOUT, 5L);
    curl_easy_setopt(curl, CURLOPT_WRITEFUNCTION, cl_curl_write);

    CURLcode rc = curl_easy_perform(curl);
    long status = 0;
    curl_easy_getinfo(curl, CURLINFO_RESPONSE_CODE, &status);
    curl_slist_free_all(headers);
    curl_easy_cleanup(curl);

    if (rc != CURLE_OK || status != 200) {
        cl_dropped += blen;
        fprintf(stderr, "[central-logs] flush failed (%zu dropped total): %sHTTP %ld\n",
                cl_dropped, rc == CURLE_OK ? "" : curl_easy_strerror(rc), status);
        for (size_t i = 0; i < blen; i++) fprintf(stderr, "%s\n", batch[i]);
    }
    for (size_t i = 0; i < blen; i++) free(batch[i]);
    free(batch);
    free(body);
}

void central_logs_shutdown(void) {
    if (cl_closed) return;
    cl_closed = 1;
    /* let the flusher wake once so it observes cl_closed */
    usleep((useconds_t)cl_flush_ms * 1000);
    central_logs_flush_impl();
}

#ifdef CL_DEMO
int main(void) {
    central_logs_init(NULL);
    central_logs_log("info", "central-logs c client smoke test");
    central_logs_logf("warn", "queue lag %ds", "duration_ms,I:12000;queue,S:events");
    central_logs_shutdown();
    printf("flushed; verify with: curl '.../api/logs?filter=service:default-app&window=5m'\n");
    return 0;
}
#endif
