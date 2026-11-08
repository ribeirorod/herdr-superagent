//! Stable content-derived identifiers: equal inputs always produce the same id.

use sha2::{Digest, Sha256};

/// 32 hex characters of the SHA-256 of the JSON array of `values`.
pub fn key(values: &[&str]) -> String {
    let json = serde_json::to_string(values).unwrap_or_default();
    let digest = Sha256::digest(json.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    hex[..32].to_string()
}

/// A short human-friendly ticket id: `t-` plus the first 6 characters of a key.
pub fn short(prefix: &str, values: &[&str]) -> String {
    format!("{prefix}-{}", &key(values)[..6])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_stable_and_distinct() {
        assert_eq!(key(&["a", "b"]), key(&["a", "b"]));
        assert_ne!(key(&["a", "b"]), key(&["ab"]));
        assert_eq!(key(&["x"]).len(), 32);
        assert!(short("t", &["x"]).starts_with("t-"));
    }
}
