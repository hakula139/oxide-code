//! Anthropic billing attestation header computation.

use anyhow::{Result, bail};
use sha2::{Digest, Sha256};
use xxhash_rust::xxh64;

/// Salt hardcoded in the Claude Code JS source.
const FINGERPRINT_SALT: &str = "59cf53e54c78";
const FINGERPRINT_INDICES: [usize; 3] = [4, 7, 20];
/// xxHash64 seed for the cch body hash (extracted from the Bun binary).
const CCH_SEED: u64 = 0x6E52_736A_C806_831E;
const CCH_PLACEHOLDER: &str = "cch=00000";

// ── Public API ──

/// 3-char hex suffix for `cc_version`: first 3 hex of `SHA-256(salt + chars_at_indices + version)`.
pub(super) fn compute_fingerprint(first_user_message: &str, version: &str) -> String {
    let chars: String = FINGERPRINT_INDICES
        .iter()
        .map(|&i| first_user_message.chars().nth(i).unwrap_or('0'))
        .collect();

    let input = format!("{FINGERPRINT_SALT}{chars}{version}");
    let hash = Sha256::digest(input.as_bytes());
    format!("{:02x}{:02x}", hash[0], hash[1])[..3].to_string()
}

/// Builds the billing attribution header; [`inject_cch`] replaces the `cch=00000` placeholder.
pub(super) fn build_billing_header(version: &str, fingerprint: &str) -> String {
    format!(
        "x-anthropic-billing-header: \
         cc_version={version}.{fingerprint}; \
         cc_entrypoint=cli; \
         {CCH_PLACEHOLDER};"
    )
}

/// Replaces the first `cch=00000` with `cch={5-hex of xxHash64(body)}`. Errors if absent.
pub(super) fn inject_cch(body: &str) -> Result<String> {
    if !body.contains(CCH_PLACEHOLDER) {
        bail!("billing header placeholder `{CCH_PLACEHOLDER}` missing from request body");
    }

    let hash = xxh64::xxh64(body.as_bytes(), CCH_SEED);
    let cch = format!("{:05x}", hash & 0xFFFFF);
    Ok(body.replacen(CCH_PLACEHOLDER, &format!("cch={cch}"), 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── compute_fingerprint ──

    // Verified via `echo -n "59cf53e54c78'li2.1.37" | shasum -a 256` → "9e71...".
    // Chars at indices [4, 7, 20] of "Say 'hello'..." are `'`, `l`, `i`.
    #[test]
    fn compute_fingerprint_known_vector() {
        let fp = compute_fingerprint("Say 'hello' and nothing else.", "2.1.37");
        assert_eq!(fp, "9e7");
    }

    #[test]
    fn compute_fingerprint_varies_with_version() {
        let fp1 = compute_fingerprint("hello world", "2.1.37");
        let fp2 = compute_fingerprint("hello world", "2.1.87");
        assert_ne!(
            fp1, fp2,
            "different versions should produce different fingerprints"
        );
    }

    #[test]
    fn compute_fingerprint_short_message_pads_with_zero() {
        // "Hi" (len 2): all fingerprint indices are out of bounds; chars default to '0'.
        let short = compute_fingerprint("Hi", "2.1.87");
        let empty = compute_fingerprint("", "2.1.87");
        assert_eq!(short, empty, "both should pad all positions with '0'");
    }

    #[test]
    fn compute_fingerprint_partial_bounds() {
        // "Hello" (len 5): index 4 = 'o'; 7 and 20 are out of bounds.
        let fp = compute_fingerprint("Hello", "2.1.87");
        let fp_all_zero = compute_fingerprint("", "2.1.87");
        assert_eq!(fp.len(), 3);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(
            fp, fp_all_zero,
            "partial in-bounds should differ from all-zero"
        );
    }

    // ── build_billing_header ──

    #[test]
    fn build_billing_header_format() {
        let header = build_billing_header("2.1.87", "abc");
        assert_eq!(
            header,
            "x-anthropic-billing-header: cc_version=2.1.87.abc; cc_entrypoint=cli; cch=00000;"
        );
    }

    // ── inject_cch ──

    #[test]
    fn inject_cch_replaces_placeholder() {
        let body = r#"{"system":[{"type":"text","text":"cch=00000;"}],"messages":[]}"#;
        let result = inject_cch(body).unwrap();

        assert!(
            !result.contains(CCH_PLACEHOLDER),
            "placeholder should be replaced"
        );

        let hash = xxh64::xxh64(body.as_bytes(), CCH_SEED);
        let expected = format!("{:05x}", hash & 0xFFFFF);
        assert!(
            result.contains(&format!("cch={expected}")),
            "result should contain cch={expected}, got: {result}"
        );
    }

    #[test]
    fn inject_cch_deterministic() {
        let body = r#"{"system":[{"type":"text","text":"cch=00000;"}],"messages":[]}"#;
        assert_eq!(inject_cch(body).unwrap(), inject_cch(body).unwrap());
    }

    #[test]
    fn inject_cch_produces_five_hex_chars() {
        let body = r#"{"system":[{"type":"text","text":"cch=00000;"}]}"#;
        let result = inject_cch(body).unwrap();

        let cch_start = result.find("cch=").expect("cch= not found") + 4;
        let cch_value = &result[cch_start..cch_start + 5];
        assert!(
            cch_value.chars().all(|c| c.is_ascii_hexdigit()),
            "cch should be 5 hex chars, got: {cch_value}"
        );
    }

    #[test]
    fn inject_cch_replaces_only_first_occurrence() {
        // `system` precedes `messages` — matches our struct field order.
        let body = r#"{"system":[{"type":"text","text":"cch=00000;"}],"messages":[{"role":"user","content":[{"type":"text","text":"cch=00000"}]}]}"#;
        let result = inject_cch(body).unwrap();

        assert_eq!(
            result.matches("cch=00000").count(),
            1,
            "only the second occurrence (in messages) should remain"
        );
    }

    #[test]
    fn inject_cch_errors_when_placeholder_missing() {
        let err = inject_cch(r#"{"system":[],"messages":[]}"#).expect_err("must error");
        assert!(
            format!("{err:#}").contains(CCH_PLACEHOLDER),
            "error names placeholder: {err:#}",
        );
    }
}
