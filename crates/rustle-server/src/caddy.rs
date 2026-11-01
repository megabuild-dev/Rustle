//! Caddy gateway. All changes go through Caddy's Admin API (a unix socket) addressed by `@id`,
//! never a full `/load` after first boot, so one failed call cannot wipe other sites' routes.

use crate::httpc::{self, Req, Target};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GwError {
    #[error("gateway unavailable: {0}")]
    Unavailable(String),
    #[error("gateway rejected request: {0}")]
    Rejected(String),
}

pub type GwResult<T> = Result<T, GwError>;

/// Inputs that shape the base Caddy configuration and the admin route.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BaseParams {
    /// Domain that serves the dashboard. `None` => IP mode (self-signed HTTPS on the IP).
    pub admin_domain: Option<String>,
    pub acme_email: Option<String>,
    pub public_ip: Option<String>,
    /// Caddy `dial` string for Rustle, e.g. `unix//run/rustle/rustle.sock`.
    pub admin_upstream: String,
    /// Admin API listen address inside the Caddy container.
    pub admin_listen: String,
}

pub const ADMIN_LISTEN_DEFAULT: &str = "unix//run/rustle/caddy-admin.sock|0666";

fn proxy_handler(id: &str, dial: &str) -> Value {
    json!({ "handler": "reverse_proxy", "@id": id, "upstreams": [{ "dial": dial }] })
}

pub fn admin_route(p: &BaseParams) -> Value {
    let mut r = json!({
        "@id": "admin",
        "handle": [proxy_handler("up-admin", &p.admin_upstream)],
        "terminal": true
    });
    if let Some(d) = &p.admin_domain {
        r["match"] = json!([{ "host": [d] }]);
    }
    r
}

pub fn site_route(name: &str, domain: &str, dial: &str) -> Value {
    json!({
        "@id": format!("site-{name}"),
        "match": [{ "host": [domain] }],
        "handle": [proxy_handler(&format!("up-{name}"), dial)],
        "terminal": true
    })
}

/// Full base configuration. Used at install time (printed by `rustle caddy-config`) and to repair
/// a gateway that lost its `main` server.
pub fn base_config(p: &BaseParams) -> Value {
    let listen = if p.admin_listen.is_empty() { ADMIN_LISTEN_DEFAULT } else { &p.admin_listen };
    let mut server = json!({
        "listen": [":443", ":80"],
        "routes": [admin_route(p)],
    });
    let mut apps = json!({});
    let mut policies: Vec<Value> = Vec::new();
    if p.admin_domain.is_none() {
        if let Some(ip) = &p.public_ip {
            // IP mode: no public CA will issue for an IP here, so use Caddy's internal CA.
            server["tls_connection_policies"] = json!([{ "default_sni": ip }]);
            policies.push(json!({ "subjects": [ip], "issuers": [{ "module": "internal" }] }));
        }
    }
    if let Some(email) = &p.acme_email {
        policies.push(json!({ "issuers": [{ "module": "acme", "email": email }] }));
    }
    if !policies.is_empty() {
        apps["tls"] = json!({ "automation": { "policies": policies } });
    }
    apps["http"] = json!({ "servers": { "main": server } });
    json!({ "admin": { "listen": listen }, "apps": apps })
}

#[async_trait]
pub trait Gateway: Send + Sync {
    async fn ping(&self) -> GwResult<()>;
    /// Make sure the `main` server and the admin route exist; repairs them if missing.
    async fn ensure_base(&self, p: &BaseParams) -> GwResult<()>;
    /// Replace the admin route (domain change).
    async fn set_admin(&self, p: &BaseParams) -> GwResult<()>;
    async fn upsert_site(&self, name: &str, domain: &str, dial: &str) -> GwResult<()>;
    async fn set_upstream(&self, name: &str, dial: &str) -> GwResult<()>;
    async fn remove_site(&self, name: &str) -> GwResult<()>;
    /// `@id`s of site routes currently present (`site-<name>`).
    async fn site_ids(&self) -> GwResult<Vec<String>>;
}

pub struct HttpGateway {
    target: Target,
}

impl HttpGateway {
    pub fn new(target: Target) -> Self {
        Self { target }
    }

    async fn call(&self, req: Req) -> GwResult<httpc::Resp> {
        httpc::send(&self.target, &req, Duration::from_secs(10)).await.map_err(|e| GwError::Unavailable(e.to_string()))
    }

