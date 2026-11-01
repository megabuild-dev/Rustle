//! Minimal HTTP/1.1 client on hyper. One short-lived connection per request (no pooling,
//! no background state): the cheapest option for a process that talks to Docker, Caddy and
//! GitHub only occasionally. Supports TCP, unix sockets and TLS (rustls + ring).

use crate::config::Endpoint;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};

#[derive(Debug, Clone)]
pub enum Target {
    Tcp(String),
    Unix(PathBuf),
    /// host, port. Verified against the webpki root set.
    Tls(String, u16),
}

impl From<&Endpoint> for Target {
    fn from(e: &Endpoint) -> Self {
        match e {
            Endpoint::Tcp(s) => Target::Tcp(s.clone()),
            Endpoint::Unix(p) => Target::Unix(p.clone()),
        }
    }
}

impl Target {
    /// Parse `http://host:port` or `https://host[:port]` (used for the GitHub API and tests).
    pub fn from_url(url: &str) -> Result<(Target, String), HttpError> {
        if let Some(rest) = url.strip_prefix("https://") {
            let (hostport, path) = split_path(rest);
            let (host, port) = match hostport.rsplit_once(':') {
                Some((h, p)) => (
                    h.to_string(),
                    p.parse().map_err(|_| HttpError::Url(url.into()))?,
                ),
                None => (hostport.to_string(), 443),
            };
            Ok((Target::Tls(host, port), path))
        } else if let Some(rest) = url.strip_prefix("http://") {
            let (hostport, path) = split_path(rest);
            Ok((Target::Tcp(hostport.to_string()), path))
        } else {
            Err(HttpError::Url(url.into()))
        }
    }

    fn host_header(&self) -> String {
        match self {
            Target::Tcp(a) => a.clone(),
            Target::Unix(_) => "localhost".into(),
            Target::Tls(h, _) => h.clone(),
        }
    }
}

fn split_path(rest: &str) -> (&str, String) {
    match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, String::new()),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("invalid url: {0}")]
    Url(String),
    #[error("connect: {0}")]
    Connect(String),
    #[error("http: {0}")]
    Http(String),
    #[error("timeout")]
    Timeout,
    #[error("response body too large")]
    TooLarge,
}

#[derive(Debug, Clone)]
pub struct Req {
    pub method: Method,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl Req {
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        Req {
            method,
            path: path.into(),
            headers: Vec::new(),
            body: Bytes::new(),
        }
    }
    pub fn get(path: impl Into<String>) -> Self {
        Self::new(Method::GET, path)
    }
    pub fn post(path: impl Into<String>) -> Self {
        Self::new(Method::POST, path)
    }
    pub fn header(mut self, k: &str, v: impl Into<String>) -> Self {
        self.headers.push((k.to_string(), v.into()));
        self
    }
    pub fn json<T: serde::Serialize>(mut self, v: &T) -> Self {
        self.body = Bytes::from(serde_json::to_vec(v).expect("serializable"));
        self.headers
            .push(("content-type".into(), "application/json".into()));
        self
    }
    pub fn body(mut self, b: impl Into<Bytes>) -> Self {
        self.body = b.into();
        self
    }
}

#[derive(Debug, Clone)]
pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl Resp {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, HttpError> {
        serde_json::from_slice(&self.body).map_err(|e| HttpError::Http(format!("bad json: {e}")))
    }
    pub fn header(&self, k: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    }
}

const MAX_BODY: usize = 8 * 1024 * 1024;

fn build(target: &Target, r: &Req) -> Result<Request<Full<Bytes>>, HttpError> {
    let mut b = Request::builder().method(r.method.clone()).uri(&r.path);
    b = b
        .header("host", target.host_header())
        .header("connection", "close")
        .header("user-agent", "rustle");
    for (k, v) in &r.headers {
        let name =
            HeaderName::from_bytes(k.as_bytes()).map_err(|e| HttpError::Http(e.to_string()))?;
        let val = HeaderValue::from_str(v).map_err(|e| HttpError::Http(e.to_string()))?;
        b = b.header(name, val);
    }
    b.body(Full::new(r.body.clone()))
        .map_err(|e| HttpError::Http(e.to_string()))
}

