//! Anthropic billing attestation header computation.

use anyhow::{Result, bail};
use sha2::{Digest, Sha256};
use xxhash_rust::xxh64;

/// Claude Code's salt for the `cc_version` fingerprint.
const FINGERPRINT_SALT: &str = "59cf53e54c78";
const FINGERPRINT_INDICES: [usize; 3] = [4, 7, 20];
/// Claude Code's xxHash64 seed for the `cch` body hash.
const CCH_SEED: u64 = 0x6E52_736A_C806_831E;
const CCH_PLACEHOLDER: &str = "cch=00000";

// ── Public API ──

/// 3-char hex suffix for `cc_version`: first 3 hex of `SHA-256(salt + chars_at_indices + version)`.
/// Indices count UTF-16 code units, as Claude Code's JavaScript string indexing does.
pub(super) fn compute_fingerprint(first_prompt: &str, version: &str) -> String {
    // An index inside a surrogate pair picks a lone surrogate, which Bun and Node hash as U+FFFD.
    let chars: String = FINGERPRINT_INDICES
        .iter()
        .map(|&i| {
            first_prompt.encode_utf16().nth(i).map_or('0', |unit| {
                char::from_u32(unit.into()).unwrap_or(char::REPLACEMENT_CHARACTER)
            })
        })
        .collect();

    let input = format!("{FINGERPRINT_SALT}{chars}{version}");
    let hash = Sha256::digest(input.as_bytes());
    format!("{:02x}{:02x}", hash[0], hash[1])[..3].to_owned()
}

/// Builds the billing attribution header. With `hashed`, it ends in the `cch=00000` placeholder
/// that [`inject_cch`] replaces.
pub(super) fn build_billing_header(version: &str, fingerprint: &str, hashed: bool) -> String {
    let header = format!(
        "x-anthropic-billing-header: cc_version={version}.{fingerprint}; cc_entrypoint=cli;"
    );
    if hashed {
        format!("{header} {CCH_PLACEHOLDER};")
    } else {
        header
    }
}

/// Returns whether `attribution` carries the placeholder [`inject_cch`] fills.
pub(super) fn is_hashed(attribution: &str) -> bool {
    attribution.contains(CCH_PLACEHOLDER)
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

    mod compute_fingerprint {
        use super::*;

        // Verified via `echo -n "59cf53e54c78'li2.1.37" | shasum -a 256` → "9e71...".
        // Chars at indices [4, 7, 20] of "Say 'hello'..." are `'`, `l`, `i`.
        #[test]
        fn matches_a_known_vector() {
            let fp = compute_fingerprint("Say 'hello' and nothing else.", "2.1.37");
            assert_eq!(fp, "9e7");
        }

        #[test]
        fn varies_with_version() {
            let fp1 = compute_fingerprint("hello world", "2.1.37");
            let fp2 = compute_fingerprint("hello world", "2.1.87");
            assert_ne!(
                fp1, fp2,
                "different versions should produce different fingerprints"
            );
        }

        #[test]
        fn short_message_pads_with_zero() {
            // "Hi" (len 2) has no char at any fingerprint index, so every position defaults to '0'.
            let short = compute_fingerprint("Hi", "2.1.87");
            let empty = compute_fingerprint("", "2.1.87");
            assert_eq!(short, empty, "both should pad all positions with '0'");
        }

        #[test]
        fn pads_only_out_of_bounds_indices() {
            // Index 4 of "Hello" is `o`, and 7 and 20 are out of bounds.
            assert_eq!(compute_fingerprint("Hello", "2.1.37"), "a74");
        }

        // Expected values in the UTF-16 tests come from Claude Code's fingerprint function run
        // under both Bun and Node.
        #[test]
        fn counts_utf16_code_units() {
            // 📦 takes two code units, which puts the `t` of "to" at index 20.
            let fp = compute_fingerprint("Rename the 📦 crate to 🦀", "2.1.296");
            assert_eq!(fp, "0bf");
        }

        #[test]
        fn hashes_a_split_surrogate_pair_as_replacement_character() {
            // Index 4 holds the high surrogate of 🦀.
            let fp = compute_fingerprint("Fix 🦀 the parser, then test it", "2.1.296");
            assert_eq!(fp, "070");
        }
    }

    mod build_billing_header {
        use super::*;

        #[test]
        fn formats_hashed_and_stable_headers() {
            let hashed = build_billing_header("2.1.87", "abc", true);
            assert_eq!(
                hashed,
                "x-anthropic-billing-header: cc_version=2.1.87.abc; cc_entrypoint=cli; cch=00000;"
            );
            assert!(is_hashed(&hashed));

            let stable = build_billing_header("2.1.87", "abc", false);
            assert_eq!(
                stable,
                "x-anthropic-billing-header: cc_version=2.1.87.abc; cc_entrypoint=cli;"
            );
            assert!(!is_hashed(&stable));
        }
    }

    mod inject_cch {
        use super::*;

        #[test]
        fn replaces_placeholder() {
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
        fn is_deterministic() {
            let body = r#"{"system":[{"type":"text","text":"cch=00000;"}],"messages":[]}"#;
            assert_eq!(inject_cch(body).unwrap(), inject_cch(body).unwrap());
        }

        #[test]
        fn produces_five_hex_chars() {
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
        fn replaces_only_first_occurrence() {
            // `system` precedes `messages`, as in the request struct's field order.
            let body = r#"{"system":[{"type":"text","text":"cch=00000;"}],"messages":[{"role":"user","content":[{"type":"text","text":"cch=00000"}]}]}"#;
            let result = inject_cch(body).unwrap();

            assert_eq!(
                result.matches("cch=00000").count(),
                1,
                "only the second occurrence (in messages) should remain"
            );
        }

        #[test]
        fn errors_when_placeholder_missing() {
            let err = inject_cch(r#"{"system":[],"messages":[]}"#).expect_err("must error");
            assert!(
                format!("{err:#}").contains(CCH_PLACEHOLDER),
                "error names placeholder: {err:#}",
            );
        }
    }
}
