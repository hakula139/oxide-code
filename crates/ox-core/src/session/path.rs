//! Filesystem-safe project subdir names, as in `$XDG_DATA_HOME/ox/sessions/{project}/`.

use std::path::Path;

use xxhash_rust::xxh64::xxh64;

/// Char-length cap before truncating and hashing. 80 stays well under filesystem `NAME_MAX` (255).
const MAX_PROJECT_DIR_LEN: usize = 80;

/// Hex chars in the truncation hash suffix.
const HASH_SUFFIX_HEX_LEN: usize = 16;

/// The subdir for a project root with no safe characters to keep, such as `/`.
const UNKNOWN_PROJECT_DIR: &str = "_unknown_";

/// Derives a filesystem-safe subdir name. Non-`[A-Za-z0-9._-]` chars become `-`, and a long name
/// gets an xxh64 suffix over the raw path bytes, so names that truncate alike stay distinct, as do
/// non-UTF-8 paths whose lossy forms collide.
pub(super) fn project_dir_name(path: &Path) -> String {
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

    if trimmed.len() <= MAX_PROJECT_DIR_LEN {
        return trimmed.to_owned();
    }

    let cut = MAX_PROJECT_DIR_LEN - HASH_SUFFIX_HEX_LEN - 1;
    let hash = xxh64(path.as_os_str().as_encoded_bytes(), 0);
    format!("{}-{hash:016x}", &trimmed[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    mod project_dir_name {
        use super::*;

        #[test]
        fn replaces_reserved_characters_and_preserves_safe_names() {
            for (case, input, expected) in [
                (
                    "Unix separators",
                    "/Users/alice/project",
                    "Users-alice-project",
                ),
                (
                    "Windows spelling",
                    r"C:\Users\alice\project",
                    "C--Users-alice-project",
                ),
                (
                    "safe punctuation",
                    "/home/user/my-proj_v1.0",
                    "home-user-my-proj_v1.0",
                ),
                (
                    "reserved characters",
                    "/sub dir/with spaces&symbols!",
                    "sub-dir-with-spaces-symbols",
                ),
                ("root", "/", UNKNOWN_PROJECT_DIR),
                ("empty", "", UNKNOWN_PROJECT_DIR),
                ("repeated separators", "///", UNKNOWN_PROJECT_DIR),
            ] {
                assert_eq!(project_dir_name(Path::new(input)), expected, "{case}");
            }
        }

        #[test]
        fn truncates_long_paths_with_stable_hash_suffix() {
            let long = "/".to_owned() + &"a".repeat(200);

            let out = project_dir_name(Path::new(&long));

            assert_eq!(out, format!("{}-2067408b9eac3941", "a".repeat(63)));
        }

        #[test]
        fn truncation_distinguishes_similar_long_paths() {
            let base = "/".to_owned() + &"a".repeat(200);
            let a = project_dir_name(Path::new(&(base.clone() + "/alpha")));
            let b = project_dir_name(Path::new(&(base + "/beta")));
            assert_ne!(a, b, "hash suffix must disambiguate");
        }

        #[cfg(unix)]
        #[test]
        fn distinguishes_non_utf8_paths_with_same_lossy_form() {
            // Both invalid bytes lossy-render as U+FFFD, so the raw-byte hash must disambiguate.
            use std::ffi::OsStr;
            use std::os::unix::ffi::OsStrExt as _;
            use std::path::PathBuf;

            let base = "/".to_owned() + &"a".repeat(200) + "/";
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
                project_dir_name(&path_a),
                project_dir_name(&path_b),
                "raw-byte hash must disambiguate"
            );
        }
    }
}
