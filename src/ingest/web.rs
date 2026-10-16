//! Web-analytics ingest: expand `Protocol::WebEvent` envelopes into
//! `web_events` rows (PLAN-WEB-ANALYTICS.md, W1).
//!
//! Runs on ingest workers (heavy work belongs here, not the hot path):
//! envelope decode → event `ts` from `receive_ts - dt` → UA classification
//! (device/browser/os/bot) → referrer/UTM attribution → daily-rotating
//! visitor hash → GeoIP. The raw IP is **never** copied into the row.

use std::sync::Arc;

use serde_json::Value;

use crate::store::appender::WebRow;
use crate::wal::meta::WalMeta;
use crate::{Protocol, RawRecord};

/// Maximum user-agent length hashed/stored.
const MAX_UA_LEN: usize = 512;
/// Maximum stored query length (cardinality control, plan §8).
const MAX_QUERY_LEN: usize = 512;

/// UTM parameter names extracted into the `utm` JSON column.
const UTM_KEYS: &[&str] = &[
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_term",
    "utm_content",
];

pub struct WebParser {
    meta: Arc<WalMeta>,
    /// GeoIP behind the `geoip` feature (shared with the log enricher).
    #[cfg(feature = "geoip")]
    geo: Option<Arc<crate::ingest::enrich::GeoEnricher>>,
}

impl WebParser {
    pub fn new(
        meta: Arc<WalMeta>,
        #[cfg(feature = "geoip")] geo: Option<Arc<crate::ingest::enrich::GeoEnricher>>,
    ) -> Self {
        Self {
            meta,
            #[cfg(feature = "geoip")]
            geo,
        }
    }

