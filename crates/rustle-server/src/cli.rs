//! Command line helpers: `rustle init`, `rustle restore`, `rustle key export`, `rustle caddy-config`.

use crate::backup;
use crate::caddy::{base_config, BaseParams};
use crate::config::Config;
use crate::crypto::{self, MasterKey};
use crate::metrics;
use crate::store::Store;
use rand::Rng;

pub struct InitResult {
    pub key_created: bool,
    pub key_hex: String,
    /// Present only when an admin account was created (or the password was reset) just now.
    pub password: Option<String>,
}

const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";

pub fn random_password(len: usize) -> String {
    let mut rng = rand::rng();
    (0..len)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect()
}

/// Idempotent first-run setup: master key, admin account, baseline RAM measurement, system jobs.
pub fn init(cfg: &Config, reset_password: bool) -> Result<InitResult, String> {
    let existed = cfg.master_key_file.exists();
    if !existed {
        refuse_orphaned_database(cfg)?;
    }
    let key =
        MasterKey::load(&cfg.master_key_file, true).map_err(|e| format!("master key: {e}"))?;
    let store = Store::open(&cfg.db_path).map_err(|e| e.message)?;
    record_key_check(&store, &key)?;
    let password = ensure_admin(&store, reset_password)?;
    record_baseline(&store, cfg)?;
    apply_env_settings(&store, |k| std::env::var(k).ok())?;
    crate::jobs::seed_system_jobs(&store).map_err(|e| e.message)?;
    Ok(InitResult {
        key_created: !existed,
        key_hex: key.to_hex(),
        password,
    })
}

/// A database with an admin but no key file: generating a new key would orphan every secret in it.
fn refuse_orphaned_database(cfg: &Config) -> Result<(), String> {
    if !cfg.db_path.exists() {
        return Ok(());
    }
    let has_data = Store::open(&cfg.db_path)
        .map_err(|e| e.message)?
        .admin()
        .map_err(|e| e.message)?
        .is_some();
    if has_data {
        return Err(format!(
            "{} exists but the master key {} does not. A new key would make every stored secret unreadable. \
             Restore the original key file (hex, mode 0600) and run again.",
            cfg.db_path.display(),
            cfg.master_key_file.display()
        ));
    }
    Ok(())
}

fn record_key_check(store: &Store, key: &MasterKey) -> Result<(), String> {
    match store.setting("key_check").map_err(|e| e.message)? {
        Some(c) if c != key.check_value() => {
            Err("the master key does not match this database".into())
        }
        Some(_) => Ok(()),
        None => store
            .set_setting("key_check", &key.check_value())
            .map_err(|e| e.message),
    }
}

/// Create the admin account, or reset its password on request. Returns the new password, if any.
fn ensure_admin(store: &Store, reset_password: bool) -> Result<Option<String>, String> {
    let exists = store.admin().map_err(|e| e.message)?.is_some();
    if exists && !reset_password {
        return Ok(None);
    }
    let pw = random_password(24);
    let hash = crypto::hash_password(&pw);
    if exists {
        store.set_admin_password(&hash).map_err(|e| e.message)?;
    } else {
        store.create_admin("admin", &hash).map_err(|e| e.message)?;
    }
    Ok(Some(pw))
}

/// Baseline RAM in use before any site runs: the budget is derived from it.
fn record_baseline(store: &Store, cfg: &Config) -> Result<(), String> {
    if store
        .setting("base_used_mb")
        .map_err(|e| e.message)?
        .is_some()
    {
        return Ok(());
    }
    let m = metrics::meminfo(&cfg.proc_root);
    if m.total_kb == 0 {
        return Ok(());
    }
    let used = (m.total_kb.saturating_sub(m.available_kb) / 1024).max(1);
    store
        .set_setting("base_used_mb", &used.to_string())
        .map_err(|e| e.message)
}

