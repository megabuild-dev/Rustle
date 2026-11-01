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
    if !existed && cfg.db_path.exists() {
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
    }
    let key =
        MasterKey::load(&cfg.master_key_file, true).map_err(|e| format!("master key: {e}"))?;
    let store = Store::open(&cfg.db_path).map_err(|e| e.message)?;
    match store.setting("key_check").map_err(|e| e.message)? {
        Some(c) if c != key.check_value() => {
            return Err("the master key does not match this database".into())
        }
        Some(_) => {}
        None => store
            .set_setting("key_check", &key.check_value())
            .map_err(|e| e.message)?,
    }
    let mut password = None;
    if store.admin().map_err(|e| e.message)?.is_none() || reset_password {
        let pw = random_password(24);
        let hash = crypto::hash_password(&pw);
        if store.admin().map_err(|e| e.message)?.is_none() {
            store.create_admin("admin", &hash).map_err(|e| e.message)?;
        } else {
            store.set_admin_password(&hash).map_err(|e| e.message)?;
        }
        password = Some(pw);
    }
    // Baseline RAM in use before any site runs: the budget is derived from it.
    if store
        .setting("base_used_mb")
        .map_err(|e| e.message)?
        .is_none()
    {
        let m = metrics::meminfo(&cfg.proc_root);
        if m.total_kb > 0 {
            let used = (m.total_kb.saturating_sub(m.available_kb) / 1024).max(1);
            store
                .set_setting("base_used_mb", &used.to_string())
                .map_err(|e| e.message)?;
        }
    }
    for (env, key) in [
        ("RUSTLE_PUBLIC_IP", "public_ip"),
        ("RUSTLE_ADMIN_DOMAIN", "admin_domain"),
        ("RUSTLE_ACME_EMAIL", "acme_email"),
    ] {
        if let Some(v) = std::env::var(env).ok().filter(|v| !v.trim().is_empty()) {
            store.set_setting(key, v.trim()).map_err(|e| e.message)?;
        }
    }
    crate::jobs::seed_system_jobs(&store).map_err(|e| e.message)?;
    Ok(InitResult {
        key_created: !existed,
        key_hex: key.to_hex(),
        password,
    })
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

    #[test]
    fn passwords_are_unambiguous() {
        let p = random_password(200);
        assert!(!p.contains(['0', 'O', 'l', 'I', '1']));
    }
}
