//! Askama templates for the dashboard. Path relative to `templates/` directory.

use askama::Template;

#[derive(Template)]
#[template(path = "dashboard.html")]
pub struct DashboardTemplate {
    pub refresh_secs: u32,
    pub generated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Template)]
#[template(path = "metrics.txt")]
pub struct MetricsApiTemplate {
    pub generated_at: chrono::DateTime<chrono::Utc>,
}

/// Server-rendered sign-in page. Public (no auth required to load it).
#[derive(Template)]
#[template(path = "login.html")]
pub struct LoginTemplate {
    /// When non-empty, rendered in the error banner.
    pub error: String,
}
