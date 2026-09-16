// central-logs integration client — batched NDJSON inserts (C++17).
//
// Dependencies: libcurl, pthreads.
//   Build:  g++ -std=c++17 app.cpp central_logs.cpp -o app -lcurl -lpthread
//   Demo:   g++ -std=c++17 -DCL_DEMO central_logs.cpp -o cl-demo -lcurl -lpthread
//
// Setup (see INTEGRATION.md):
//   1. Admin key pinned in the server's .env (CENTRAL_LOGS_HTTP_API_KEY).
//   2. Mint an insert-only key (POST /v1/api-keys, scopes: "insert").
//   3. Export for this app:
//        CENTRAL_LOGS_URL      (default http://localhost:8080)
//        CENTRAL_LOGS_API_KEY  (the raw clk_... value — required)
//        CENTRAL_LOGS_SERVICE  (default "default-app")
//
// Usage:
//   centrallogs::init();
//   centrallogs::log(centrallogs::Level::Info, "app started", {
//       {"version", "1.2.3"},
//       {"pid",     centrallogs::raw(std::to_string(getpid()))},
//   });
//   centrallogs::shutdown();  // final flush
//
// A background std::thread flushes every CENTRAL_LOGS_FLUSH_MS (default
// 1000). On persistent failure the batch goes to stderr and the queue is
// bounded, so outages can't exhaust memory.

#include "central_logs.hpp"

#include <curl/curl.h>

#include <atomic>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <deque>
#include <mutex>
#include <thread>
#include <vector>

