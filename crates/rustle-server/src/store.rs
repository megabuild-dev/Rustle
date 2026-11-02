//! SQLite persistence. A single connection behind a std Mutex: every call is short and
//! synchronous; nothing may hold the lock across `.await` or blocking work.

use crate::error::AppResult;
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::path::Path;
use std::sync::Mutex;

const SCHEMA_V1: &str = r#"
CREATE TABLE admin(
  id INTEGER PRIMARY KEY CHECK(id = 1),
  username TEXT NOT NULL,
  password_hash TEXT NOT NULL,
  token_version INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE registries(
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  host TEXT NOT NULL,
  username TEXT NOT NULL,
  secret_enc BLOB NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE TABLE sites(
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  domain TEXT NOT NULL UNIQUE,
  image_repo TEXT NOT NULL,
  container_port INTEGER NOT NULL,
  ram_limit_mb INTEGER NOT NULL,
  status TEXT NOT NULL,
  current_deployment_id TEXT,
  registry_id TEXT REFERENCES registries(id) ON DELETE SET NULL,
  zero_downtime INTEGER NOT NULL DEFAULT 0,
  compat_caps INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE TABLE site_env(
  site_id TEXT NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
  key TEXT NOT NULL,
  value_enc BLOB NOT NULL,
  PRIMARY KEY(site_id, key)
);
CREATE TABLE deploy_tokens(
  id TEXT PRIMARY KEY,
  site_id TEXT NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
  prefix TEXT NOT NULL,
  token_hash TEXT NOT NULL UNIQUE,
  created_at INTEGER NOT NULL,
  revoked_at INTEGER,
  last_used_at INTEGER
);
CREATE TABLE deployments(
  id TEXT PRIMARY KEY,
  site_id TEXT NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
  image_ref TEXT NOT NULL,
  image_digest TEXT,
  container_id TEXT,
  container_name TEXT,
  status TEXT NOT NULL,
  trigger TEXT NOT NULL,
  started_at INTEGER NOT NULL,
  finished_at INTEGER,
  error TEXT,
  env_snapshot_enc BLOB
);
CREATE INDEX deployments_site ON deployments(site_id, started_at DESC);
CREATE TABLE cron_jobs(
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  name TEXT NOT NULL,
  schedule TEXT NOT NULL,
  site_id TEXT REFERENCES sites(id) ON DELETE CASCADE,
  argv_json TEXT,
  enabled INTEGER NOT NULL DEFAULT 1,
  system_key TEXT UNIQUE,
  last_fired_minute INTEGER
);
CREATE TABLE cron_runs(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  job_id TEXT NOT NULL REFERENCES cron_jobs(id) ON DELETE CASCADE,
  started_at INTEGER NOT NULL,
  finished_at INTEGER,
  exit_code INTEGER,
  log_tail TEXT NOT NULL DEFAULT ''
);
CREATE TABLE audit_log(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts INTEGER NOT NULL,
  actor TEXT NOT NULL,
  action TEXT NOT NULL,
  target TEXT NOT NULL
);
CREATE TABLE settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE backup_runs(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts INTEGER NOT NULL,
  status TEXT NOT NULL,
  bytes INTEGER NOT NULL,
  remote_path TEXT NOT NULL
);
"#;

pub struct Store {
    conn: Mutex<Connection>,
    path: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SiteRow {
    pub id: String,
    pub name: String,
    pub domain: String,
    pub image_repo: String,
    pub container_port: u16,
    pub ram_limit_mb: u32,
    pub status: String,
    pub current_deployment_id: Option<String>,
    pub registry_id: Option<String>,
    pub zero_downtime: bool,
    pub compat_caps: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeploymentRow {
    pub id: String,
    pub site_id: String,
    pub image_ref: String,
    pub image_digest: Option<String>,
    pub container_id: Option<String>,
    pub container_name: Option<String>,
    pub status: String,
    pub trigger: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub error: Option<String>,
    pub env_snapshot_enc: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TokenRow {
    pub id: String,
    pub site_id: String,
    pub prefix: String,
    pub token_hash: String,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
    pub last_used_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CronRow {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub schedule: String,
    pub site_id: Option<String>,
    pub argv_json: Option<String>,
    pub enabled: bool,
    pub system_key: Option<String>,
    pub last_fired_minute: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CronRunRow {
    pub id: i64,
    pub job_id: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub exit_code: Option<i64>,
    pub log_tail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RegistryRow {
    pub id: String,
    pub name: String,
    pub host: String,
    pub username: String,
    pub secret_enc: Vec<u8>,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BackupRunRow {
    pub id: i64,
    pub ts: i64,
    pub status: String,
    pub bytes: i64,
    pub remote_path: String,
}

fn apply_pragmas(c: &Connection) -> rusqlite::Result<()> {
    c.pragma_update(None, "journal_mode", "WAL")?;
    c.pragma_update(None, "synchronous", "NORMAL")?;
    c.pragma_update(None, "foreign_keys", "ON")?;
    c.pragma_update(None, "cache_size", -512)?;
    c.pragma_update(None, "mmap_size", 0)?;
    c.pragma_update(None, "journal_size_limit", 4 * 1024 * 1024)?;
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}

const SCHEMA_VERSION: i64 = 1;

fn migrate(c: &Connection) -> rusqlite::Result<()> {
    let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if v > SCHEMA_VERSION {
        // Written by a newer Rustle (e.g. after an automatic revert): running on it could corrupt data.
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_MISMATCH),
            Some(format!(
                "database schema v{v} is newer than this binary (v{SCHEMA_VERSION})"
            )),
        ));
    }
    if v < 1 {
        c.execute_batch("BEGIN;")?;
        let r = c
            .execute_batch(SCHEMA_V1)
            .and_then(|_| c.pragma_update(None, "user_version", 1));
        match r {
            Ok(()) => c.execute_batch("COMMIT;")?,
            Err(e) => {
                let _ = c.execute_batch("ROLLBACK;");
                return Err(e);
            }
        }
    }
    Ok(())
}

fn site_from(r: &Row) -> rusqlite::Result<SiteRow> {
    Ok(SiteRow {
        id: r.get(0)?,
        name: r.get(1)?,
        domain: r.get(2)?,
        image_repo: r.get(3)?,
        container_port: r.get::<_, i64>(4)? as u16,
        ram_limit_mb: r.get::<_, i64>(5)? as u32,
        status: r.get(6)?,
        current_deployment_id: r.get(7)?,
        registry_id: r.get(8)?,
        zero_downtime: r.get::<_, i64>(9)? != 0,
        compat_caps: r.get::<_, i64>(10)? != 0,
        created_at: r.get(11)?,
        updated_at: r.get(12)?,
    })
}
const SITE_COLS: &str = "id,name,domain,image_repo,container_port,ram_limit_mb,status,current_deployment_id,registry_id,zero_downtime,compat_caps,created_at,updated_at";

fn dep_from(r: &Row) -> rusqlite::Result<DeploymentRow> {
    Ok(DeploymentRow {
        id: r.get(0)?,
        site_id: r.get(1)?,
        image_ref: r.get(2)?,
        image_digest: r.get(3)?,
        container_id: r.get(4)?,
        container_name: r.get(5)?,
        status: r.get(6)?,
        trigger: r.get(7)?,
        started_at: r.get(8)?,
        finished_at: r.get(9)?,
        error: r.get(10)?,
        env_snapshot_enc: r.get(11)?,
    })
}
const DEP_COLS: &str = "id,site_id,image_ref,image_digest,container_id,container_name,status,trigger,started_at,finished_at,error,env_snapshot_enc";

fn tok_from(r: &Row) -> rusqlite::Result<TokenRow> {
    Ok(TokenRow {
        id: r.get(0)?,
        site_id: r.get(1)?,
        prefix: r.get(2)?,
        token_hash: r.get(3)?,
        created_at: r.get(4)?,
        revoked_at: r.get(5)?,
        last_used_at: r.get(6)?,
    })
}
const TOK_COLS: &str = "id,site_id,prefix,token_hash,created_at,revoked_at,last_used_at";

fn cron_from(r: &Row) -> rusqlite::Result<CronRow> {
    Ok(CronRow {
        id: r.get(0)?,
        kind: r.get(1)?,
        name: r.get(2)?,
        schedule: r.get(3)?,
        site_id: r.get(4)?,
        argv_json: r.get(5)?,
        enabled: r.get::<_, i64>(6)? != 0,
        system_key: r.get(7)?,
        last_fired_minute: r.get(8)?,
    })
}
const CRON_COLS: &str =
    "id,kind,name,schedule,site_id,argv_json,enabled,system_key,last_fired_minute";

impl Store {
    pub fn open(path: &Path) -> AppResult<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(crate::error::AppError::internal)?;
        }
        let c = Connection::open(path)?;
        apply_pragmas(&c)?;
        migrate(&c).map_err(|e| {
            crate::error::AppError::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "db_schema",
                e.to_string(),
            )
        })?;
        Ok(Store {
            conn: Mutex::new(c),
            path: Some(path.to_path_buf()),
        })
    }

    pub fn open_memory() -> AppResult<Store> {
        let c = Connection::open_in_memory()?;
        c.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&c)?;
        Ok(Store {
            conn: Mutex::new(c),
            path: None,
        })
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn c(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    // ---- admin -------------------------------------------------------------------------
    pub fn admin(&self) -> AppResult<Option<(String, String, i64)>> {
        Ok(self
            .c()
            .query_row(
                "SELECT username,password_hash,token_version FROM admin WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
    }

    pub fn create_admin(&self, username: &str, hash: &str) -> AppResult<()> {
        self.c().execute(
            "INSERT INTO admin(id,username,password_hash) VALUES(1,?1,?2)",
            params![username, hash],
        )?;
        Ok(())
    }

    pub fn bump_token_version(&self) -> AppResult<()> {
        self.c().execute(
            "UPDATE admin SET token_version=token_version+1 WHERE id=1",
            [],
        )?;
        Ok(())
    }

    /// Changing the password invalidates every existing session.
    pub fn set_admin_password(&self, hash: &str) -> AppResult<()> {
        self.c().execute(
            "UPDATE admin SET password_hash=?1, token_version=token_version+1 WHERE id=1",
            params![hash],
        )?;
        Ok(())
    }

    // ---- settings ----------------------------------------------------------------------
    pub fn setting(&self, key: &str) -> AppResult<Option<String>> {
        Ok(self
            .c()
            .query_row(
                "SELECT value FROM settings WHERE key=?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> AppResult<()> {
        self.c().execute(
            "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn delete_setting(&self, key: &str) -> AppResult<()> {
        self.c()
            .execute("DELETE FROM settings WHERE key=?1", params![key])?;
        Ok(())
    }

    // ---- registries --------------------------------------------------------------------
    pub fn insert_registry(&self, r: &RegistryRow) -> AppResult<()> {
        self.c().execute(
            "INSERT INTO registries(id,name,host,username,secret_enc,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
            params![r.id, r.name, r.host, r.username, r.secret_enc, r.created_at],
        )?;
        Ok(())
    }

    pub fn list_registries(&self) -> AppResult<Vec<RegistryRow>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT id,name,host,username,secret_enc,created_at FROM registries ORDER BY name",
        )?;
        let rows = st.query_map([], reg_from)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn get_registry(&self, id: &str) -> AppResult<Option<RegistryRow>> {
        Ok(self
            .c()
            .query_row(
                "SELECT id,name,host,username,secret_enc,created_at FROM registries WHERE id=?1",
                params![id],
                reg_from,
            )
            .optional()?)
    }

    pub fn delete_registry(&self, id: &str) -> AppResult<bool> {
        Ok(self
            .c()
            .execute("DELETE FROM registries WHERE id=?1", params![id])?
            > 0)
    }

    // ---- sites -------------------------------------------------------------------------
    pub fn insert_site(&self, s: &SiteRow) -> AppResult<()> {
        self.c().execute(
            "INSERT INTO sites(id,name,domain,image_repo,container_port,ram_limit_mb,status,current_deployment_id,registry_id,zero_downtime,compat_caps,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![s.id, s.name, s.domain, s.image_repo, s.container_port, s.ram_limit_mb, s.status, s.current_deployment_id, s.registry_id, s.zero_downtime as i64, s.compat_caps as i64, s.created_at, s.updated_at],
        )?;
        Ok(())
    }

    pub fn update_site(&self, s: &SiteRow) -> AppResult<()> {
        self.c().execute(
            "UPDATE sites SET domain=?2,image_repo=?3,container_port=?4,ram_limit_mb=?5,status=?6,current_deployment_id=?7,registry_id=?8,zero_downtime=?9,compat_caps=?10,updated_at=?11 WHERE id=?1",
            params![s.id, s.domain, s.image_repo, s.container_port, s.ram_limit_mb, s.status, s.current_deployment_id, s.registry_id, s.zero_downtime as i64, s.compat_caps as i64, s.updated_at],
        )?;
        Ok(())
    }

    pub fn get_site(&self, id: &str) -> AppResult<Option<SiteRow>> {
        Ok(self
            .c()
            .query_row(
                &format!("SELECT {SITE_COLS} FROM sites WHERE id=?1"),
                params![id],
                site_from,
            )
            .optional()?)
    }

    pub fn get_site_by_name(&self, name: &str) -> AppResult<Option<SiteRow>> {
        Ok(self
            .c()
            .query_row(
                &format!("SELECT {SITE_COLS} FROM sites WHERE name=?1"),
                params![name],
                site_from,
            )
            .optional()?)
    }

    pub fn list_sites(&self) -> AppResult<Vec<SiteRow>> {
        let c = self.c();
        let mut st = c.prepare(&format!("SELECT {SITE_COLS} FROM sites ORDER BY name"))?;
        let rows = st
            .query_map([], site_from)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn delete_site(&self, id: &str) -> AppResult<bool> {
        Ok(self
            .c()
            .execute("DELETE FROM sites WHERE id=?1", params![id])?
            > 0)
    }

    /// Sum of RAM limits of all sites, optionally excluding one (for edits).
    pub fn sum_ram_mb(&self, exclude: Option<&str>) -> AppResult<u32> {
        let v: i64 = self.c().query_row(
            "SELECT COALESCE(SUM(ram_limit_mb),0) FROM sites WHERE (?1 IS NULL OR id<>?1)",
            params![exclude],
            |r| r.get(0),
        )?;
        Ok(v as u32)
    }

    pub fn set_site_status(&self, id: &str, status: &str) -> AppResult<()> {
        self.c().execute(
            "UPDATE sites SET status=?2, updated_at=strftime('%s','now') WHERE id=?1",
            params![id, status],
        )?;
        Ok(())
    }

    pub fn set_current_deployment(&self, site_id: &str, dep: Option<&str>) -> AppResult<()> {
        self.c().execute(
            "UPDATE sites SET current_deployment_id=?2, updated_at=strftime('%s','now') WHERE id=?1",
            params![site_id, dep],
        )?;
        Ok(())
    }

    // ---- env ---------------------------------------------------------------------------
    pub fn replace_env(&self, site_id: &str, vars: &[(String, Vec<u8>)]) -> AppResult<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM site_env WHERE site_id=?1", params![site_id])?;
        for (k, v) in vars {
            tx.execute(
                "INSERT INTO site_env(site_id,key,value_enc) VALUES(?1,?2,?3)",
                params![site_id, k, v],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list_env(&self, site_id: &str) -> AppResult<Vec<(String, Vec<u8>)>> {
        let c = self.c();
        let mut st =
            c.prepare("SELECT key,value_enc FROM site_env WHERE site_id=?1 ORDER BY key")?;
        let rows = st
            .query_map(params![site_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ---- deploy tokens -----------------------------------------------------------------
    pub fn insert_token(&self, t: &TokenRow) -> AppResult<()> {
        self.c().execute(
            "INSERT INTO deploy_tokens(id,site_id,prefix,token_hash,created_at) VALUES(?1,?2,?3,?4,?5)",
            params![t.id, t.site_id, t.prefix, t.token_hash, t.created_at],
        )?;
        Ok(())
    }

    pub fn list_tokens(&self, site_id: &str) -> AppResult<Vec<TokenRow>> {
        let c = self.c();
        let mut st = c.prepare(&format!(
            "SELECT {TOK_COLS} FROM deploy_tokens WHERE site_id=?1 ORDER BY created_at DESC"
        ))?;
        let rows = st
            .query_map(params![site_id], tok_from)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn revoke_token(&self, site_id: &str, id: &str, now: i64) -> AppResult<bool> {
        Ok(self.c().execute(
            "UPDATE deploy_tokens SET revoked_at=?3 WHERE id=?2 AND site_id=?1 AND revoked_at IS NULL",
            params![site_id, id, now],
        )? > 0)
    }

    pub fn find_active_token(&self, hash: &str) -> AppResult<Option<TokenRow>> {
        Ok(self
            .c()
            .query_row(
                &format!("SELECT {TOK_COLS} FROM deploy_tokens WHERE token_hash=?1 AND revoked_at IS NULL"),
                params![hash],
                tok_from,
            )
            .optional()?)
    }

    pub fn touch_token(&self, id: &str, now: i64) -> AppResult<()> {
        self.c().execute(
            "UPDATE deploy_tokens SET last_used_at=?2 WHERE id=?1",
            params![id, now],
        )?;
        Ok(())
    }

    // ---- deployments -------------------------------------------------------------------
    pub fn insert_deployment(&self, d: &DeploymentRow) -> AppResult<()> {
        self.c().execute(
            &format!("INSERT INTO deployments({DEP_COLS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)"),
            params![d.id, d.site_id, d.image_ref, d.image_digest, d.container_id, d.container_name, d.status, d.trigger, d.started_at, d.finished_at, d.error, d.env_snapshot_enc],
        )?;
        Ok(())
    }

    pub fn update_deployment(&self, d: &DeploymentRow) -> AppResult<()> {
        self.c().execute(
            "UPDATE deployments SET image_ref=?2,image_digest=?3,container_id=?4,container_name=?5,status=?6,finished_at=?7,error=?8,env_snapshot_enc=?9 WHERE id=?1",
            params![d.id, d.image_ref, d.image_digest, d.container_id, d.container_name, d.status, d.finished_at, d.error, d.env_snapshot_enc],
        )?;
        Ok(())
    }

    pub fn get_deployment(&self, id: &str) -> AppResult<Option<DeploymentRow>> {
        Ok(self
            .c()
            .query_row(
                &format!("SELECT {DEP_COLS} FROM deployments WHERE id=?1"),
                params![id],
                dep_from,
            )
            .optional()?)
    }

    pub fn list_deployments(&self, site_id: &str, limit: u32) -> AppResult<Vec<DeploymentRow>> {
        let c = self.c();
        let mut st = c.prepare(&format!(
            "SELECT {DEP_COLS} FROM deployments WHERE site_id=?1 ORDER BY started_at DESC, rowid DESC LIMIT ?2"
        ))?;
        let rows = st
            .query_map(params![site_id, limit], dep_from)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Image digests that must survive an image prune: the last `keep` successful deployments of every site.
    pub fn protected_digests(&self, keep: u32) -> AppResult<Vec<String>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT image_digest FROM (SELECT image_digest, ROW_NUMBER() OVER (PARTITION BY site_id ORDER BY started_at DESC, rowid DESC) AS rn FROM deployments WHERE image_digest IS NOT NULL AND status IN ('running','superseded')) WHERE rn<=?1",
        )?;
        let rows = st
            .query_map(params![keep], |r| r.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(rows)
    }

    pub fn trim_deployments(&self, site_id: &str, keep: u32) -> AppResult<()> {
        self.c().execute(
            "DELETE FROM deployments WHERE site_id=?1 AND id NOT IN (SELECT id FROM deployments WHERE site_id=?1 ORDER BY started_at DESC, rowid DESC LIMIT ?2) AND status NOT IN ('pending','running')",
            params![site_id, keep],
        )?;
        Ok(())
    }

    /// Deployments left `pending` by a crash are marked failed at boot.
    pub fn fail_stuck_deployments(&self, now: i64) -> AppResult<usize> {
        Ok(self.c().execute(
            "UPDATE deployments SET status='failed', finished_at=?1, error='interrupted by restart' WHERE status='pending'",
            params![now],
        )?)
    }

    // ---- cron --------------------------------------------------------------------------
    pub fn insert_cron(&self, j: &CronRow) -> AppResult<()> {
        self.c().execute(
            &format!("INSERT INTO cron_jobs({CRON_COLS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)"),
            params![
                j.id,
                j.kind,
                j.name,
                j.schedule,
                j.site_id,
                j.argv_json,
                j.enabled as i64,
                j.system_key,
                j.last_fired_minute
            ],
        )?;
        Ok(())
    }

    pub fn list_cron(&self) -> AppResult<Vec<CronRow>> {
        let c = self.c();
        let mut st = c.prepare(&format!(
            "SELECT {CRON_COLS} FROM cron_jobs ORDER BY kind DESC, name"
        ))?;
        let rows = st
            .query_map([], cron_from)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn get_cron(&self, id: &str) -> AppResult<Option<CronRow>> {
        Ok(self
            .c()
            .query_row(
                &format!("SELECT {CRON_COLS} FROM cron_jobs WHERE id=?1"),
                params![id],
                cron_from,
            )
            .optional()?)
    }

    pub fn get_system_cron(&self, key: &str) -> AppResult<Option<CronRow>> {
        Ok(self
            .c()
            .query_row(
                &format!("SELECT {CRON_COLS} FROM cron_jobs WHERE system_key=?1"),
                params![key],
                cron_from,
            )
            .optional()?)
    }

    pub fn update_cron(
        &self,
        id: &str,
        enabled: Option<bool>,
        schedule: Option<&str>,
    ) -> AppResult<bool> {
        let c = self.c();
        let mut n = 0;
        if let Some(e) = enabled {
            n += c.execute(
                "UPDATE cron_jobs SET enabled=?2 WHERE id=?1",
                params![id, e as i64],
            )?;
        }
        if let Some(s) = schedule {
            n += c.execute(
                "UPDATE cron_jobs SET schedule=?2 WHERE id=?1",
                params![id, s],
            )?;
        }
        Ok(n > 0)
    }

    pub fn delete_cron(&self, id: &str) -> AppResult<bool> {
        Ok(self.c().execute(
            "DELETE FROM cron_jobs WHERE id=?1 AND kind<>'system'",
            params![id],
        )? > 0)
    }

    pub fn set_last_fired(&self, id: &str, minute: i64) -> AppResult<()> {
        self.c().execute(
            "UPDATE cron_jobs SET last_fired_minute=?2 WHERE id=?1",
            params![id, minute],
        )?;
        Ok(())
    }

    pub fn insert_run(&self, job_id: &str, started: i64) -> AppResult<i64> {
        let c = self.c();
        c.execute(
            "INSERT INTO cron_runs(job_id,started_at) VALUES(?1,?2)",
            params![job_id, started],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn finish_run(
        &self,
        id: i64,
        finished: i64,
        exit: Option<i64>,
        tail: &str,
    ) -> AppResult<()> {
        self.c().execute(
            "UPDATE cron_runs SET finished_at=?2, exit_code=?3, log_tail=?4 WHERE id=?1",
            params![id, finished, exit, tail],
        )?;
        Ok(())
    }

    pub fn list_runs(&self, job_id: &str, limit: u32) -> AppResult<Vec<CronRunRow>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT id,job_id,started_at,finished_at,exit_code,log_tail FROM cron_runs WHERE job_id=?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = st
            .query_map(params![job_id, limit], run_from)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn last_run(&self, job_id: &str) -> AppResult<Option<CronRunRow>> {
        Ok(self
            .c()
            .query_row(
                "SELECT id,job_id,started_at,finished_at,exit_code,log_tail FROM cron_runs WHERE job_id=?1 ORDER BY id DESC LIMIT 1",
                params![job_id],
                run_from,
            )
            .optional()?)
    }

    pub fn trim_runs(&self, job_id: &str, keep: u32) -> AppResult<()> {
        self.c().execute(
            "DELETE FROM cron_runs WHERE job_id=?1 AND id NOT IN (SELECT id FROM cron_runs WHERE job_id=?1 ORDER BY id DESC LIMIT ?2)",
            params![job_id, keep],
        )?;
        Ok(())
    }

    /// Runs left open by a crash are closed at boot.
    pub fn fail_stuck_runs(&self, now: i64) -> AppResult<usize> {
        Ok(self.c().execute(
            "UPDATE cron_runs SET finished_at=?1, exit_code=-1, log_tail='interrupted by restart' WHERE finished_at IS NULL",
            params![now],
        )?)
    }

    // ---- audit / backups ---------------------------------------------------------------
    pub fn audit(&self, ts: i64, actor: &str, action: &str, target: &str) {
        let r = self.c().execute(
            "INSERT INTO audit_log(ts,actor,action,target) VALUES(?1,?2,?3,?4)",
            params![ts, actor, action, target],
        );
        if let Err(e) = r {
            tracing::warn!("audit insert failed: {e}");
        }
    }

    pub fn trim_audit(&self, older_than: i64) -> AppResult<()> {
        self.c()
            .execute("DELETE FROM audit_log WHERE ts<?1", params![older_than])?;
        Ok(())
    }

    /// (actor, action, target), oldest first. Tests only: there is no API for reading the audit log yet.
    #[cfg(test)]
    pub fn audit_entries(&self) -> Vec<(String, String, String)> {
        let c = self.c();
        let mut st = c
            .prepare("SELECT actor,action,target FROM audit_log ORDER BY id")
            .unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    pub fn audit_count(&self) -> AppResult<i64> {
        Ok(self
            .c()
            .query_row("SELECT COUNT(*) FROM audit_log", [], |r| r.get(0))?)
    }

    pub fn insert_backup_run(
        &self,
        ts: i64,
        status: &str,
        bytes: i64,
        remote_path: &str,
    ) -> AppResult<()> {
        self.c().execute(
            "INSERT INTO backup_runs(ts,status,bytes,remote_path) VALUES(?1,?2,?3,?4)",
            params![ts, status, bytes, remote_path],
        )?;
        Ok(())
    }

    pub fn list_backup_runs(&self, limit: u32) -> AppResult<Vec<BackupRunRow>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT id,ts,status,bytes,remote_path FROM backup_runs ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = st
            .query_map(params![limit], |r| {
                Ok(BackupRunRow {
                    id: r.get(0)?,
                    ts: r.get(1)?,
                    status: r.get(2)?,
                    bytes: r.get(3)?,
                    remote_path: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn trim_backup_runs(&self, keep: u32) -> AppResult<()> {
        self.c().execute(
            "DELETE FROM backup_runs WHERE id NOT IN (SELECT id FROM backup_runs ORDER BY id DESC LIMIT ?1)",
            params![keep],
        )?;
        Ok(())
    }
}

fn reg_from(r: &Row) -> rusqlite::Result<RegistryRow> {
    Ok(RegistryRow {
        id: r.get(0)?,
        name: r.get(1)?,
        host: r.get(2)?,
        username: r.get(3)?,
        secret_enc: r.get(4)?,
        created_at: r.get(5)?,
    })
}

fn run_from(r: &Row) -> rusqlite::Result<CronRunRow> {
    Ok(CronRunRow {
        id: r.get(0)?,
        job_id: r.get(1)?,
        started_at: r.get(2)?,
        finished_at: r.get(3)?,
        exit_code: r.get(4)?,
        log_tail: r.get(5)?,
    })
}

/// Create a consistent copy of the database using a *separate read-only connection*
/// so the shared connection is never blocked (call from `spawn_blocking`).
pub fn snapshot_to(db_path: &Path, dest: &Path) -> AppResult<()> {
    use rusqlite::OpenFlags;
    let c = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    c.busy_timeout(std::time::Duration::from_secs(10))?;
    let _ = std::fs::remove_file(dest);
    let dest_s = dest
        .to_str()
        .ok_or_else(|| crate::error::AppError::internal("non-utf8 path"))?;
    c.execute("VACUUM INTO ?1", params![dest_s])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn site(id: &str, name: &str, domain: &str, ram: u32) -> SiteRow {
        SiteRow {
            id: id.into(),
            name: name.into(),
            domain: domain.into(),
            image_repo: format!("ghcr.io/o/{name}"),
            container_port: 3000,
            ram_limit_mb: ram,
            status: "created".into(),
            current_deployment_id: None,
            registry_id: None,
            zero_downtime: false,
            compat_caps: false,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn dep(id: &str, site: &str, t: i64, status: &str, digest: Option<&str>) -> DeploymentRow {
        DeploymentRow {
            id: id.into(),
            site_id: site.into(),
            image_ref: "ghcr.io/o/a:1".into(),
            image_digest: digest.map(String::from),
            container_id: None,
            container_name: None,
            status: status.into(),
            trigger: "webhook".into(),
            started_at: t,
            finished_at: None,
            error: None,
            env_snapshot_enc: None,
        }
    }

    #[test]
    fn sites_crud_unique_and_budget() {
        let s = Store::open_memory().unwrap();
        s.insert_site(&site("1", "a", "a.example.com", 256))
            .unwrap();
        s.insert_site(&site("2", "b", "b.example.com", 128))
            .unwrap();
        assert!(
            s.insert_site(&site("3", "a", "c.example.com", 64)).is_err(),
            "name unique"
        );
        assert!(
            s.insert_site(&site("4", "d", "a.example.com", 64)).is_err(),
            "domain unique"
        );
        assert_eq!(s.sum_ram_mb(None).unwrap(), 384);
        assert_eq!(s.sum_ram_mb(Some("1")).unwrap(), 128);
        assert_eq!(s.list_sites().unwrap().len(), 2);
        assert!(s.delete_site("1").unwrap());
        assert!(s.get_site("1").unwrap().is_none());
    }

    #[test]
    fn env_replace_and_cascade() {
        let s = Store::open_memory().unwrap();
        s.insert_site(&site("1", "a", "a.example.com", 256))
            .unwrap();
        s.replace_env("1", &[("B".into(), vec![1]), ("A".into(), vec![2])])
            .unwrap();
        assert_eq!(s.list_env("1").unwrap()[0].0, "A");
        s.replace_env("1", &[("C".into(), vec![3])]).unwrap();
        assert_eq!(s.list_env("1").unwrap().len(), 1);
        s.delete_site("1").unwrap();
        assert!(s.list_env("1").unwrap().is_empty());
    }

    #[test]
    fn tokens_revoke() {
        let s = Store::open_memory().unwrap();
        s.insert_site(&site("1", "a", "a.example.com", 256))
            .unwrap();
        let t = TokenRow {
            id: "t1".into(),
            site_id: "1".into(),
            prefix: "rt_ab".into(),
            token_hash: "h".into(),
            created_at: 1,
            revoked_at: None,
            last_used_at: None,
        };
        s.insert_token(&t).unwrap();
        assert!(s.find_active_token("h").unwrap().is_some());
        assert!(
            !s.revoke_token("other", "t1", 5).unwrap(),
            "wrong site cannot revoke"
        );
        assert!(s.revoke_token("1", "t1", 5).unwrap());
        assert!(s.find_active_token("h").unwrap().is_none());
    }

    #[test]
    fn deployments_protected_digests_and_trim() {
        let s = Store::open_memory().unwrap();
        s.insert_site(&site("1", "a", "a.example.com", 256))
            .unwrap();
        for i in 0..6 {
            s.insert_deployment(&dep(
                &format!("d{i}"),
                "1",
                i,
                "superseded",
                Some(&format!("sha256:{i}")),
            ))
            .unwrap();
        }
        s.insert_deployment(&dep("f", "1", 10, "failed", Some("sha256:failed")))
            .unwrap();
        let p = s.protected_digests(3).unwrap();
        assert_eq!(p.len(), 3);
        assert!(p.contains(&"sha256:5".to_string()) && !p.contains(&"sha256:failed".to_string()));
        s.trim_deployments("1", 2).unwrap();
        assert_eq!(s.list_deployments("1", 100).unwrap().len(), 2);
    }

    #[test]
    fn stuck_marked_failed() {
        let s = Store::open_memory().unwrap();
        s.insert_site(&site("1", "a", "a.example.com", 256))
            .unwrap();
        s.insert_deployment(&dep("p", "1", 1, "pending", None))
            .unwrap();
        assert_eq!(s.fail_stuck_deployments(9).unwrap(), 1);
        assert_eq!(s.get_deployment("p").unwrap().unwrap().status, "failed");
    }

    #[test]
    fn admin_password_bumps_version() {
        let s = Store::open_memory().unwrap();
        assert!(s.admin().unwrap().is_none());
        s.create_admin("admin", "h1").unwrap();
        assert!(s.create_admin("x", "h").is_err());
        s.set_admin_password("h2").unwrap();
        let (_, h, tv) = s.admin().unwrap().unwrap();
        assert_eq!((h.as_str(), tv), ("h2", 1));
    }

    #[test]
    fn cron_and_runs() {
        let s = Store::open_memory().unwrap();
        let j = CronRow {
            id: "j".into(),
            kind: "system".into(),
            name: "prune".into(),
            schedule: "0 2 * * 0".into(),
            site_id: None,
            argv_json: None,
            enabled: true,
            system_key: Some("prune".into()),
            last_fired_minute: None,
        };
        s.insert_cron(&j).unwrap();
        assert!(
            !s.delete_cron("j").unwrap(),
            "system jobs cannot be deleted"
        );
        assert!(s.update_cron("j", Some(false), None).unwrap());
        assert!(!s.get_cron("j").unwrap().unwrap().enabled);
        for i in 0..5 {
            let id = s.insert_run("j", i).unwrap();
            s.finish_run(id, i + 1, Some(0), "ok").unwrap();
        }
        s.trim_runs("j", 2).unwrap();
        assert_eq!(s.list_runs("j", 10).unwrap().len(), 2);
        let open = s.insert_run("j", 50).unwrap();
        assert_eq!(s.fail_stuck_runs(60).unwrap(), 1);
        assert_eq!(s.last_run("j").unwrap().unwrap().id, open);
    }

    #[test]
    fn settings_and_audit_and_backups() {
        let s = Store::open_memory().unwrap();
        assert!(s.setting("k").unwrap().is_none());
        s.set_setting("k", "v1").unwrap();
        s.set_setting("k", "v2").unwrap();
        assert_eq!(s.setting("k").unwrap().as_deref(), Some("v2"));
        s.audit(1, "admin", "login", "-");
        s.audit(100, "admin", "login", "-");
        s.trim_audit(50).unwrap();
        assert_eq!(s.audit_count().unwrap(), 1);
        for i in 0..5 {
            s.insert_backup_run(i, "ok", 10, "p").unwrap();
        }
        s.trim_backup_runs(3).unwrap();
        assert_eq!(s.list_backup_runs(10).unwrap().len(), 3);
    }

    #[test]
    fn refuses_a_database_from_a_newer_version() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.db");
        drop(Store::open(&p).unwrap());
        Connection::open(&p)
            .unwrap()
            .pragma_update(None, "user_version", 99)
            .unwrap();
        let err = Store::open(&p).err().expect("must refuse");
        assert!(err.message.contains("newer"), "{}", err.message);
    }

    #[test]
    fn snapshot_copy_is_consistent() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.db");
        let s = Store::open(&p).unwrap();
        s.set_setting("x", "1").unwrap();
        let dest = d.path().join("snap.db");
        snapshot_to(&p, &dest).unwrap();
        let s2 = Store::open(&dest).unwrap();
        assert_eq!(s2.setting("x").unwrap().as_deref(), Some("1"));
    }
}
