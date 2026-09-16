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
import "./index.css";

// Theme/font are applied before paint by the inline script in index.html;
// this re-applies the persisted selections (idempotent) and covers any
// entry path that skips it.
initTheme();

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