/// Seed settings the installer passes through the environment.
fn apply_env_settings(store: &Store, get: impl Fn(&str) -> Option<String>) -> Result<(), String> {
    for (env, key) in [
        ("RUSTLE_PUBLIC_IP", "public_ip"),
        ("RUSTLE_ADMIN_DOMAIN", "admin_domain"),
        ("RUSTLE_ACME_EMAIL", "acme_email"),
    ] {
        if let Some(v) = get(env).filter(|v| !v.trim().is_empty()) {
            store.set_setting(key, v.trim()).map_err(|e| e.message)?;
        }
    }
    Ok(())
}

pub fn restore(cfg: &Config, file: &std::path::Path, force: bool) -> Result<(), String> {
    let key =
        MasterKey::load(&cfg.master_key_file, false).map_err(|e| format!("master key: {e}"))?;
    let blob = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    backup::restore_to(&key, &blob, &cfg.db_path, force)
}

pub fn key_export(cfg: &Config) -> Result<String, String> {
    Ok(MasterKey::load(&cfg.master_key_file, false)
        .map_err(|e| format!("master key: {e}"))?
        .to_hex())
}

/// Initial Caddy configuration for `install.sh` (written to disk before Caddy first starts).
pub fn caddy_config(cfg: &Config) -> Result<String, String> {
    let store = Store::open(&cfg.db_path).map_err(|e| e.message)?;
    let p = BaseParams {
        admin_domain: store
            .setting("admin_domain")
            .ok()
            .flatten()
            .filter(|d| !d.is_empty()),
        acme_email: store
            .setting("acme_email")
            .ok()
            .flatten()
            .filter(|d| !d.is_empty()),
        public_ip: store
            .setting("public_ip")
            .ok()
            .flatten()
            .or_else(|| cfg.public_ip.clone()),
        admin_upstream: cfg.admin_upstream.clone(),
        admin_listen: String::new(),
    };
    serde_json::to_string_pretty(&base_config(&p)).map_err(|e| e.to_string())
}

// ---- import from a site-ctl style directory -----------------------------------------------------

#[derive(Debug, Default)]
pub struct ImportReport {
    pub lines: Vec<String>,
    pub imported: usize,
    pub skipped: usize,
}

fn meta_value(meta: &str, key: &str) -> Option<String> {
    meta.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}=")))
        .map(|v| v.trim().to_string())
}

/// `KEY=VALUE` lines of an env file. Comments, blanks and a leading `export ` are ignored; one pair of
/// surrounding quotes is removed; the last definition of a key wins.
pub fn parse_env_file(text: &str) -> Vec<rustle_types::EnvVar> {
    let mut out: Vec<rustle_types::EnvVar> = Vec::new();
    for line in text.lines() {
        // Blank lines and comments need no special case: neither has `KEY=` with a valid key.
        let line = line.trim_start();
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        if !crate::util::valid_env_key(k) {
            continue;
        }
        let v = v.trim_end_matches(['\r', '\n']);
        let v = match (v.chars().next(), v.chars().last()) {
            (Some(a), Some(b)) if v.len() >= 2 && (a == '"' || a == '\'') && a == b => {
                &v[1..v.len() - 1]
            }
            _ => v,
        };
        out.retain(|e| e.key != k);
        out.push(rustle_types::EnvVar {
            key: k.to_string(),
            value: v.to_string(),
        });
    }
    out
}

/// What one `site-ctl` site becomes in Rustle.
#[derive(Debug)]
struct SitePlan {
    name: String,
    domain: String,
    repo: String,
    ram: u32,
    vars: Vec<rustle_types::EnvVar>,
    crons: Vec<(String, String)>,
    notes: Vec<String>,
}