    fn check(r: &httpc::Resp) -> GwResult<()> {
        if r.ok() {
            Ok(())
        } else {
            Err(GwError::Rejected(format!("{}: {}", r.status, r.text().chars().take(300).collect::<String>())))
        }
    }

    async fn exists(&self, id: &str) -> GwResult<bool> {
        let r = self.call(Req::get(format!("/id/{id}"))).await?;
        match r.status {
            200 => Ok(true),
            404 => Ok(false),
            // Caddy answers unknown ids with 404 or 500 depending on version.
            500 if r.text().contains("unknown object ID") => Ok(false),
            _ => Err(GwError::Rejected(format!("{}: {}", r.status, r.text()))),
        }
    }
}

const ROUTES: &str = "/config/apps/http/servers/main/routes";

#[async_trait]
impl Gateway for HttpGateway {
    async fn ping(&self) -> GwResult<()> {
        let r = self.call(Req::get("/config/")).await?;
        Self::check(&r)
    }

    async fn ensure_base(&self, p: &BaseParams) -> GwResult<()> {
        let r = self.call(Req::get("/config/apps/http/servers/main")).await?;
        let have_server = r.ok() && r.text().trim() != "null";
        if !have_server {
            tracing::warn!("caddy has no main server; loading base config");
            let r = self.call(Req::post("/load").json(&base_config(p))).await?;
            return Self::check(&r);
        }
        if !self.exists("admin").await? {
            // Admin route must stay LAST so site routes (inserted at index 0) match first.
            let r = self.call(Req::post(ROUTES).json(&admin_route(p))).await?;
            return Self::check(&r);
        }
        Ok(())
    }

    async fn set_admin(&self, p: &BaseParams) -> GwResult<()> {
        let r = self.call(Req::new(hyper::Method::PATCH, "/id/admin").json(&admin_route(p))).await?;
        Self::check(&r)
    }

    async fn upsert_site(&self, name: &str, domain: &str, dial: &str) -> GwResult<()> {
        let route = site_route(name, domain, dial);
        let id = format!("site-{name}");
        let r = if self.exists(&id).await? {
            self.call(Req::new(hyper::Method::PATCH, format!("/id/{id}")).json(&route)).await?
        } else {
            // PUT on an array index inserts, putting the route before the catch-all admin route.
            self.call(Req::new(hyper::Method::PUT, format!("{ROUTES}/0")).json(&route)).await?
        };
        Self::check(&r)
    }

    async fn set_upstream(&self, name: &str, dial: &str) -> GwResult<()> {
        let r = self
            .call(Req::new(hyper::Method::PATCH, format!("/id/up-{name}/upstreams")).json(&json!([{ "dial": dial }])))
            .await?;
        Self::check(&r)
    }

    async fn remove_site(&self, name: &str) -> GwResult<()> {
        let r = self.call(Req::new(hyper::Method::DELETE, format!("/id/site-{name}"))).await?;
        if r.ok() || r.status == 404 || (r.status == 500 && r.text().contains("unknown object ID")) {
            Ok(())
        } else {
            Self::check(&r)
        }
    }

