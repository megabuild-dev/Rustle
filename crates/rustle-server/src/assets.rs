//! Embedded dashboard assets: precompressed (`.br`) files are served when the client accepts
//! brotli; unknown non-API paths fall back to `index.html` (single-page app).

use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../ui-dist/"]
struct Assets;

fn mime(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn accepts(headers: &HeaderMap, enc: &str) -> bool {
    headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|p| p.trim().split(';').next().map(str::trim) == Some(enc)))
}

pub async fn serve(uri: Uri, headers: HeaderMap) -> Response {
    let raw = uri.path().trim_start_matches('/');
    if raw.starts_with("api/") || raw == "api" {
        return (StatusCode::NOT_FOUND, axum::Json(rustle_types::ApiError {
            error: rustle_types::ApiErrorBody { code: "not_found".into(), message: "no such endpoint".into() },
        }))
        .into_response();
    }
    let mut path = if raw.is_empty() { "index.html".to_string() } else { raw.to_string() };
    if path.contains("..") || path.contains('\\') {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let mut file = Assets::get(&path);
    if file.is_none() && !path.rsplit('/').next().unwrap_or("").contains('.') {
        path = "index.html".into();
        file = Assets::get(&path);
    }
    let Some(plain) = file else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut builder = Response::builder().header(header::CONTENT_TYPE, mime(&path)).header(header::VARY, "accept-encoding");
    builder = if path == "index.html" {
        builder.header(header::CACHE_CONTROL, "no-cache")
    } else {
        builder.header(header::CACHE_CONTROL, "public, max-age=3600")
    };
    let body = if accepts(&headers, "br") {
        match Assets::get(&format!("{path}.br")) {
            Some(br) => {
                builder = builder.header(header::CONTENT_ENCODING, "br");
                br.data.into_owned()
            }
            None => plain.data.into_owned(),
        }
    } else {
        plain.data.into_owned()
    };
    builder.body(Body::from(body)).unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_types() {
        assert_eq!(mime("a.wasm"), "application/wasm");
        assert_eq!(mime("x/y.js"), "text/javascript; charset=utf-8");
        assert_eq!(mime("noext"), "application/octet-stream");
    }

    #[test]
    fn accept_encoding_parse() {
        let mut h = HeaderMap::new();
        h.insert(header::ACCEPT_ENCODING, "gzip, br;q=1.0".parse().unwrap());
        assert!(accepts(&h, "br") && accepts(&h, "gzip") && !accepts(&h, "zstd"));
        assert!(!accepts(&HeaderMap::new(), "br"));
    }

    #[tokio::test]
    async fn index_served_and_spa_fallback_and_traversal_blocked() {
        let r = serve("/".parse().unwrap(), HeaderMap::new()).await;
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()[header::CACHE_CONTROL], "no-cache");
        let r = serve("/sites/42".parse().unwrap(), HeaderMap::new()).await;
        assert_eq!(r.status(), 200, "client-side route falls back to the app shell");
        let r = serve("/missing.js".parse().unwrap(), HeaderMap::new()).await;
        assert_eq!(r.status(), 404, "missing files with an extension are real 404s");
        let r = serve("/a/..%2f..".parse().unwrap(), HeaderMap::new()).await;
        assert_ne!(r.status(), 500);
        let r = serve("/api/v1/nope".parse().unwrap(), HeaderMap::new()).await;
        assert_eq!(r.status(), 404);
    }
}