    /// Parse one WAL record. Invalid envelopes return an empty vec (counted
    /// upstream as parse errors).
    pub fn parse(&self, rec: &RawRecord) -> Vec<WebRow> {
        debug_assert_eq!(rec.protocol, Protocol::WebEvent);
        let Ok(text) = std::str::from_utf8(&rec.raw) else {
            return Vec::new();
        };
        let Ok(env) = serde_json::from_str::<Value>(text) else {
            return Vec::new();
        };
        let Some(site) = env.get("site").and_then(Value::as_str) else {
            return Vec::new();
        };
        let Some(events) = env.get("ev").and_then(Value::as_array) else {
            return Vec::new();
        };
        let ua = env
            .get("ua")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(MAX_UA_LEN)
            .collect::<String>();
        let origin_host = env
            .get("origin")
            .and_then(Value::as_str)
            .and_then(origin_hostname)
            .unwrap_or_default();
        let uid = env.get("uid").and_then(Value::as_str).unwrap_or("");
        let user_id = if uid.is_empty() {
            None
        } else {
            Some(hash64(&(format!("cl-user-v1|{site}|{uid}")).as_bytes()))
        };
        // W3C traceparent → trace_id/span_id for backend-log correlation.
        let (trace_id, span_id) = env
            .get("tp")
            .and_then(Value::as_str)
            .map(split_traceparent)
            .unwrap_or((String::new(), String::new()));

        let device_cls = classify_ua(&ua);
        let client_ip = parse_ip(&rec.source_addr);

        // Geography, in precedence order:
        // 1. GeoIP City DB (country + region + city) when the `geoip`
        //    feature is built and `geoip_db_path` resolves the IP;
        // 2. the envelope's `cc` (Cloudflare `CF-IPCountry`, country only) —
        //    works with zero setup behind Cloudflare.
        #[cfg(feature = "geoip")]
        let geo = client_ip
            .as_ref()
            .and_then(|ip| self.geo.as_ref().map(|g| g.lookup(*ip)))
            .unwrap_or_default();
        #[cfg(feature = "geoip")]
        let country = if geo.country.as_deref().unwrap_or("").is_empty() {
            env.get("cc")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        } else {
            geo.country.clone().unwrap_or_default()
        };
        #[cfg(feature = "geoip")]
        let (region, city) = (
            geo.region.clone().unwrap_or_default(),
            geo.city.clone().unwrap_or_default(),
        );
        #[cfg(not(feature = "geoip"))]
        let country: String = env
            .get("cc")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        #[cfg(not(feature = "geoip"))]
        let (region, city): (String, String) = (String::new(), String::new());

        // Operator's own visit, flagged at the edge (see `build_envelope`):
        // the collector matched the resolved client IP against the project's
        // `exclude_ips` list. Stored so the dashboard can filter it out.
        let is_internal = env.get("internal").and_then(Value::as_bool).unwrap_or(false);

        let mut rows = Vec::with_capacity(events.len());
        for ev in events {
            let Some(kind) = ev.get("e").and_then(Value::as_str) else {
                continue;
            };
            // Event timestamp = receive_ts - dt (client clocks are untrusted,
            // plan §3.1). dt is clamped at the collector; clamp again anyway.
            let dt_ms = ev
                .get("dt")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
                .clamp(0.0, 24.0 * 3600.0 * 1000.0) as i64;
            let ts = rec
                .receive_ts
                - chrono::Duration::milliseconds(dt_ms);

            // Path + query: split once at '?', then trim tracking params out
            // of the stored query (allowlist = everything except utm_*, which
            // moves to the `utm` JSON column).
            let (path, query, utm) = split_url_and_utm(ev.get("u").and_then(Value::as_str));
            let referrer = ev
                .get("r")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let (source, medium) = classify_referrer(&referrer, &utm, &origin_host);

            // Visitor hash uses the EVENT date's salt (decision D5): late
            // events hash identically to same-day ones, and salts rotate
            // daily so visitors cannot be tracked across days.
            let visitor_id = match self.meta.web_salt_for_date(ts.date_naive()) {
                Ok(salt) => {
                    let mut input = Vec::with_capacity(32 + site.len() + ua.len() + 21);
                    input.extend_from_slice(&salt);
                    input.extend_from_slice(b"|");
                    input.extend_from_slice(site.as_bytes());
                    input.extend_from_slice(b"|");
                    input.extend_from_slice(client_ip.as_ref().map(|i| i.to_string()).unwrap_or_default().as_bytes());
                    input.extend_from_slice(b"|");
                    input.extend_from_slice(ua.as_bytes());
                    hash64(&input)
                }
                Err(e) => {
                    tracing::warn!(?e, "web salt unavailable; visitor hash zeroed");
                    0
                }
            };

            rows.push(WebRow {
                ts,
                insert_ts: rec.receive_ts,
                site_id: site.to_string(),
                event: kind.to_string(),
                name: ev
                    .get("n")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                value: ev.get("v").and_then(Value::as_f64).map(|v| {
                    // Abuse guard: a single engagement report is a heartbeat
                    // delta (~30s by default); anything above an hour is a
                    // forged or ancient-tracker artifact, not real attention.
                    if kind == "engagement" {
                        v.clamp(0.0, 3_600_000.0)
                    } else {
                        v
                    }
                }),
                hostname: origin_host.to_string(),
                path,
                query,
                referrer: referrer.chars().take(2048).collect(),
                source,
                medium,
                utm,
                visitor_id,
                session_id: None,
                user_id,
                is_internal,
                country: country.clone(),
                region: region.clone(),
                city: city.clone(),
                device: device_cls.device.to_string(),
                browser: device_cls.browser.to_string(),
                os: device_cls.os.to_string(),
                screen_w: ev
                    .get("w")
                    .and_then(Value::as_f64)
                    .map(|w| w.clamp(0.0, 65535.0) as u16),
                is_bot: device_cls.is_bot,
                props: ev.get("p").cloned().unwrap_or(Value::Null),
                trace_id: trace_id.clone(),
                span_id: span_id.clone(),
            });
        }
        rows
    }
}

// =====================================================================
// Hashing
// =====================================================================

