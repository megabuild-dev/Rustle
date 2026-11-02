//! Encrypted backups of the SQLite database to a PRIVATE GitHub repository (Contents API).
//! The master key never leaves the machine: without it a backup is useless, which is why
//! `rustle init` makes the operator acknowledge having saved it.

use crate::app::AppState;
use crate::error::{AppError, AppResult};
use crate::store::snapshot_to;
use crate::util::{now, stamp};
use rustle_types::BackupConfig;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const AAD: &[u8] = b"rustle-backup-v1";
const KEEP: usize = 14;
const MAX_DB_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Stored {
    enabled: bool,
    repo: String,
    branch: String,
    path_prefix: String,
}

fn load(state: &AppState) -> Stored {
    state
        .store
        .setting("backup_config")
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or(Stored {
            enabled: false,
            repo: String::new(),
            branch: "main".into(),
            path_prefix: "rustle-backups".into(),
        })
}

fn token(state: &AppState) -> Option<String> {
    let hexed = state.store.setting("backup_token_enc").ok().flatten()?;
    let blob = hex::decode(hexed).ok()?;
    state.key.open_str(&blob, "backup-token").ok()
}

pub fn get_config(state: &AppState) -> BackupConfig {
    let s = load(state);
    BackupConfig {
        enabled: s.enabled,
        repo: s.repo,
        branch: s.branch,
        path_prefix: s.path_prefix,
        token: None,
        token_set: token(state).is_some(),
    }
}

fn valid_repo(r: &str) -> bool {
    let mut it = r.split('/');
    let ok = |p: &str| {
        !p.is_empty()
            && p.len() <= 100
            && p.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c == b'.')
    };
    matches!((it.next(), it.next(), it.next()), (Some(a), Some(b), None) if ok(a) && ok(b))
}

fn valid_branch_or_path(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && !s.starts_with('/')
        && !s.contains("..")
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'/'))
}

pub async fn set_config(state: &AppState, cfg: BackupConfig) -> AppResult<()> {
    let mut stored = Stored {
        enabled: cfg.enabled,
        repo: cfg.repo.trim().to_string(),
        branch: if cfg.branch.is_empty() {
            "main".into()
        } else {
            cfg.branch.trim().to_string()
        },
        path_prefix: if cfg.path_prefix.is_empty() {
            "rustle-backups".into()
        } else {
            cfg.path_prefix.trim().to_string()
        },
    };
    if stored.enabled {
        if !valid_repo(&stored.repo) {
            return Err(AppError::bad_request("repo must look like owner/name"));
        }
        if !valid_branch_or_path(&stored.branch) || !valid_branch_or_path(&stored.path_prefix) {
            return Err(AppError::bad_request("invalid branch or path prefix"));
        }
        let tok = match &cfg.token {
            Some(t) if !t.is_empty() => t.clone(),
            _ => token(state).ok_or_else(|| AppError::bad_request("a GitHub token is required"))?,
        };
        // An encrypted backup in a public repository is still a leak of ciphertext: refuse.
        match state.github.repo_private(&tok, &stored.repo).await {
            Ok(true) => {}
            Ok(false) => {
                return Err(AppError::unprocessable(
                    "repo_public",
                    "the backup repository must be private",
                ))
            }
            Err(e) => return Err(AppError::unprocessable("github_error", e.to_string())),
        }
        if let Some(t) = cfg.token.filter(|t| !t.is_empty()) {
            state.store.set_setting(
                "backup_token_enc",
                &hex::encode(state.key.seal_str(&t, "backup-token")),
            )?;
        }
    } else {
        stored.repo = stored.repo.chars().take(200).collect();
    }
    state.store.set_setting(
        "backup_config",
        &serde_json::to_string(&stored).map_err(AppError::internal)?,
    )?;
    state
        .store
        .audit(now(), "admin", "backup.config", &stored.repo);
    Ok(())
}

