(function () {
function D() { return +new Date(); }
var S = document.currentScript, Q = [], t0 = D(), last, act = 0, at = D(), smx = 0;
if (!S) {
  // Dynamically injected scripts (e.g. the dashboard's self-tracking loader
  // in main.tsx) execute with document.currentScript === null. Fall back to
  // the last script tag carrying our config attributes.
  var ss = document.querySelectorAll('script[data-site]');
  S = ss.length ? ss[ss.length - 1] : null;
}
if (!S) return;
var site = S.getAttribute("data-site"), ep = S.getAttribute("data-endpoint") || "/_/e";
if (!site) return;
var nv = navigator, WA = window.addEventListener, PO = window.PerformanceObserver;
if (nv.doNotTrack === "1" || window.doNotTrack === "1" ||
    nv.globalPrivacyControl === true || document.visibilityState === "prerender") return;
if (!window.cl) window.cl = { q: [] };
function vis() { return document.visibilityState === "visible"; }
function send(evs) {
  var p = { s: site, ev: evs }, m = document.querySelector('meta[name="traceparent"]'), b;
  if (m) p.tp = m.getAttribute("content");
  b = JSON.stringify(p);
  try { if (nv.sendBeacon(ep, b)) return; } catch (e) {}
  try { fetch(ep, { method: "POST", body: b, keepalive: true }).catch(function () {}); } catch (e) {}
}
function flush() {
  if (!Q.length) return;
  send(Q.splice(0, 20));
  if (Q.length) setTimeout(flush, 50);
}
function push(ev) { ev.dt = Math.max(0, D() - t0); Q.push(ev); if (Q.length >= 20) flush(); }
function clip(v, n) { return String(v).slice(0, n); }
function pg() { return location.pathname + location.search; }
function pv() {
  var u = location.pathname + location.search;
  if (u === last) return;
  last = u;
  push({ e: "pageview", u: u, r: document.referrer, w: screen.width });
}
["pushState", "replaceState"].forEach(function (n) {
  var o = history[n];
  if (o) history[n] = function () { var r = o.apply(this, arguments); setTimeout(pv); return r; };
});
WA("popstate", pv);
function track(n, p) {
  if (!n) return;
  var o, k = 0, pr;
  if (p && typeof p === "object") {
    pr = {};
    for (var key in p) {
      if ({}.hasOwnProperty.call(p, key) && k < 16 && p[key] != null) {
        var v = p[key], t = typeof v;
        pr[clip(key, 64)] = t === "number" || t === "boolean" ? v : clip(v, 256);
        k++;
      }
    }
    o = pr;
  }
  push({ e: "custom", n: clip(n, 128), u: pg(), p: o });
}
window.cl.track = track;
document.addEventListener("click", function (ev) {
  var el = ev.target;
  while (el && el !== document && el.tagName !== "A") el = el.parentNode;
  if (!el || el === document) return;
  var href = el.getAttribute("href");
  if (!href || href[0] === "#" || href.indexOf("javascript:") === 0) return;
  var a = document.createElement("a");
  a.href = href;
  var path = a.pathname + (a.search || "");
  if (a.host && a.host !== location.host && a.protocol.indexOf("http") === 0)
    push({ e: "outbound", u: path, n: a.host });
  else if (/\.(zip|tar|t?gz|dmg|exe|msi|apk|pdf|doc|xls|ppt|csv|mp[34]|webm)$/i.test(path))
    push({ e: "download", u: path });
}, false);
function accrue() { if (vis()) act += D() - at; at = D(); }
function engage(fin) {
  if (fin) accrue();
  if (act < 1000) return;
  var ev = { e: "engagement", u: pg(), v: act, sd: smx || undefined };
  act = 0;
  if (fin) { Q.push(ev); flush(); } else push(ev);
}
setInterval(function () { if (vis()) { accrue(); engage(false); } }, 30000);
document.addEventListener("visibilitychange", function () {
  accrue();
  if (!vis()) engage(true);
});
WA("focus", function () { at = D(); });
WA("blur", accrue);
WA("scroll", function () {
  var h = document.documentElement, tot = h.scrollHeight - h.clientHeight;
  if (tot > 0) {
    var pct = Math.round((h.scrollTop / tot) * 100);
    if (pct > smx) smx = Math.min(100, pct);
  }
});
WA("pagehide", function () { engage(true); });
WA("beforeunload", function () { engage(true); });
function vital(n, v) {
  if (typeof v === "number" && isFinite(v)) push({ e: "vital", n: n, u: pg(), v: Math.round(v * 100) / 100 });
}
function vitals() {
  try {
    new PO(function (l) {
      var e = l.getEntries(), f = e[e.length - 1];
      if (f) vital("LCP", f.startTime);
    }).observe({ type: "largest-contentful-paint", buffered: true });
  } catch (e) {}
  try {
    var cls = 0;
    new PO(function (l) {
      var e = l.getEntries(), i;
      for (i = 0; i < e.length; i++) if (!e[i].hadRecentInput) cls += e[i].value;
    }).observe({ type: "layout-shift", buffered: true });
    var sc = function () { vital("CLS", cls); };
    document.addEventListener("visibilitychange", function () { if (!vis()) sc(); }, { once: true });
    WA("pagehide", sc, { once: true });
  } catch (e) {}
  try {
    new PO(function (l) {
      var e = l.getEntries(), w = 0, i;
      for (i = 0; i < e.length; i++) if (e[i].duration > w) w = e[i].duration;
      if (w > 0) vital("INP", w);
    }).observe({ type: "event", buffered: true, durationThreshold: 40 });
  } catch (e) {}
}
function cm(m) { return String(m == null ? "unknown" : m).split("?")[0].slice(0, 200); }
window.onerror = function (m) { push({ e: "error", n: cm(m), u: pg() }); };
WA("unhandledrejection", function (ev) {
  var r = ev.reason;
  push({ e: "error", n: "unhandledrejection: " + cm(r && r.message ? r.message : r), u: pg() });
});
function boot() {
  pv();
  vitals();
  var q = window.cl.q, i;
  if (q && q.length) { window.cl.q = []; for (i = 0; i < q.length; i++) track(q[i][0], q[i][1]); }
}
if (document.readyState === "complete") boot();
else WA("load", boot);
})();
