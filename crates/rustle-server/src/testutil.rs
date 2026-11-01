//! Test helpers: a scripted mock HTTP server (TCP or unix). Compiled unconditionally; the linker
//! drops it from release builds because nothing in the shipped binary references it.
#![allow(dead_code)]

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn header(&self, k: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.clone())
    }
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

type Handler = dyn Fn(&Recorded) -> (u16, Vec<(String, String)>, String) + Send + Sync;

pub struct MockHttp {
    pub addr: String,
    log: Arc<Mutex<Vec<Recorded>>>,
}

impl MockHttp {
    pub fn requests(&self) -> Vec<Recorded> {
        self.log.lock().unwrap().clone()
    }

    pub async fn start<F>(f: F) -> MockHttp
    where
        F: Fn(&Recorded) -> (u16, Vec<(String, String)>, String) + Send + Sync + 'static,
    {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let log = Arc::new(Mutex::new(Vec::new()));
        let h: Arc<Handler> = Arc::new(f);
        let log2 = log.clone();
        tokio::spawn(async move {
            loop {
                let Ok((s, _)) = l.accept().await else { break };
                serve(TokioIo::new(s), h.clone(), log2.clone());
            }
        });
        MockHttp { addr, log }
    }

    pub async fn start_unix<F>(path: &Path, f: F) -> MockHttp
    where
        F: Fn(&Recorded) -> (u16, Vec<(String, String)>, String) + Send + Sync + 'static,
    {
        let _ = std::fs::remove_file(path);
        let l = tokio::net::UnixListener::bind(path).unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let h: Arc<Handler> = Arc::new(f);
        let log2 = log.clone();
        tokio::spawn(async move {
            loop {
                let Ok((s, _)) = l.accept().await else { break };
                serve(TokioIo::new(s), h.clone(), log2.clone());
            }
        });
        MockHttp {
            addr: path.display().to_string(),
            log,
        }
    }
}

fn serve<I>(io: I, h: Arc<Handler>, log: Arc<Mutex<Vec<Recorded>>>)
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let svc = service_fn(move |req: Request<Incoming>| {
            let h = h.clone();
            let log = log.clone();
            async move {
                let (parts, body) = req.into_parts();
                let body = body
                    .collect()
                    .await
                    .map(|b| b.to_bytes().to_vec())
                    .unwrap_or_default();
                let rec = Recorded {
                    method: parts.method.to_string(),
                    path: parts
                        .uri
                        .path_and_query()
                        .map(|p| p.to_string())
                        .unwrap_or_default(),
                    headers: parts
                        .headers
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                        .collect(),
                    body,
                };
                log.lock().unwrap().push(rec.clone());
                let (status, headers, text) = h(&rec);
                let mut b = Response::builder().status(status);
                for (k, v) in headers {
                    b = b.header(k, v);
                }
                Ok::<_, Infallible>(b.body(Full::new(Bytes::from(text))).unwrap())
            }
        });
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(io, svc)
            .await;
    });
}
