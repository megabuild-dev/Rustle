//! Self-update from GitHub Releases. The release asset must carry a detached ed25519 signature
//! that verifies against the public key compiled into this binary; without a key, updating is
//! disabled. The binary lives in `<state>/bin/` so it can be replaced under `ProtectSystem=strict`.

use crate::app::AppState;
use crate::config::VERSION;
use crate::crypto::ed25519_verify;
use crate::error::{AppError, AppResult};
use rustle_types::UpdateInfo;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

const MAX_BINARY: usize = 40 * 1024 * 1024;

pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.trim().trim_start_matches('v').split(['-', '+']).next()?;
    let mut it = core.split('.');
    let v = (
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    );
    it.next().is_none().then_some(v)
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse_version(latest), parse_version(current)), (Some(l), Some(c)) if l > c)
}

pub fn asset_name() -> String {
    format!("rustle-{}-linux", std::env::consts::ARCH)
}

pub async fn check(state: &AppState) -> UpdateInfo {
    let mut info = UpdateInfo {
        current: VERSION.to_string(),
        ..Default::default()
    };
    if state.cfg.update_pubkey.is_none() {
        info.reason = Some("this build has no update-signing key; updates are disabled".into());
        return info;
    }
    info.enabled = true;
    match state.github.latest_release(&state.cfg.release_repo).await {
        Ok(rel) => {
            info.update_available = is_newer(&rel.tag, VERSION);
            info.latest = Some(rel.tag);
        }
        Err(e) => info.reason = Some(e.to_string()),
    }
    info
}

fn decode_signature(raw: &[u8]) -> Option<Vec<u8>> {
    if raw.len() == 64 {
        return Some(raw.to_vec());
    }
    let text = std::str::from_utf8(raw).ok()?.trim();
    hex::decode(text)
        .ok()
        .filter(|v| v.len() == 64)
        .or_else(|| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(text)
                .ok()
                .filter(|v| v.len() == 64)
        })
}

/// Download, verify and install the latest release. On success the caller must exit the process
/// so systemd starts the new binary; `maintenance` stays set so no new work begins meanwhile.
pub async fn apply(state: &Arc<AppState>) -> AppResult<String> {
    let pubkey = state
        .cfg
        .update_pubkey
        .ok_or_else(|| AppError::conflict("updates are disabled in this build"))?;
    let rel = state
        .github
        .latest_release(&state.cfg.release_repo)
        .await
        .map_err(|e| AppError::unavailable(e.to_string()))?;
    if !is_newer(&rel.tag, VERSION) {
        return Err(AppError::conflict("already up to date"));
    }
    let name = asset_name();
    let find = |n: &str| {
        rel.assets
            .iter()
            .find(|(a, _)| a == n)
            .map(|(_, u)| u.clone())
    };
    let bin_url = find(&name).ok_or_else(|| {
        AppError::unprocessable("no_asset", format!("release has no {name} asset"))
    })?;
    let sig_url = find(&format!("{name}.sig"))
        .ok_or_else(|| AppError::unprocessable("no_signature", "release has no signature asset"))?;

    state.maintenance.store(true, Ordering::SeqCst);
    let result = async {
        if !state.deployer.wait_all_idle(Duration::from_secs(120)).await {
            return Err(AppError::conflict(
                "deployments are still running; try again shortly",
            ));
        }
        let bin = state
            .github
            .download(&bin_url, MAX_BINARY)
            .await
            .map_err(|e| AppError::unavailable(e.to_string()))?;
        let sig_raw = state
            .github
            .download(&sig_url, 4096)
            .await
            .map_err(|e| AppError::unavailable(e.to_string()))?;
        let sig = decode_signature(&sig_raw).ok_or_else(|| {
            AppError::unprocessable("bad_signature", "signature file is malformed")
        })?;
        if !ed25519_verify(&pubkey, &bin, &sig) {
            return Err(AppError::unprocessable(
                "bad_signature",
                "signature verification failed; refusing to install",
            ));
        }
        if bin.len() < 1024 || &bin[..4] != b"\x7fELF" {
            return Err(AppError::unprocessable(
                "not_elf",
                "downloaded file is not an executable",
            ));
        }
        install(&state.cfg.bin_dir, &bin).map_err(AppError::internal)?;
        Ok(rel.tag.clone())
    }
    .await;
    if result.is_err() {
        state.maintenance.store(false, Ordering::SeqCst);
    }
    if let Ok(tag) = &result {
        state
            .store
            .audit(crate::util::now(), "admin", "update.apply", tag);
    }
    result
}

