//! The connection secret (story 24-1, the security review's P14-A).
//!
//! A per-state-directory **host key** — 32 bytes from the OS random
//! source, written once to `<state>/shim-host-key`, owner-only — and, per
//! launch, `secret = HMAC-SHA-256(host_key, launch_id)`. Any engine that can
//! read the host key recomputes a live Shim's secret after a restart, so the
//! spawn record keeps only `SHA-256(secret)` (for the Shim's own check) and
//! no secret is ever stored in the clear. Regenerating the host key rotates
//! every secret at once: live Shims then answer `secret-mismatch`, which an
//! engine reports and never punishes.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The host key's file name under the state directory.
pub const HOST_KEY_FILE: &str = "shim-host-key";

/// HMAC-SHA-256's block size.
const BLOCK: usize = 64;

/// HMAC-SHA-256 (RFC 2104) over the engine's own SHA-256.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&crate::sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Vec::with_capacity(BLOCK + message.len());
    inner.extend(block.iter().map(|b| b ^ 0x36));
    inner.extend_from_slice(message);
    let inner = crate::sha256::digest(&inner);
    let mut outer = Vec::with_capacity(BLOCK + 32);
    outer.extend(block.iter().map(|b| b ^ 0x5c));
    outer.extend_from_slice(&inner);
    crate::sha256::digest(&outer)
}

/// The connection secret for `launch_id` under `host_key`.
pub fn connection_secret(host_key: &[u8; 32], launch_id: &str) -> [u8; 32] {
    hmac_sha256(host_key, launch_id.as_bytes())
}

/// What the spawn record keeps: the secret's SHA-256.
pub fn secret_hash(secret: &[u8; 32]) -> [u8; 32] {
    crate::sha256::digest(secret)
}

/// Lowercase hex.
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parse 32 bytes of lowercase or uppercase hex.
pub fn from_hex(text: &str) -> Option<[u8; 32]> {
    let text = text.trim();
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, chunk) in text.as_bytes().chunks(2).enumerate() {
        let pair = std::str::from_utf8(chunk).ok()?;
        out[index] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

/// Constant-time comparison of two hashes.
pub fn equal(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The host key's path under `state_dir`.
pub fn host_key_path(state_dir: &Path) -> PathBuf {
    state_dir.join(HOST_KEY_FILE)
}

/// Read the state directory's host key, creating it (owner-only) on first
/// use. A file of the wrong size is an error, never silently replaced (that
/// would rotate every live Shim's secret).
pub fn host_key(state_dir: &Path) -> std::io::Result<[u8; 32]> {
    let path = host_key_path(state_dir);
    match std::fs::read(&path) {
        Ok(bytes) => <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} is not a 32-byte host key", path.display()),
            )
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let mut key = [0u8; 32];
            getrandom::getrandom(&mut key).map_err(|err| std::io::Error::other(err.to_string()))?;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            crate::backends::restrict_to_owner(&path)?;
            file.write_all(&key)?;
            file.sync_all()?;
            Ok(key)
        }
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_the_rfc_4231_vectors() {
        // RFC 4231 test case 2.
        assert_eq!(
            to_hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Test case 6: a key longer than the block.
        assert_eq!(
            to_hex(&hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn the_host_key_is_created_once_and_the_secret_is_recomputable() {
        let dir = tempfile::tempdir().unwrap();
        let key = host_key(dir.path()).unwrap();
        assert_eq!(
            host_key(dir.path()).unwrap(),
            key,
            "read back, not regenerated"
        );
        let a = connection_secret(&key, "launch-1");
        assert_eq!(a, connection_secret(&key, "launch-1"));
        assert_ne!(a, connection_secret(&key, "launch-2"));
        let hash = secret_hash(&a);
        assert!(equal(&hash, &secret_hash(&a)));
        assert!(!equal(
            &hash,
            &secret_hash(&connection_secret(&key, "launch-2"))
        ));
        assert_eq!(from_hex(&to_hex(&hash)), Some(hash));
        assert_eq!(from_hex("zz"), None);
        std::fs::write(host_key_path(dir.path()), b"short").unwrap();
        assert!(host_key(dir.path()).is_err());
    }
}
