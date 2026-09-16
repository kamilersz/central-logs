// central-logs C++ client — public API.
// Build:  g++ -std=c++17 app.cpp central_logs.cpp -o app -lcurl -lpthread
#pragma once

#include <map>
#include <string>

namespace centrallogs {

enum class Level { Debug, Info, Warn, Error, Fatal };

// Global client configured from CENTRAL_LOGS_URL / _API_KEY / _SERVICE env
// (defaults: http://localhost:8080, required key, "default-app").
// `base_url` overrides the env URL when non-empty. Call once at startup;
// the destructor (or centrallogs::shutdown()) sends the final batch.
void init(const std::string &base_url = "");

void log(Level level, const std::string &msg);

// `fields` values: strings are JSON-escaped; use raw() to emit a value
// verbatim (numbers, booleans, nested JSON).
void log(Level level, const std::string &msg,
         const std::map<std::string, std::string> &fields);

struct Raw {
    std::string json;
};
inline Raw raw(std::string json) { return Raw{std::move(json)}; }

void flush();
void shutdown();

// Escape `s` as a JSON string (with quotes). Exposed for tests.
std::string json_escape(const std::string &s);

} // namespace centrallogs