    async fn site_ids(&self) -> GwResult<Vec<String>> {
        let r = self.call(Req::get(ROUTES)).await?;
        Self::check(&r)?;
        let v: Value = r.json().map_err(|e| GwError::Rejected(e.to_string()))?;
        Ok(v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x["@id"].as_str())
                    .filter(|id| id.starts_with("site-"))
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------------------------
#[derive(Default)]
struct FakeGwState {
    base: Option<BaseParams>,
    /// name -> (domain, dial). Insertion order is irrelevant for the fake.
    sites: BTreeMap<String, (String, String)>,
    calls: Vec<String>,
    down: bool,
}

#[derive(Default)]
pub struct FakeGateway {
    st: Mutex<FakeGwState>,
}

impl FakeGateway {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set_down(&self, down: bool) {
        self.st.lock().unwrap().down = down;
    }
    pub fn calls(&self) -> Vec<String> {
        self.st.lock().unwrap().calls.clone()
    }
    pub fn clear_calls(&self) {
        self.st.lock().unwrap().calls.clear();
    }
    pub fn sites(&self) -> BTreeMap<String, (String, String)> {
        self.st.lock().unwrap().sites.clone()
    }
    pub fn base(&self) -> Option<BaseParams> {
        self.st.lock().unwrap().base.clone()
    }
    /// Simulate Caddy losing its configuration (e.g. container recreated without volume).
    pub fn wipe(&self) {
        let mut s = self.st.lock().unwrap();
        s.sites.clear();
        s.base = None;
    }
    fn guard(&self, call: String) -> GwResult<std::sync::MutexGuard<'_, FakeGwState>> {
        let mut s = self.st.lock().unwrap();
        s.calls.push(call);
        if s.down {
            return Err(GwError::Unavailable("down".into()));
        }
        Ok(s)
    }
}

#[async_trait]
impl Gateway for FakeGateway {
    async fn ping(&self) -> GwResult<()> {
        self.guard("ping".into()).map(|_| ())
    }
    async fn ensure_base(&self, p: &BaseParams) -> GwResult<()> {
        let mut s = self.guard("ensure_base".into())?;
        if s.base.is_none() {
            s.base = Some(p.clone());
        }
        Ok(())
    }
    async fn set_admin(&self, p: &BaseParams) -> GwResult<()> {
        let mut s = self.guard("set_admin".into())?;
        s.base = Some(p.clone());
        Ok(())
    }
    async fn upsert_site(&self, name: &str, domain: &str, dial: &str) -> GwResult<()> {
        let mut s = self.guard(format!("upsert {name} {domain} {dial}"))?;
        s.sites.insert(name.into(), (domain.into(), dial.into()));
        Ok(())
    }
    async fn set_upstream(&self, name: &str, dial: &str) -> GwResult<()> {
        let mut s = self.guard(format!("upstream {name} {dial}"))?;
        match s.sites.get_mut(name) {
            Some(v) => {
                v.1 = dial.into();
                Ok(())
            }
            None => Err(GwError::Rejected("unknown site".into())),
        }
    }
    async fn remove_site(&self, name: &str) -> GwResult<()> {
        let mut s = self.guard(format!("remove {name}"))?;
        s.sites.remove(name);
        Ok(())
    }
    async fn site_ids(&self) -> GwResult<Vec<String>> {
        let s = self.guard("site_ids".into())?;
        Ok(s.sites.keys().map(|n| format!("site-{n}")).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::MockHttp;

    fn params(domain: Option<&str>) -> BaseParams {
        BaseParams {
            admin_domain: domain.map(String::from),
            acme_email: Some("ops@example.com".into()),
            public_ip: Some("203.0.113.9".into()),
            admin_upstream: "unix//run/rustle/rustle.sock".into(),
            admin_listen: String::new(),
        }
    }

    #[test]
    fn base_config_domain_mode() {
        let c = base_config(&params(Some("admin.example.com")));
        assert_eq!(c["admin"]["listen"], ADMIN_LISTEN_DEFAULT);
        let routes = &c["apps"]["http"]["servers"]["main"]["routes"];
        assert_eq!(routes[0]["@id"], "admin");
        assert_eq!(routes[0]["match"][0]["host"][0], "admin.example.com");
        assert_eq!(routes[0]["handle"][0]["upstreams"][0]["dial"], "unix//run/rustle/rustle.sock");
        assert!(c["apps"]["http"]["servers"]["main"].get("tls_connection_policies").is_none());
        assert_eq!(c["apps"]["tls"]["automation"]["policies"][0]["issuers"][0]["email"], "ops@example.com");
    }

    #[test]
    fn base_config_ip_mode_uses_internal_ca_and_matches_everything() {
        let c = base_config(&params(None));
        let routes = &c["apps"]["http"]["servers"]["main"]["routes"];
        assert!(routes[0].get("match").is_none(), "catch-all so the IP serves the dashboard");
        assert_eq!(c["apps"]["http"]["servers"]["main"]["tls_connection_policies"][0]["default_sni"], "203.0.113.9");
        assert_eq!(c["apps"]["tls"]["automation"]["policies"][0]["issuers"][0]["module"], "internal");
    }

    #[test]
    fn admin_listens_on_a_unix_socket_not_tcp() {
        let c = base_config(&params(None));
        let l = c["admin"]["listen"].as_str().unwrap();
        assert!(l.starts_with("unix/"), "admin API must never listen on TCP inside rustle-net: {l}");
    }

    #[tokio::test]
    async fn upsert_inserts_at_index_zero_then_patches() {
        let m = MockHttp::start(|r| match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/id/site-blog") => {
                // first call: unknown, afterwards known
                (404, vec![], "{\"error\":\"unknown object ID\"}".into())
            }
            _ => (200, vec![], "{}".into()),
        })
        .await;
        let gw = HttpGateway::new(Target::Tcp(m.addr.clone()));
        gw.upsert_site("blog", "blog.example.com", "rustle-blog-abc:3000").await.unwrap();
        let reqs = m.requests();
        let put = reqs.iter().find(|r| r.method == "PUT").unwrap();
        assert_eq!(put.path, "/config/apps/http/servers/main/routes/0");
        let body = put.json();
        assert_eq!(body["@id"], "site-blog");
        assert_eq!(body["match"][0]["host"][0], "blog.example.com");
        assert_eq!(body["handle"][0]["@id"], "up-blog");
        assert_eq!(body["handle"][0]["upstreams"][0]["dial"], "rustle-blog-abc:3000");
    }

