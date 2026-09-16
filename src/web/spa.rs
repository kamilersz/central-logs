//! Serves the embedded SPA built from `web/`.
//!
//! `web/dist` is embedded at compile time via rust-embed. If the directory
//! doesn't exist (e.g. user hasn't run `npm run build` yet), we serve a
//! placeholder HTML that explains what to do.
//!
//! To build the SPA: `cd web && npm install && npm run build`. The output goes
//! to `target/web-dist` (per `vite.config.ts`) and is picked up here.

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use rust_embed::RustEmbed;
use tracing;

#[derive(RustEmbed)]
#[folder = "target/web-dist"]
struct SpaAssets;

/// Build a router that serves SPA assets at any non-API path. The catch-all
/// fallback returns `index.html` so client-side routing works.
pub fn router() -> Router {
    Router::new()
        .route("/", any(serve_index))
        .route("/{*path}", any(serve_path))
}

async fn serve_index() -> Response {
    tracing::debug!("spa: serve_index called");
    serve_file("index.html")
}

async fn serve_path(axum::extract::Path(path): axum::extract::Path<String>) -> Response {
    tracing::debug!(%path, "spa: serve_path called");
    // If the request looks like a file (has a dot suffix), try to serve it
    // directly. Otherwise fall back to index.html for client-side routing.
    if path.contains('.') {
        let resp = serve_file(&path);
        if resp.status() != StatusCode::NOT_FOUND {
            return resp;
        }
    }
    serve_file("index.html")
}

fn serve_file(path: &str) -> Response {
    let asset = SpaAssets::get(path);
    tracing::debug!(path, found = asset.is_some(), "spa: lookup");
    match asset {
        Some(asset) => {
            let mime = mime_guess::from_path(path)
                .first_or_octet_stream()
                .to_string();
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_str(&mime).unwrap_or(HeaderValue::from_static(
                    "application/octet-stream",
                )),
            );
            // Hashed asset filenames from Vite are safe to cache aggressively.
            // index.html shouldn't be cached (it references the latest hashes).
            if path != "index.html" && path.contains('.') {
                headers.insert(
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("public, max-age=31536000, immutable"),
                );
            }
            (
                StatusCode::OK,
                headers,
                Body::from(asset.data.into_owned()),
            )
                .into_response()
        }
        None => {
            // Fallback for SPA client-side routes (any non-asset path). For
            // root, this should serve the real index.html; if it isn't
            // embedded yet, show a build hint instead of a 404.
            let _ = path;
            let html = r#"<!doctype html><html><body style="font-family:monospace;background:#0e1116;color:#e6edf3;padding:40px"><h1>central-logs</h1><p>The SPA frontend is not built yet.</p><p>To build it:</p><pre>cd web
npm install
npm run build
cargo build</pre><p>Or run in dev mode: <code>npm run dev</code> in <code>web/</code> (proxies /api to the Rust server on :18080).</p></body></html>"#;
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                Body::from(html),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_index_html_exists() {
        // The SPA build places index.html at the root of the embedded folder.
        // If this test fails, run `cd web && npm install && npm run build`
        // before `cargo build`.
        assert!(
            SpaAssets::get("index.html").is_some(),
            "target/web-dist/index.html not embedded. Did you run `npm run build` in web/?"
        );
    }

    #[test]
    fn embedded_assets_dir_has_files() {
        let names: Vec<String> = SpaAssets::iter().map(|p| p.to_string()).collect();
        let has_assets = names.iter().any(|n| n.starts_with("assets/"));
        assert!(has_assets, "no files under assets/ found in embed; names: {names:?}");
    }
}
