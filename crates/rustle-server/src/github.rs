//! Minimal GitHub REST client (Contents API for backups, Releases for updates).

use crate::httpc::{self, Req, Target};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GhError {
    #[error("github unreachable: {0}")]
    Unreachable(String),
    #[error("github api {status}: {message}")]
    Api { status: u16, message: String },
    #[error("unexpected response: {0}")]
    Bad(String),
}

pub type GhResult<T> = Result<T, GhError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhEntry {
    pub name: String,
    pub path: String,
    pub sha: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReleaseInfo {
    pub tag: String,
    /// (asset name, download url)
    pub assets: Vec<(String, String)>,
}

#[async_trait]
pub trait Github: Send + Sync {
    async fn repo_private(&self, token: &str, repo: &str) -> GhResult<bool>;
    async fn put_file(
        &self,
        token: &str,
        repo: &str,
        branch: &str,
        path: &str,
        content: &[u8],
        message: &str,
    ) -> GhResult<()>;
    async fn list_dir(
        &self,
        token: &str,
        repo: &str,
        branch: &str,
        dir: &str,
    ) -> GhResult<Vec<GhEntry>>;
    async fn delete_file(
        &self,
        token: &str,
        repo: &str,
        branch: &str,
        path: &str,
        sha: &str,
        message: &str,
    ) -> GhResult<()>;
    async fn latest_release(&self, repo: &str) -> GhResult<ReleaseInfo>;
    async fn download(&self, url: &str, max_bytes: usize) -> GhResult<Vec<u8>>;
}

pub struct HttpGithub {
    api: String,
}

impl HttpGithub {
    pub fn new(api: String) -> Self {
        Self { api }
    }

    async fn call(
        &self,
        method: hyper::Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> GhResult<httpc::Resp> {
        let (target, base) =
            Target::from_url(&self.api).map_err(|e| GhError::Unreachable(e.to_string()))?;
        let mut req = Req::new(method, format!("{base}{path}"))
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28");
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        httpc::send(&target, &req, Duration::from_secs(30))
            .await
            .map_err(|e| GhError::Unreachable(e.to_string()))
    }

    fn check(r: httpc::Resp) -> GhResult<httpc::Resp> {
        if r.ok() {
            return Ok(r);
        }
        let message = serde_json::from_slice::<Value>(&r.body)
            .ok()
            .and_then(|v| v["message"].as_str().map(String::from))
            .unwrap_or_else(|| r.text().chars().take(200).collect());
        Err(GhError::Api {
            status: r.status,
            message,
        })
    }
}

/// Release assets redirect to GitHub's own CDN over https; nothing else is followed.
fn redirect_allowed(url: &str, allow_plain: bool) -> bool {
    if allow_plain && url.starts_with("http://") {
        return true;
    }
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', ':']).next().unwrap_or("");
    host == "github.com"
        || host.ends_with(".github.com")
        || host.ends_with(".githubusercontent.com")
}

fn enc_path(p: &str) -> String {
    p.split('/')
        .map(|s| urlencoding::encode(s).into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

#[async_trait]
impl Github for HttpGithub {
    async fn repo_private(&self, token: &str, repo: &str) -> GhResult<bool> {
        let r = Self::check(
            self.call(
                hyper::Method::GET,
                &format!("/repos/{repo}"),
                Some(token),
                None,
            )
            .await?,
        )?;
        let v: Value = r.json().map_err(|e| GhError::Bad(e.to_string()))?;
        Ok(v["private"].as_bool().unwrap_or(false))
    }

    async fn put_file(
        &self,
        token: &str,
        repo: &str,
        branch: &str,
        path: &str,
        content: &[u8],
        message: &str,
    ) -> GhResult<()> {
        let body = json!({
            "message": message,
            "content": base64::engine::general_purpose::STANDARD.encode(content),
            "branch": branch,
        });
        Self::check(
            self.call(
                hyper::Method::PUT,
                &format!("/repos/{repo}/contents/{}", enc_path(path)),
                Some(token),
                Some(body),
            )
            .await?,
        )?;
        Ok(())
    }

    async fn list_dir(
        &self,
        token: &str,
        repo: &str,
        branch: &str,
        dir: &str,
    ) -> GhResult<Vec<GhEntry>> {
        let path = format!(
            "/repos/{repo}/contents/{}?ref={}",
            enc_path(dir),
            urlencoding::encode(branch)
        );
        let r = self
            .call(hyper::Method::GET, &path, Some(token), None)
            .await?;
        if r.status == 404 {
            return Ok(vec![]);
        }
        let r = Self::check(r)?;
        let v: Vec<Value> = r.json().map_err(|e| GhError::Bad(e.to_string()))?;
        Ok(v.iter()
            .filter(|e| e["type"] == "file")
            .map(|e| GhEntry {
                name: e["name"].as_str().unwrap_or("").into(),
                path: e["path"].as_str().unwrap_or("").into(),
                sha: e["sha"].as_str().unwrap_or("").into(),
            })
            .collect())
    }

    async fn delete_file(
        &self,
        token: &str,
        repo: &str,
        branch: &str,
        path: &str,
        sha: &str,
        message: &str,
    ) -> GhResult<()> {
        let body = json!({"message": message, "sha": sha, "branch": branch});
        Self::check(
            self.call(
                hyper::Method::DELETE,
                &format!("/repos/{repo}/contents/{}", enc_path(path)),
                Some(token),
                Some(body),
            )
            .await?,
        )?;
        Ok(())
    }

    async fn latest_release(&self, repo: &str) -> GhResult<ReleaseInfo> {
        let r = Self::check(
            self.call(
                hyper::Method::GET,
                &format!("/repos/{repo}/releases/latest"),
                None,
                None,
            )
            .await?,
        )?;
        let v: Value = r.json().map_err(|e| GhError::Bad(e.to_string()))?;
        Ok(ReleaseInfo {
            tag: v["tag_name"].as_str().unwrap_or("").to_string(),
            assets: v["assets"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| {
                            Some((
                                x["name"].as_str()?.to_string(),
                                x["browser_download_url"].as_str()?.to_string(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    async fn download(&self, url: &str, max_bytes: usize) -> GhResult<Vec<u8>> {
        let mut url = url.to_string();
        // Plain http is only ever used against local mock servers in tests.
        let allow_plain = url.starts_with("http://");
        for _ in 0..5 {
            let (target, path) =
                Target::from_url(&url).map_err(|e| GhError::Unreachable(e.to_string()))?;
            let r = httpc::send(&target, &Req::get(path), Duration::from_secs(120))
                .await
                .map_err(|e| GhError::Unreachable(e.to_string()))?;
            if (300..400).contains(&r.status) {
                let loc = r
                    .header("location")
                    .ok_or_else(|| GhError::Bad("redirect without location".into()))?;
                url = resolve_location(&url, loc);
                if !redirect_allowed(&url, allow_plain) {
                    return Err(GhError::Bad(
                        "refusing to follow a redirect to an untrusted location".into(),
                    ));
                }
                continue;
            }
            let r = Self::check(r)?;
            if r.body.len() > max_bytes {
                return Err(GhError::Bad("download too large".into()));
            }
            return Ok(r.body.to_vec());
        }
        Err(GhError::Bad("too many redirects".into()))
    }
}

/// A `Location` header relative to the URL that produced it (absolute URLs pass through).
fn resolve_location(current: &str, loc: &str) -> String {
    if !loc.starts_with('/') {
        return loc.to_string();
    }
    let scheme_end = current.find("://").map(|i| i + 3).unwrap_or(0);
    let host_end = current[scheme_end..]
        .find('/')
        .map(|i| i + scheme_end)
        .unwrap_or(current.len());
    format!("{}{}", &current[..host_end], loc)
}

#[derive(Default)]
struct FakeGhState {
    private: bool,
    /// Tokens presented to `repo_private`, oldest first.
    tokens: Vec<String>,
    files: BTreeMap<String, Vec<u8>>,
    release: ReleaseInfo,
    downloads: BTreeMap<String, Vec<u8>>,
    fail: Option<String>,
    puts: Vec<String>,
}

pub struct FakeGithub {
    st: Mutex<FakeGhState>,
}

impl Default for FakeGithub {
    fn default() -> Self {
        Self {
            st: Mutex::new(FakeGhState {
                private: true,
                ..Default::default()
            }),
        }
    }
}

impl FakeGithub {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set_private(&self, p: bool) {
        self.st.lock().unwrap().private = p;
    }
    pub fn tokens_seen(&self) -> Vec<String> {
        self.st.lock().unwrap().tokens.clone()
    }
    pub fn set_fail(&self, m: Option<&str>) {
        self.st.lock().unwrap().fail = m.map(String::from);
    }
    pub fn files(&self) -> BTreeMap<String, Vec<u8>> {
        self.st.lock().unwrap().files.clone()
    }
    pub fn set_release(&self, tag: &str, assets: &[(&str, &[u8])]) {
        let mut s = self.st.lock().unwrap();
        s.release.tag = tag.into();
        s.release.assets.clear();
        for (n, data) in assets {
            let url = format!("https://example.invalid/{tag}/{n}");
            s.release.assets.push((n.to_string(), url.clone()));
            s.downloads.insert(url, data.to_vec());
        }
    }
    fn chk(&self) -> GhResult<std::sync::MutexGuard<'_, FakeGhState>> {
        let s = self.st.lock().unwrap();
        match &s.fail {
            Some(m) => Err(GhError::Unreachable(m.clone())),
            None => Ok(s),
        }
    }
}

#[async_trait]
impl Github for FakeGithub {
    async fn repo_private(&self, t: &str, _r: &str) -> GhResult<bool> {
        let mut s = self.chk()?;
        s.tokens.push(t.to_string());
        Ok(s.private)
    }
    async fn put_file(
        &self,
        _t: &str,
        _r: &str,
        _b: &str,
        path: &str,
        content: &[u8],
        _m: &str,
    ) -> GhResult<()> {
        let mut s = self.chk()?;
        s.puts.push(path.to_string());
        s.files.insert(path.to_string(), content.to_vec());
        Ok(())
    }
    async fn list_dir(&self, _t: &str, _r: &str, _b: &str, dir: &str) -> GhResult<Vec<GhEntry>> {
        let s = self.chk()?;
        Ok(s.files
            .keys()
            .filter(|p| p.starts_with(&format!("{dir}/")))
            .map(|p| GhEntry {
                name: p.rsplit('/').next().unwrap().into(),
                path: p.clone(),
                sha: format!("sha-{p}"),
            })
            .collect())
    }
    async fn delete_file(
        &self,
        _t: &str,
        _r: &str,
        _b: &str,
        path: &str,
        sha: &str,
        _m: &str,
    ) -> GhResult<()> {
        let mut s = self.chk()?;
        if sha != format!("sha-{path}") {
            return Err(GhError::Api {
                status: 409,
                message: "sha mismatch".into(),
            });
        }
        s.files.remove(path);
        Ok(())
    }
    async fn latest_release(&self, _r: &str) -> GhResult<ReleaseInfo> {
        Ok(self.chk()?.release.clone())
    }
    async fn download(&self, url: &str, max: usize) -> GhResult<Vec<u8>> {
        let s = self.chk()?;
        let d = s.downloads.get(url).cloned().ok_or(GhError::Api {
            status: 404,
            message: "no such asset".into(),
        })?;
        if d.len() > max {
            return Err(GhError::Bad("download too large".into()));
        }
        Ok(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::MockHttp;

    #[test]
    fn redirect_resolution_and_allow_list() {
        assert_eq!(
            resolve_location("https://github.com/o/r/x", "/y/z"),
            "https://github.com/y/z"
        );
        assert_eq!(
            resolve_location("http://127.0.0.1:80", "/y"),
            "http://127.0.0.1:80/y"
        );
        assert_eq!(
            resolve_location("https://a.com/p", "https://b.com/q"),
            "https://b.com/q"
        );
        assert_eq!(resolve_location("noscheme/p", "/y"), "noscheme/y");
        assert!(redirect_allowed("https://github.com/x", false));
        assert!(redirect_allowed(
            "https://objects.githubusercontent.com/x",
            false
        ));
        assert!(redirect_allowed("https://api.github.com:443/x", false));
        assert!(!redirect_allowed("https://evilgithub.com/x", false));
        assert!(!redirect_allowed("https://github.com.evil.io/x", false));
        assert!(
            !redirect_allowed("http://github.com/x", false),
            "no downgrade"
        );
        assert!(!redirect_allowed("http://127.0.0.1/x", false));
        assert!(
            redirect_allowed("http://127.0.0.1/x", true),
            "tests against a local mock"
        );
        assert!(!redirect_allowed("ftp://github.com/x", true));
    }

    #[tokio::test]
    async fn download_follows_safe_redirects_and_enforces_limits() {
        let m = MockHttp::start(|r| match r.path.as_str() {
            "/a" => (302, vec![("location".into(), "/b".into())], String::new()),
            "/b" => (200, vec![], "payload".into()),
            "/big" => (200, vec![], "x".repeat(100)),
            "/loop" => (
                302,
                vec![("location".into(), "/loop".into())],
                String::new(),
            ),
            "/nolocation" => (302, vec![], String::new()),
            "/evil" => (
                301,
                vec![("location".into(), "https://evil.example/x".into())],
                String::new(),
            ),
            "/missing" => (404, vec![], "{\"message\":\"Not Found\"}".into()),
            _ => (500, vec![], String::new()),
        })
        .await;
        let gh = HttpGithub::new(format!("http://{}", m.addr));
        let u = |p: &str| format!("http://{}{}", m.addr, p);
        assert_eq!(gh.download(&u("/a"), 1000).await.unwrap(), b"payload");
        assert_eq!(
            gh.download(&u("/big"), 100).await.unwrap().len(),
            100,
            "exactly the limit is fine"
        );
        assert!(gh
            .download(&u("/big"), 99)
            .await
            .unwrap_err()
            .to_string()
            .contains("too large"));
        assert!(gh
            .download(&u("/loop"), 1000)
            .await
            .unwrap_err()
            .to_string()
            .contains("too many redirects"));
        assert!(gh
            .download(&u("/nolocation"), 1000)
            .await
            .unwrap_err()
            .to_string()
            .contains("without location"));
        assert!(gh
            .download(&u("/evil"), 1000)
            .await
            .unwrap_err()
            .to_string()
            .contains("untrusted"));
        assert!(matches!(
            gh.download(&u("/missing"), 1000).await,
            Err(GhError::Api { status: 404, .. })
        ));
        assert!(matches!(
            gh.download("http://127.0.0.1:1/x", 10).await,
            Err(GhError::Unreachable(_))
        ));
        assert!(matches!(
            gh.download("not a url", 10).await,
            Err(GhError::Unreachable(_))
        ));
    }

    #[tokio::test]
    async fn put_and_list_and_delete_shapes() {
        let m = MockHttp::start(|r| match r.method.as_str() {
            "GET" if r.path.starts_with("/repos/o/r/contents/backups?") => (
                200,
                vec![],
                json!([{"type":"file","name":"a.enc","path":"backups/a.enc","sha":"s1"},{"type":"dir","name":"x","path":"backups/x","sha":"s2"}]).to_string(),
            ),
            "GET" => (200, vec![], json!({"private": true}).to_string()),
            _ => (200, vec![], "{}".into()),
        })
        .await;
        let gh = HttpGithub::new(format!("http://{}", m.addr));
        assert!(gh.repo_private("tok", "o/r").await.unwrap());
        gh.put_file("tok", "o/r", "main", "backups/a b.enc", b"data", "msg")
            .await
            .unwrap();
        let put = m
            .requests()
            .into_iter()
            .find(|r| r.method == "PUT")
            .unwrap();
        assert_eq!(put.path, "/repos/o/r/contents/backups/a%20b.enc");
        assert_eq!(put.header("authorization").as_deref(), Some("Bearer tok"));
        let b = put.json();
        assert_eq!(b["branch"], "main");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(b["content"].as_str().unwrap())
                .unwrap(),
            b"data"
        );
        let l = gh.list_dir("tok", "o/r", "main", "backups").await.unwrap();
        assert_eq!(
            l,
            vec![GhEntry {
                name: "a.enc".into(),
                path: "backups/a.enc".into(),
                sha: "s1".into()
            }]
        );
        gh.delete_file("tok", "o/r", "main", "backups/a.enc", "s1", "rm")
            .await
            .unwrap();
        let del = m
            .requests()
            .into_iter()
            .find(|r| r.method == "DELETE")
            .unwrap();
        assert_eq!(del.json()["sha"], "s1");
    }

    #[test]
    fn redirects_only_to_github_over_https() {
        assert!(redirect_allowed(
            "https://objects.githubusercontent.com/x",
            false
        ));
        assert!(redirect_allowed(
            "https://github.com/o/r/releases/download/v1/a",
            false
        ));
        assert!(!redirect_allowed("http://github.com/x", false));
        assert!(!redirect_allowed("https://evil.example.com/x", false));
        assert!(!redirect_allowed("https://github.com.evil.com/x", false));
        assert!(!redirect_allowed("https://evilgithub.com/x", false));
        assert!(redirect_allowed("http://127.0.0.1:1/x", true));
    }

    #[tokio::test]
    async fn api_errors_and_missing_dir() {
        let m = MockHttp::start(|r| {
            if r.path.contains("contents") && r.method == "GET" {
                (404, vec![], "{\"message\":\"Not Found\"}".into())
            } else {
                (422, vec![], "{\"message\":\"sha wasn't supplied\"}".into())
            }
        })
        .await;
        let gh = HttpGithub::new(format!("http://{}", m.addr));
        assert!(gh
            .list_dir("t", "o/r", "main", "none")
            .await
            .unwrap()
            .is_empty());
        let e = gh
            .put_file("t", "o/r", "main", "x", b"", "m")
            .await
            .unwrap_err();
        assert_eq!(
            e,
            GhError::Api {
                status: 422,
                message: "sha wasn't supplied".into()
            }
        );
    }

    #[tokio::test]
    async fn release_and_redirected_download() {
        let m = MockHttp::start(|r| match r.path.as_str() {
            "/repos/o/r/releases/latest" => (200, vec![], json!({"tag_name":"v1.2.3","assets":[{"name":"rustle-x86_64-linux","browser_download_url":"http://HOST/dl/bin"}]}).to_string()),
            "/dl/bin" => (302, vec![("location".into(), "/real/bin".into())], String::new()),
            "/real/bin" => (200, vec![], "BINARY".into()),
            _ => (404, vec![], "{}".into()),
        })
        .await;
        let gh = HttpGithub::new(format!("http://{}", m.addr));
        let rel = gh.latest_release("o/r").await.unwrap();
        assert_eq!(rel.tag, "v1.2.3");
        assert_eq!(rel.assets[0].0, "rustle-x86_64-linux");
        let url = format!("http://{}/dl/bin", m.addr);
        assert_eq!(gh.download(&url, 100).await.unwrap(), b"BINARY");
        assert!(gh.download(&url, 3).await.is_err(), "size cap enforced");
    }

    #[test]
    fn relative_redirects_work_for_short_hosts_too() {
        assert_eq!(resolve_location("http://h/p/q", "/y"), "http://h/y");
        assert_eq!(
            resolve_location("https://a.io", "/y"),
            "https://a.io/y",
            "no path at all"
        );
        assert_eq!(
            resolve_location("http://127.0.0.1:8080/a", "/b?x=1"),
            "http://127.0.0.1:8080/b?x=1"
        );
    }

    #[tokio::test]
    async fn fake_download_enforces_its_limit_exactly() {
        let gh = FakeGithub::new();
        gh.set_release("v1", &[("a", b"12345")]);
        let url = gh.latest_release("o/r").await.unwrap().assets[0].1.clone();
        assert_eq!(gh.download(&url, 5).await.unwrap(), b"12345");
        assert!(gh.download(&url, 4).await.is_err());
        assert!(gh
            .download("https://example.invalid/none", 100)
            .await
            .is_err());
    }
}