async fn handshake<S>(io: S, req: Request<Full<Bytes>>) -> Result<Response<Incoming>, HttpError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io))
        .await
        .map_err(|e| HttpError::Connect(e.to_string()))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    sender
        .send_request(req)
        .await
        .map_err(|e| HttpError::Http(e.to_string()))
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    static CFG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CFG.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cfg = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("tls versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        Arc::new(cfg)
    })
    .clone()
}

/// Send a request and return the response with a streaming body.
pub async fn send_stream(target: &Target, r: &Req) -> Result<Response<Incoming>, HttpError> {
    let req = build(target, r)?;
    match target {
        Target::Tcp(addr) => {
            let s = tokio::net::TcpStream::connect(addr)
                .await
                .map_err(|e| HttpError::Connect(format!("{addr}: {e}")))?;
            let _ = s.set_nodelay(true);
            handshake(s, req).await
        }
        Target::Unix(p) => {
            let s = tokio::net::UnixStream::connect(p)
                .await
                .map_err(|e| HttpError::Connect(format!("{}: {e}", p.display())))?;
            handshake(s, req).await
        }
        Target::Tls(host, port) => {
            let s = tokio::net::TcpStream::connect((host.as_str(), *port))
                .await
                .map_err(|e| HttpError::Connect(format!("{host}:{port}: {e}")))?;
            let name = rustls::pki_types::ServerName::try_from(host.clone())
                .map_err(|e| HttpError::Connect(e.to_string()))?;
            let tls = tokio_rustls::TlsConnector::from(tls_config())
                .connect(name, s)
                .await
                .map_err(|e| HttpError::Connect(format!("tls: {e}")))?;
            handshake(tls, req).await
        }
    }
}

/// Send a request and buffer the whole response (capped at 8 MiB).
pub async fn send(target: &Target, r: &Req, timeout: Duration) -> Result<Resp, HttpError> {
    let fut = async {
        let resp = send_stream(target, r).await?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let body = Limited::new(resp.into_body(), MAX_BODY)
            .collect()
            .await
            .map_err(|_| HttpError::TooLarge)?
            .to_bytes();
        Ok(Resp {
            status,
            headers,
            body,
        })
    };
    tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| HttpError::Timeout)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::MockHttp;

    #[tokio::test]
    async fn tcp_roundtrip_and_headers() {
        let m = MockHttp::start(|r| {
            assert_eq!(r.header("x-test").as_deref(), Some("1"));
            (
                201,
                vec![("x-reply".into(), "yes".into())],
                format!("echo:{}", r.body_text()),
            )
        })
        .await;
        let t = Target::Tcp(m.addr.clone());
        let resp = send(
            &t,
            &Req::post("/a/b?c=d").header("x-test", "1").body("hi"),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.text(), "echo:hi");
        assert_eq!(resp.header("X-Reply"), Some("yes"));
        let reqs = m.requests();
        assert_eq!(reqs[0].method, "POST");
        assert_eq!(reqs[0].path, "/a/b?c=d");
    }

    #[tokio::test]
    async fn unix_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.sock");
        let _m = MockHttp::start_unix(&p, |_| (200, vec![], "unix-ok".into())).await;
        let resp = send(&Target::Unix(p), &Req::get("/x"), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(resp.text(), "unix-ok");
    }

    #[tokio::test]
    async fn connect_error_and_timeout() {
        let e = send(
            &Target::Tcp("127.0.0.1:1".into()),
            &Req::get("/"),
            Duration::from_secs(2),
        )
        .await
        .unwrap_err();
        assert!(matches!(e, HttpError::Connect(_)));
        let m = MockHttp::start(|_| {
            std::thread::sleep(Duration::from_millis(400));
            (200, vec![], String::new())
        })
        .await;
        let e = send(
            &Target::Tcp(m.addr.clone()),
            &Req::get("/"),
            Duration::from_millis(100),
        )
        .await
        .unwrap_err();
        assert!(matches!(e, HttpError::Timeout));
    }

    #[test]
    fn url_parse() {
        let (t, p) = Target::from_url("https://api.github.com/repos/x").unwrap();
        assert!(matches!(t, Target::Tls(h, 443) if h == "api.github.com"));
        assert_eq!(p, "/repos/x");
        let (t, p) = Target::from_url("http://127.0.0.1:9").unwrap();
        assert!(matches!(t, Target::Tcp(a) if a == "127.0.0.1:9"));
        assert_eq!(p, "");
        assert!(Target::from_url("ftp://x").is_err());
    }
}