    #[tokio::test]
    async fn upsert_existing_uses_patch_by_id() {
        let m = MockHttp::start(|_| (200, vec![], "{}".into())).await;
        let gw = HttpGateway::new(Target::Tcp(m.addr.clone()));
        gw.upsert_site("blog", "blog.example.com", "x:1").await.unwrap();
        assert!(m.requests().iter().any(|r| r.method == "PATCH" && r.path == "/id/site-blog"));
    }

    #[tokio::test]
    async fn set_upstream_targets_handler_id() {
        let m = MockHttp::start(|_| (200, vec![], "{}".into())).await;
        let gw = HttpGateway::new(Target::Tcp(m.addr.clone()));
        gw.set_upstream("blog", "rustle-blog-new:3000").await.unwrap();
        let r = &m.requests()[0];
        assert_eq!((r.method.as_str(), r.path.as_str()), ("PATCH", "/id/up-blog/upstreams"));
        assert_eq!(r.json()[0]["dial"], "rustle-blog-new:3000");
        assert_eq!(r.header("content-type").as_deref(), Some("application/json"));
    }

    #[tokio::test]
    async fn remove_is_idempotent_and_errors_surface() {
        let m = MockHttp::start(|r| if r.path.contains("gone") { (404, vec![], "{}".into()) } else { (500, vec![], "{\"error\":\"boom\"}".into()) }).await;
        let gw = HttpGateway::new(Target::Tcp(m.addr.clone()));
        gw.remove_site("gone").await.unwrap();
        assert!(matches!(gw.remove_site("other").await, Err(GwError::Rejected(_))));
        let down = HttpGateway::new(Target::Tcp("127.0.0.1:1".into()));
        assert!(matches!(down.ping().await, Err(GwError::Unavailable(_))));
    }

    #[tokio::test]
    async fn ensure_base_loads_when_server_missing_and_appends_admin_last() {
        let m = MockHttp::start(|r| match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/config/apps/http/servers/main") => (200, vec![], "null".into()),
            _ => (200, vec![], "{}".into()),
        })
        .await;
        let gw = HttpGateway::new(Target::Tcp(m.addr.clone()));
        gw.ensure_base(&params(None)).await.unwrap();
        assert!(m.requests().iter().any(|r| r.method == "POST" && r.path == "/load"));

        let m2 = MockHttp::start(|r| match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/config/apps/http/servers/main") => (200, vec![], "{\"listen\":[\":443\"]}".into()),
            ("GET", "/id/admin") => (404, vec![], "{}".into()),
            _ => (200, vec![], "{}".into()),
        })
        .await;
        let gw = HttpGateway::new(Target::Tcp(m2.addr.clone()));
        gw.ensure_base(&params(None)).await.unwrap();
        let post = m2.requests().into_iter().find(|r| r.method == "POST").unwrap();
        assert_eq!(post.path, "/config/apps/http/servers/main/routes", "POST appends => admin stays last");
    }

    #[tokio::test]
    async fn site_ids_lists_only_site_routes() {
        let m = MockHttp::start(|_| (200, vec![], json!([{"@id":"site-a"},{"@id":"admin"},{"match":[]},{"@id":"site-b"}]).to_string())).await;
        let gw = HttpGateway::new(Target::Tcp(m.addr.clone()));
        assert_eq!(gw.site_ids().await.unwrap(), vec!["site-a".to_string(), "site-b".to_string()]);
    }

    #[tokio::test]
    async fn fake_gateway_behaves() {
        let g = FakeGateway::new();
        g.ensure_base(&params(None)).await.unwrap();
        g.upsert_site("a", "a.example.com", "x:1").await.unwrap();
        g.set_upstream("a", "y:2").await.unwrap();
        assert_eq!(g.sites()["a"].1, "y:2");
        assert!(g.set_upstream("zzz", "y:2").await.is_err());
        g.wipe();
        assert!(g.site_ids().await.unwrap().is_empty());
        g.set_down(true);
        assert!(g.ping().await.is_err());
    }
}
