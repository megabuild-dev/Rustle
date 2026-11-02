//! Container runtime abstraction. `HttpRuntime` speaks the Docker Engine HTTP API through the
//! socket proxy; `FakeRuntime` is the in-memory double used by tests and `dev-fakes`.
//!
//! SECURITY: `create_body` is the ONLY place a container-creation payload is built. It accepts a
//! tiny typed spec and emits a fixed-shape HostConfig, so no API caller can influence privileged
//! flags, mounts or namespaces.

use crate::httpc::{self, Req, Target};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RtError {
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("runtime unavailable: {0}")]
    Unavailable(String),
    #[error("{0}")]
    Other(String),
}

pub type RtResult<T> = Result<T, RtError>;

#[derive(Debug, Clone)]
pub struct RegistryAuth {
    pub username: String,
    pub password: String,
    pub server: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContainerSpec {
    pub name: String,
    /// `repo@sha256:…` or `repo:tag`.
    pub image: String,
    /// `KEY=value` pairs.
    pub env: Vec<String>,
    pub ram_limit_mb: u32,
    pub labels: BTreeMap<String, String>,
    pub compat_caps: bool,
    pub network: String,
    /// One-shot command (argv). `None` keeps the image default.
    pub cmd: Option<Vec<String>>,
    /// Long-running app (restart unless-stopped) vs. one-shot.
    pub long_running: bool,
}

pub const COMPAT_CAPS: [&str; 6] = [
    "CHOWN",
    "SETUID",
    "SETGID",
    "DAC_OVERRIDE",
    "FOWNER",
    "NET_BIND_SERVICE",
];

/// Build the JSON body for `POST /containers/create`.
pub fn create_body(spec: &ContainerSpec) -> Value {
    let mem = (spec.ram_limit_mb as u64) * 1024 * 1024;
    let mut host = json!({
        "Memory": mem,
        // memory + swap == memory: the container gets no swap and is OOM-killed at the limit.
        "MemorySwap": mem,
        "PidsLimit": 512,
        "Privileged": false,
        "CapDrop": ["ALL"],
        "SecurityOpt": ["no-new-privileges"],
        "NetworkMode": spec.network,
        "RestartPolicy": { "Name": if spec.long_running { "unless-stopped" } else { "no" } },
        "LogConfig": { "Type": "json-file", "Config": { "max-size": "10m", "max-file": "3" } },
    });
    if spec.compat_caps {
        host["CapAdd"] = json!(COMPAT_CAPS);
    }
    let mut body = json!({
        "Image": spec.image,
        "Env": spec.env,
        "Labels": spec.labels,
        "HostConfig": host,
    });
    if let Some(cmd) = &spec.cmd {
        body["Cmd"] = json!(cmd);
        // `[""]` clears the image ENTRYPOINT; `null` would keep it and turn the argv into its arguments.
        body["Entrypoint"] = json!([""]);
    }
    body
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub running: bool,
    pub status: String,
    pub exit_code: i64,
    pub oom_killed: bool,
    pub ip: Option<String>,
    pub image_id: String,
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContainerSummary {
    pub id: String,
    pub name: String,
    pub image_id: String,
    pub state: String,
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImageInfo {
    pub id: String,
    pub repo_digests: Vec<String>,
    pub size: u64,
}

#[async_trait]
pub trait Runtime: Send + Sync {
    async fn ping(&self) -> RtResult<()>;
    /// Pull `image` (`repo:tag` or `repo@digest`).
    async fn pull(&self, image: &str, auth: Option<&RegistryAuth>) -> RtResult<()>;
    /// Resolve the immutable reference (`repo@sha256:…`) of a locally present image.
    async fn resolve_digest(&self, image: &str, repo_key: &str) -> RtResult<String>;
    async fn create(&self, spec: &ContainerSpec) -> RtResult<String>;
    async fn start(&self, id: &str) -> RtResult<()>;
    async fn stop(&self, id: &str, timeout_secs: u32) -> RtResult<()>;
    async fn kill(&self, id: &str) -> RtResult<()>;
    async fn remove(&self, id: &str, force: bool) -> RtResult<()>;
    async fn inspect(&self, id: &str) -> RtResult<ContainerInfo>;
    async fn list_containers(&self, label: Option<&str>) -> RtResult<Vec<ContainerSummary>>;
    async fn logs(&self, id: &str, tail: u32) -> RtResult<String>;
    async fn logs_follow(&self, id: &str, tail: u32) -> RtResult<mpsc::Receiver<String>>;
    /// Block until the container exits or `timeout` elapses (then `Ok(None)`).
    async fn wait(&self, id: &str, timeout: Duration) -> RtResult<Option<i64>>;
    async fn list_images(&self) -> RtResult<Vec<ImageInfo>>;
    async fn remove_image(&self, id: &str) -> RtResult<()>;
}

// ---------------------------------------------------------------------------------------------
// HTTP implementation
// ---------------------------------------------------------------------------------------------

pub struct HttpRuntime {
    target: Target,
    network: String,
}

impl HttpRuntime {
    pub fn new(target: Target, network: String) -> Self {
        Self { target, network }
    }

    async fn call(&self, req: Req, timeout: Duration) -> RtResult<httpc::Resp> {
        httpc::send(&self.target, &req, timeout)
            .await
            .map_err(|e| RtError::Unavailable(e.to_string()))
    }

    fn map_err(resp: &httpc::Resp) -> RtError {
        let msg = serde_json::from_slice::<Value>(&resp.body)
            .ok()
            .and_then(|v| v.get("message").and_then(|m| m.as_str().map(String::from)))
            .unwrap_or_else(|| resp.text());
        match resp.status {
            404 => RtError::NotFound,
            409 => RtError::Conflict(msg),
            403 => RtError::Other(format!("forbidden by socket proxy: {msg}")),
            s => RtError::Other(format!("docker api {s}: {msg}")),
        }
    }

    async fn simple(&self, req: Req, timeout: Duration) -> RtResult<()> {
        let r = self.call(req, timeout).await?;
        // 304 = already in the requested state (start/stop).
        if r.ok() || r.status == 304 {
            Ok(())
        } else {
            Err(Self::map_err(&r))
        }
    }
}

fn q(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

fn check_pull_line(line: &[u8]) -> RtResult<()> {
    if let Ok(v) = serde_json::from_slice::<Value>(line) {
        if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
            // Credentials are never echoed by Docker, but cap the length anyway.
            return Err(RtError::Other(format!(
                "pull failed: {}",
                e.chars().take(300).collect::<String>()
            )));
        }
    }
    Ok(())
}

/// Split Docker's multiplexed log stream (8-byte frame headers) into text.
pub fn demux_logs(data: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0;
    // Heuristic for TTY containers: no frame header (first byte not 0/1/2).
    if data.first().is_some_and(|b| *b > 2) {
        return String::from_utf8_lossy(data).into_owned();
    }
    while i + 8 <= data.len() {
        let len = u32::from_be_bytes([data[i + 4], data[i + 5], data[i + 6], data[i + 7]]) as usize;
        let end = (i + 8 + len).min(data.len());
        out.push_str(&String::from_utf8_lossy(&data[i + 8..end]));
        i = end;
    }
    out
}

#[async_trait]
impl Runtime for HttpRuntime {
    async fn ping(&self) -> RtResult<()> {
        let r = self
            .call(Req::get("/_ping"), Duration::from_secs(3))
            .await?;
        if r.ok() {
            Ok(())
        } else {
            Err(Self::map_err(&r))
        }
    }

    async fn pull(&self, image: &str, auth: Option<&RegistryAuth>) -> RtResult<()> {
        let r =
            crate::imageref::ImageRef::parse(image).map_err(|e| RtError::Other(e.to_string()))?;
        let mut req = Req::post(format!(
            "/images/create?fromImage={}&tag={}",
            q(&r.repo_key()),
            q(&r.pull_tag())
        ));
        if let Some(a) = auth {
            let j =
                json!({"username": a.username, "password": a.password, "serveraddress": a.server});
            let enc = base64::engine::general_purpose::URL_SAFE.encode(j.to_string());
            req = req.header("x-registry-auth", enc);
        }
        // The body is a long stream of JSON progress lines; failures are reported in-band with
        // HTTP 200. Parse it incrementally and keep only the current partial line in memory.
        use http_body_util::BodyExt;
        let pull = async {
            let resp = httpc::send_stream(&self.target, &req)
                .await
                .map_err(|e| RtError::Unavailable(e.to_string()))?;
            if !resp.status().is_success() {
                let status = resp.status().as_u16();
                let body = resp
                    .into_body()
                    .collect()
                    .await
                    .map(|b| b.to_bytes())
                    .unwrap_or_default();
                return Err(Self::map_err(&httpc::Resp {
                    status,
                    headers: vec![],
                    body,
                }));
            }
            let mut body = resp.into_body();
            let mut line: Vec<u8> = Vec::new();
            while let Some(frame) = body.frame().await {
                let frame = frame.map_err(|e| RtError::Unavailable(e.to_string()))?;
                let Some(data) = frame.data_ref() else {
                    continue;
                };
                for &b in data.iter() {
                    if b == b'\n' {
                        check_pull_line(&line)?;
                        line.clear();
                    } else if line.len() < 64 * 1024 {
                        line.push(b);
                    }
                }
            }
            check_pull_line(&line)
        };
        tokio::time::timeout(Duration::from_secs(900), pull)
            .await
            .map_err(|_| RtError::Unavailable("image pull timed out".into()))??;
        Ok(())
    }

    async fn resolve_digest(&self, image: &str, repo_key: &str) -> RtResult<String> {
        let r = self
            .call(
                Req::get(format!("/images/{}/json", q(image))),
                Duration::from_secs(15),
            )
            .await?;
        if !r.ok() {
            return Err(Self::map_err(&r));
        }
        let v: Value = r.json().map_err(|e| RtError::Other(e.to_string()))?;
        let prefix = format!("{repo_key}@");
        if let Some(d) = v["RepoDigests"].as_array().and_then(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .find(|s| s.starts_with(&prefix))
        }) {
            return Ok(d.to_string());
        }
        // Locally built or loaded image: fall back to the immutable image id.
        v["Id"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| RtError::Other("image has no id".into()))
    }

    async fn create(&self, spec: &ContainerSpec) -> RtResult<String> {
        let mut spec = spec.clone();
        spec.network = self.network.clone();
        let req = Req::post(format!("/containers/create?name={}", q(&spec.name)))
            .json(&create_body(&spec));
        let r = self.call(req, Duration::from_secs(30)).await?;
        if !r.ok() {
            return Err(Self::map_err(&r));
        }
        let v: Value = r.json().map_err(|e| RtError::Other(e.to_string()))?;
        v["Id"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| RtError::Other("create returned no id".into()))
    }

    async fn start(&self, id: &str) -> RtResult<()> {
        self.simple(
            Req::post(format!("/containers/{}/start", q(id))),
            Duration::from_secs(30),
        )
        .await
    }

    async fn stop(&self, id: &str, timeout_secs: u32) -> RtResult<()> {
        self.simple(
            Req::post(format!("/containers/{}/stop?t={timeout_secs}", q(id))),
            Duration::from_secs(timeout_secs as u64 + 30),
        )
        .await
    }

    async fn kill(&self, id: &str) -> RtResult<()> {
        self.simple(
            Req::post(format!("/containers/{}/kill", q(id))),
            Duration::from_secs(15),
        )
        .await
    }

    async fn remove(&self, id: &str, force: bool) -> RtResult<()> {
        let r = self
            .call(
                Req::new(
                    hyper::Method::DELETE,
                    format!("/containers/{}?force={}&v=true", q(id), force),
                ),
                Duration::from_secs(30),
            )
            .await?;
        if r.ok() || r.status == 404 {
            Ok(())
        } else {
            Err(Self::map_err(&r))
        }
    }

    async fn inspect(&self, id: &str) -> RtResult<ContainerInfo> {
        let r = self
            .call(
                Req::get(format!("/containers/{}/json", q(id))),
                Duration::from_secs(15),
            )
            .await?;
        if !r.ok() {
            return Err(Self::map_err(&r));
        }
        let v: Value = r.json().map_err(|e| RtError::Other(e.to_string()))?;
        let st = &v["State"];
        let ip = v["NetworkSettings"]["Networks"][&self.network]["IPAddress"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from);
        let labels = v["Config"]["Labels"]
            .as_object()
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        Ok(ContainerInfo {
            id: v["Id"].as_str().unwrap_or(id).to_string(),
            name: v["Name"]
                .as_str()
                .unwrap_or("")
                .trim_start_matches('/')
                .to_string(),
            running: st["Running"].as_bool().unwrap_or(false),
            status: st["Status"].as_str().unwrap_or("").to_string(),
            exit_code: st["ExitCode"].as_i64().unwrap_or(0),
            oom_killed: st["OOMKilled"].as_bool().unwrap_or(false),
            ip,
            image_id: v["Image"].as_str().unwrap_or("").to_string(),
            labels,
        })
    }

    async fn list_containers(&self, label: Option<&str>) -> RtResult<Vec<ContainerSummary>> {
        let mut path = "/containers/json?all=1".to_string();
        if let Some(l) = label {
            path.push_str(&format!(
                "&filters={}",
                q(&json!({"label": [l]}).to_string())
            ));
        }
        let r = self.call(Req::get(path), Duration::from_secs(20)).await?;
        if !r.ok() {
            return Err(Self::map_err(&r));
        }
        let v: Vec<Value> = r.json().map_err(|e| RtError::Other(e.to_string()))?;
        Ok(v.iter()
            .map(|c| ContainerSummary {
                id: c["Id"].as_str().unwrap_or("").to_string(),
                name: c["Names"][0]
                    .as_str()
                    .unwrap_or("")
                    .trim_start_matches('/')
                    .to_string(),
                image_id: c["ImageID"].as_str().unwrap_or("").to_string(),
                state: c["State"].as_str().unwrap_or("").to_string(),
                labels: c["Labels"]
                    .as_object()
                    .map(|o| {
                        o.iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect())
    }

    async fn logs(&self, id: &str, tail: u32) -> RtResult<String> {
        // Log lines are tenant-controlled: read at most 256 KiB, whatever `tail` says.
        let req = Req::get(format!(
            "/containers/{}/logs?stdout=1&stderr=1&timestamps=1&tail={}",
            q(id),
            tail.min(500)
        ));
        let (r, truncated) =
            httpc::send_capped(&self.target, &req, Duration::from_secs(20), 256 * 1024)
                .await
                .map_err(|e| RtError::Unavailable(e.to_string()))?;
        if !r.ok() {
            return Err(Self::map_err(&r));
        }
        let mut text = demux_logs(&r.body);
        if truncated {
            text.push_str("\n[rustle] output truncated\n");
        }
        Ok(text)
    }

    async fn logs_follow(&self, id: &str, tail: u32) -> RtResult<mpsc::Receiver<String>> {
        use http_body_util::BodyExt;
        let req = Req::get(format!(
            "/containers/{}/logs?stdout=1&stderr=1&timestamps=1&follow=1&tail={tail}",
            q(id)
        ));
        let resp = httpc::send_stream(&self.target, &req)
            .await
            .map_err(|e| RtError::Unavailable(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(if resp.status().as_u16() == 404 {
                RtError::NotFound
            } else {
                RtError::Other("logs failed".into())
            });
        }
        let (tx, rx) = mpsc::channel::<String>(16);
        tokio::spawn(async move {
            let mut body = resp.into_body();
            let mut buf: Vec<u8> = Vec::new();
            loop {
                // Wake up when the subscriber leaves even if the container is silent, so the
                // connection to the proxy is released immediately.
                let frame = tokio::select! {
                    _ = tx.closed() => return,
                    f = body.frame() => f,
                };
                let Some(Ok(frame)) = frame else { return };
                let Some(data) = frame.data_ref() else {
                    continue;
                };
                buf.extend_from_slice(data);
                if buf.len() > 1024 * 1024 {
                    return; // a runaway producer: stop rather than grow
                }
                // Emit only complete frames.
                let mut i = 0;
                let mut text = String::new();
                while i + 8 <= buf.len() {
                    let len = u32::from_be_bytes([buf[i + 4], buf[i + 5], buf[i + 6], buf[i + 7]])
                        as usize;
                    if i + 8 + len > buf.len() {
                        break;
                    }
                    text.push_str(&String::from_utf8_lossy(&buf[i + 8..i + 8 + len]));
                    i += 8 + len;
                }
                buf.drain(..i);
                if !text.is_empty() && tx.send(text).await.is_err() {
                    return;
                }
            }
        });
        Ok(rx)
    }

    async fn wait(&self, id: &str, timeout: Duration) -> RtResult<Option<i64>> {
        let req = Req::post(format!("/containers/{}/wait?condition=not-running", q(id)));
        match httpc::send(&self.target, &req, timeout).await {
            Ok(r) if r.ok() => {
                let v: Value = r.json().map_err(|e| RtError::Other(e.to_string()))?;
                Ok(Some(v["StatusCode"].as_i64().unwrap_or(-1)))
            }
            Ok(r) => Err(Self::map_err(&r)),
            Err(httpc::HttpError::Timeout) => Ok(None),
            Err(e) => Err(RtError::Unavailable(e.to_string())),
        }
    }

    async fn list_images(&self) -> RtResult<Vec<ImageInfo>> {
        let r = self
            .call(Req::get("/images/json"), Duration::from_secs(20))
            .await?;
        if !r.ok() {
            return Err(Self::map_err(&r));
        }
        let v: Vec<Value> = r.json().map_err(|e| RtError::Other(e.to_string()))?;
        Ok(v.iter()
            .map(|i| ImageInfo {
                id: i["Id"].as_str().unwrap_or("").to_string(),
                repo_digests: i["RepoDigests"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
                size: i["Size"].as_u64().unwrap_or(0),
            })
            .collect())
    }

    async fn remove_image(&self, id: &str) -> RtResult<()> {
        let r = self
            .call(
                Req::new(
                    hyper::Method::DELETE,
                    format!("/images/{}?noprune=false", q(id)),
                ),
                Duration::from_secs(60),
            )
            .await?;
        if r.ok() || r.status == 404 {
            Ok(())
        } else {
            Err(Self::map_err(&r))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// In-memory fake
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct FakeContainer {
    pub id: String,
    pub spec: ContainerSpec,
    pub running: bool,
    pub exit_code: i64,
    pub ip: String,
}

#[derive(Default)]
struct FakeState {
    counter: u32,
    images: BTreeMap<String, ImageInfo>,
    containers: BTreeMap<String, FakeContainer>,
    calls: Vec<String>,
    fail_pull: Option<String>,
    fail_start_images: Vec<String>,
    fail_starts: u32,
    oneshot_exit: i64,
    oneshot_logs: String,
    oneshot_hang: bool,
}

#[derive(Default)]
pub struct FakeRuntime {
    st: Mutex<FakeState>,
    fixed_ip: Mutex<Option<String>>,
}

impl FakeRuntime {
    pub fn new() -> Self {
        Self::default()
    }
    /// Every container created from now on reports this address (local end-to-end tests).
    pub fn with_fixed_ip(ip: Option<String>) -> Self {
        let r = Self::default();
        *r.fixed_ip.lock().unwrap() = ip;
        r
    }
    pub fn calls(&self) -> Vec<String> {
        self.st.lock().unwrap().calls.clone()
    }
    pub fn clear_calls(&self) {
        self.st.lock().unwrap().calls.clear();
    }
    pub fn fail_pull_with(&self, msg: Option<&str>) {
        self.st.lock().unwrap().fail_pull = msg.map(String::from);
    }
    /// Containers created from images containing this substring fail to start.
    pub fn fail_start_for(&self, image_substr: &str) {
        self.st
            .lock()
            .unwrap()
            .fail_start_images
            .push(image_substr.to_string());
    }
    /// The next `n` calls to `start` fail.
    pub fn fail_next_starts(&self, n: u32) {
        self.st.lock().unwrap().fail_starts = n;
    }
    pub fn set_oneshot(&self, exit: i64, logs: &str, hang: bool) {
        let mut s = self.st.lock().unwrap();
        s.oneshot_exit = exit;
        s.oneshot_logs = logs.into();
        s.oneshot_hang = hang;
    }
    pub fn containers(&self) -> Vec<FakeContainer> {
        self.st
            .lock()
            .unwrap()
            .containers
            .values()
            .cloned()
            .collect()
    }
    pub fn add_foreign_container(&self, name: &str, labels: &[(&str, &str)]) -> String {
        let mut s = self.st.lock().unwrap();
        s.counter += 1;
        let id = format!("fake{:04}", s.counter);
        let spec = ContainerSpec {
            name: name.into(),
            image: "x".into(),
            env: vec![],
            ram_limit_mb: 64,
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            compat_caps: false,
            network: "rustle-net".into(),
            cmd: None,
            long_running: true,
        };
        s.containers.insert(
            id.clone(),
            FakeContainer {
                id: id.clone(),
                spec,
                running: true,
                exit_code: 0,
                ip: "10.0.0.250".into(),
            },
        );
        id
    }
    /// Simulate Docker handing a container a different address (restart, reboot).
    pub fn set_ip(&self, id: &str, ip: &str) {
        if let Some(c) = self.st.lock().unwrap().containers.get_mut(id) {
            c.ip = ip.into();
        }
    }
    pub fn image_ids(&self) -> Vec<String> {
        self.st.lock().unwrap().images.keys().cloned().collect()
    }
    pub fn add_image(&self, id: &str, digests: &[&str]) {
        self.st.lock().unwrap().images.insert(
            id.into(),
            ImageInfo {
                id: id.into(),
                repo_digests: digests.iter().map(|s| s.to_string()).collect(),
                size: 1000,
            },
        );
    }
    fn digest_of(image: &str) -> String {
        format!("sha256:{}", crate::crypto::sha256_hex(image.as_bytes()))
    }
}

#[async_trait]
impl Runtime for FakeRuntime {
    async fn ping(&self) -> RtResult<()> {
        Ok(())
    }

    async fn pull(&self, image: &str, _auth: Option<&RegistryAuth>) -> RtResult<()> {
        let mut s = self.st.lock().unwrap();
        s.calls.push(format!("pull {image}"));
        if let Some(m) = &s.fail_pull {
            return Err(RtError::Other(m.clone()));
        }
        let r =
            crate::imageref::ImageRef::parse(image).map_err(|e| RtError::Other(e.to_string()))?;
        let digest = r
            .digest
            .clone()
            .unwrap_or_else(|| Self::digest_of(&r.canonical()));
        let id = format!("sha256:img{}", &digest[7..19]);
        let rd = format!("{}@{}", r.repo_key(), digest);
        s.images.entry(id.clone()).or_insert(ImageInfo {
            id,
            repo_digests: vec![rd],
            size: 1000,
        });
        Ok(())
    }

    async fn resolve_digest(&self, image: &str, repo_key: &str) -> RtResult<String> {
        let s = self.st.lock().unwrap();
        let r =
            crate::imageref::ImageRef::parse(image).map_err(|e| RtError::Other(e.to_string()))?;
        let digest = r
            .digest
            .clone()
            .unwrap_or_else(|| Self::digest_of(&r.canonical()));
        let want = format!("{repo_key}@{digest}");
        if s.images.values().any(|i| i.repo_digests.contains(&want)) {
            Ok(want)
        } else {
            Err(RtError::NotFound)
        }
    }

    async fn create(&self, spec: &ContainerSpec) -> RtResult<String> {
        let mut s = self.st.lock().unwrap();
        s.calls.push(format!("create {}", spec.name));
        if s.containers.values().any(|c| c.spec.name == spec.name) {
            return Err(RtError::Conflict(format!("name {} in use", spec.name)));
        }
        // The HostConfig guard must hold for every container Rustle ever creates.
        let body = create_body(spec);
        assert_eq!(body["HostConfig"]["Privileged"], json!(false));
        s.counter += 1;
        let id = format!("fake{:04}", s.counter);
        let ip = self
            .fixed_ip
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| format!("10.0.0.{}", s.counter));
        s.containers.insert(
            id.clone(),
            FakeContainer {
                id: id.clone(),
                spec: spec.clone(),
                running: false,
                exit_code: 0,
                ip,
            },
        );
        Ok(id)
    }

    async fn start(&self, id: &str) -> RtResult<()> {
        let mut s = self.st.lock().unwrap();
        let (image, name) = {
            let c = s.containers.get(id).ok_or(RtError::NotFound)?;
            (c.spec.image.clone(), c.spec.name.clone())
        };
        s.calls.push(format!("start {name}"));
        if s.fail_starts > 0 {
            s.fail_starts -= 1;
            return Err(RtError::Other("failed to start".into()));
        }
        if s.fail_start_images
            .iter()
            .any(|f| image.contains(f.as_str()))
        {
            return Err(RtError::Other("failed to start".into()));
        }
        let one = !s.containers[id].spec.long_running;
        let exit = s.oneshot_exit;
        let hang = s.oneshot_hang;
        let c = s.containers.get_mut(id).unwrap();
        c.running = true;
        if one && !hang {
            c.running = false;
            c.exit_code = exit;
        }
        Ok(())
    }

    async fn stop(&self, id: &str, _t: u32) -> RtResult<()> {
        let mut s = self.st.lock().unwrap();
        let name = s
            .containers
            .get(id)
            .ok_or(RtError::NotFound)?
            .spec
            .name
            .clone();
        s.calls.push(format!("stop {name}"));
        s.containers.get_mut(id).unwrap().running = false;
        Ok(())
    }

    async fn kill(&self, id: &str) -> RtResult<()> {
        let mut s = self.st.lock().unwrap();
        let name = s
            .containers
            .get(id)
            .ok_or(RtError::NotFound)?
            .spec
            .name
            .clone();
        s.calls.push(format!("kill {name}"));
        let c = s.containers.get_mut(id).unwrap();
        c.running = false;
        c.exit_code = 137;
        Ok(())
    }

    async fn remove(&self, id: &str, _force: bool) -> RtResult<()> {
        let mut s = self.st.lock().unwrap();
        if let Some(c) = s.containers.remove(id) {
            s.calls.push(format!("remove {}", c.spec.name));
        }
        Ok(())
    }

    async fn inspect(&self, id: &str) -> RtResult<ContainerInfo> {
        let s = self.st.lock().unwrap();
        let c = s.containers.get(id).ok_or(RtError::NotFound)?;
        Ok(ContainerInfo {
            id: c.id.clone(),
            name: c.spec.name.clone(),
            running: c.running,
            status: if c.running {
                "running".into()
            } else {
                "exited".into()
            },
            exit_code: c.exit_code,
            oom_killed: false,
            ip: Some(c.ip.clone()),
            image_id: c.spec.image.clone(),
            labels: c.spec.labels.clone(),
        })
    }

    async fn list_containers(&self, label: Option<&str>) -> RtResult<Vec<ContainerSummary>> {
        let s = self.st.lock().unwrap();
        Ok(s.containers
            .values()
            .filter(|c| match label {
                Some(l) => c.spec.labels.contains_key(l),
                None => true,
            })
            .map(|c| ContainerSummary {
                id: c.id.clone(),
                name: c.spec.name.clone(),
                image_id: c.spec.image.clone(),
                state: if c.running {
                    "running".into()
                } else {
                    "exited".into()
                },
                labels: c.spec.labels.clone(),
            })
            .collect())
    }

    async fn logs(&self, id: &str, _tail: u32) -> RtResult<String> {
        let s = self.st.lock().unwrap();
        let c = s.containers.get(id).ok_or(RtError::NotFound)?;
        if c.spec.long_running {
            Ok(format!("[fake] {} started\n", c.spec.name))
        } else {
            Ok(s.oneshot_logs.clone())
        }
    }

    async fn logs_follow(&self, id: &str, tail: u32) -> RtResult<mpsc::Receiver<String>> {
        let first = self.logs(id, tail).await?;
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(async move {
            let _ = tx.send(first).await;
            let mut n = 0;
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                n += 1;
                if tx.send(format!("[fake] heartbeat {n}\n")).await.is_err() {
                    break;
                }
            }
        });
        Ok(rx)
    }

    async fn wait(&self, id: &str, _timeout: Duration) -> RtResult<Option<i64>> {
        let s = self.st.lock().unwrap();
        let c = s.containers.get(id).ok_or(RtError::NotFound)?;
        if c.running {
            Ok(None)
        } else {
            Ok(Some(c.exit_code))
        }
    }

    async fn list_images(&self) -> RtResult<Vec<ImageInfo>> {
        Ok(self.st.lock().unwrap().images.values().cloned().collect())
    }

    async fn remove_image(&self, id: &str) -> RtResult<()> {
        let mut s = self.st.lock().unwrap();
        s.calls.push(format!("rmi {id}"));
        s.images.remove(id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::MockHttp;

    fn spec(compat: bool, long: bool) -> ContainerSpec {
        ContainerSpec {
            name: "rustle-a-12345678".into(),
            image: "ghcr.io/o/a@sha256:abc".into(),
            env: vec!["A=1".into()],
            ram_limit_mb: 256,
            labels: BTreeMap::from([("rustle.managed".to_string(), "1".to_string())]),
            compat_caps: compat,
            network: "rustle-net".into(),
            cmd: if long {
                None
            } else {
                Some(vec!["node".into(), "job.js".into()])
            },
            long_running: long,
        }
    }

    #[test]
    fn host_config_has_no_dangerous_keys() {
        for (compat, long) in [(false, true), (true, true), (false, false)] {
            let b = create_body(&spec(compat, long));
            let h = &b["HostConfig"];
            let s = b.to_string();
            for forbidden in [
                "Binds",
                "Devices",
                "Mounts",
                "PidMode",
                "IpcMode",
                "UsernsMode",
                "UTSMode",
                "Sysctls",
                "CgroupParent",
                "Runtime",
                "Tmpfs",
                "VolumesFrom",
                "Links",
                "PortBindings",
                "PublishAllPorts",
                "ExtraHosts",
                "Dns",
            ] {
                assert!(
                    !s.contains(&format!("\"{forbidden}\"")),
                    "{forbidden} must never be emitted"
                );
            }
            assert_eq!(h["Privileged"], false);
            assert_eq!(h["NetworkMode"], "rustle-net");
            assert_eq!(h["Memory"], 256u64 * 1024 * 1024);
            assert_eq!(h["MemorySwap"], h["Memory"], "no swap for containers");
            assert_eq!(h["CapDrop"], json!(["ALL"]));
            assert_eq!(h["SecurityOpt"], json!(["no-new-privileges"]));
            assert!(
                h.get("MemorySwappiness").is_none(),
                "unsupported on cgroup v2"
            );
            assert_eq!(h["CapAdd"].is_array(), compat);
            assert_eq!(
                h["RestartPolicy"]["Name"],
                if long { "unless-stopped" } else { "no" }
            );
            assert!(h["PidsLimit"].as_i64().unwrap() > 0);
        }
        let b = create_body(&spec(false, false));
        assert_eq!(b["Cmd"], json!(["node", "job.js"]));
        assert_eq!(b["Entrypoint"], json!([""]));
    }

    #[test]
    fn demux_frames_and_tty() {
        let mut data = vec![1, 0, 0, 0, 0, 0, 0, 3];
        data.extend_from_slice(b"hi\n");
        data.extend_from_slice(&[2, 0, 0, 0, 0, 0, 0, 2]);
        data.extend_from_slice(b"e\n");
        assert_eq!(demux_logs(&data), "hi\ne\n");
        assert_eq!(demux_logs(b"plain tty"), "plain tty");
    }

    #[tokio::test]
    async fn http_runtime_request_shapes() {
        let m = MockHttp::start(|r| {
            if r.path.starts_with("/images/create") {
                (200, vec![], "{\"status\":\"Pulling\"}\n{\"error\":\"denied: nope\"}\n".into())
            } else if r.path.starts_with("/containers/create") {
                (201, vec![], "{\"Id\":\"abc123\"}".into())
            } else if r.path.contains("/json") && r.path.starts_with("/containers/abc123") {
                (200, vec![], json!({"Id":"abc123","Name":"/n","State":{"Running":true,"Status":"running","ExitCode":0,"OOMKilled":false},"NetworkSettings":{"Networks":{"rustle-net":{"IPAddress":"172.30.0.5"}}},"Image":"sha256:i","Config":{"Labels":{"rustle.site":"s1"}}}).to_string())
            } else if r.path.starts_with("/images/") && r.path.ends_with("/json") {
                (200, vec![], json!({"Id":"sha256:img","RepoDigests":["other/x@sha256:1","ghcr.io/o/a@sha256:2"]}).to_string())
            } else if r.method == "DELETE" {
                (404, vec![], "{\"message\":\"gone\"}".into())
            } else {
                (204, vec![], String::new())
            }
        })
        .await;
        let rt = HttpRuntime::new(Target::Tcp(m.addr.clone()), "rustle-net".into());
        // pull: in-band error is surfaced; credentials travel only in the header
        let auth = RegistryAuth {
            username: "u".into(),
            password: "p".into(),
            server: "ghcr.io".into(),
        };
        let e = rt.pull("ghcr.io/o/a:v1", Some(&auth)).await.unwrap_err();
        assert!(e.to_string().contains("denied"));
        let reqs = m.requests();
        assert_eq!(
            reqs[0].path,
            "/images/create?fromImage=ghcr.io%2Fo%2Fa&tag=v1"
        );
        let hdr = reqs[0].header("x-registry-auth").unwrap();
        let dec = base64::engine::general_purpose::URL_SAFE
            .decode(hdr)
            .unwrap();
        assert!(String::from_utf8(dec)
            .unwrap()
            .contains("\"serveraddress\":\"ghcr.io\""));
        assert!(!reqs[0].path.contains('p'.to_string().repeat(8).as_str()));
        // create
        let id = rt.create(&spec(false, true)).await.unwrap();
        assert_eq!(id, "abc123");
        let creq = m
            .requests()
            .into_iter()
            .find(|r| r.path.starts_with("/containers/create"))
            .unwrap();
        assert_eq!(creq.path, "/containers/create?name=rustle-a-12345678");
        assert_eq!(creq.json()["HostConfig"]["Privileged"], false);
        // inspect parses the IP on our network
        let info = rt.inspect("abc123").await.unwrap();
        assert!(info.running);
        assert_eq!(info.ip.as_deref(), Some("172.30.0.5"));
        assert_eq!(
            info.labels.get("rustle.site").map(String::as_str),
            Some("s1")
        );
        // digest resolution picks the matching repo
        assert_eq!(
            rt.resolve_digest("ghcr.io/o/a:v1", "ghcr.io/o/a")
                .await
                .unwrap(),
            "ghcr.io/o/a@sha256:2"
        );
        // start 204 ok, remove 404 tolerated
        rt.start("abc123").await.unwrap();
        rt.remove("abc123", true).await.unwrap();
    }

    fn frame(stream: u8, text: &str) -> String {
        let mut v = vec![stream, 0, 0, 0];
        v.extend_from_slice(&(text.len() as u32).to_be_bytes());
        let mut s = String::from_utf8(v).unwrap();
        s.push_str(text);
        s
    }

    #[tokio::test]
    async fn list_calls_parse_and_filter() {
        let m = MockHttp::start(|r| {
            if r.path.starts_with("/containers/json") {
                (200, vec![], json!([
                    {"Id":"c1","Names":["/rustle-a"],"ImageID":"sha256:i","State":"running","Labels":{"rustle.managed":"1","n":5}},
                    {"Id":"c2"}
                ]).to_string())
            } else if r.path == "/images/json" {
                (200, vec![], json!([
                    {"Id":"sha256:a","RepoDigests":["x@sha256:1","y@sha256:2"],"Size":123},
                    {"Id":"sha256:b"}
                ]).to_string())
            } else {
                (500, vec![], "{\"message\":\"boom\"}".into())
            }
        })
        .await;
        let rt = HttpRuntime::new(Target::Tcp(m.addr.clone()), "rustle-net".into());
        let all = rt.list_containers(None).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].name, "rustle-a");
        assert_eq!(
            (all[0].id.as_str(), all[0].state.as_str()),
            ("c1", "running")
        );
        assert_eq!(all[0].image_id, "sha256:i");
        assert_eq!(all[0].labels.len(), 1, "non-string labels are dropped");
        assert_eq!(all[0].labels["rustle.managed"], "1");
        assert_eq!((all[1].name.as_str(), all[1].state.as_str()), ("", ""));
        let reqs = m.requests();
        assert_eq!(reqs[0].path, "/containers/json?all=1");
        rt.list_containers(Some("rustle.managed=1")).await.unwrap();
        let p = m.requests()[1].path.clone();
        assert!(p.starts_with("/containers/json?all=1&filters="), "{p}");
        assert!(p.contains("rustle.managed%3D1"), "{p}");

        let imgs = rt.list_images().await.unwrap();
        assert_eq!(imgs.len(), 2);
        assert_eq!(imgs[0].repo_digests, vec!["x@sha256:1", "y@sha256:2"]);
        assert_eq!((imgs[0].size, imgs[1].size), (123, 0));
        assert!(imgs[1].repo_digests.is_empty());

        let bad = MockHttp::start(|_| (500, vec![], "{\"message\":\"boom\"}".into())).await;
        let rt = HttpRuntime::new(Target::Tcp(bad.addr.clone()), "rustle-net".into());
        assert!(rt.list_containers(None).await.is_err());
        assert!(rt.list_images().await.is_err());
        assert!(rt.logs("c", 10).await.is_err());
        assert!(rt.inspect("c").await.is_err());
        assert!(rt.resolve_digest("i", "r").await.is_err());
        assert!(rt.create(&spec(false, true)).await.is_err());
        assert!(rt.remove_image("i").await.is_err());
        assert!(rt.remove("c", false).await.is_err());
        assert!(rt.ping().await.is_err());
        assert!(rt.wait("c", Duration::from_secs(2)).await.is_err());
        assert!(rt.stop("c", 1).await.is_err());
        assert!(rt.kill("c").await.is_err());
    }

    #[tokio::test]
    async fn logs_wait_ping_and_misc() {
        let m = MockHttp::start(|r| {
            if r.path == "/_ping" {
                (200, vec![], "OK".into())
            } else if r.path.starts_with("/containers/c1/logs") {
                (
                    200,
                    vec![],
                    format!("{}{}", frame(1, "out\n"), frame(2, "err\n")),
                )
            } else if r.path.starts_with("/containers/c1/wait") {
                (200, vec![], "{\"StatusCode\":3}".into())
            } else if r.path.starts_with("/containers/slow/wait") {
                std::thread::sleep(Duration::from_millis(600));
                (200, vec![], "{\"StatusCode\":0}".into())
            } else if r.path.starts_with("/images/create") {
                (
                    200,
                    vec![],
                    "{\"status\":\"a\"}\n{\"status\":\"b\"}\n{\"status\":\"done\"}".into(),
                )
            } else if r.path.starts_with("/images/") && r.path.ends_with("/json") {
                (
                    200,
                    vec![],
                    json!({"Id":"sha256:only","RepoDigests":[]}).to_string(),
                )
            } else if r.path.starts_with("/containers/create") {
                (201, vec![], "{}".into())
            } else if r.path.starts_with("/containers/c1/json") {
                (200, vec![], json!({"State":{}}).to_string())
            } else if r.method == "DELETE" {
                (204, vec![], String::new())
            } else {
                (404, vec![], "{\"message\":\"nope\"}".into())
            }
        })
        .await;
        let rt = HttpRuntime::new(Target::Tcp(m.addr.clone()), "rustle-net".into());
        rt.ping().await.unwrap();
        assert_eq!(rt.logs("c1", 10_000).await.unwrap(), "out\nerr\n");
        assert!(
            m.requests().last().unwrap().path.ends_with("tail=500"),
            "tail is capped"
        );
        assert_eq!(
            rt.wait("c1", Duration::from_secs(2)).await.unwrap(),
            Some(3)
        );
        assert_eq!(
            rt.wait("slow", Duration::from_millis(100)).await.unwrap(),
            None,
            "timeout is not an error"
        );
        assert!(matches!(
            rt.wait("gone", Duration::from_secs(2)).await,
            Err(RtError::NotFound)
        ));
        // progress without a trailing newline is still checked
        rt.pull("ghcr.io/o/a:v1", None).await.unwrap();
        assert!(m
            .requests()
            .last()
            .unwrap()
            .header("x-registry-auth")
            .is_none());
        // no RepoDigests: fall back to the image id
        assert_eq!(
            rt.resolve_digest("i", "ghcr.io/o/a").await.unwrap(),
            "sha256:only"
        );
        // create without an id
        let e = rt.create(&spec(false, true)).await.unwrap_err();
        assert!(e.to_string().contains("no id"), "{e}");
        // inspect with an empty State
        let i = rt.inspect("c1").await.unwrap();
        assert!(!i.running && i.ip.is_none() && i.labels.is_empty() && i.exit_code == 0);
        rt.remove_image("sha256:x").await.unwrap();
        rt.remove("c1", true).await.unwrap();
        assert!(m.requests().last().unwrap().path.contains("force=true"));
        // invalid image refs never reach the daemon
        assert!(rt.pull("bad image", None).await.is_err());
        // unreachable daemon
        let dead = HttpRuntime::new(Target::Tcp("127.0.0.1:1".into()), "rustle-net".into());
        assert!(matches!(
            dead.wait("c", Duration::from_secs(1)).await,
            Err(RtError::Unavailable(_))
        ));
        assert!(matches!(
            dead.pull("ghcr.io/o/a:v1", None).await,
            Err(RtError::Unavailable(_))
        ));
        assert!(matches!(
            dead.logs("c", 1).await,
            Err(RtError::Unavailable(_))
        ));
        assert!(matches!(
            dead.logs_follow("c", 1).await,
            Err(RtError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn pull_http_errors_and_truncated_log_marker() {
        let m = MockHttp::start(|r| {
            if r.path.contains("fromImage=ghcr.io%2Fo%2Fmissing") {
                (404, vec![], "{\"message\":\"manifest unknown\"}".into())
            } else if r.path.starts_with("/containers/big/logs") {
                (200, vec![], frame(1, &"x".repeat(262_144)))
            } else {
                (200, vec![], String::new())
            }
        })
        .await;
        let rt = HttpRuntime::new(Target::Tcp(m.addr.clone()), "rustle-net".into());
        let e = rt.pull("ghcr.io/o/missing:v1", None).await.unwrap_err();
        assert!(
            e.to_string().contains("manifest unknown") || matches!(e, RtError::NotFound),
            "{e}"
        );
        let text = rt.logs("big", 100).await.unwrap();
        assert!(text.ends_with("[rustle] output truncated\n"));
        assert!(text.len() < 300 * 1024);
    }

    #[tokio::test]
    async fn follow_streams_complete_frames_and_stops() {
        let m = MockHttp::start(|r| {
            if r.path.starts_with("/containers/c1/logs") {
                (
                    200,
                    vec![],
                    format!("{}{}", frame(1, "one\n"), frame(2, "two\n")),
                )
            } else if r.path.starts_with("/containers/boom/logs") {
                (500, vec![], "x".into())
            } else {
                (404, vec![], "{\"message\":\"no\"}".into())
            }
        })
        .await;
        let rt = HttpRuntime::new(Target::Tcp(m.addr.clone()), "rustle-net".into());
        let mut rx = rt.logs_follow("c1", 5).await.unwrap();
        let mut got = String::new();
        while let Some(t) = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
        {
            got.push_str(&t);
        }
        assert_eq!(got, "one\ntwo\n");
        assert!(
            m.requests()[0].path.contains("follow=1") && m.requests()[0].path.contains("tail=5")
        );
        assert!(matches!(
            rt.logs_follow("nope", 1).await,
            Err(RtError::NotFound)
        ));
        assert!(matches!(
            rt.logs_follow("boom", 1).await,
            Err(RtError::Other(_))
        ));
        // dropping the receiver stops the pump task
        let rx = rt.logs_follow("c1", 5).await.unwrap();
        drop(rx);
    }

    #[tokio::test]
    async fn proxy_denial_is_reported_clearly() {
        let m = MockHttp::start(|_| (403, vec![], "Forbidden".into())).await;
        let rt = HttpRuntime::new(Target::Tcp(m.addr.clone()), "rustle-net".into());
        let e = rt.start("x").await.unwrap_err();
        assert!(e.to_string().contains("forbidden by socket proxy"));
    }

    #[tokio::test]
    async fn fake_roundtrip() {
        let f = FakeRuntime::new();
        f.pull("ghcr.io/o/a:1", None).await.unwrap();
        let d = f
            .resolve_digest("ghcr.io/o/a:1", "ghcr.io/o/a")
            .await
            .unwrap();
        assert!(d.starts_with("ghcr.io/o/a@sha256:"));
        let id = f.create(&spec(false, true)).await.unwrap();
        assert!(f.create(&spec(false, true)).await.is_err(), "name conflict");
        f.start(&id).await.unwrap();
        assert!(f.inspect(&id).await.unwrap().running);
        f.stop(&id, 10).await.unwrap();
        assert!(!f.inspect(&id).await.unwrap().running);
        f.remove(&id, true).await.unwrap();
        assert!(f.inspect(&id).await.is_err());
    }
}