/// `Err(reason)` means the site is skipped.
fn plan_site(dir: &std::path::Path, name: &str, meta: &str, yml: &str) -> Result<SitePlan, String> {
    use crate::util::{valid_domain, valid_site_name};
    let image = yml
        .lines()
        .find_map(|l| l.trim().strip_prefix("image:"))
        .map(|v| v.trim().to_string())
        .unwrap_or_default();
    let domain = meta_value(meta, "DOMAIN")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mem: u32 = meta_value(meta, "MEM")
        .and_then(|m| m.parse().ok())
        .unwrap_or(256);
    if !valid_site_name(name) {
        return Err("the name is not valid in Rustle (letters, digits and dashes, no leading or trailing dash)".into());
    }
    if !valid_domain(&domain) {
        return Err(format!("invalid domain '{domain}'"));
    }
    let repo = ghcr_repo(&image)?;
    let mut notes = Vec::new();
    let ram = mem.div_ceil(32).saturating_mul(32).clamp(64, 512);
    if ram != mem {
        notes.push(format!(
            "RAM {mem} MB becomes {ram} MB (Rustle uses steps of 32, 64 to 512)"
        ));
    }
    let read = |sub: &str, ext: &str| {
        std::fs::read_to_string(dir.join(sub).join(format!("{name}.{ext}"))).unwrap_or_default()
    };
    let vars = parse_env_file(&read("env", "env"));
    let crons = parse_cron_lines(&read("cron.d", "cron"), &mut notes);
    unsupported_warnings(meta, &mut notes);
    if !crons.is_empty() && !vars.iter().any(|v| v.key == "CRON_SECRET") {
        notes.push("WARNING: the site has cron jobs but no CRON_SECRET in its env file; they will be called without a bearer token".into());
    }
    notes.push(format!(
        "{domain}  {repo}  {ram} MB  {} env vars  {} cron jobs",
        vars.len(),
        crons.len()
    ));
    Ok(SitePlan {
        name: name.to_string(),
        domain,
        repo,
        ram,
        vars,
        crons,
        notes,
    })
}

fn ghcr_repo(image: &str) -> Result<String, String> {
    match crate::imageref::ImageRef::parse(image) {
        Ok(r) if r.is_ghcr() => Ok(r.repo_key()),
        Ok(r) => Err(format!(
            "image '{image}' is on {}; only ghcr.io images are supported",
            r.host
        )),
        Err(e) => Err(format!("cannot read the image ('{image}'): {e}")),
    }
}

fn unsupported_warnings(meta: &str, notes: &mut Vec<String>) {
    if meta_value(meta, "WWW").is_some_and(|w| w != "0") {
        notes.push(
            "WARNING: the www redirect is not supported by Rustle yet; keep it in your Caddyfile"
                .into(),
        );
    }
    if meta_value(meta, "CACHE").is_some_and(|c| c == "1") {
        notes.push("WARNING: the persistent .next/cache volume is not supported (Rustle runs stateless containers); the cache is rebuilt after each deploy".into());
    }
    if meta_value(meta, "HEALTH").is_some_and(|h| h != "/") {
        notes.push("WARNING: Rustle checks health on '/', not on your custom path".into());
    }
}

/// `<schedule>\t<path>` lines of a site's cron file.
fn parse_cron_lines(text: &str, notes: &mut Vec<String>) -> Vec<(String, String)> {
    let mut crons = Vec::new();
    for l in text.lines().filter(|l| !l.trim().is_empty()) {
        match l.split_once('\t') {
            Some((s, p))
                if crate::cron::CronExpr::parse(s).is_ok()
                    && crate::jobs::validate_http_path(p.trim()).is_ok() =>
            {
                crons.push((s.trim().to_string(), p.trim().to_string()))
            }
            _ => notes.push(format!("WARNING: ignored an unreadable cron line: {l}")),
        }
    }
    crons
}

fn persist_site(store: &Store, key: &MasterKey, plan: &SitePlan) -> Result<(), String> {
    use crate::store::{CronRow, SiteRow};
    use crate::util::{now, rand_hex};
    let t = now();
    let row = SiteRow {
        id: rand_hex(6),
        name: plan.name.clone(),
        domain: plan.domain.clone(),
        image_repo: plan.repo.clone(),
        container_port: 3000,
        ram_limit_mb: plan.ram,
        status: "created".into(),
        current_deployment_id: None,
        registry_id: None,
        zero_downtime: false,
        compat_caps: false,
        created_at: t,
        updated_at: t,
    };
    store.insert_site(&row).map_err(|e| {
        format!(
            "could not create it ({}): it may already exist in Rustle",
            e.message
        )
    })?;
    store
        .replace_env(&row.id, &crate::sites::seal_env(key, &row.id, &plan.vars))
        .map_err(|e| e.message)?;
    for (i, (sched, path)) in plan.crons.iter().enumerate() {
        store
            .insert_cron(&CronRow {
                id: rand_hex(6),
                kind: "http".into(),
                name: format!("{} cron {}", plan.name, i + 1),
                schedule: sched.clone(),
                site_id: Some(row.id.clone()),
                argv_json: Some(serde_json::to_string(&[path]).map_err(|e| e.to_string())?),
                enabled: true,
                system_key: None,
                last_fired_minute: None,
            })
            .map_err(|e| e.message)?;
    }
    store.audit(t, "cli", "site.import", &plan.name);
    Ok(())
}