/// First 8 bytes of SHA-256, big-endian → UBIGINT for DuckDB.
fn hash64(input: &[u8]) -> u64 {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(input);
    let out = h.finalize();
    u64::from_be_bytes(out[..8].try_into().unwrap())
}

fn parse_ip(s: &str) -> Option<std::net::IpAddr> {
    s.trim().parse().ok()
}

fn origin_hostname(origin: &str) -> Option<String> {
    let rest = origin.split_once("://")?.1;
    let host = rest.split('/').next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// `00-<32hex>-<16hex>-<2hex>` → (trace_id, span_id). Anything malformed
/// yields empty strings (the collector already validated, so this is
/// defense in depth).
fn split_traceparent(tp: &str) -> (String, String) {
    let parts: Vec<&str> = tp.split('-').collect();
    if parts.len() == 4 && parts[0] == "00" {
        (
            parts[1].to_ascii_lowercase(),
            parts[2].to_ascii_lowercase(),
        )
    } else {
        (String::new(), String::new())
    }
}

// =====================================================================
// URL / UTM handling
// =====================================================================

type UrlParts = (String, String, Value);

/// Split `path?query`, trim `utm_*` params out of the stored query (they
/// move to the `utm` JSON object — cardinality + noise control).
fn split_url_and_utm(u: Option<&str>) -> UrlParts {
    let Some(u) = u else {
        return (String::new(), String::new(), Value::Null);
    };
    let (path, query) = match u.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (u.to_string(), String::new()),
    };
    let path = path.chars().take(1024).collect::<String>();
    if query.is_empty() {
        return (path, String::new(), Value::Null);
    }
    let mut kept: Vec<String> = Vec::new();
    let mut utm = serde_json::Map::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        let kl = k.to_ascii_lowercase();
        if let Some(short) = kl.strip_prefix("utm_") {
            if UTM_KEYS.contains(&kl.as_str()) && !v.is_empty() {
                utm.insert(short.to_string(), Value::String(v.to_string()));
            }
            continue; // utm_* never stays in the stored query
        }
        kept.push(pair.to_string());
    }
    let query_out = if kept.is_empty() {
        String::new()
    } else {
        kept.join("&").chars().take(MAX_QUERY_LEN).collect()
    };
    let utm_val = if utm.is_empty() {
        Value::Null
    } else {
        Value::Object(utm)
    };
    (path, query_out, utm_val)
}

// =====================================================================
// Referrer / campaign attribution
// =====================================================================

/// Known search engines → medium `organic`.
const SEARCH_ENGINES: &[(&str, &str)] = &[
    ("google.", "google"),
    ("bing.com", "bing"),
    ("duckduckgo.com", "duckduckgo"),
    ("search.yahoo.com", "yahoo"),
    ("yandex.", "yandex"),
    ("baidu.com", "baidu"),
    ("ecosia.org", "ecosia"),
    ("startpage.com", "startpage"),
];

/// Known social platforms → medium `social`.
const SOCIAL: &[(&str, &str)] = &[
    ("t.co", "twitter"),
    ("twitter.com", "twitter"),
    ("x.com", "twitter"),
    ("facebook.com", "facebook"),
    ("instagram.com", "instagram"),
    ("linkedin.com", "linkedin"),
    ("reddit.com", "reddit"),
    ("youtube.com", "youtube"),
    ("tiktok.com", "tiktok"),
    ("pinterest.com", "pinterest"),
    ("news.ycombinator.com", "hn"),
    ("mastodon.", "mastodon"),
    ("whatsapp.com", "whatsapp"),
    ("t.me", "telegram"),
];

/// AI assistants/chatbots → medium `ai` (Statcounter-style "AI Chatbot"
/// channel: chatgpt.com, Claude, Perplexity, Gemini, Copilot, Grok …).
const AI_ASSISTANTS: &[(&str, &str)] = &[
    ("chatgpt.com", "chatgpt"),
    ("chat.openai.com", "chatgpt"),
    ("claude.ai", "claude"),
    ("perplexity.ai", "perplexity"),
    ("gemini.google.com", "gemini"),
    ("copilot.microsoft.com", "copilot"),
    ("grok.com", "grok"),
    ("x.ai", "grok"),
    ("deepseek.com", "deepseek"),
    ("chat.deepseek.com", "deepseek"),
    ("poe.com", "poe"),
    ("you.com", "you"),
    ("phind.com", "phind"),
];

