//! Filesystem-safe project directory derivation from an absolute path.
//!
//! Sessions live under `$XDG_DATA_HOME/ox/sessions/{sanitized-cwd}/` so
//! listings stay scoped to the project the user is working in. The
//! sanitization here turns an arbitrary path into a single directory
//! name: path separators and other reserved characters become `-`, and
//! very long paths fall back to a truncation + hash fingerprint so
//! distinct paths cannot collide after truncation.

use std::path::Path;

/// Maximum character length of a project subdirectory name before we
/// truncate and append a hash. 80 keeps names readable while staying
/// well below filesystem `NAME_MAX` limits (255 on ext4 / APFS).
const MAX_PROJECT_DIR_LEN: usize = 80;

/// Width of the hash suffix (in hex chars) appended to truncated names.
const HASH_SUFFIX_HEX_LEN: usize = 16;

/// Fallback subdirectory when the current working directory cannot be
/// resolved. Rare in practice — the process would be running from a
/// deleted directory.
pub(crate) const UNKNOWN_PROJECT_DIR: &str = "_unknown_";

/// Derive a filesystem-safe subdirectory name from a working-directory
/// path. Reserved characters (`/`, `\`, `:`, and anything not
/// `[A-Za-z0-9._-]`) become `-`. Leading and trailing `-` characters
/// are trimmed. Names longer than [`MAX_PROJECT_DIR_LEN`] are
/// truncated and suffixed with a 16-char FNV-1a hash of the original
/// path so distinct long paths never collide.
pub(crate) fn sanitize_cwd(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches('-');
    if trimmed.is_empty() {
        return UNKNOWN_PROJECT_DIR.to_owned();
    }

    let char_count = trimmed.chars().count();
    if char_count <= MAX_PROJECT_DIR_LEN {
        return trimmed.to_owned();
    }

    let keep_chars = MAX_PROJECT_DIR_LEN - HASH_SUFFIX_HEX_LEN - 1;
    let cut = trimmed
        .char_indices()
        .nth(keep_chars)
        .map_or(trimmed.len(), |(i, _)| i);
    let hash = fnv1a_64(raw.as_bytes());
    format!("{}-{hash:016x}", &trimmed[..cut])
}

/// FNV-1a 64-bit hash. Stable across Rust versions and platforms, which
/// `std::hash::DefaultHasher` is not — so the same CWD always maps to
/// the same project subdirectory name.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── sanitize_cwd ──

    #[test]
    fn sanitize_cwd_replaces_slashes_and_trims_leading() {
        let out = sanitize_cwd(Path::new("/Users/alice/project"));
        assert_eq!(out, "Users-alice-project");
    }

    #[test]
    fn sanitize_cwd_replaces_backslash_and_drive_letter() {
        let out = sanitize_cwd(Path::new(r"C:\Users\alice\project"));
        assert_eq!(out, "C--Users-alice-project");
    }

    #[test]
    fn sanitize_cwd_preserves_dots_underscores_and_dashes() {
        let out = sanitize_cwd(Path::new("/home/user/my-proj_v1.0"));
        assert_eq!(out, "home-user-my-proj_v1.0");
    }

    #[test]
    fn sanitize_cwd_replaces_other_reserved_chars_with_dash() {
        let out = sanitize_cwd(Path::new("/pro ject/with spaces&symbols!"));
        assert_eq!(out, "pro-ject-with-spaces-symbols");
    }

    #[test]
    fn sanitize_cwd_returns_unknown_for_empty_result() {
        assert_eq!(sanitize_cwd(Path::new("/")), UNKNOWN_PROJECT_DIR);
        assert_eq!(sanitize_cwd(Path::new("")), UNKNOWN_PROJECT_DIR);
        assert_eq!(sanitize_cwd(Path::new("///")), UNKNOWN_PROJECT_DIR);
    }

    #[test]
    fn sanitize_cwd_truncates_long_paths_with_stable_hash_suffix() {
        let long = "/".to_string() + &"a".repeat(200);
        let out = sanitize_cwd(Path::new(&long));
        assert_eq!(out.chars().count(), MAX_PROJECT_DIR_LEN);
        // Suffix is 16 hex chars preceded by a separator.
        let suffix = &out[out.len() - (HASH_SUFFIX_HEX_LEN + 1)..];
        assert!(suffix.starts_with('-'));
        assert!(suffix[1..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sanitize_cwd_truncation_distinguishes_similar_long_paths() {
        // Two paths that share the same prefix beyond the truncation
        // point must still yield different subdir names.
        let base = "/".to_string() + &"a".repeat(200);
        let a = sanitize_cwd(Path::new(&(base.clone() + "/alpha")));
        let b = sanitize_cwd(Path::new(&(base + "/beta")));
        assert_ne!(a, b, "hash suffix must disambiguate");
    }

    #[test]
    fn sanitize_cwd_is_deterministic() {
        let p = Path::new("/some/deterministic/path");
        assert_eq!(sanitize_cwd(p), sanitize_cwd(p));
    }

    // ── fnv1a_64 ──

    #[test]
    fn fnv1a_64_known_vectors() {
        // Canonical FNV-1a 64-bit vectors (see
        // <http://www.isthe.com/chongo/tech/comp/fnv/>).
        assert_eq!(fnv1a_64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a_64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a_64(b"foobar"), 0x8594_4171_f739_67e8);
    }
}
