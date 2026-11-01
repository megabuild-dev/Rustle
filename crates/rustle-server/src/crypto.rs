//! Cryptographic helpers. Everything secret flows through here.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::path::Path;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct MasterKey([u8; 32]);

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(**redacted**)")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("decryption failed")]
    Decrypt,
    #[error("malformed ciphertext")]
    Malformed,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid master key file")]
    BadKeyFile,
}

impl MasterKey {
    pub fn from_bytes(b: [u8; 32]) -> Self {
        MasterKey(b)
    }

    pub fn generate() -> Self {
        let mut b = [0u8; 32];
        rand::rng().fill_bytes(&mut b);
        MasterKey(b)
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(s: &str) -> Result<Self, CryptoError> {
        let v = hex::decode(s.trim()).map_err(|_| CryptoError::BadKeyFile)?;
        Ok(MasterKey(v.try_into().map_err(|_| CryptoError::BadKeyFile)?))
    }

    /// Load the key file, or create it (mode 0600) when it does not exist and `create` is set.
    pub fn load(path: &Path, create: bool) -> Result<Self, CryptoError> {
        match std::fs::read_to_string(path) {
            Ok(s) => Self::from_hex(&s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
                let k = Self::generate();
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                write_secret_file(path, k.to_hex().as_bytes())?;
                Ok(k)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// HKDF-SHA256 derived sub-key for a purpose label.
    pub fn derive(&self, label: &str) -> [u8; 32] {
        let hk = Hkdf::<Sha256>::new(Some(b"rustle-v1"), &self.0);
        let mut out = [0u8; 32];
        hk.expand(label.as_bytes(), &mut out).expect("32 bytes is a valid HKDF length");
        out
    }

    /// AES-256-GCM. Output = 12-byte nonce || ciphertext+tag. `aad` binds the ciphertext to its row.
    pub fn seal(&self, plain: &[u8], aad: &[u8]) -> Vec<u8> {
        let key = self.derive("data");
        let cipher = Aes256Gcm::new_from_slice(&key).expect("key length");
        let mut nonce = [0u8; 12];
        rand::rng().fill_bytes(&mut nonce);
        let ct = cipher
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: plain, aad })
            .expect("AES-GCM encryption cannot fail for in-memory buffers");
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ct);
        out
    }

    pub fn open(&self, blob: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if blob.len() < 12 + 16 {
            return Err(CryptoError::Malformed);
        }
        let key = self.derive("data");
        let cipher = Aes256Gcm::new_from_slice(&key).expect("key length");
        let (nonce, ct) = blob.split_at(12);
        cipher.decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad }).map_err(|_| CryptoError::Decrypt)
    }

    pub fn seal_str(&self, s: &str, aad: &str) -> Vec<u8> {
        self.seal(s.as_bytes(), aad.as_bytes())
    }

    pub fn open_str(&self, blob: &[u8], aad: &str) -> Result<String, CryptoError> {
        String::from_utf8(self.open(blob, aad.as_bytes())?).map_err(|_| CryptoError::Malformed)
    }

    pub fn hmac(&self, label: &str, data: &[u8]) -> Vec<u8> {
        let key = self.derive(label);
        let mut m = <HmacSha256 as Mac>::new_from_slice(&key).expect("hmac accepts any key length");
        m.update(data);
        m.finalize().into_bytes().to_vec()
    }
}

#[cfg(unix)]
fn write_secret_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    f.write_all(data)
}

#[cfg(not(unix))]
fn write_secret_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, data)
}

pub fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = <HmacSha256 as Mac>::new_from_slice(key).expect("hmac accepts any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Constant-time (for equal lengths) comparison; length differences are not secret here.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

/// URL-safe random token with `bytes` bytes of entropy.
pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

pub fn b64url(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

pub fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(s).ok()
}

fn argon() -> Argon2<'static> {
    // 8 MiB, 3 passes, 1 lane: fits comfortably inside the 48 MB service cap.
    let params = Params::new(8 * 1024, 3, 1, None).expect("valid argon2 params");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Give freed heap pages back to the OS (glibc keeps them otherwise). Called after password
/// hashing, whose 8 MiB working set is by far the biggest allocation spike in the process.
pub fn release_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim has no preconditions; it only returns unused heap memory to the kernel.
    unsafe {
        libc::malloc_trim(0);
    }
}

pub fn hash_password(pw: &str) -> String {
    let mut salt_bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut salt_bytes);
    let salt = SaltString::encode_b64(&salt_bytes).expect("salt encoding");
    argon().hash_password(pw.as_bytes(), &salt).expect("argon2 hashing").to_string()
}

pub fn verify_password(pw: &str, hash: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(h) => argon().verify_password(pw.as_bytes(), &h).is_ok(),
        Err(_) => false,
    }
}

/// Verify an ed25519 signature (used for signed self-updates).
pub fn ed25519_verify(pubkey: &[u8; 32], msg: &[u8], sig: &[u8]) -> bool {
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, pubkey).verify(msg, sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip_and_tamper() {
        let k = MasterKey::generate();
        let blob = k.seal_str("postgres://secret", "site1:DATABASE_URL");
        assert_eq!(k.open_str(&blob, "site1:DATABASE_URL").unwrap(), "postgres://secret");
        // wrong aad
        assert!(k.open_str(&blob, "site2:DATABASE_URL").is_err());
        // tampered
        let mut bad = blob.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(k.open_str(&bad, "site1:DATABASE_URL").is_err());
        // other key
        assert!(MasterKey::generate().open_str(&blob, "site1:DATABASE_URL").is_err());
        // short
        assert!(k.open(&[1, 2, 3], b"").is_err());
    }

    #[test]
    fn nonces_differ() {
        let k = MasterKey::generate();
        assert_ne!(k.seal(b"x", b""), k.seal(b"x", b""));
    }

    #[test]
    fn derive_is_label_separated_and_stable() {
        let k = MasterKey::from_bytes([7; 32]);
        assert_eq!(k.derive("jwt"), k.derive("jwt"));
        assert_ne!(k.derive("jwt"), k.derive("csrf"));
    }

    #[test]
    fn password_hash() {
        let h = hash_password("correct horse");
        assert!(verify_password("correct horse", &h));
        assert!(!verify_password("wrong", &h));
        assert!(!verify_password("x", "not-a-hash"));
    }

    #[test]
    fn key_file_create_and_load() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("k/master.key");
        let k = MasterKey::load(&p, true).unwrap();
        let k2 = MasterKey::load(&p, false).unwrap();
        assert_eq!(k.to_hex(), k2.to_hex());
        assert!(MasterKey::load(&d.path().join("nope"), false).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn ed25519_roundtrip() {
        use ring::signature::{Ed25519KeyPair, KeyPair};
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let kp = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let sig = kp.sign(b"hello");
        let pk: [u8; 32] = kp.public_key().as_ref().try_into().unwrap();
        assert!(ed25519_verify(&pk, b"hello", sig.as_ref()));
        assert!(!ed25519_verify(&pk, b"hellO", sig.as_ref()));
    }

    #[test]
    fn ct_and_hash() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }
}
