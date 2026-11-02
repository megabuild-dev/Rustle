use std::path::PathBuf;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone)]
pub enum Endpoint {
    Tcp(String),
    Unix(PathBuf),
}

impl Endpoint {
    pub fn parse(s: &str) -> Endpoint {
        if let Some(p) = s.strip_prefix("unix://") {
            Endpoint::Unix(PathBuf::from(p))
        } else if let Some(p) = s.strip_prefix("unix:") {
            Endpoint::Unix(PathBuf::from(p))
        } else {
            let s = s
                .strip_prefix("tcp://")
                .or_else(|| s.strip_prefix("http://"))
                .unwrap_or(s);
            Endpoint::Tcp(s.trim_end_matches('/').to_string())
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Loopback TCP listener (SSH-tunnel fallback, dev). Empty disables it.
    pub local_bind: Option<String>,
    /// Unix socket served to Caddy.
    pub unix_socket: Option<PathBuf>,
    pub state_dir: PathBuf,
    pub db_path: PathBuf,
    pub master_key_file: PathBuf,
    pub docker: Endpoint,
    pub caddy_admin: Endpoint,
    /// `true` when Rustle shares a Caddy that already exists on the host instead of running its own:
    /// it then only adds/removes its own `@id` routes, never loads a whole configuration, and
    /// reaches containers by IP (the host cannot resolve Docker names).
    pub gateway_external: bool,
    /// A shared Caddy that runs as a container attached to `rustle-net` resolves container names (like
    /// Rustle's own Caddy); one on the host cannot and is given container IPs.
    pub gateway_names: bool,
    /// Name of Caddy's HTTP server to add routes to (autodetected in external mode).
    pub caddy_server: Option<String>,
    /// Upstream string Caddy uses to reach Rustle (`unix//run/rustle/rustle.sock` or `host:port`).
    pub admin_upstream: String,
    pub public_ip: Option<String>,
    pub network: String,
    pub cgroup_root: PathBuf,
    pub github_api: String,
    pub release_repo: String,
    pub update_pubkey: Option<[u8; 32]>,
    pub bin_dir: PathBuf,
    pub proc_root: PathBuf,
    pub fake_backends: bool,
    /// With `dev-fakes`: keep the real Caddy gateway and fake only Docker/GitHub/DNS.
    pub real_gateway: bool,
    /// With `dev-fakes`: every fake container reports this IP (so a local test server can stand in for it).
    pub fake_container_ip: Option<String>,
}

impl Config {
    pub fn from_env() -> Config {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Build the configuration from any key lookup (the environment in production, a map in tests).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Config {
        let get = |k: &str| lookup(k).filter(|v| !v.is_empty());
        let state_dir =
            PathBuf::from(get("RUSTLE_STATE_DIR").unwrap_or_else(|| "/var/lib/rustle".into()));
        let unix_socket =
            optional_or(get("RUSTLE_SOCKET"), "/run/rustle/rustle.sock").map(PathBuf::from);
        let local_bind = optional_or(get("RUSTLE_BIND"), "127.0.0.1:3000");
        let gateway_external = get("RUSTLE_GATEWAY").is_some_and(|v| v == "external");
        let path_or = |k: &str, d: &str| PathBuf::from(get(k).unwrap_or_else(|| d.into()));
        let str_or = |k: &str, d: &str| get(k).unwrap_or_else(|| d.into());
        let dev = dev_flags(&get);
        Config {
            admin_upstream: admin_upstream(
                get("RUSTLE_ADMIN_UPSTREAM"),
                gateway_external,
                &unix_socket,
            ),
            caddy_admin: Endpoint::parse(
                &get("CADDY_ADMIN_URL")
                    .unwrap_or_else(|| default_caddy_admin(gateway_external).into()),
            ),
            local_bind,
            unix_socket,
            db_path: get("RUSTLE_DB")
                .map(PathBuf::from)
                .unwrap_or_else(|| state_dir.join("rustle.db")),
            master_key_file: path_or("RUSTLE_MASTER_KEY_FILE", "/etc/rustle/master.key"),
            docker: Endpoint::parse(&str_or("DOCKER_HOST", "tcp://172.31.0.2:2375")),
            gateway_external,
            gateway_names: get("RUSTLE_GATEWAY_NAMES").is_some_and(|v| v == "1"),
            caddy_server: get("RUSTLE_CADDY_SERVER"),
            public_ip: get("RUSTLE_PUBLIC_IP"),
            network: str_or("RUSTLE_NETWORK", "rustle-net"),
            cgroup_root: path_or("RUSTLE_CGROUP_ROOT", "/sys/fs/cgroup"),
            github_api: str_or("RUSTLE_GITHUB_API", "https://api.github.com"),
            release_repo: str_or("RUSTLE_RELEASE_REPO", "megalabs-dev/rustle"),
            update_pubkey: option_env!("RUSTLE_UPDATE_PUBKEY_HEX").and_then(parse_pubkey),
            bin_dir: state_dir.join("bin"),
            proc_root: path_or("RUSTLE_PROC_ROOT", "/proc"),
            fake_backends: dev.0,
            real_gateway: dev.1,
            fake_container_ip: dev.2,
            state_dir,
        }
    }

    /// Test/dev configuration rooted in a temp directory.
    pub fn for_tests(dir: &std::path::Path) -> Config {
        Config {
            local_bind: None,
            unix_socket: None,
            state_dir: dir.to_path_buf(),
            db_path: dir.join("rustle.db"),
            master_key_file: dir.join("master.key"),
            docker: Endpoint::Tcp("127.0.0.1:1".into()),
            caddy_admin: Endpoint::Tcp("127.0.0.1:1".into()),
            gateway_external: false,
            gateway_names: false,
            caddy_server: None,
            admin_upstream: "127.0.0.1:3000".into(),
            public_ip: None,
            network: "rustle-net".into(),
            cgroup_root: dir.join("cgroup"),
            github_api: "http://127.0.0.1:1".into(),
            release_repo: "o/r".into(),
            update_pubkey: None,
            bin_dir: dir.join("bin"),
            proc_root: dir.join("proc"),
            fake_backends: true,
            real_gateway: false,
            fake_container_ip: None,
        }
    }
}

/// `None` when the value is the literal "none", the default when unset, else the value.
fn optional_or(v: Option<String>, default: &str) -> Option<String> {
    match v {
        Some(s) if s == "none" => None,
        Some(s) => Some(s),
        None => Some(default.into()),
    }
}

fn admin_upstream(explicit: Option<String>, external: bool, socket: &Option<PathBuf>) -> String {
    if let Some(v) = explicit {
        return v;
    }
    match (external, socket) {
        (true, _) | (false, None) => "127.0.0.1:3000".into(),
        (false, Some(p)) => format!("unix/{}", p.display()),
    }
}

fn default_caddy_admin(external: bool) -> &'static str {
    if external {
        "http://127.0.0.1:2019" // Caddy's default admin address
    } else {
        "unix:///run/rustle/caddy-admin.sock"
    }
}

fn parse_pubkey(h: &str) -> Option<[u8; 32]> {
    hex::decode(h).ok()?.try_into().ok()
}

/// (fake_backends, real_gateway, fake_container_ip): only honoured in `dev-fakes` builds.
fn dev_flags(get: &impl Fn(&str) -> Option<String>) -> (bool, bool, Option<String>) {
    if !cfg!(feature = "dev-fakes") {
        return (false, false, None);
    }
    let mode = get("RUSTLE_FAKE_BACKENDS");
    (
        mode.is_some(),
        mode.as_deref() == Some("docker"),
        get("RUSTLE_FAKE_CONTAINER_IP"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_parse() {
        assert!(
            matches!(Endpoint::parse("unix:///run/x.sock"), Endpoint::Unix(p) if p.to_str() == Some("/run/x.sock"))
        );
        assert!(
            matches!(Endpoint::parse("tcp://127.0.0.1:2375"), Endpoint::Tcp(s) if s == "127.0.0.1:2375")
        );
        assert!(
            matches!(Endpoint::parse("http://localhost:2019/"), Endpoint::Tcp(s) if s == "localhost:2019")
        );
    }

    fn cfg_from(pairs: &[(&str, &str)]) -> Config {
        let m: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(move |k| m.get(k).cloned())
    }

    #[test]
    fn defaults_are_the_managed_layout() {
        let c = cfg_from(&[]);
        assert_eq!(c.state_dir, PathBuf::from("/var/lib/rustle"));
        assert_eq!(c.db_path, PathBuf::from("/var/lib/rustle/rustle.db"));
        assert_eq!(c.bin_dir, PathBuf::from("/var/lib/rustle/bin"));
        assert_eq!(
            c.unix_socket,
            Some(PathBuf::from("/run/rustle/rustle.sock"))
        );
        assert_eq!(c.local_bind.as_deref(), Some("127.0.0.1:3000"));
        assert_eq!(c.admin_upstream, "unix//run/rustle/rustle.sock");
        assert!(
            matches!(&c.caddy_admin, Endpoint::Unix(p) if p.to_str() == Some("/run/rustle/caddy-admin.sock"))
        );
        assert!(matches!(&c.docker, Endpoint::Tcp(s) if s == "172.31.0.2:2375"));
        assert_eq!(c.network, "rustle-net");
        assert!(!c.gateway_external && !c.gateway_names && c.caddy_server.is_none());
        assert_eq!(c.proc_root, PathBuf::from("/proc"));
        assert_eq!(c.cgroup_root, PathBuf::from("/sys/fs/cgroup"));
        assert_eq!(c.github_api, "https://api.github.com");
        assert_eq!(c.master_key_file, PathBuf::from("/etc/rustle/master.key"));
    }

    #[test]
    fn empty_values_count_as_unset_and_none_disables_listeners() {
        let c = cfg_from(&[
            ("RUSTLE_STATE_DIR", ""),
            ("RUSTLE_SOCKET", "none"),
            ("RUSTLE_BIND", "none"),
        ]);
        assert_eq!(c.state_dir, PathBuf::from("/var/lib/rustle"));
        assert!(c.unix_socket.is_none() && c.local_bind.is_none());
        assert_eq!(
            c.admin_upstream, "127.0.0.1:3000",
            "no socket: fall back to loopback"
        );
        let c = cfg_from(&[
            ("RUSTLE_SOCKET", "/tmp/x.sock"),
            ("RUSTLE_BIND", "0.0.0.0:9"),
        ]);
        assert_eq!(c.unix_socket, Some(PathBuf::from("/tmp/x.sock")));
        assert_eq!(c.local_bind.as_deref(), Some("0.0.0.0:9"));
        assert_eq!(c.admin_upstream, "unix//tmp/x.sock");
    }

    #[test]
    fn shared_caddy_mode_changes_the_defaults() {
        let c = cfg_from(&[
            ("RUSTLE_GATEWAY", "external"),
            ("RUSTLE_GATEWAY_NAMES", "1"),
            ("RUSTLE_CADDY_SERVER", "srv0"),
        ]);
        assert!(c.gateway_external && c.gateway_names);
        assert_eq!(c.caddy_server.as_deref(), Some("srv0"));
        assert_eq!(c.admin_upstream, "127.0.0.1:3000");
        assert!(matches!(&c.caddy_admin, Endpoint::Tcp(s) if s == "127.0.0.1:2019"));
        // explicit settings win over the mode defaults
        let c = cfg_from(&[
            ("RUSTLE_GATEWAY", "external"),
            ("RUSTLE_ADMIN_UPSTREAM", "10.0.0.5:3000"),
            ("CADDY_ADMIN_URL", "http://caddy:2019"),
        ]);
        assert_eq!(c.admin_upstream, "10.0.0.5:3000");
        assert!(matches!(&c.caddy_admin, Endpoint::Tcp(s) if s == "caddy:2019"));
        // other values of the switches do not enable anything
        let c = cfg_from(&[
            ("RUSTLE_GATEWAY", "internal"),
            ("RUSTLE_GATEWAY_NAMES", "yes"),
        ]);
        assert!(!c.gateway_external && !c.gateway_names);
    }

    #[test]
    fn overrides_are_applied() {
        let c = cfg_from(&[
            ("RUSTLE_STATE_DIR", "/s"),
            ("RUSTLE_DB", "/d/x.db"),
            ("RUSTLE_MASTER_KEY_FILE", "/k"),
            ("DOCKER_HOST", "unix:///var/run/docker.sock"),
            ("RUSTLE_NETWORK", "n"),
            ("RUSTLE_PUBLIC_IP", "1.2.3.4"),
            ("RUSTLE_PROC_ROOT", "/p"),
            ("RUSTLE_CGROUP_ROOT", "/c"),
            ("RUSTLE_GITHUB_API", "http://gh"),
            ("RUSTLE_RELEASE_REPO", "o/r"),
        ]);
        assert_eq!(c.db_path, PathBuf::from("/d/x.db"));
        assert_eq!(c.bin_dir, PathBuf::from("/s/bin"));
        assert_eq!(c.master_key_file, PathBuf::from("/k"));
        assert!(
            matches!(&c.docker, Endpoint::Unix(p) if p.to_str() == Some("/var/run/docker.sock"))
        );
        assert_eq!(
            (c.network.as_str(), c.public_ip.as_deref()),
            ("n", Some("1.2.3.4"))
        );
        assert_eq!(
            (c.proc_root, c.cgroup_root),
            (PathBuf::from("/p"), PathBuf::from("/c"))
        );
        assert_eq!(
            (c.github_api.as_str(), c.release_repo.as_str()),
            ("http://gh", "o/r")
        );
    }

    #[test]
    fn pubkey_and_dev_flags() {
        assert_eq!(parse_pubkey(&"ab".repeat(32)), Some([0xab; 32]));
        assert_eq!(parse_pubkey("abcd"), None);
        assert_eq!(parse_pubkey("zz"), None);
        let none = |_: &str| None;
        assert_eq!(dev_flags(&none), (false, false, None));
        let docker = |k: &str| match k {
            "RUSTLE_FAKE_BACKENDS" => Some("docker".to_string()),
            "RUSTLE_FAKE_CONTAINER_IP" => Some("10.1.1.1".to_string()),
            _ => None,
        };
        if cfg!(feature = "dev-fakes") {
            assert_eq!(dev_flags(&docker), (true, true, Some("10.1.1.1".into())));
            let all = |k: &str| (k == "RUSTLE_FAKE_BACKENDS").then(|| "1".to_string());
            assert_eq!(dev_flags(&all), (true, false, None));
        } else {
            assert_eq!(dev_flags(&docker), (false, false, None));
        }
    }
}
