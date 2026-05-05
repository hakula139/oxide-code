//! Filesystem-safe project subdir derivation. Sessions live under
//! `$XDG_DATA_HOME/ox/sessions/{sanitized-cwd}/`; reserved chars become `-` and long names get
//! a hash suffix to prevent post-truncation collisions.

use std::path::Path;

use xxhash_rust::xxh64::xxh64;

/// Char-length cap before truncate + hash. 80 stays well under filesystem `NAME_MAX` (255).
const MAX_PROJECT_DIR_LEN: usize = 80;

/// Hex chars in the truncation hash suffix.
const HASH_SUFFIX_HEX_LEN: usize = 16;

/// Fallback when the cwd cannot be resolved (e.g. running from a deleted directory).
pub(crate) const UNKNOWN_PROJECT_DIR: &str = "_unknown_";

/// Derives a filesystem-safe subdir name. Non-`[A-Za-z0-9._-]` chars become `-`; long names get
/// a 16-char xxh64 suffix over the raw path bytes (so non-UTF8 paths whose lossy forms collide
/// still hash apart).
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
    let hash = xxh64(path.as_os_str().as_encoded_bytes(), 0);
    format!("{}-{hash:016x}", &trimmed[..cut])
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
        let out = sanitize_cwd(Path::new("/sub dir/with spaces&symbols!"));
        assert_eq!(out, "sub-dir-with-spaces-symbols");
    }

    #[test]
    fn sanitize_cwd_is_unknown_for_empty_result() {
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
        // Paths sharing a prefix beyond the truncation point must still hash apart.
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

    #[cfg(unix)]
    #[test]
    fn sanitize_cwd_distinguishes_non_utf8_paths_with_same_lossy_form() {
        // Different invalid bytes both lossy-render as U+FFFD; raw-byte hash must disambiguate.
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        use std::path::PathBuf;

        let base = "/".to_string() + &"a".repeat(200) + "/";
        let mut bytes_a = base.as_bytes().to_vec();
        bytes_a.push(0xFF);
        let mut bytes_b = base.as_bytes().to_vec();
        bytes_b.push(0xFE);

        let path_a = PathBuf::from(OsStr::from_bytes(&bytes_a));
        let path_b = PathBuf::from(OsStr::from_bytes(&bytes_b));

        assert_eq!(
            path_a.to_string_lossy(),
            path_b.to_string_lossy(),
            "precondition: lossy forms collide"
        );
        assert_ne!(
            sanitize_cwd(&path_a),
            sanitize_cwd(&path_b),
            "raw-byte hash must disambiguate"
        );
    }
}
