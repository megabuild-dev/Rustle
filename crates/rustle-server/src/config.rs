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
}

impl Config {
    pub fn from_env() -> Config {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let state_dir =
            PathBuf::from(get("RUSTLE_STATE_DIR").unwrap_or_else(|| "/var/lib/rustle".into()));
        let unix_socket = match get("RUSTLE_SOCKET") {
            Some(s) if s == "none" => None,
            Some(s) => Some(PathBuf::from(s)),
            None => Some(PathBuf::from("/run/rustle/rustle.sock")),
        };
        let local_bind = match get("RUSTLE_BIND") {
            Some(s) if s == "none" => None,
            Some(s) => Some(s),
            None => Some("127.0.0.1:3000".into()),
        };
        let admin_upstream = get("RUSTLE_ADMIN_UPSTREAM").unwrap_or_else(|| match &unix_socket {
            Some(p) => format!("unix/{}", p.display()),
            None => "127.0.0.1:3000".into(),
        });
        Config {
            local_bind,
            unix_socket,
            db_path: get("RUSTLE_DB")
                .map(PathBuf::from)
                .unwrap_or_else(|| state_dir.join("rustle.db")),
            master_key_file: get("RUSTLE_MASTER_KEY_FILE")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/etc/rustle/master.key")),
            docker: Endpoint::parse(
                &get("DOCKER_HOST").unwrap_or_else(|| "tcp://172.31.0.2:2375".into()),
            ),
            caddy_admin: Endpoint::parse(
                &get("CADDY_ADMIN_URL")
                    .unwrap_or_else(|| "unix:///run/rustle/caddy-admin.sock".into()),
            ),
            admin_upstream,
            public_ip: get("RUSTLE_PUBLIC_IP"),
            network: get("RUSTLE_NETWORK").unwrap_or_else(|| "rustle-net".into()),
            cgroup_root: PathBuf::from(
                get("RUSTLE_CGROUP_ROOT").unwrap_or_else(|| "/sys/fs/cgroup".into()),
            ),
            github_api: get("RUSTLE_GITHUB_API").unwrap_or_else(|| "https://api.github.com".into()),
            release_repo: get("RUSTLE_RELEASE_REPO")
                .unwrap_or_else(|| "megalabs-dev/rustle".into()),
            update_pubkey: option_env!("RUSTLE_UPDATE_PUBKEY_HEX").and_then(|h| {
                let v = hex::decode(h).ok()?;
                v.try_into().ok()
            }),
            bin_dir: state_dir.join("bin"),
            proc_root: PathBuf::from(get("RUSTLE_PROC_ROOT").unwrap_or_else(|| "/proc".into())),
            fake_backends: cfg!(feature = "dev-fakes") && get("RUSTLE_FAKE_BACKENDS").is_some(),
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
        }
    }
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
}
