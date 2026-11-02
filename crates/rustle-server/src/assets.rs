//! Embedded dashboard assets: precompressed (`.br`) files are served when the client accepts
//! brotli; unknown non-API paths fall back to `index.html` (single-page app).

use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

/// Shown when the binary was built without the dashboard (`scripts/build-ui.sh` not run).
const FALLBACK_INDEX: &str = "<!doctype html><meta charset=utf-8><title>Rustle</title><body style=\"font-family:system-ui;background:#0b1020;color:#e6e9f2;padding:2rem\"><h1>Rustle</h1><p>The dashboard is not built into this binary. Run <code>scripts/build-ui.sh</code> and rebuild.</p><p>The API is available under <code>/api/v1</code>.</p></body>";

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

/// The bytes to serve for `path`: the embedded file, or a built-in notice page when the dashboard
/// was not compiled into this binary.
fn page_body(path: &str, file: Option<Vec<u8>>) -> Option<Vec<u8>> {
    match file {
        Some(f) => Some(f),
        None if path == "index.html" => Some(FALLBACK_INDEX.as_bytes().to_vec()),
        None => None,
    }
}

fn accepts(headers: &HeaderMap, enc: &str) -> bool {
    headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .any(|p| p.trim().split(';').next().map(str::trim) == Some(enc))
        })
}

pub async fn serve(uri: Uri, headers: HeaderMap) -> Response {
    let raw = uri.path().trim_start_matches('/');
    if raw.starts_with("api/") || raw == "api" {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(rustle_types::ApiError {
                error: rustle_types::ApiErrorBody {
                    code: "not_found".into(),
                    message: "no such endpoint".into(),
                },
            }),
        )
            .into_response();
    }
    let mut path = if raw.is_empty() {
        "index.html".to_string()
    } else {
        raw.to_string()
    };
    if path.contains("..") || path.contains('\\') {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let mut file = Assets::get(&path);
    if file.is_none() && !path.rsplit('/').next().unwrap_or("").contains('.') {
        path = "index.html".into();
        file = Assets::get(&path);
    }
    let Some(plain) = page_body(&path, file.map(|f| f.data.into_owned())) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, mime(&path))
        .header(header::VARY, "accept-encoding");
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
            None => plain,
        }
    } else {
        plain
    };
    builder
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
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
        assert_eq!(
            r.status(),
            200,
            "client-side route falls back to the app shell"
        );
        let r = serve("/missing.js".parse().unwrap(), HeaderMap::new()).await;
        assert_eq!(
            r.status(),
            404,
            "missing files with an extension are real 404s"
        );
        let r = serve("/a/..%2f..".parse().unwrap(), HeaderMap::new()).await;
        assert_ne!(r.status(), 500);
        let r = serve("/api/v1/nope".parse().unwrap(), HeaderMap::new()).await;
        assert_eq!(r.status(), 404);
    }

    #[test]
    fn every_extension_has_its_type() {
        for (f, t) in [
            ("a.html", "text/html; charset=utf-8"),
            ("a.js", "text/javascript; charset=utf-8"),
            ("a.mjs", "text/javascript; charset=utf-8"),
            ("a.wasm", "application/wasm"),
            ("a.css", "text/css; charset=utf-8"),
            ("a.json", "application/json"),
            ("a.svg", "image/svg+xml"),
            ("a.png", "image/png"),
            ("a.ico", "image/x-icon"),
            ("a.txt", "text/plain; charset=utf-8"),
            ("a.bin", "application/octet-stream"),
        ] {
            assert_eq!(mime(f), t, "{f}");
        }
    }

    #[test]
    fn missing_dashboard_gets_the_notice_page_only_for_the_index() {
        let notice = page_body("index.html", None).unwrap();
        assert!(String::from_utf8(notice)
            .unwrap()
            .contains("not built into this binary"));
        assert!(page_body("app.js", None).is_none());
        assert_eq!(page_body("app.js", Some(b"x".to_vec())).unwrap(), b"x");
    }

    #[tokio::test]
    async fn headers_encoding_and_path_rules() {
        let get = |p: &'static str, br: bool| async move {
            let mut h = HeaderMap::new();
            if br {
                h.insert(header::ACCEPT_ENCODING, "br".parse().unwrap());
            }
            serve(p.parse().unwrap(), h).await
        };
        // "/api" itself is not the app shell
        assert_eq!(get("/api", false).await.status(), 404);
        assert_eq!(
            get("/apix", false).await.status(),
            200,
            "only api/ and api are reserved"
        );
        assert_eq!(get("/a\\b", false).await.status(), 400);
        assert_eq!(get("/x/../y", false).await.status(), 400);
        let r = get("/", false).await;
        assert_eq!(
            r.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        assert_eq!(r.headers()[header::VARY], "accept-encoding");
        assert!(r.headers().get(header::CONTENT_ENCODING).is_none());
        // brotli only when asked for and available
        let r = get("/", true).await;
        assert_eq!(r.headers()[header::CONTENT_ENCODING], "br");
        // hashed assets are cacheable
        let js = Assets::iter()
            .find(|f| f.ends_with(".js"))
            .expect("a js asset in ui-dist");
        let uri: &'static str = Box::leak(format!("/{js}").into_boxed_str());
        let r = get(uri, false).await;
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()[header::CACHE_CONTROL], "public, max-age=3600");
        assert_eq!(
            r.headers()[header::CONTENT_TYPE],
            "text/javascript; charset=utf-8"
        );
    }
}