/// (source, medium) for a referrer URL, with UTM overrides taking priority.
/// `origin_host` is the site's own hostname (from the envelope `origin`):
/// self-referrals (in-site navigation) count as direct, per analytics
/// convention — otherwise every internal click would read as a "referral".
pub fn classify_referrer(referrer: &str, utm: &Value, origin_host: &str) -> (String, String) {
    // UTM wins when present (plan: source = utm_source …).
    let utm_source = utm.get("source").and_then(Value::as_str);
    let utm_medium = utm.get("medium").and_then(Value::as_str);
    if let Some(s) = utm_source {
        let m = utm_medium.unwrap_or("campaign");
        let medium = if matches!(m, "organic" | "social" | "email" | "paid" | "referral" | "direct" | "cpc" | "affiliate")
        {
            m
        } else {
            "campaign"
        };
        return (s.chars().take(128).collect(), medium.to_string());
    }
    if referrer.is_empty() {
        return ("direct".to_string(), "direct".to_string());
    }
    let host = host_of(referrer).to_ascii_lowercase();
    if !origin_host.is_empty() && host == origin_host.to_ascii_lowercase() {
        return ("direct".to_string(), "direct".to_string());
    }
    // AI assistants before search engines: `gemini.google.com` contains
    // `google.` but is Gemini traffic, not Google Search.
    for (needle, name) in AI_ASSISTANTS {
        if host.contains(needle) {
            return (name.to_string(), "ai".to_string());
        }
    }
    for (needle, name) in SEARCH_ENGINES {
        if host.contains(needle) {
            return (name.to_string(), "organic".to_string());
        }
    }
    for (needle, name) in SOCIAL {
        if host.contains(needle) {
            return (name.to_string(), "social".to_string());
        }
    }
    (
        host.chars().take(128).collect(),
        "referral".to_string(),
    )
}

fn host_of(url: &str) -> String {
    match url.split_once("://") {
        Some((_, rest)) => rest.split('/').next().unwrap_or("").to_string(),
        None => url.split('/').next().unwrap_or("").to_string(),
    }
}

// =====================================================================
// User-agent classification (no external crate — plan fallback path)
// =====================================================================

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UaClass {
    pub device: &'static str,
    pub browser: &'static str,
    pub os: &'static str,
    pub is_bot: bool,
}

const BOT_NEEDLES: &[&str] = &[
    "bot", "crawler", "spider", "slurp", "crawl", "curl/", "wget", "python-requests",
    "python-urllib", "go-http-client", "java/", "apache-httpclient", "headless",
    "phantomjs", "lighthouse", "pagespeed", "uptime", "monitoring", "pingdom",
    "datadog", "semrush", "ahrefs", "mj12", "dotbot", "petalbot", "bytespider",
    "facebookexternalhit", "embedly", "preview", "scanner", "zgrab", "masscan",
    "nikto", "nmap", "okhttp", "libwww", "httpie",
];