/// Atomically replace `<dir>/rustle`, keeping the previous binary as `rustle.prev`.
pub fn install(dir: &std::path::Path, bin: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    let cur = dir.join("rustle");
    let new = dir.join("rustle.new");
    std::fs::write(&new, bin)?;
    std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
    if cur.exists() {
        std::fs::copy(&cur, dir.join("rustle.prev"))?;
    }
    std::fs::rename(&new, &cur)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::TestEnv;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    fn keypair() -> (Ed25519KeyPair, [u8; 32]) {
        let rng = ring::rand::SystemRandom::new();
        let pk8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let kp = Ed25519KeyPair::from_pkcs8(pk8.as_ref()).unwrap();
        let pubkey: [u8; 32] = kp.public_key().as_ref().try_into().unwrap();
        (kp, pubkey)
    }

    fn fake_elf(tag: &str) -> Vec<u8> {
        let mut v = b"\x7fELF".to_vec();
        v.extend_from_slice(tag.as_bytes());
        v.extend(std::iter::repeat_n(0u8, 2000));
        v
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("x.y.z"), None);
        assert!(is_newer("v0.2.0", "0.1.9") && is_newer("1.0.0", "0.99.99"));
        assert!(
            !is_newer("v0.1.0", "0.1.0")
                && !is_newer("0.0.9", "0.1.0")
                && !is_newer("garbage", "0.1.0")
        );
    }

    #[tokio::test]
    async fn disabled_without_a_key() {
        let e = TestEnv::new();
        let info = check(&e.state).await;
        assert!(!info.enabled && info.reason.unwrap().contains("disabled"));
        assert_eq!(apply(&e.state).await.unwrap_err().status, 409);
    }

    fn env_with_key(pubkey: [u8; 32]) -> TestEnv {
        let mut e = TestEnv::new();
        let st = Arc::get_mut(&mut e.state).unwrap();
        st.cfg.update_pubkey = Some(pubkey);
        e
    }

    #[tokio::test]
    async fn applies_a_correctly_signed_release_and_keeps_prev() {
        let (kp, pubkey) = keypair();
        let e = env_with_key(pubkey);
        let bin = fake_elf("new");
        let sig = kp.sign(&bin);
        let name = asset_name();
        e.github.set_release(
            "v9.9.9",
            &[
                (name.as_str(), bin.as_slice()),
                (&format!("{name}.sig"), hex::encode(sig.as_ref()).as_bytes()),
            ],
        );
        let dir = &e.state.cfg.bin_dir;
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("rustle"), fake_elf("old")).unwrap();
        assert!(check(&e.state).await.update_available);
        let tag = apply(&e.state).await.unwrap();
        assert_eq!(tag, "v9.9.9");
        assert_eq!(std::fs::read(dir.join("rustle")).unwrap(), bin);
        assert_eq!(
            std::fs::read(dir.join("rustle.prev")).unwrap(),
            fake_elf("old"),
            "previous binary kept for auto-revert"
        );
        assert!(
            e.state.maintenance.load(Ordering::SeqCst),
            "stays in maintenance until the process restarts"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(dir.join("rustle"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }

    #[tokio::test]
    async fn rejects_tampered_wrong_key_and_missing_signature() {
        let (kp, pubkey) = keypair();
        let (other, _) = keypair();
        let name = asset_name();
        let bin = fake_elf("new");
        // signed with a different key
        let e = env_with_key(pubkey);
        e.github.set_release(
            "v9.9.9",
            &[
                (name.as_str(), bin.as_slice()),
                (&format!("{name}.sig"), other.sign(&bin).as_ref()),
            ],
        );
        let err = apply(&e.state).await.unwrap_err();
        assert_eq!(err.code, "bad_signature");
        assert!(
            !e.state.maintenance.load(Ordering::SeqCst),
            "maintenance released on failure"
        );
        assert!(
            !e.state.cfg.bin_dir.join("rustle").exists(),
            "nothing installed"
        );
        // binary modified after signing
        let e = env_with_key(pubkey);
        let sig = kp.sign(&bin);
        let mut tampered = bin.clone();
        tampered[10] ^= 1;
        e.github.set_release(
            "v9.9.9",
            &[
                (name.as_str(), tampered.as_slice()),
                (&format!("{name}.sig"), sig.as_ref()),
            ],
        );
        assert_eq!(apply(&e.state).await.unwrap_err().code, "bad_signature");
        // no signature asset
        let e = env_with_key(pubkey);
        e.github
            .set_release("v9.9.9", &[(name.as_str(), bin.as_slice())]);
        assert_eq!(apply(&e.state).await.unwrap_err().code, "no_signature");
        // signed but not an ELF
        let e = env_with_key(pubkey);
        let junk = vec![b'x'; 2000];
        e.github.set_release(
            "v9.9.9",
            &[
                (name.as_str(), junk.as_slice()),
                (&format!("{name}.sig"), kp.sign(&junk).as_ref()),
            ],
        );
        assert_eq!(apply(&e.state).await.unwrap_err().code, "not_elf");
        // older release
        let e = env_with_key(pubkey);
        e.github.set_release("v0.0.1", &[]);
        assert_eq!(apply(&e.state).await.unwrap_err().status, 409);
    }

    #[test]
    fn signature_encodings() {
        let raw = vec![7u8; 64];
        assert_eq!(decode_signature(&raw), Some(raw.clone()));
        assert_eq!(
            decode_signature(hex::encode(&raw).as_bytes()),
            Some(raw.clone())
        );
        use base64::Engine;
        assert_eq!(
            decode_signature(
                base64::engine::general_purpose::STANDARD
                    .encode(&raw)
                    .as_bytes()
            ),
            Some(raw)
        );
        assert_eq!(decode_signature(b"short"), None);
    }
}