/// One backup run. Returns a human readable summary; `Err` is recorded as a failed run.
pub async fn run(state: &Arc<AppState>) -> Result<String, String> {
    let cfg = load(state);
    if !cfg.enabled {
        return Ok("skipped: backups are not enabled".into());
    }
    let tok = token(state).ok_or("backup token missing")?;
    let t = now();
    let remote = format!("{}/rustle-{}.db.enc", cfg.path_prefix, stamp(t));
    let result = do_backup(state, &cfg, &tok, &remote).await;
    match &result {
        Ok(bytes) => {
            let _ = state
                .store
                .insert_backup_run(t, "ok", *bytes as i64, &remote);
        }
        Err(e) => {
            let _ = state.store.insert_backup_run(
                t,
                &format!("failed: {}", e.chars().take(120).collect::<String>()),
                0,
                &remote,
            );
        }
    }
    result.map(|b| format!("uploaded {remote} ({b} bytes)"))
}

/// The whole file is held in memory (plain + ciphertext + base64): refuse when it is unreasonably large.
fn check_size(len: usize) -> Result<(), String> {
    if len > MAX_DB_BYTES {
        return Err(format!(
            "database is {} MiB, above the {} MiB backup limit",
            len >> 20,
            MAX_DB_BYTES >> 20
        ));
    }
    Ok(())
}

async fn do_backup(
    state: &Arc<AppState>,
    cfg: &Stored,
    tok: &str,
    remote: &str,
) -> Result<usize, String> {
    let db = state
        .store
        .db_path()
        .ok_or("database is not file backed")?
        .to_path_buf();
    let tmp = state
        .cfg
        .state_dir
        .join(format!("backup-tmp-{}.db", crate::util::rand_hex(4)));
    let (db2, tmp2) = (db.clone(), tmp.clone());
    // Separate read-only connection on the blocking pool: the shared connection stays free.
    tokio::task::spawn_blocking(move || snapshot_to(&db2, &tmp2))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.message)?;
    let plain = tokio::fs::read(&tmp).await.map_err(|e| e.to_string());
    let _ = tokio::fs::remove_file(&tmp).await;
    let plain = plain?;
    check_size(plain.len())?;
    let enc = state.key.seal(&plain, AAD);
    state
        .github
        .put_file(tok, &cfg.repo, &cfg.branch, remote, &enc, "rustle backup")
        .await
        .map_err(|e| e.to_string())?;
    // Retention: keep the newest KEEP files (names embed a sortable UTC stamp).
    let mut files = state
        .github
        .list_dir(tok, &cfg.repo, &cfg.branch, &cfg.path_prefix)
        .await
        .map_err(|e| e.to_string())?;
    files.retain(|f| f.name.starts_with("rustle-") && f.name.ends_with(".db.enc"));
    files.sort_by(|a, b| b.name.cmp(&a.name));
    for old in files.into_iter().skip(KEEP) {
        if let Err(e) = state
            .github
            .delete_file(
                tok,
                &cfg.repo,
                &cfg.branch,
                &old.path,
                &old.sha,
                "rustle backup retention",
            )
            .await
        {
            tracing::warn!("could not delete old backup {}: {e}", old.path);
        }
    }
    Ok(enc.len())
}

pub fn decrypt(key: &crate::crypto::MasterKey, blob: &[u8]) -> Result<Vec<u8>, String> {
    key.open(blob, AAD)
        .map_err(|_| "cannot decrypt: wrong master key or corrupted file".to_string())
}

/// Used by `rustle restore`: decrypt `blob` and write the database to `dest`.
pub fn restore_to(
    key: &crate::crypto::MasterKey,
    blob: &[u8],
    dest: &std::path::Path,
    force: bool,
) -> Result<(), String> {
    if dest.exists() && !force {
        return Err(format!(
            "{} already exists; use --force to overwrite",
            dest.display()
        ));
    }
    let plain = decrypt(key, blob)?;
    if plain.len() < 16 || &plain[..15] != b"SQLite format 3" {
        return Err("decrypted data is not a SQLite database".into());
    }
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    for ext in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", dest.display()));
    }
    // Write next to the destination and rename, so a crash never leaves a half-written database,
    // and give the file to the owner of its directory: `rustle restore` is typically run with sudo,
    // while the service runs as an unprivileged user that must be able to open it read-write.
    let tmp = dest.with_extension("restore-tmp");
    std::fs::write(&tmp, plain).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    if let Some(dir) = dest.parent() {
        use std::os::unix::fs::MetadataExt;
        if let Ok(m) = std::fs::metadata(dir) {
            let _ = std::os::unix::fs::chown(&tmp, Some(m.uid()), Some(m.gid()));
        }
    }
    std::fs::rename(&tmp, dest).map_err(|e| e.to_string())
}