/// Import the sites of a `site-ctl.sh` directory (`sites.d/*.meta`, `sites.d/*.yml`, `env/*.env`,
/// `cron.d/*.cron`) into Rustle. Sites are created without touching traffic; each one goes live on its
/// first deploy. Run it after `rustle init`, as the user that owns the database.
pub fn import_site_ctl(
    cfg: &Config,
    dir: &std::path::Path,
    dry_run: bool,
) -> Result<ImportReport, String> {
    let sites_d = dir.join("sites.d");
    if !sites_d.is_dir() {
        return Err(format!(
            "{} has no sites.d directory: is this a site-ctl directory?",
            dir.display()
        ));
    }
    let key = MasterKey::load(&cfg.master_key_file, false)
        .map_err(|e| format!("master key: {e} (run `rustle init` first)"))?;
    let store = Store::open(&cfg.db_path).map_err(|e| e.message)?;
    if store.admin().map_err(|e| e.message)?.is_none() {
        return Err("not initialized: run `rustle init` first".into());
    }
    let mut metas: Vec<_> = std::fs::read_dir(&sites_d)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "meta"))
        .collect();
    metas.sort();
    let mut rep = ImportReport::default();
    for meta_path in metas {
        let name = meta_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        rep.lines.push(format!("{name}:"));
        let meta = std::fs::read_to_string(&meta_path).map_err(|e| e.to_string())?;
        let yml = std::fs::read_to_string(sites_d.join(format!("{name}.yml"))).unwrap_or_default();
        let outcome = plan_site(dir, &name, &meta, &yml).and_then(|plan| {
            rep.lines
                .extend(plan.notes.iter().map(|n| format!("  {n}")));
            if dry_run {
                return Ok(());
            }
            persist_site(&store, &key, &plan)
        });
        match outcome {
            Ok(()) => rep.imported += 1,
            Err(why) => {
                rep.skipped += 1;
                rep.lines.push(format!("  SKIPPED: {why}"));
            }
        }
    }
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(d: &std::path::Path) -> Config {
        let c = Config::for_tests(d);
        std::fs::create_dir_all(&c.proc_root).unwrap();
        std::fs::write(
            c.proc_root.join("meminfo"),
            "MemTotal: 1015808 kB\nMemAvailable: 760000 kB\n",
        )
        .unwrap();
        c
    }

    #[test]
    fn init_guards_and_environment_seeding() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(d.path());
        init(&c, false).unwrap();
        // key file lost but data present: refuse to mint a new key
        std::fs::remove_file(&c.master_key_file).unwrap();
        let e = init(&c, false).err().unwrap();
        assert!(
            e.contains("does not") && e.contains("exists but the master key"),
            "{e}"
        );
        // key file replaced by a different key: refused as well
        crate::crypto::MasterKey::load(&c.master_key_file, true).unwrap();
        let e = init(&c, false).err().unwrap();
        assert!(e.contains("does not match"), "{e}");
        // an empty database without a key is fine (fresh install that was interrupted)
        let d2 = tempfile::tempdir().unwrap();
        let c2 = cfg(d2.path());
        Store::open(&c2.db_path).unwrap();
        assert!(init(&c2, false).unwrap().key_created);

        let st = Store::open(&c2.db_path).unwrap();
        let env = |k: &str| match k {
            "RUSTLE_PUBLIC_IP" => Some(" 203.0.113.9 ".to_string()),
            "RUSTLE_ADMIN_DOMAIN" => Some("   ".to_string()),
            _ => None,
        };
        apply_env_settings(&st, env).unwrap();
        assert_eq!(
            st.setting("public_ip").unwrap().as_deref(),
            Some("203.0.113.9")
        );
        assert!(
            st.setting("admin_domain").unwrap().is_none(),
            "blank values are ignored"
        );
        assert!(st.setting("acme_email").unwrap().is_none());
        // no meminfo: no baseline recorded rather than a bogus one
        let d3 = tempfile::tempdir().unwrap();
        let mut c3 = Config::for_tests(d3.path());
        c3.proc_root = d3.path().join("empty");
        let st3 = Store::open(&c3.db_path).unwrap();
        record_baseline(&st3, &c3).unwrap();
        assert!(st3.setting("base_used_mb").unwrap().is_none());
    }

    #[test]
    fn init_is_idempotent_and_shows_the_password_once() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(d.path());
        let first = init(&c, false).unwrap();
        assert!(first.key_created);
        let pw = first.password.clone().unwrap();
        assert_eq!(pw.len(), 24);
        // the stored hash verifies the printed password
        let st = Store::open(&c.db_path).unwrap();
        assert!(crypto::verify_password(
            &pw,
            &st.admin().unwrap().unwrap().1
        ));
        assert_eq!(st.setting("base_used_mb").unwrap().as_deref(), Some("249"));
        assert_eq!(st.list_cron().unwrap().len(), 4);
        drop(st);
        let second = init(&c, false).unwrap();
        assert!(
            !second.key_created && second.password.is_none(),
            "re-running never reveals or changes anything"
        );
        assert_eq!(second.key_hex, first.key_hex);
        let third = init(&c, true).unwrap();
        let new_pw = third.password.unwrap();
        assert_ne!(new_pw, pw);
        let st = Store::open(&c.db_path).unwrap();
        assert!(crypto::verify_password(
            &new_pw,
            &st.admin().unwrap().unwrap().1
        ));
        assert_eq!(
            st.admin().unwrap().unwrap().2,
            1,
            "reset revokes existing sessions"
        );
    }

    #[test]
    fn init_refuses_to_replace_a_lost_master_key() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(d.path());
        init(&c, false).unwrap();
        std::fs::remove_file(&c.master_key_file).unwrap();
        let err = init(&c, false).err().expect("must refuse");
        assert!(err.contains("master key"), "{err}");
        assert!(
            !c.master_key_file.exists(),
            "no replacement key was created"
        );
        // a different key restored by hand is also detected
        std::fs::write(&c.master_key_file, MasterKey::generate().to_hex()).unwrap();
        assert!(init(&c, false).err().unwrap().contains("does not match"));
    }

    #[test]
    fn restore_roundtrip_via_cli() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(d.path());
        init(&c, false).unwrap();
        let key = MasterKey::load(&c.master_key_file, false).unwrap();
        // make an encrypted backup the same way the scheduler does
        let snap = d.path().join("snap.db");
        crate::store::snapshot_to(&c.db_path, &snap).unwrap();
        let enc = d.path().join("b.enc");
        std::fs::write(
            &enc,
            key.seal(&std::fs::read(&snap).unwrap(), b"rustle-backup-v1"),
        )
        .unwrap();
        let mut c2 = cfg(&d.path().join("new-vps"));
        c2.master_key_file = c.master_key_file.clone();
        std::fs::create_dir_all(d.path().join("new-vps")).unwrap();
        restore(&c2, &enc, false).unwrap();
        assert!(Store::open(&c2.db_path).unwrap().admin().unwrap().is_some());
        assert!(restore(&c2, &enc, false).is_err());
        assert_eq!(key_export(&c).unwrap(), key.to_hex());
        assert!(restore(&c2, &d.path().join("missing"), true).is_err());
    }

    #[test]
    fn caddy_config_has_unix_admin_listener() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(d.path());
        init(&c, false).unwrap();
        let j: serde_json::Value = serde_json::from_str(&caddy_config(&c).unwrap()).unwrap();
        assert!(j["admin"]["listen"].as_str().unwrap().starts_with("unix/"));
        assert!(j["apps"]["http"]["servers"]["main"]["routes"][0]["@id"] == "admin");
    }

    fn write(p: &std::path::Path, text: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn fixture(d: &std::path::Path) {
        let a = d.join("apps");
        write(
            &a.join("sites.d/blog.meta"),
            "DOMAIN=Blog.Example.com\nWWW=1\nMEM=220\nCACHE=1\nHEALTH=/api/health\n",
        );
        write(
            &a.join("sites.d/blog.yml"),
            "  blog:\n    image: ghcr.io/acme/blog:abc123\n    restart: unless-stopped\n",
        );
        write(&a.join("env/blog.env"), "# comment\nNODE_ENV=production\nexport CRON_SECRET=\"s3cret\"\nBAD KEY=x\nA='q'\nA=last\n");
        write(
            &a.join("cron.d/blog.cron"),
            "*/5 * * * *\t/api/cron/sync\nnot a cron line\n",
        );
        write(
            &a.join("sites.d/docs.meta"),
            "DOMAIN=docs.example.com\nWWW=0\nMEM=64\nCACHE=0\nHEALTH=/\n",
        );
        write(
            &a.join("sites.d/docs.yml"),
            "  docs:\n    image: ghcr.io/acme/docs:1\n",
        );
        write(
            &a.join("sites.d/legacy.meta"),
            "DOMAIN=old.example.com\nMEM=128\n",
        );
        write(
            &a.join("sites.d/legacy.yml"),
            "  legacy:\n    image: registry.example.com/acme/legacy:1\n",
        );
    }

    #[test]
    fn env_file_parsing() {
        let v = parse_env_file("# c\n\nA=1\nexport B=\"two words\"\nC='x'\n1BAD=no\nD=a=b\nA=2\n");
        let m: std::collections::BTreeMap<_, _> = v
            .iter()
            .map(|e| (e.key.as_str(), e.value.as_str()))
            .collect();
        assert_eq!(m["A"], "2", "last definition wins");
        assert_eq!(m["B"], "two words");
        assert_eq!(m["C"], "x");
        assert_eq!(m["D"], "a=b", "only the first '=' splits");
        assert!(!m.contains_key("1BAD"));
    }

    #[test]
    fn imports_a_site_ctl_directory_without_touching_traffic() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(&d.path().join("state"));
        std::fs::create_dir_all(d.path().join("state")).unwrap();
        init(&c, false).unwrap();
        fixture(d.path());
        let dir = d.path().join("apps");
        // a dry run reports and writes nothing
        let dry = import_site_ctl(&c, &dir, true).unwrap();
        assert_eq!((dry.imported, dry.skipped), (2, 1));
        assert!(Store::open(&c.db_path)
            .unwrap()
            .list_sites()
            .unwrap()
            .is_empty());
        let rep = import_site_ctl(&c, &dir, false).unwrap();
        let text = rep.lines.join("\n");
        assert_eq!((rep.imported, rep.skipped), (2, 1), "{text}");
        assert!(text.contains("220 MB becomes 224 MB"), "{text}");
        assert!(
            text.contains("www redirect")
                && text.contains("cache volume")
                && text.contains("custom path")
                || text.contains("health on '/'"),
            "{text}"
        );
        assert!(
            text.contains("registry.example.com") && text.contains("SKIPPED"),
            "{text}"
        );
        assert!(text.contains("unreadable cron line"), "{text}");
        let st = Store::open(&c.db_path).unwrap();
        let blog = st.get_site_by_name("blog").unwrap().unwrap();
        assert_eq!(
            (
                blog.domain.as_str(),
                blog.image_repo.as_str(),
                blog.ram_limit_mb
            ),
            ("blog.example.com", "ghcr.io/acme/blog", 224)
        );
        assert_eq!(
            blog.status, "created",
            "no deployment, so no traffic is touched"
        );
        // env is sealed with the site's key and can be read back
        let key = MasterKey::load(&c.master_key_file, false).unwrap();
        let env: std::collections::BTreeMap<String, String> = st
            .list_env(&blog.id)
            .unwrap()
            .into_iter()
            .map(|(k, blob)| {
                (
                    k.clone(),
                    key.open_str(&blob, &format!("env:{}:{k}", blog.id))
                        .unwrap(),
                )
            })
            .collect();
        assert_eq!(env["CRON_SECRET"], "s3cret");
        assert_eq!(env["A"], "last");
        assert!(!env.contains_key("BAD KEY"));
        // cron lines become HTTP jobs
        let jobs: Vec<_> = st
            .list_cron()
            .unwrap()
            .into_iter()
            .filter(|j| j.kind == "http")
            .collect();
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            (jobs[0].schedule.as_str(), jobs[0].argv_json.as_deref()),
            ("*/5 * * * *", Some("[\"/api/cron/sync\"]"))
        );
        // running it again is harmless: existing sites are skipped, not duplicated
        let again = import_site_ctl(&c, &dir, false).unwrap();
        assert_eq!(again.imported, 0);
        assert_eq!(
            Store::open(&c.db_path).unwrap().list_sites().unwrap().len(),
            2
        );
        // not a site-ctl directory
        assert!(import_site_ctl(&c, d.path(), true).is_err());
    }

    #[test]
    fn passwords_are_unambiguous() {
        let p = random_password(200);
        assert!(!p.contains(['0', 'O', 'l', 'I', '1']));
    }

    #[test]
    fn env_files_comments_quotes_and_duplicates() {
        let vars = parse_env_file(
            "# A=1\n#B=2\n   # C=3\n\n  \nexport D=4\nE = 5\nF=\"q\"\nG='s'\nH=\"mismatch'\nI=\"\nJ=\nK=a b\nbad key=1\n9X=1\nL=1\nL=2\n",
        );
        let m: Vec<(&str, &str)> = vars
            .iter()
            .map(|v| (v.key.as_str(), v.value.as_str()))
            .collect();
        assert_eq!(
            m,
            vec![
                ("D", "4"),
                ("E", " 5"),
                ("F", "q"),
                ("G", "s"),
                ("H", "\"mismatch'"),
                ("I", "\""),
                ("J", ""),
                ("K", "a b"),
                ("L", "2"),
            ]
        );
    }

    #[test]
    fn caddy_config_ignores_blank_settings_and_uses_real_ones() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(d.path());
        init(&c, false).unwrap();
        let st = Store::open(&c.db_path).unwrap();
        st.set_setting("admin_domain", "").unwrap();
        st.set_setting("acme_email", "").unwrap();
        st.set_setting("public_ip", "203.0.113.5").unwrap();
        let blank = caddy_config(&c).unwrap();
        assert!(blank.contains("203.0.113.5"), "IP mode: {blank}");
        assert!(!blank.contains("\"email\""), "no empty ACME email: {blank}");
        st.set_setting("admin_domain", "dash.example.com").unwrap();
        st.set_setting("acme_email", "ops@example.com").unwrap();
        let full = caddy_config(&c).unwrap();
        assert!(
            full.contains("dash.example.com") && full.contains("ops@example.com"),
            "{full}"
        );
        // without a stored public IP the configured one is used
        let mut c2 = c.clone();
        c2.public_ip = Some("198.51.100.9".into());
        st.delete_setting("public_ip").unwrap();
        st.set_setting("admin_domain", "").unwrap();
        assert!(caddy_config(&c2).unwrap().contains("198.51.100.9"));
    }

    fn warnings(meta: &str) -> Vec<String> {
        let mut n = Vec::new();
        unsupported_warnings(meta, &mut n);
        n
    }

    #[test]
    fn unsupported_site_ctl_features_are_reported_only_when_used() {
        assert!(warnings("DOMAIN=a.example.com\n").is_empty());
        assert!(warnings("WWW=0\nCACHE=0\nHEALTH=/\n").is_empty());
        let w = warnings("WWW=1\n");
        assert!(w.len() == 1 && w[0].contains("www redirect"), "{w:?}");
        let w = warnings("CACHE=1\n");
        assert!(w.len() == 1 && w[0].contains("cache volume"), "{w:?}");
        let w = warnings("HEALTH=/healthz\n");
        assert!(
            w.len() == 1 && w[0].contains("custom path") || w[0].contains("not on your"),
            "{w:?}"
        );
        assert_eq!(warnings("WWW=yes\nCACHE=1\nHEALTH=/x\n").len(), 3);
    }

    #[test]
    fn cron_lines_need_a_schedule_a_tab_and_a_safe_path() {
        let mut notes = Vec::new();
        let ok = parse_cron_lines(
            "*/5 * * * *\t/api/cron/a\n\n  \n0 3 * * *\t /api/cron/b \nnot a schedule\t/x\n* * * * *\t//evil\n* * * * * /no-tab\n* * * * *\n",
            &mut notes,
        );
        assert_eq!(
            ok,
            vec![
                ("*/5 * * * *".to_string(), "/api/cron/a".to_string()),
                ("0 3 * * *".to_string(), "/api/cron/b".to_string())
            ]
        );
        assert_eq!(notes.len(), 4, "{notes:?}");
        assert!(notes.iter().all(|n| n.contains("unreadable cron line")));
        // a valid schedule with a bad path, and a bad schedule with a good path, are each refused
        let mut n = Vec::new();
        assert!(parse_cron_lines("* * * * *\t//evil\n", &mut n).is_empty() && n.len() == 1);
        let mut n = Vec::new();
        assert!(parse_cron_lines("99 * * * *\t/ok\n", &mut n).is_empty() && n.len() == 1);
    }

    #[test]
    fn a_missing_cron_secret_is_flagged_only_for_sites_with_cron_jobs() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("env")).unwrap();
        std::fs::create_dir_all(d.path().join("cron.d")).unwrap();
        let meta = "DOMAIN=a.example.com\nMEM=256\n";
        let yml = "image: ghcr.io/o/a:1\n";
        let write = |sub: &str, ext: &str, text: &str| {
            std::fs::write(d.path().join(sub).join(format!("a.{ext}")), text).unwrap();
        };
        let warn = |p: &SitePlan| p.notes.iter().any(|n| n.contains("no CRON_SECRET"));
        // cron jobs, no secret
        write("cron.d", "cron", "* * * * *\t/api/cron/x\n");
        let p = plan_site(d.path(), "a", meta, yml).unwrap();
        assert!(warn(&p) && p.crons.len() == 1);
        // cron jobs and a secret
        write("env", "env", "CRON_SECRET=abc\n");
        assert!(!warn(&plan_site(d.path(), "a", meta, yml).unwrap()));
        // no cron jobs and no secret
        write("env", "env", "OTHER=1\n");
        write("cron.d", "cron", "");
        let p = plan_site(d.path(), "a", meta, yml).unwrap();
        assert!(!warn(&p) && p.crons.is_empty());
        // the summary line is last and counts what was found
        assert!(
            p.notes.last().unwrap().contains("1 env vars  0 cron jobs"),
            "{:?}",
            p.notes
        );
        // refusals
        assert!(plan_site(d.path(), "A_b", meta, yml)
            .unwrap_err()
            .contains("not valid"));
        assert!(plan_site(d.path(), "a", "DOMAIN=nope\n", yml)
            .unwrap_err()
            .contains("invalid domain"));
        assert!(plan_site(d.path(), "a", meta, "image: docker.io/x/y\n")
            .unwrap_err()
            .contains("only ghcr.io"));
        assert!(plan_site(d.path(), "a", meta, "")
            .unwrap_err()
            .contains("cannot read the image"));
        // RAM rounding: 220 -> 224, 8 -> 64 (minimum), 9999 -> 512 (maximum), 256 unchanged
        for (mem, want) in [
            ("220", 224),
            ("8", 64),
            ("9999", 512),
            ("256", 256),
            ("", 256),
        ] {
            let m = format!("DOMAIN=a.example.com\nMEM={mem}\n");
            let p = plan_site(d.path(), "a", &m, yml).unwrap();
            assert_eq!(p.ram, want, "{mem}");
            assert_eq!(
                p.notes.iter().any(|n| n.contains("becomes")),
                mem != "256" && !mem.is_empty() && want != mem.parse().unwrap_or(0),
                "{mem}"
            );
        }
    }
}
