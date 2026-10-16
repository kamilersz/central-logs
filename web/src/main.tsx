import React from "react";
import ReactDOM from "react-dom/client";
// Self-hosted webfonts (@fontsource, bundled by Vite + rust-embed — no CDN,
// works fully offline). Each import registers @font-face rules; the Variable
// packages expose the "<Name> Variable" family across all weights.
import "@fontsource-variable/inter";
import "@fontsource-variable/jetbrains-mono";
import "@fontsource-variable/roboto";
import "@fontsource-variable/ibm-plex-sans";
import "@fontsource-variable/space-grotesk";
import "@fontsource-variable/fira-code";
import "@fontsource-variable/cascadia-code";
import "@fontsource/ibm-plex-mono/400.css";
import "@fontsource/ibm-plex-mono/600.css";
import App from "./App";
import { initTheme } from "./theme";
import { api } from "./api";
import "./index.css";

// Theme/font are applied before paint by the inline script in index.html;
// this re-applies the persisted selections (idempotent) and covers any
// entry path that skips it.
initTheme();

// Self-tracking (dogfood): every dashboard beacons its own usage to the
// same-origin collector when the server has web analytics enabled with a
// `central-logs` project. Same-origin means no proxy config, no CORS, and
// the Origin header always matches an allowed origin of the deployment.
// Silent no-op when disabled/unauthenticated — never blocks rendering.
api
  .webSites()
  .then((r) => {
    if (!r.enabled || !r.sites.some((s) => s.id === "central-logs")) return;
    if (document.querySelector('script[data-cl-self]')) return;
    const el = document.createElement("script");
    el.src = "/v1/web/t.js";
    el.defer = true;
    el.setAttribute("data-cl-self", "1");
    el.setAttribute("data-site", "central-logs");
    el.setAttribute("data-endpoint", "/v1/web/collect");
    document.head.appendChild(el);
  })
  .catch(() => {
    /* collector off or not logged in — nothing to track with */
  });

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
