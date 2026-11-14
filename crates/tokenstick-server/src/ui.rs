//! Embedded web UI. Plain HTML/CSS/JS compiled into the binary, so headless
//! hosts serve it with no extra files and CI needs no Node toolchain.

use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;

use crate::AppState;

const INDEX_HTML: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.js");
const APP_CSS: &str = include_str!("../ui/app.css");
const LOGO_SVG: &str = include_str!("../ui/logo.svg");

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route("/ui/app.js", get(app_js))
        .route("/ui/app.css", get(app_css))
        .route("/ui/logo.svg", get(logo_svg))
}

fn asset(content_type: &'static str, body: &'static str) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
}

async fn index() -> impl IntoResponse {
    asset("text/html; charset=utf-8", INDEX_HTML)
}

async fn app_js() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", APP_JS)
}

async fn app_css() -> impl IntoResponse {
    asset("text/css; charset=utf-8", APP_CSS)
}

async fn logo_svg() -> impl IntoResponse {
    asset("image/svg+xml", LOGO_SVG)
}
