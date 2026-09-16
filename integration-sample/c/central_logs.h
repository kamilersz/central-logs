/* central-logs C client — public API.
 * Build:  cc -std=c11 app.c central_logs.c -o app -lcurl -lpthread
 */
#ifndef CENTRAL_LOGS_H
#define CENTRAL_LOGS_H

#ifdef __cplusplus
extern "C" {
#endif

/* Configure from CENTRAL_LOGS_URL / _API_KEY / _SERVICE env (defaults:
 * http://localhost:8080, required key, "default-app"). `base_url` overrides
 * the env URL when non-NULL. Call once at startup. */
void central_logs_init(const char *base_url);

/* Queue one record. level: "debug"|"info"|"warn"|"error"|"fatal". */
void central_logs_log(const char *level, const char *msg);

/* Queue one record with queryable attribute fields.
 * fields format (semicolon-separated): "key,S:val;count,I:42;ratio,F:1.5;ok,B:true"
 *   S = JSON string (escaped), I = integer, F = float, B = boolean (raw JSON). */
void central_logs_logf(const char *level, const char *msg, const char *fields);

/* Send the queued batch now (also called by the background flusher). */
void central_logs_flush(void);

/* Stop the background flusher and send the final batch. */
void central_logs_shutdown(void);

/* Escape `src` as a JSON string (including quotes) into a malloc'd buffer.
 * Exposed for tests. */
char *cl_json_escape(const char *src);

#ifdef __cplusplus
}
#endif

#endif /* CENTRAL_LOGS_H */