/// Cheap, dependency-free UA classification for the top browser/OS/device
/// families (plan W1 fallback parser). Unknown values are empty strings.
pub fn classify_ua(ua: &str) -> UaClass {
    let l = ua.to_ascii_lowercase();
    if ua.is_empty() {
        return UaClass {
            device: "",
            browser: "",
            os: "",
            is_bot: false,
        };
    }
    let is_bot = BOT_NEEDLES.iter().any(|n| l.contains(n));
    if is_bot {
        return UaClass {
            device: "bot",
            browser: "",
            os: "",
            is_bot: true,
        };
    }
    // Browser: order matters (Edge/Chrome/Chromium all contain "safari").
    let browser = if l.contains("edg/") || l.contains("edge/") || l.contains("edga/") || l.contains("edgios/") {
        "Edge"
    } else if l.contains("opr/") || l.contains("opera") {
        "Opera"
    } else if l.contains("samsungbrowser") {
        "Samsung Internet"
    } else if l.contains("firefox") || l.contains("fxios") {
        "Firefox"
    } else if l.contains("crios") || l.contains("chrome") || l.contains("chromium") {
        "Chrome"
    } else if l.contains("safari") {
        "Safari"
    } else if l.contains("msie") || l.contains("trident/") {
        "IE"
    } else {
        ""
    };
    // OS.
    let os = if l.contains("windows") {
        "Windows"
    } else if l.contains("iphone") || l.contains("ipod") {
        "iOS"
    } else if l.contains("ipad") {
        "iPadOS"
    } else if l.contains("mac os x") || l.contains("macintosh") {
        "macOS"
    } else if l.contains("android") {
        "Android"
    } else if l.contains("cros") {
        "ChromeOS"
    } else if l.contains("linux") || l.contains("x11") {
        "Linux"
    } else {
        ""
    };
    // Device.
    let device = if l.contains("ipad") || l.contains("tablet") || (l.contains("android") && !l.contains("mobile")) {
        "tablet"
    } else if l.contains("mobile") || l.contains("iphone") || l.contains("ipod") || l.contains("android") {
        "mobile"
    } else {
        "desktop"
    };
    UaClass {
        device,
        browser,
        os,
        is_bot: false,
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Test parser without a GeoIP DB (mirrors production without
    /// `geoip_db_path`: country falls back to the envelope `cc` field).
    fn parser_without_geo(meta: Arc<WalMeta>) -> WebParser {
        #[cfg(feature = "geoip")]
        return WebParser::new(meta, None);
        #[cfg(not(feature = "geoip"))]
        return WebParser::new(meta);
    }

    #[test]
    fn ua_classification_top_families() {
        let chrome_win = classify_ua(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36",
        );
        assert_eq!(chrome_win.device, "desktop");
        assert_eq!(chrome_win.browser, "Chrome");
        assert_eq!(chrome_win.os, "Windows");
        assert!(!chrome_win.is_bot);

        let safari_iphone = classify_ua(
            "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1",
        );
        assert_eq!(safari_iphone.device, "mobile");
        assert_eq!(safari_iphone.browser, "Safari");
        assert_eq!(safari_iphone.os, "iOS");

        let ipad = classify_ua(
            "Mozilla/5.0 (iPad; CPU OS 16_6 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.6 Mobile/15E148 Safari/604.1",
        );
        assert_eq!(ipad.device, "tablet");

        let edge = classify_ua(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36 Edg/125.0.0.0",
        );
        assert_eq!(edge.browser, "Edge");

        let bot = classify_ua(
            "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
        );
        assert!(bot.is_bot);
        assert_eq!(bot.device, "bot");

        let empty = classify_ua("");
        assert_eq!(empty.browser, "");
        assert!(!empty.is_bot);
    }

    #[test]
    fn referrer_classification_and_utm_priority() {
        let (s, m) = classify_referrer("https://www.google.com/", &Value::Null, "");
        assert_eq!((s.as_str(), m.as_str()), ("google", "organic"));
        let (s, m) = classify_referrer("https://t.co/abc", &Value::Null, "");
        assert_eq!((s.as_str(), m.as_str()), ("twitter", "social"));
        let (s, m) = classify_referrer("https://blog.example.io/post", &Value::Null, "");
        assert_eq!((s.as_str(), m.as_str()), ("blog.example.io", "referral"));
        let (s, m) = classify_referrer("", &Value::Null, "");
        assert_eq!((s.as_str(), m.as_str()), ("direct", "direct"));
        // UTM overrides the referrer.
        let utm = serde_json::json!({"source": "newsletter", "medium": "email"});
        let (s, m) = classify_referrer("https://google.com/", &utm, "");
        assert_eq!((s.as_str(), m.as_str()), ("newsletter", "email"));
        // Unknown UTM medium folds to "campaign".
        let utm = serde_json::json!({"source": "partner-x", "medium": "weird"});
        let (_, m) = classify_referrer("", &utm, "");
        assert_eq!(m, "campaign");
    }

    #[test]
    fn ai_assistants_classify_to_ai_medium() {
        for (url, source) in [
            ("https://chatgpt.com/share/abc", "chatgpt"),
            ("https://chat.openai.com/c/abc", "chatgpt"),
            ("https://claude.ai/chat/abc", "claude"),
            ("https://www.perplexity.ai/search?q=x", "perplexity"),
            ("https://gemini.google.com/share/abc", "gemini"),
            ("https://copilot.microsoft.com/c/abc", "copilot"),
            ("https://grok.com/share/abc", "grok"),
            ("https://chat.deepseek.com/a/chat/s/abc", "deepseek"),
        ] {
            let (s, m) = classify_referrer(url, &Value::Null, "");
            assert_eq!((s.as_str(), m.as_str()), (source, "ai"), "{url}");
        }
    }

    #[test]
    fn self_referrals_count_as_direct() {
        // In-site navigation must not read as a "referral" (analytics
        // convention) — this is what was painting ajakan.or.id traffic.
        let (s, m) = classify_referrer(
            "https://ajakan.or.id/program",
            &Value::Null,
            "ajakan.or.id",
        );
        assert_eq!((s.as_str(), m.as_str()), ("direct", "direct"));
        // Case-insensitive host match.
        let (s, m) = classify_referrer(
            "https://AJAKAN.OR.ID/donasi",
            &Value::Null,
            "ajakan.or.id",
        );
        assert_eq!((s.as_str(), m.as_str()), ("direct", "direct"));
        // A different host still classifies normally.
        let (s, m) = classify_referrer(
            "https://blog.example.io/post",
            &Value::Null,
            "ajakan.or.id",
        );
        assert_eq!((s.as_str(), m.as_str()), ("blog.example.io", "referral"));
    }

    #[test]
    fn url_split_trims_utm_and_keeps_the_rest() {
        let (path, query, utm) = split_url_and_utm(Some(
            "/search?q=logs&utm_source=google&utm_medium=organic&utm_campaign=launch",
        ));
        assert_eq!(path, "/search");
        assert_eq!(query, "q=logs");
        assert_eq!(utm["source"], "google");
        assert_eq!(utm["medium"], "organic");
        assert_eq!(utm["campaign"], "launch");

        let (path, query, utm) = split_url_and_utm(Some("/pricing"));
        assert_eq!(path, "/pricing");
        assert_eq!(query, "");
        assert_eq!(utm, Value::Null);

        let (_, query, _) = split_url_and_utm(Some("/x?token=SECRET&utm_term=t"));
        assert_eq!(query, "token=SECRET");
    }

    #[test]
    fn traceparent_split() {
        let tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let (tid, sid) = split_traceparent(tp);
        assert_eq!(tid, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(sid, "00f067aa0ba902b7");
        let (tid, sid) = split_traceparent("garbage");
        assert!(tid.is_empty() && sid.is_empty());
    }

    #[test]
    fn visitor_hash_is_stable_and_ip_only_enters_the_hash() {
        // Stability of the hash function itself (salt comes from WalMeta).
        let a = hash64(b"salt|site|1.2.3.4|UA");
        let b = hash64(b"salt|site|1.2.3.4|UA");
        assert_eq!(a, b);
        assert_ne!(a, hash64(b"salt|site|1.2.3.4|OtherUA"));
        assert_ne!(a, hash64(b"othersalt|site|1.2.3.4|UA"));
    }

    #[test]
    fn country_falls_back_to_envelope_cc_without_geoip_db() {
        // No GeoIP DB configured (or geoip feature off): the collector's
        // `cf-ipcountry` capture still yields per-country analytics.
        let tmp = tempfile::tempdir().unwrap();
        let meta = Arc::new(WalMeta::open(&tmp.path().join("meta.redb")).unwrap());
        let parser = parser_without_geo(meta);
        let env = serde_json::json!({
            "v": 1, "site": "site_abc", "ua": "TestBrowser/1.0", "lang": "en",
            "origin": "https://example.com", "uid": null, "cc": "ID",
            "ev": [{"e": "pageview", "u": "/", "dt": 0}]
        });
        let rec = RawRecord {
            receive_ts: chrono::Utc::now(),
            source_addr: "203.0.113.9".to_string(),
            protocol: Protocol::WebEvent,
            raw: bytes::Bytes::from(serde_json::to_vec(&env).unwrap()),
        };
        let rows = parser.parse(&rec);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].country, "ID");
        assert_eq!(rows[0].region, "");
        assert_eq!(rows[0].city, "");
    }

    #[test]
    fn full_envelope_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let meta = Arc::new(WalMeta::open(&tmp.path().join("meta.redb")).unwrap());
        let parser = parser_without_geo(meta);
        let env = serde_json::json!({
            "v": 1,
            "site": "site_abc",
            "ua": "Mozilla/5.0 (Windows NT 10.0) Chrome/125.0.0.0 Safari/537.36",
            "lang": "id-ID",
            "origin": "https://example.com",
            "uid": null,
            "ev": [
                {"e": "pageview", "u": "/pricing?utm_source=x", "r": "https://google.com/", "w": 1440, "dt": 100},
                {"e": "vital", "n": "LCP", "v": 1840.5, "dt": 2100},
                {"e": "error", "n": "TypeError: x is not a function", "dt": 3000}
            ],
            "tp": "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        });
        let rec = RawRecord {
            receive_ts: chrono::Utc::now(),
            source_addr: "203.0.113.9".to_string(),
            protocol: Protocol::WebEvent,
            raw: bytes::Bytes::from(serde_json::to_vec(&env).unwrap()),
        };
        let rows = parser.parse(&rec);
        assert_eq!(rows.len(), 3);

        let pv = &rows[0];
        assert_eq!(pv.site_id, "site_abc");
        assert_eq!(pv.event, "pageview");
        assert_eq!(pv.path, "/pricing");
        assert_eq!(pv.query, "");
        assert_eq!(pv.utm["source"], "x");
        assert_eq!(pv.source, "x");
        assert_eq!(pv.medium, "campaign");
        assert_eq!(pv.browser, "Chrome");
        assert_eq!(pv.device, "desktop");
        assert_eq!(pv.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(pv.span_id, "00f067aa0ba902b7");
        assert_eq!(pv.screen_w, Some(1440));
        assert!(!pv.is_bot);
        assert_eq!(pv.visitor_id, rows[1].visitor_id, "same visitor in one beacon");
        assert!(pv.session_id.is_none());
        assert!(pv.user_id.is_none());
        // ts must be receive - dt.
        let dt_ms = (rec.receive_ts - pv.ts).num_milliseconds();
        assert_eq!(dt_ms, 100);

        let vital = &rows[1];
        assert_eq!(vital.name, "LCP");
        assert_eq!(vital.value, Some(1840.5));

        let err = &rows[2];
        assert_eq!(err.event, "error");
        assert_eq!(err.name, "TypeError: x is not a function");
    }

    #[test]
    fn bad_envelopes_yield_no_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let meta = Arc::new(WalMeta::open(&tmp.path().join("meta.redb")).unwrap());
        let parser = parser_without_geo(meta);
        for bad in ["", "not json", r#"{"site":"x"}"#, r#"{"site":"x","ev":"nope"}"#] {
            let rec = RawRecord {
                receive_ts: chrono::Utc::now(),
                source_addr: "1.2.3.4".into(),
                protocol: Protocol::WebEvent,
                raw: bytes::Bytes::from(bad.to_string()),
            };
            assert!(parser.parse(&rec).is_empty(), "envelope {bad:?} must not parse");
        }
    }
}
