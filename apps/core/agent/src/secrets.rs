//! Random opaque secrets and their stored digests. A raw value exists only in
//! the response that issues it; every row and lookup uses the SHA-256 digest.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use bilibi_database::{Error, Result};
use sha2::{Digest, Sha256};

/// 256 random bits as 43 URL-safe characters without padding.
pub fn secret() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| Error::Internal)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
pub fn digest(value: &str) -> Vec<u8> {
    Sha256::digest(value.as_bytes()).to_vec()
}
/// The S256 PKCE form of a digest.
pub fn digest_base64(value: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(value.as_bytes()))
}
/// Eight Base32 characters from 40 random bits, shown as `XXXX-XXXX`.
pub fn user_code() -> Result<String> {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut bytes = [0u8; 5];
    getrandom::fill(&mut bytes).map_err(|_| Error::Internal)?;
    let bits = u64::from_be_bytes([0, 0, 0, bytes[0], bytes[1], bytes[2], bytes[3], bytes[4]]);
    let code: String = (0..8)
        .map(|index| ALPHABET[((bits >> (35 - 5 * index)) & 31) as usize] as char)
        .collect();
    Ok(format!("{}-{}", &code[..4], &code[4..]))
}
/// Canonical eight-character user code, accepting an optional separator and
/// any case. Other lengths are returned compacted so the lookup simply misses.
pub fn normalize_user_code(code: &str) -> String {
    let compact = code.trim().to_uppercase().replace('-', "");
    if compact.len() == 8 && compact.is_ascii() {
        format!("{}-{}", &compact[..4], &compact[4..])
    } else {
        compact
    }
}
