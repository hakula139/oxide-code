//! Extra-trust-anchor helpers. `reqwest` with `rustls-tls` only trusts the baked-in
//! `webpki-roots` Mozilla bundle, so corporate or self-signed endpoints need explicit PEM
//! bundles appended to the client builder.

use std::path::Path;

use anyhow::{Context, Result};
use reqwest::Certificate;

/// Reads a PEM-encoded bundle from disk and returns one [`Certificate`] per `BEGIN CERTIFICATE`
/// block. Empty bundles surface as an explicit error so silent misconfiguration does not
/// degrade into "still rejecting the corp CA".
pub(crate) fn load_extra_ca_certs(path: &Path) -> Result<Vec<Certificate>> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read extra CA bundle at {}", path.display()))?;
    let certs = Certificate::from_pem_bundle(&bytes)
        .with_context(|| format!("failed to parse PEM bundle at {}", path.display()))?;
    if certs.is_empty() {
        anyhow::bail!(
            "no PEM certificates found in {} (expected one or more `-----BEGIN CERTIFICATE-----` blocks)",
            path.display(),
        );
    }
    Ok(certs)
}

/// Self-signed throwaway P-256 cert generated once with `openssl req -x509 ...`. Embedding
/// keeps tests hermetic and never touches the network. Shared across any in-crate test that
/// needs a real trust-anchor PEM.
#[cfg(test)]
pub(crate) const TEST_CA_PEM: &str = indoc::indoc! {"
    -----BEGIN CERTIFICATE-----
    MIIBhTCCASugAwIBAgIUP8gTuzOaUClHkfbBRwh5D+v7nt0wCgYIKoZIzj0EAwIw
    GDEWMBQGA1UEAwwNb3hpZGUtdGVzdC1jYTAeFw0yNjA1MTMwNzIxNTlaFw0zNjA1
    MTAwNzIxNTlaMBgxFjAUBgNVBAMMDW94aWRlLXRlc3QtY2EwWTATBgcqhkjOPQIB
    BggqhkjOPQMBBwNCAAQy5JPDldjwa2hBGxGCFB3l15yVesaxS0JNumy9OMUXAEEM
    WHqiHpZq6IaNV2RxATGjSsXL8DgZGDNDTcMqKogRo1MwUTAdBgNVHQ4EFgQUkXs3
    E+J6fk50kCUhGArrVnQqrFswHwYDVR0jBBgwFoAUkXs3E+J6fk50kCUhGArrVnQq
    rFswDwYDVR0TAQH/BAUwAwEB/zAKBggqhkjOPQQDAgNIADBFAiAtNtc4gyeMsui7
    HT8UUyVjGWlOGVCTNkkEf4cPeMheIwIhAOmxcsmpYu8Brz64j2MnN2LUGTsZAZ6T
    MziN3FfztHCm
    -----END CERTIFICATE-----
"};

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;

    fn write_pem(contents: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        file.flush().unwrap();
        file
    }

    // ── load_extra_ca_certs ──

    #[test]
    fn load_extra_ca_certs_parses_single_and_multi_block_bundles() {
        for (label, body, expected) in [
            ("single", TEST_CA_PEM.to_owned(), 1),
            ("bundle", format!("{TEST_CA_PEM}\n{TEST_CA_PEM}"), 2),
        ] {
            let file = write_pem(&body);
            let certs =
                load_extra_ca_certs(file.path()).unwrap_or_else(|e| panic!("{label}: {e:#}"));
            assert_eq!(certs.len(), expected, "{label}");
        }
    }

    #[test]
    fn load_extra_ca_certs_rejects_empty_bundle() {
        let file = write_pem("# comments only, no certificate blocks\n");
        let err = load_extra_ca_certs(file.path()).expect_err("empty bundle must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("no PEM certificates found"), "{msg}");
    }

    #[test]
    fn load_extra_ca_certs_reports_filename_on_read_and_parse_failures() {
        // Missing path + malformed body share the same invariant: the error must mention the
        // filepath so users can debug without reading source.
        let missing = Path::new("/definitely/does/not/exist.pem");
        let err = load_extra_ca_certs(missing).expect_err("missing path must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to read extra CA bundle"), "{msg}");
        assert!(msg.contains("/definitely/does/not/exist.pem"), "{msg}");

        let malformed = write_pem(indoc::indoc! {"
            -----BEGIN CERTIFICATE-----
            not base64 data
            -----END CERTIFICATE-----
        "});
        let err = load_extra_ca_certs(malformed.path()).expect_err("malformed PEM must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to parse PEM bundle"), "{msg}");
        assert!(
            msg.contains(&malformed.path().display().to_string()),
            "{msg}"
        );
    }
}