namespace centrallogs {

namespace {

std::string g_url, g_key, g_service;
long g_flush_ms = 1000;

std::mutex g_mutex;
std::deque<std::string> g_queue;
std::atomic<bool> g_closed{false};
std::atomic<long> g_dropped{0};
std::thread g_thread;

constexpr size_t kMaxQueue = 5000;
constexpr size_t kMaxBatch = 100;

std::string env_or(const char *key, const char *def) {
    const char *v = std::getenv(key);
    return (v && *v) ? std::string(v) : std::string(def);
}

std::string timestamp_now() {
    using namespace std::chrono;
    const auto now = system_clock::now();
    const auto secs = time_point_cast<seconds>(now);
    const auto ms = duration_cast<milliseconds>(now - secs).count();
    std::time_t t = system_clock::to_time_t(now);
    std::tm tm{};
    gmtime_r(&t, &tm);
    char buf[40];
    std::snprintf(buf, sizeof buf, "%04d-%02d-%02dT%02d:%02d:%02d.%03lldZ",
                  tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday,
                  tm.tm_hour, tm.tm_min, tm.tm_sec, static_cast<long long>(ms));
    return buf;
}

const char *level_name(Level l) {
    switch (l) {
    case Level::Debug: return "debug";
    case Level::Warn:  return "warn";
    case Level::Error: return "error";
    case Level::Fatal: return "fatal";
    default:           return "info";
    }
}

} // namespace

std::string json_escape(const std::string &s) {
    std::string out;
    out.reserve(s.size() + 2);
    out += '"';
    for (unsigned char c : s) {
        switch (c) {
        case '"':  out += "\\\""; break;
        case '\\': out += "\\\\"; break;
        case '\n': out += "\\n";  break;
        case '\r': out += "\\r";  break;
        case '\t': out += "\\t";  break;
        default:
            if (c < 0x20) {
                char buf[8];
                std::snprintf(buf, sizeof buf, "\\u%04x", c);
                out += buf;
            } else {
                out += static_cast<char>(c);
            }
        }
    }
    out += '"';
    return out;
}

void init(const std::string &base_url) {
    g_url = base_url.empty() ? env_or("CENTRAL_LOGS_URL", "http://localhost:8080") : base_url;
    while (!g_url.empty() && g_url.back() == '/') g_url.pop_back();
    g_key = env_or("CENTRAL_LOGS_API_KEY", "");
    g_service = env_or("CENTRAL_LOGS_SERVICE", "default-app");
    g_flush_ms = std::atol(env_or("CENTRAL_LOGS_FLUSH_MS", "1000").c_str());
    if (g_flush_ms < 100) g_flush_ms = 100;

    g_closed = false;
    g_thread = std::thread([] {
        while (!g_closed.load()) {
            std::this_thread::sleep_for(std::chrono::milliseconds(g_flush_ms));
            if (g_closed.load()) break;
            flush();
        }
    });
}

void log(Level level, const std::string &msg) { log(level, msg, {}); }

void log(Level level, const std::string &msg,
         const std::map<std::string, std::string> &fields) {
    if (g_closed.load()) return;

    std::string line = "{\"service\":" + json_escape(g_service) +
                       ",\"level\":\"" + level_name(level) +
                       "\",\"msg\":" + json_escape(msg) +
                       ",\"ts\":\"" + timestamp_now() + "\"";
    for (const auto &kv : fields) {
        // A value wrapped in Raw{} is emitted verbatim (see raw() helper —
        // detected by the reserved prefix below).
        static const std::string kRawPrefix = "\x01RAW\x01";
        if (kv.second.rfind(kRawPrefix, 0) == 0) {
            line += ",\"" + kv.first + "\":" + kv.second.substr(kRawPrefix.size());
        } else {
            line += ",\"" + kv.first + "\":" + json_escape(kv.second);
        }
    }
    line += "}";

    size_t size;
    {
        std::lock_guard<std::mutex> lk(g_mutex);
        if (g_queue.size() >= kMaxQueue) g_queue.pop_front(); // bounded
        g_queue.push_back(std::move(line));
        size = g_queue.size();
    }
    if (size >= kMaxBatch) flush();
}

size_t queue_size() {
    std::lock_guard<std::mutex> lk(g_mutex);
    return g_queue.size();
}

namespace {
size_t curl_write(char *ptr, size_t size, size_t nmemb, void *ud) {
    (void)ptr; (void)ud;
    return size * nmemb;
}
} // namespace

void flush() {
    std::deque<std::string> batch;
    {
        std::lock_guard<std::mutex> lk(g_mutex);
        if (g_queue.empty()) return;
        batch.swap(g_queue);
    }

    if (g_key.empty()) {
        g_dropped += batch.size();
        std::fprintf(stderr, "[central-logs] flush failed (%ld dropped total): CENTRAL_LOGS_API_KEY not set\n",
                     g_dropped.load());
        for (const auto &line : batch) std::fprintf(stderr, "%s\n", line.c_str());
        return;
    }

    std::string body;
    body.reserve(batch.size() * 128);
    for (const auto &line : batch) {
        body += line;
        body += '\n';
    }

    CURL *curl = curl_easy_init();
    if (!curl) {
        g_dropped += batch.size();
        std::fprintf(stderr, "[central-logs] curl init failed\n");
        return;
    }
    const std::string url = g_url + "/v1/logs";
    const std::string auth = "Authorization: Bearer " + g_key;
    curl_slist *headers = nullptr;
    headers = curl_slist_append(headers, "Content-Type: application/x-ndjson");
    headers = curl_slist_append(headers, auth.c_str());
    curl_easy_setopt(curl, CURLOPT_URL, url.c_str());
    curl_easy_setopt(curl, CURLOPT_POSTFIELDS, body.c_str());
    curl_easy_setopt(curl, CURLOPT_HTTPHEADER, headers);
    curl_easy_setopt(curl, CURLOPT_TIMEOUT, 5L);
    curl_easy_setopt(curl, CURLOPT_WRITEFUNCTION, curl_write);

    CURLcode rc = curl_easy_perform(curl);
    long status = 0;
    curl_easy_getinfo(curl, CURLINFO_RESPONSE_CODE, &status);
    curl_slist_free_all(headers);
    curl_easy_cleanup(curl);

    if (rc != CURLE_OK || status != 200) {
        g_dropped += batch.size();
        std::fprintf(stderr, "[central-logs] flush failed (%ld dropped total): %sHTTP %ld\n",
                     g_dropped.load(), rc == CURLE_OK ? "" : curl_easy_strerror(rc), status);
        for (const auto &line : batch) std::fprintf(stderr, "%s\n", line.c_str());
    }
}

void shutdown() {
    if (g_closed.exchange(true)) return;
    if (g_thread.joinable()) g_thread.join();
    flush();
}

} // namespace centrallogs

#ifdef CL_DEMO
int main() {
    centrallogs::init();
    centrallogs::log(centrallogs::Level::Info, "central-logs c++ client smoke test");
    centrallogs::log(centrallogs::Level::Warn, "queue lag 12s", {
        {"duration_ms", centrallogs::raw("12000")},
        {"queue", "events"},
    });
    centrallogs::shutdown();
    std::puts("flushed; verify with: curl '.../api/logs?filter=service:default-app&window=5m'");
    return 0;
}
#endif