pub fn history(state: &AppState) -> AppResult<Vec<rustle_types::BackupRunDto>> {
    Ok(state
        .store
        .list_backup_runs(30)?
        .into_iter()
        .map(|r| rustle_types::BackupRunDto {
            id: r.id,
            ts: r.ts,
            status: r.status,
            bytes: r.bytes,
            remote_path: r.remote_path,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AppState, FakeHealth, FakeResolver, Tuning};
    use crate::auth::AuthState;
    use crate::config::Config;
    use crate::crypto::MasterKey;
    use crate::deploy::Deployer;
    use crate::github::Github;
    use crate::store::Store;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::Mutex;

    /// A state with a file-backed database (backups need a real file).
    fn file_env() -> (
        Arc<AppState>,
        Arc<crate::github::FakeGithub>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::for_tests(dir.path());
        let gh = Arc::new(crate::github::FakeGithub::new());
        let key = MasterKey::from_bytes([9; 32]);
        let st = Arc::new(AppState {
            store: Store::open(&cfg.db_path).unwrap(),
            auth: AuthState::new(&key),
            key,
            runtime: Arc::new(crate::docker::FakeRuntime::new()),
            gateway: Arc::new(crate::caddy::FakeGateway::new()),
            health: Arc::new(FakeHealth::new(true)),
            resolver: Arc::new(FakeResolver::default()),
            github: gh.clone(),
            deployer: Deployer::default(),
            sampler: Mutex::new(Default::default()),
            stats_subscribers: AtomicUsize::new(0),
            degraded: Mutex::new(None),
            maintenance: AtomicBool::new(false),
            tuning: Tuning::default(),
            running_jobs: Mutex::new(HashSet::new()),
            cfg,
        });
        (st, gh, dir)
    }

    fn cfg(enabled: bool) -> BackupConfig {
        BackupConfig {
            enabled,
            repo: "me/backups".into(),
            branch: "main".into(),
            path_prefix: "rustle-backups".into(),
            token: Some("ghp_x".into()),
            token_set: false,
        }
    }

    #[tokio::test]
    async fn config_validation_and_token_handling() {
        let (st, gh, _d) = file_env();
        let mut bad = cfg(true);
        bad.repo = "not-a-repo".into();
        assert!(set_config(&st, bad).await.is_err());
        let mut bad = cfg(true);
        bad.path_prefix = "../etc".into();
        assert!(set_config(&st, bad).await.is_err());
        gh.set_private(false);
        assert_eq!(
            set_config(&st, cfg(true)).await.unwrap_err().code,
            "repo_public"
        );
        gh.set_private(true);
        let mut no_token = cfg(true);
        no_token.token = None;
        assert!(
            set_config(&st, no_token.clone()).await.is_err(),
            "needs a token the first time"
        );
        set_config(&st, cfg(true)).await.unwrap();
        let got = get_config(&st);
        assert!(
            got.enabled && got.token_set && got.token.is_none(),
            "token is write-only"
        );
        // token is sealed at rest
        let raw = st.store.setting("backup_token_enc").unwrap().unwrap();
        assert!(!raw.contains("ghp_x"));
        // later edits may omit the token
        set_config(&st, no_token).await.unwrap();
        assert!(get_config(&st).token_set);
    }

    #[tokio::test]
    async fn backup_is_encrypted_restorable_and_retained() {
        let (st, gh, dir) = file_env();
        st.store.set_setting("marker", "hello-backup").unwrap();
        set_config(&st, cfg(true)).await.unwrap();
        let msg = run(&st).await.unwrap();
        assert!(msg.contains("rustle-backups/rustle-"));
        let files = gh.files();
        assert_eq!(files.len(), 1);
        let blob = files.values().next().unwrap().clone();
        assert!(
            !blob.windows(15).any(|w| w == b"SQLite format 3"),
            "ciphertext only"
        );
        assert!(!blob.windows(12).any(|w| w == b"hello-backup"));
        // restore to a fresh location and read the marker back
        let dest = dir.path().join("restored/rustle.db");
        restore_to(&st.key, &blob, &dest, false).unwrap();
        assert_eq!(
            Store::open(&dest)
                .unwrap()
                .setting("marker")
                .unwrap()
                .as_deref(),
            Some("hello-backup")
        );
        assert!(
            restore_to(&st.key, &blob, &dest, false).is_err(),
            "refuses to overwrite without --force"
        );
        restore_to(&st.key, &blob, &dest, true).unwrap();
        // wrong key cannot restore
        let other = MasterKey::from_bytes([1; 32]);
        assert!(restore_to(&other, &blob, &dir.path().join("x.db"), false).is_err());
        // no temp snapshot left behind
        assert!(std::fs::read_dir(&st.cfg.state_dir).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("backup-tmp")));
        let h = history(&st).unwrap();
        assert_eq!(h[0].status, "ok");
    }

    #[tokio::test]
    async fn retention_keeps_newest_fourteen() {
        let (st, gh, _d) = file_env();
        set_config(&st, cfg(true)).await.unwrap();
        for i in 0..20 {
            gh.put_file(
                "t",
                "me/backups",
                "main",
                &format!("rustle-backups/rustle-20260101-0000{i:02}.db.enc"),
                b"x",
                "m",
            )
            .await
            .unwrap();
        }
        gh.put_file(
            "t",
            "me/backups",
            "main",
            "rustle-backups/README.md",
            b"keep me",
            "m",
        )
        .await
        .unwrap();
        run(&st).await.unwrap();
        let names: Vec<String> = gh.files().keys().cloned().collect();
        let backups = names.iter().filter(|n| n.ends_with(".db.enc")).count();
        assert_eq!(backups, 14);
        assert!(
            names.iter().any(|n| n.ends_with("README.md")),
            "unrelated files are never deleted"
        );
        assert!(
            !names.iter().any(|n| n.ends_with("000000.db.enc")),
            "oldest pruned"
        );
    }

    #[tokio::test]
    async fn failure_is_recorded_and_disabled_is_a_noop() {
        let (st, gh, _d) = file_env();
        assert!(run(&st).await.unwrap().starts_with("skipped"));
        set_config(&st, cfg(true)).await.unwrap();
        gh.set_fail(Some("network down"));
        assert!(run(&st).await.is_err());
        assert!(history(&st).unwrap()[0].status.starts_with("failed"));
    }

    #[test]
    fn restore_rejects_non_sqlite() {
        let key = MasterKey::from_bytes([2; 32]);
        let blob = key.seal(b"this is not a database at all.....", AAD);
        let d = tempfile::tempdir().unwrap();
        assert!(restore_to(&key, &blob, &d.path().join("x.db"), false)
            .unwrap_err()
            .contains("not a SQLite"));
    }

    #[test]
    fn repo_names() {
        for ok in [
            "o/r",
            "a-b_c.d/e",
            &format!("{}/{}", "a".repeat(100), "b".repeat(100)),
        ] {
            assert!(valid_repo(ok), "{ok}");
        }
        for bad in [
            "",
            "o",
            "o/",
            "/r",
            "o/r/x",
            "o r/x",
            "o/r$",
            "o//r",
            &format!("{}/b", "a".repeat(101)),
            &format!("a/{}", "b".repeat(101)),
        ] {
            assert!(!valid_repo(bad), "{bad}");
        }
    }

    #[test]
    fn branch_and_path_names() {
        for ok in ["main", "rustle-backups", "a/b_c.d-e", &"a".repeat(100)] {
            assert!(valid_branch_or_path(ok), "{ok}");
        }
        for bad in ["", "/abs", "a..b", "a b", "a:b", "a\\b", &"a".repeat(101)] {
            assert!(!valid_branch_or_path(bad), "{bad}");
        }
    }

    #[test]
    fn size_limit_is_exactly_16_mib() {
        assert_eq!(MAX_DB_BYTES, 16 * 1024 * 1024);
        assert!(check_size(0).is_ok());
        assert!(check_size(MAX_DB_BYTES).is_ok());
        let e = check_size(MAX_DB_BYTES + 1).unwrap_err();
        assert!(e.contains("16 MiB"), "{e}");
        assert_eq!(KEEP, 14);
    }

    #[test]
    fn restore_requires_a_real_sqlite_header() {
        let key = crate::crypto::MasterKey::from_bytes([5; 32]);
        let d = tempfile::tempdir().unwrap();
        let dest = d.path().join("rustle.db");
        let attempt = |plain: &[u8]| restore_to(&key, &key.seal(plain, AAD), &dest, true);
        assert!(
            attempt(b"SQLite format 3")
                .unwrap_err()
                .contains("not a SQLite"),
            "15 bytes"
        );
        assert!(attempt(b"SQLite format 4\0")
            .unwrap_err()
            .contains("not a SQLite"));
        assert!(attempt(b"short").unwrap_err().contains("not a SQLite"));
        assert!(!dest.exists());
        attempt(b"SQLite format 3\0").expect("16 bytes with the header is accepted");
        assert_eq!(std::fs::read(&dest).unwrap(), b"SQLite format 3\0");
        // without --force an existing database is never replaced
        let e = restore_to(
            &key,
            &key.seal(b"SQLite format 3\0 more", AAD),
            &dest,
            false,
        )
        .unwrap_err();
        assert!(e.contains("already exists"), "{e}");
        assert_eq!(std::fs::read(&dest).unwrap(), b"SQLite format 3\0");
        // a wrong key is reported as such
        let other = crate::crypto::MasterKey::from_bytes([6; 32]);
        let e = restore_to(&other, &key.seal(b"SQLite format 3\0", AAD), &dest, true).unwrap_err();
        assert!(e.contains("wrong master key"), "{e}");
    }

    #[tokio::test]
    async fn retention_only_touches_backup_files_and_keeps_the_newest() {
        let (st, gh, _d) = file_env();
        for i in 0..16 {
            gh.put_file(
                "t",
                "o/r",
                "main",
                &format!("p/rustle-2026010{:02}.db.enc", i),
                b"x",
                "m",
            )
            .await
            .unwrap();
        }
        for other in ["p/notes.txt", "p/rustle-keep.txt", "p/other.db.enc"] {
            gh.put_file("t", "o/r", "main", other, b"x", "m")
                .await
                .unwrap();
        }
        let cfg = Stored {
            enabled: true,
            repo: "o/r".into(),
            branch: "main".into(),
            path_prefix: "p".into(),
        };
        do_backup(&st, &cfg, "t", "p/rustle-20260199.db.enc")
            .await
            .unwrap();
        let files = gh.files();
        let backups: Vec<&String> = files
            .keys()
            .filter(|k| k.starts_with("p/rustle-") && k.ends_with(".db.enc"))
            .collect();
        assert_eq!(backups.len(), KEEP, "newest {KEEP} remain");
        assert!(
            files.contains_key("p/rustle-20260199.db.enc"),
            "the new one is kept"
        );
        assert!(
            !files.contains_key("p/rustle-202601000.db.enc")
                && !files.keys().any(|k| k == "p/rustle-20260100.db.enc")
        );
        for other in ["p/notes.txt", "p/rustle-keep.txt", "p/other.db.enc"] {
            assert!(files.contains_key(other), "{other} must never be deleted");
        }
    }

    #[tokio::test]
    async fn an_empty_token_in_the_form_keeps_the_stored_one() {
        let (st, gh, _d) = file_env();
        let cfg = |token: Option<&str>| BackupConfig {
            enabled: true,
            repo: "o/r".into(),
            branch: String::new(),
            path_prefix: String::new(),
            token: token.map(String::from),
            ..Default::default()
        };
        assert!(
            set_config(&st, cfg(None)).await.is_err(),
            "no token anywhere"
        );
        set_config(&st, cfg(Some("first-token"))).await.unwrap();
        set_config(&st, cfg(Some(""))).await.unwrap();
        set_config(&st, cfg(None)).await.unwrap();
        set_config(&st, cfg(Some("second-token"))).await.unwrap();
        set_config(&st, cfg(Some(""))).await.unwrap();
        assert_eq!(
            gh.tokens_seen(),
            vec![
                "first-token",
                "first-token",
                "first-token",
                "second-token",
                "second-token"
            ]
        );
    }
}
