//! Embedded static assets (C13: single binary).
//!
//! `index.html`, `app.js`, `app.css` and the self-hosted Bort fonts are
//! compiled into the binary via rust-embed. The design language (tokens,
//! box recipe, fonts) is a hand-ported snapshot of the owner's blog — see
//! arc42 Appendix E; the `Bort/` folder itself is never imported (C17).

use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "assets/"]
struct Assets;

/// Serve embedded files; unknown paths are a plain 404. Path traversal is
/// not a concern: names are looked up in a compile-time file table.
pub async fn static_handler(uri: Uri) -> Response {
    let path = uri.path();
    let path = path.trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match Assets::get(path) {
        Some(file) => {
            let mime = mime_for(path);
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, mime),
                    // Static assets are cacheable; API responses never are.
                    (header::CACHE_CONTROL, "public, max-age=3600"),
                ],
                file.data.into_owned(),
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or_default() {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "otf" => "font/otf",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "json" => "application/json",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::util::ServiceExt;

    async fn get(path: &str) -> Response {
        let app = axum::Router::new().fallback(static_handler);
        let request = Request::builder().uri(path).body(Body::empty()).unwrap();
        app.oneshot(request).await.unwrap()
    }

    #[tokio::test]
    async fn root_serves_the_chat_shell() {
        let response = get("/").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("Kaeru"));
        assert!(html.contains("/app.js"));
    }

    #[tokio::test]
    async fn assets_have_content_types() {
        let css = get("/app.css").await;
        assert!(
            css.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/css")
        );
        let js = get("/app.js").await;
        assert!(
            js.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/javascript")
        );
        let font = get("/fonts/NationalPark-Regular.otf").await;
        assert_eq!(font.headers()["content-type"], "font/otf");
        assert_eq!(font.headers()["cache-control"], "public, max-age=3600");
    }

    #[tokio::test]
    async fn unknown_paths_are_404() {
        assert_eq!(get("/nope").await.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            get("/fonts/missing.woff").await.status(),
            StatusCode::NOT_FOUND
        );
    }
}
