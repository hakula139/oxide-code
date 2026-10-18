//! Extra trust anchors for HTTP clients, so corporate and self-signed endpoints whose CA the
//! platform trust store lacks still verify.

use std::path::Path;
#[cfg(target_os = "macos")]
use std::{cell::Cell, collections::HashSet, sync::Arc};

use anyhow::{Context, Result, bail};
#[cfg(test)]
use indoc::indoc;
use reqwest::ClientBuilder;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
#[cfg(target_os = "macos")]
use {
    core_foundation::date::CFDate,
    rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    rustls::client::{verify_server_cert_signed_by_trust_anchor, verify_server_name},
    rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms},
    rustls::pki_types::{ServerName, UnixTime},
    rustls::server::ParsedCertificate,
    rustls::{
        CertificateError, ClientConfig, DigitallySignedStruct, Error, OtherError, RootCertStore,
        SignatureScheme,
    },
    rustls_platform_verifier::Verifier as PlatformVerifier,
    security_framework::certificate::SecCertificate,
    security_framework::policy::{RevocationPolicy, SecPolicy},
    security_framework::trust::SecTrust,
    security_framework::trust_settings::{Domain, TrustSettings},
    tracing::warn,
};

// ── Client Builder ──

/// Trusts the PEM bundle at `path` on top of the platform trust store. `None` returns `builder`
/// unchanged. On macOS the bundle comes with a TLS configuration of its own, which replaces the
/// TLS options set on `builder`.
pub(crate) fn apply_extra_ca_certs(
    builder: ClientBuilder,
    path: Option<&Path>,
) -> Result<ClientBuilder> {
    let Some(path) = path else {
        return Ok(builder);
    };
    trust_extra_cas(builder, load_extra_ca_certs(path)?)
        .with_context(|| format!("failed to trust extra CA bundle at {}", path.display()))
}

/// Reads a PEM-encoded bundle from disk and returns one certificate per `BEGIN CERTIFICATE` block.
/// Empty bundles and blocks that are not X.509 certificates surface as explicit errors so silent
/// misconfiguration does not degrade into "still rejecting the corp CA".
pub(crate) fn load_extra_ca_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read extra CA bundle at {}", path.display()))?;
    let certs = CertificateDer::pem_slice_iter(&bytes)
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("failed to parse PEM bundle at {}", path.display()))?;
    if certs.is_empty() {
        bail!(
            "no PEM certificates found in {} \
             (expected one or more `-----BEGIN CERTIFICATE-----` blocks)",
            path.display(),
        );
    }
    if let Some(index) = certs
        .iter()
        .position(|cert| webpki::anchor_from_trusted_cert(cert).is_err())
    {
        bail!(
            "certificate {} in {} is not a valid X.509 certificate",
            index + 1,
            path.display(),
        );
    }
    Ok(certs)
}

/// Adds `certs` to the trust anchors of reqwest's platform verifier.
#[cfg(not(target_os = "macos"))]
fn trust_extra_cas(
    builder: ClientBuilder,
    certs: Vec<CertificateDer<'static>>,
) -> Result<ClientBuilder> {
    let certs = certs
        .into_iter()
        .map(|cert| reqwest::Certificate::from_der(&cert))
        .collect::<reqwest::Result<Vec<_>>>()?;
    Ok(builder.tls_certs_merge(certs))
}

/// Gives reqwest the TLS config it builds for itself, with [`ExtraCaVerifier`] as its verifier.
#[cfg(target_os = "macos")]
fn trust_extra_cas(
    builder: ClientBuilder,
    certs: Vec<CertificateDer<'static>>,
) -> Result<ClientBuilder> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let verifier = ExtraCaVerifier::new(certs, &provider)?;
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    // Matches the ALPN of the config reqwest builds, which offers only HTTP/1.1 without `http2`.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(builder.tls_backend_preconfigured(config))
}

// ── ExtraCaVerifier ──

/// `OSStatus` codes from the Security framework's `SecBase.h`.
#[cfg(target_os = "macos")]
mod os_status {
    pub(super) const CERTIFICATE_REVOKED: i32 = -67_820;
    pub(super) const CERTIFICATE_VALIDITY_PERIOD_TOO_LONG: i32 = -67_901;
}

/// The platform verifier, except that Apple's validity-period limit for TLS server certificates
/// does not apply to chains that end at an [exempt anchor](ExemptAnchors). Apple applies the limit
/// to every anchor, including the extra ones that private CAs with long-lived leaves need.
#[cfg(target_os = "macos")]
#[derive(Debug)]
struct ExtraCaVerifier {
    /// Trusts the system roots and every bundle CA.
    platform: PlatformVerifier,
    exempt: ExemptAnchors,
}

#[cfg(target_os = "macos")]
impl ExtraCaVerifier {
    /// Exempts the bundle CAs other than Apple's built-in roots, or none when those roots cannot
    /// be read.
    fn new(bundle: Vec<CertificateDer<'static>>, provider: &Arc<CryptoProvider>) -> Result<Self> {
        let exempt = match apple_roots() {
            Ok(roots) => ExemptAnchors::new(
                bundle.iter().filter(|cert| !roots.contains(cert.as_ref())),
                provider,
            )?,
            Err(e) => {
                warn!("keeping Apple's validity limit for extra CAs: {e:#}");
                ExemptAnchors::new([], provider)?
            }
        };
        let platform = PlatformVerifier::new_with_extra_roots(bundle, Arc::clone(provider))?;
        Ok(Self { platform, exempt })
    }
}

#[cfg(target_os = "macos")]
impl ServerCertVerifier for ExtraCaVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let rejection = match self.platform.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Err(rejection) if exceeds_validity_limit(&rejection) => rejection,
            verdict => return verdict,
        };
        match self
            .exempt
            .verify(end_entity, intermediates, server_name, ocsp_response, now)
        {
            Ok(()) => Ok(ServerCertVerified::assertion()),
            Err(Error::InvalidCertificate(CertificateError::UnknownIssuer)) => Err(rejection),
            Err(e) => Err(e),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.platform.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.platform.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.platform.supported_verify_schemes()
    }
}

/// The DER encodings of Apple's built-in roots. CAs installed in a keychain belong to the admin
/// and user trust settings domains instead.
#[cfg(target_os = "macos")]
fn apple_roots() -> Result<HashSet<Vec<u8>>> {
    let roots: HashSet<_> = TrustSettings::new(Domain::System)
        .iter()
        .context("failed to read the system trust settings")?
        .map(|cert| cert.to_der())
        .collect();
    if roots.is_empty() {
        bail!("the system trust settings list no root certificates");
    }
    Ok(roots)
}

/// Whether the platform verifier rejected the chain with `errSecCertificateValidityPeriodTooLong`,
/// which it reports only in the message because rustls has no variant for it. Apple reports this
/// code ahead of untrusted anchor, hostname, extended key usage, and expiry failures, but behind
/// a revocation.
#[cfg(target_os = "macos")]
fn exceeds_validity_limit(rejection: &Error) -> bool {
    let suffix = format!(": {}", os_status::CERTIFICATE_VALIDITY_PERIOD_TOO_LONG);
    matches!(
        rejection,
        Error::InvalidCertificate(CertificateError::Other(other)) if other.to_string().ends_with(&suffix)
    )
}

// ── ExemptAnchors ──

/// The id-kp-serverAuth OID (1.3.6.1.5.5.7.3.1) as DER content octets.
#[cfg(target_os = "macos")]
const SERVER_AUTH_OID: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01];

/// `kCFAbsoluteTimeIntervalSince1970`, the Unix time of Core Foundation's 2001 epoch.
#[cfg(target_os = "macos")]
const CF_EPOCH_UNIX_SECS: u64 = 978_307_200;

/// The bundle CAs whose chains may exceed the validity limit.
#[cfg(target_os = "macos")]
#[derive(Debug)]
struct ExemptAnchors {
    webpki: RootCertStore,
    apple: Vec<SecCertificate>,
    algorithms: WebPkiSupportedAlgorithms,
}

#[cfg(target_os = "macos")]
impl ExemptAnchors {
    fn new<'a>(
        anchors: impl IntoIterator<Item = &'a CertificateDer<'static>>,
        provider: &CryptoProvider,
    ) -> Result<Self> {
        let mut webpki = RootCertStore::empty();
        let mut apple = Vec::new();
        for cert in anchors {
            webpki.add(cert.clone())?;
            apple.push(SecCertificate::from_der(cert)?);
        }
        Ok(Self {
            webpki,
            apple,
            algorithms: provider.signature_verification_algorithms,
        })
    }

    /// Checks the chain, expiry, hostname, extended key usage, and revocation of a chain that ends
    /// at one of these anchors, without the validity-period limit. `UnknownIssuer` means the chain
    /// does not end at one of them.
    fn verify(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<(), Error> {
        let cert = ParsedCertificate::try_from(end_entity)?;
        verify_server_cert_signed_by_trust_anchor(
            &cert,
            &self.webpki,
            intermediates,
            now,
            self.algorithms.all,
        )?;
        verify_server_name(&cert, server_name)?;
        self.require_server_auth(end_entity, intermediates, now)?;
        self.evaluate_basic(end_entity, intermediates, ocsp_response, now)
    }

    /// webpki's server check accepts a leaf without the extended key usage extension, which Apple
    /// requires of a TLS server certificate. The chain already verified, so only the leaf's usage
    /// can fail here.
    fn require_server_auth(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<(), Error> {
        webpki::EndEntityCert::try_from(end_entity)
            .and_then(|cert| {
                cert.verify_for_usage(
                    self.algorithms.all,
                    &self.webpki.roots,
                    intermediates,
                    now,
                    LeafServerAuth::default(),
                    None,
                    None,
                )
                .map(drop)
            })
            .map_err(|_| Error::InvalidCertificate(CertificateError::InvalidPurpose))
    }

    /// Evaluates the chain with the Security framework under the basic X.509 policy and a
    /// revocation policy, anchored only at these CAs. This keeps Apple's chain, expiry, trust
    /// setting, and revocation checks without the TLS policy that carries the validity limit.
    /// Revocation relies on the stapled OCSP response and trustd's cache because a network fetch
    /// would block the handshake on the responder.
    fn evaluate_basic(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<(), Error> {
        let general = |e: security_framework::base::Error| Error::General(e.to_string());
        let chain = std::iter::once(end_entity)
            .chain(intermediates)
            .map(|cert| SecCertificate::from_der(cert))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| Error::InvalidCertificate(CertificateError::BadEncoding))?;
        let revocation = SecPolicy::create_revocation(
            RevocationPolicy::USE_ANY_METHOD_AVAILABLE | RevocationPolicy::NETWORK_ACCESS_DISABLED,
        )
        .map_err(general)?;
        let mut trust =
            SecTrust::create_with_certificates(&chain, &[SecPolicy::create_x509(), revocation])
                .map_err(general)?;
        let since_cf_epoch = now
            .as_secs()
            .checked_sub(CF_EPOCH_UNIX_SECS)
            .ok_or(Error::FailedToGetCurrentTime)?;
        #[expect(
            clippy::cast_precision_loss,
            reason = "seconds since 2001 stay far below 2^53, which f64 holds exactly"
        )]
        let since_cf_epoch = since_cf_epoch as f64;
        trust
            .set_trust_verify_date(&CFDate::new(since_cf_epoch))
            .map_err(general)?;
        if !ocsp_response.is_empty() {
            trust
                .set_trust_ocsp_response(std::iter::once(ocsp_response))
                .map_err(general)?;
        }
        trust
            .set_anchor_certificates(&self.apple)
            .map_err(general)?;
        trust
            .set_trust_anchor_certificates_only(true)
            .map_err(general)?;

        trust.evaluate_with_error().map_err(|e| {
            let reason = if e.code() == os_status::CERTIFICATE_REVOKED as isize {
                CertificateError::Revoked
            } else {
                let message: Box<dyn std::error::Error + Send + Sync> =
                    format!("{}: {}", e.description(), e.code()).into();
                CertificateError::Other(OtherError(Arc::from(message)))
            };
            Error::InvalidCertificate(reason)
        })
    }
}

/// Requires the `serverAuth` usage of the leaf and applies webpki's if-present rule to the CAs
/// above it, whose extended key usage is optional. webpki validates the leaf first.
#[cfg(target_os = "macos")]
#[derive(Default)]
struct LeafServerAuth {
    leaf_checked: Cell<bool>,
}

#[cfg(target_os = "macos")]
impl webpki::ExtendedKeyUsageValidator for LeafServerAuth {
    fn validate(&self, purposes: webpki::KeyPurposeIdIter<'_, '_>) -> Result<(), webpki::Error> {
        let usage = if self.leaf_checked.replace(true) {
            webpki::KeyUsage::server_auth()
        } else {
            webpki::KeyUsage::required(SERVER_AUTH_OID)
        };
        webpki::ExtendedKeyUsageValidator::validate(&usage, purposes)
    }
}

// ── Test Fixtures ──

/// Self-signed throwaway P-256 cert generated once with `openssl req -x509 ...`. Embedding
/// keeps tests hermetic and never touches the network. 100-year validity so tests do not rot.
#[cfg(test)]
pub(crate) const TEST_CA_PEM: &str = indoc! {"
    -----BEGIN CERTIFICATE-----
    MIIBhzCCAS2gAwIBAgIUSHnI8j4asiQCYFCLHv+mTjaH7PIwCgYIKoZIzj0EAwIw
    GDEWMBQGA1UEAwwNb3hpZGUtdGVzdC1jYTAgFw0yNjA1MTMwOTM1NTNaGA8yMTI2
    MDQxOTA5MzU1M1owGDEWMBQGA1UEAwwNb3hpZGUtdGVzdC1jYTBZMBMGByqGSM49
    AgEGCCqGSM49AwEHA0IABPPm0pogMrzkQroL61zCV3BzVH25tmWvt6c1OK5pT7Yy
    tOXqTKKLiqUbpsJW6XzankZ6E8LsI9mwuzXhsQYmGE+jUzBRMB0GA1UdDgQWBBQC
    hrBBOk1wizWiQQQtrpIDMACA8DAfBgNVHSMEGDAWgBQChrBBOk1wizWiQQQtrpID
    MACA8DAPBgNVHRMBAf8EBTADAQH/MAoGCCqGSM49BAMCA0gAMEUCIFiHH199T6Sd
    F2u46c+5D9+pdwYEd1dAgP+a21dwLwo3AiEAyn5ssAPGMPmSP8lKLRLuH+cFNEVQ
    PyBNMput8iNe6eE=
    -----END CERTIFICATE-----
"};

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::sync::Arc;

    use rustls::pki_types::PrivateKeyDer;
    use tempfile::NamedTempFile;
    #[cfg(target_os = "macos")]
    use time::macros::datetime;

    use super::*;

    // ── Fixtures ──

    /// A P-256 CA that issued the leaves below, valid for 100 years.
    const ISSUING_CA_PEM: &str = indoc! {"
        -----BEGIN CERTIFICATE-----
        MIIBdjCCAR2gAwIBAgIJANQtabEExkQPMAoGCCqGSM49BAMCMB4xHDAaBgNVBAMM
        E294aWRlLXRlc3QtZXh0cmEtY2EwIBcNMjYxMDE3MDU0MDE3WhgPMjEyNzAxMDEw
        NTQwMTdaMB4xHDAaBgNVBAMME294aWRlLXRlc3QtZXh0cmEtY2EwWTATBgcqhkjO
        PQIBBggqhkjOPQMBBwNCAARS+MEur56Y4lt77laLU0/XqYa0rVwAPGopLGbUCC8K
        2KTBmLZkTPnjeQlD+fj6WVb8U8lz7U9AaKQmeFFCD4gSo0IwQDAPBgNVHRMBAf8E
        BTADAQH/MA4GA1UdDwEB/wQEAwIBBjAdBgNVHQ4EFgQUTk695Gu4cijrXhtBt6Kz
        bEGrN8kwCgYIKoZIzj0EAwIDRwAwRAIgbFFmClmQjkgy6K6qJl3CoJaKXt4Yg/go
        Y9/O9FgR0sACIBpBQgyNHHVqVBNOqvHRM5BH3o342VuM6LYJ8e0Qzmqw
        -----END CERTIFICATE-----
    "};

    /// A `localhost` server leaf valid for 100 years, beyond Apple's 825-day limit, with the
    /// `serverAuth` usage and the subject alternative name Apple requires.
    const LONG_LIVED_LEAF_PEM: &str = indoc! {"
        -----BEGIN CERTIFICATE-----
        MIIBuTCCAV6gAwIBAgIJAL+LZkXFknsiMAoGCCqGSM49BAMCMB4xHDAaBgNVBAMM
        E294aWRlLXRlc3QtZXh0cmEtY2EwIBcNMjYxMDE3MDU0MDE3WhgPMjEyNjA5MjMw
        NTQwMTdaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDBZMBMGByqGSM49AgEGCCqGSM49
        AwEHA0IABJ2rBrOFyBdhHq/8W4tPgEqydqCdKJeqo1TEP/PxeGrX+LMZe9CL3aaa
        /XKsna/tL3aQpNR9LW79MkwWM/yoOYijgYwwgYkwDAYDVR0TAQH/BAIwADAOBgNV
        HQ8BAf8EBAMCB4AwEwYDVR0lBAwwCgYIKwYBBQUHAwEwFAYDVR0RBA0wC4IJbG9j
        YWxob3N0MB0GA1UdDgQWBBQinbalbsCK+pnhxTXPwZoTfrvIgzAfBgNVHSMEGDAW
        gBROTr3ka7hyKOteG0G3orNsQas3yTAKBggqhkjOPQQDAgNJADBGAiEAjAzZKDYs
        XXRpnxHfh7GltXcxq8p8EoGd5g+8k2XTDzkCIQCfvdR+zKFy92ne5zKAm8XwZ+ZW
        2unFZkzjHyLujKRJFw==
        -----END CERTIFICATE-----
    "};

    /// The throwaway private key of [`LONG_LIVED_LEAF_PEM`].
    const LONG_LIVED_LEAF_KEY_PEM: &str = indoc! {"
        -----BEGIN EC PRIVATE KEY-----
        MHcCAQEEINmz7sOs20KgX0/M7R5/avsn9FXV266iOrOi4ABp1YpDoAoGCCqGSM49
        AwEHoUQDQgAEnasGs4XIF2Eer/xbi0+ASrJ2oJ0ol6qjVMQ/8/F4atf4sxl70Ivd
        ppr9cqydr+0vdpCk1H0tbv0yTBYz/Kg5iA==
        -----END EC PRIVATE KEY-----
    "};

    /// A `localhost` server leaf from 2026-10-17 valid for one year, within Apple's limit.
    #[cfg(target_os = "macos")]
    const SHORT_LIVED_LEAF_PEM: &str = indoc! {"
        -----BEGIN CERTIFICATE-----
        MIIBtTCCAVygAwIBAgIJAL+LZkXFknsjMAoGCCqGSM49BAMCMB4xHDAaBgNVBAMM
        E294aWRlLXRlc3QtZXh0cmEtY2EwHhcNMjYxMDE3MDYwODA2WhcNMjcxMDE3MDYw
        ODA2WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjOPQMB
        BwNCAASbn/1ZFqFQ6lN+lOIzZ+EeeoO83IbUtPYOy1OhUym+gQpGY4ujmWK0bhlP
        5R6m/w8DjQPuINDIO6+AwXejaKyXo4GMMIGJMAwGA1UdEwEB/wQCMAAwDgYDVR0P
        AQH/BAQDAgeAMBMGA1UdJQQMMAoGCCsGAQUFBwMBMBQGA1UdEQQNMAuCCWxvY2Fs
        aG9zdDAdBgNVHQ4EFgQUFuMuXR3tZAKp3Csbw0La989aHO0wHwYDVR0jBBgwFoAU
        Tk695Gu4cijrXhtBt6KzbEGrN8kwCgYIKoZIzj0EAwIDRwAwRAIgHSeM0HQyAkKX
        2XB4MFPzJW0YaccGkgKesHbPJBWfEDUCIHSMVLumStU4xS9OnIXQEch+mGM+HCFM
        DnNXvQOe8uP5
        -----END CERTIFICATE-----
    "};

    /// [`LONG_LIVED_LEAF_PEM`] without the extended key usage extension.
    #[cfg(target_os = "macos")]
    const NO_USAGE_LEAF_PEM: &str = indoc! {"
        -----BEGIN CERTIFICATE-----
        MIIBojCCAUegAwIBAgIJAL+LZkXFknsmMAoGCCqGSM49BAMCMB4xHDAaBgNVBAMM
        E294aWRlLXRlc3QtZXh0cmEtY2EwIBcNMjYxMDE3MDYxNzEzWhgPMjEyNjA5MjMw
        NjE3MTNaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDBZMBMGByqGSM49AgEGCCqGSM49
        AwEHA0IABEbVOWeCKKFB2mwHTHrxmLdSGg6Id2wUhmgG39BOlDulGCxu/hOpWDMx
        Z//34zLzG7ICQ0pWgSsNsCkTWh7JgDmjdjB0MAwGA1UdEwEB/wQCMAAwDgYDVR0P
        AQH/BAQDAgeAMBQGA1UdEQQNMAuCCWxvY2FsaG9zdDAdBgNVHQ4EFgQUONLQ313u
        MqO0pii8E330h9m5ql4wHwYDVR0jBBgwFoAUTk695Gu4cijrXhtBt6KzbEGrN8kw
        CgYIKoZIzj0EAwIDSQAwRgIhAPL3GbUvRIecwnDERos9KrwNYr8slnUy7hfVYguw
        sAMuAiEAokfZytgspN6yXbUGxfpHNWUN70dwmqHvGIk+J5GJyIc=
        -----END CERTIFICATE-----
    "};

    /// An intermediate CA under [`ISSUING_CA_PEM`] without the extended key usage extension, as
    /// private CAs often issue them.
    #[cfg(target_os = "macos")]
    const INTERMEDIATE_CA_PEM: &str = indoc! {"
        -----BEGIN CERTIFICATE-----
        MIIBozCCAUigAwIBAgIJAL+LZkXFknssMAoGCCqGSM49BAMCMB4xHDAaBgNVBAMM
        E294aWRlLXRlc3QtZXh0cmEtY2EwIBcNMjYxMDE3MDcwNzEyWhgPMjEyNjA5MjMw
        NzA3MTJaMCUxIzAhBgNVBAMMGm94aWRlLXRlc3QtaW50ZXJtZWRpYXRlLWNhMFkw
        EwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEUfPDlNHmojWbgU8E8ZrlmY2OJUl8neha
        46QXc72CRt/0PpN0a2qp4ZG+DB78vvGE3ej6x3Z3dqKmrojMGeowPaNmMGQwEgYD
        VR0TAQH/BAgwBgEB/wIBADAOBgNVHQ8BAf8EBAMCAQYwHQYDVR0OBBYEFJfgFb/d
        3eIrqJMlcPege5qCvWwYMB8GA1UdIwQYMBaAFE5OveRruHIo614bQbeis2xBqzfJ
        MAoGCCqGSM49BAMCA0kAMEYCIQCR3cFAM7WdKb+N09OBEkrO6B5h9IR2Vf8wOYW+
        WGfK/QIhAIVgNmAcyl2RCmxnzriF9VgHCu3Qix/XNoL11UqCXbDv
        -----END CERTIFICATE-----
    "};

    /// A `localhost` server leaf under [`INTERMEDIATE_CA_PEM`], valid for 98 years.
    #[cfg(target_os = "macos")]
    const INTERMEDIATE_LEAF_PEM: &str = indoc! {"
        -----BEGIN CERTIFICATE-----
        MIIByzCCAXCgAwIBAgIURavIKAYby7s2CnBI92HfR/sG2lcwCgYIKoZIzj0EAwIw
        JTEjMCEGA1UEAwwab3hpZGUtdGVzdC1pbnRlcm1lZGlhdGUtY2EwIBcNMjYxMDE3
        MDcwNzEyWhgPMjEyNTA1MTEwNzA3MTJaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDBZ
        MBMGByqGSM49AgEGCCqGSM49AwEHA0IABOq5EmKbkXVFGATw+IaKJ+2E9fTGfCXW
        3cDgKeBqvxJAtFolX0ZOdf6B4qIp5CT1wtQJWLFyekclMXSujB+fDZSjgYwwgYkw
        DAYDVR0TAQH/BAIwADAOBgNVHQ8BAf8EBAMCB4AwEwYDVR0lBAwwCgYIKwYBBQUH
        AwEwFAYDVR0RBA0wC4IJbG9jYWxob3N0MB0GA1UdDgQWBBSTTLgdLOvAEP9AgZOq
        my16raFA2jAfBgNVHSMEGDAWgBSX4BW/3d3iK6iTJXD3oHuagr1sGDAKBggqhkjO
        PQQDAgNJADBGAiEA5hvTUq/xy0S/8X6PLt/Qgfs0KaDgZVTmWAd0BY/7XwMCIQCT
        SYPLgh+zja7tyAgQ5fx9RKdHEzba7Me/D2EaQB7OkA==
        -----END CERTIFICATE-----
    "};

    /// Another 100-year `localhost` leaf, which [`REVOKED_LEAF_OCSP_BASE64`] revokes. It has its
    /// own serial number so that a cached revocation cannot reach the other leaves.
    #[cfg(target_os = "macos")]
    const REVOKED_LEAF_PEM: &str = indoc! {"
        -----BEGIN CERTIFICATE-----
        MIIBuDCCAV6gAwIBAgIJAL+LZkXFknsrMAoGCCqGSM49BAMCMB4xHDAaBgNVBAMM
        E294aWRlLXRlc3QtZXh0cmEtY2EwIBcNMjYxMDE3MDYyMTI0WhgPMjEyNjA5MjMw
        NjIxMjRaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDBZMBMGByqGSM49AgEGCCqGSM49
        AwEHA0IABN9AMULiSrKSr+ov9JyuzlZqNGtozVgDuqrkdMV/i6Ld9w/ltxRGRmuk
        508rhhkN7LinwCGdMFZhLSNVUrFKfp6jgYwwgYkwDAYDVR0TAQH/BAIwADAOBgNV
        HQ8BAf8EBAMCB4AwEwYDVR0lBAwwCgYIKwYBBQUHAwEwFAYDVR0RBA0wC4IJbG9j
        YWxob3N0MB0GA1UdDgQWBBQZmjUgXCb05vtiXyYmDnFnFfRF5DAfBgNVHSMEGDAW
        gBROTr3ka7hyKOteG0G3orNsQas3yTAKBggqhkjOPQQDAgNIADBFAiByUjIhz3ea
        LNX94fhlDD1m9FxNnwGy5Dzfk4KvJlVvBQIhAP6Uyz7QC7mxQ2TmYSwnQ2+wquRm
        ZZotKLut/Uql2GF7
        -----END CERTIFICATE-----
    "};

    /// A DER OCSP response from [`ISSUING_CA_PEM`] that marks [`REVOKED_LEAF_PEM`] revoked, valid
    /// for 100 years.
    #[cfg(target_os = "macos")]
    const REVOKED_LEAF_OCSP_BASE64: &str = indoc! {"
        MIIBKgoBAKCCASMwggEfBgkrBgEFBQcwAQEEggEQMIIBDDCBsqEgMB4xHDAaBgNV
        BAMME294aWRlLXRlc3QtZXh0cmEtY2EYDzIwMjYxMDE3MDYyMTQ1WjB9MHswQjAJ
        BgUrDgMCGgUABBRTf2oks/96uCQL/yYbyFGV7w1qQAQUTk695Gu4cijrXhtBt6Kz
        bEGrN8kCCQC/i2ZFxZJ7K6ERGA8yMDI2MTAxNjA2MjEyNFoYDzIwMjYxMDE3MDYy
        MTQ1WqARGA8yMTI2MDkyMzA2MjE0NVowCgYIKoZIzj0EAwIDSQAwRgIhAMxvYbRn
        jgGm6165QREBnb1I8BSNsjN5JJZKf23SZ/t8AiEAjWm1QJnslUuelsY5V867o2YR
        lBmBkAq0IS4XXfdnf38=
    "};

    /// A 100-year `localhost` leaf whose OCSP responder is [`OCSP_RESPONDER_ADDR`].
    #[cfg(target_os = "macos")]
    const OCSP_LEAF_PEM: &str = indoc! {"
        -----BEGIN CERTIFICATE-----
        MIIB7DCCAZKgAwIBAgIJAL+LZkXFknstMAoGCCqGSM49BAMCMB4xHDAaBgNVBAMM
        E294aWRlLXRlc3QtZXh0cmEtY2EwIBcNMjYxMDE3MDcwNzEyWhgPMjEyNjA5MjMw
        NzA3MTJaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDBZMBMGByqGSM49AgEGCCqGSM49
        AwEHA0IABC3cyaJWixqJyjK6LBG7EECCjvNCStCZIa5khFxWhM5cVKw/oOp0Mw7J
        eLnxvEWT4zWJXG9hFPM0t+oN4CxexOGjgcAwgb0wDAYDVR0TAQH/BAIwADAOBgNV
        HQ8BAf8EBAMCB4AwEwYDVR0lBAwwCgYIKwYBBQUHAwEwFAYDVR0RBA0wC4IJbG9j
        YWxob3N0MB0GA1UdDgQWBBTEKCPgENuRfTIRFbXyggrHnKHfbzAfBgNVHSMEGDAW
        gBROTr3ka7hyKOteG0G3orNsQas3yTAyBggrBgEFBQcBAQQmMCQwIgYIKwYBBQUH
        MAGGFmh0dHA6Ly8xMjcuMC4wLjE6MzkyNTQwCgYIKoZIzj0EAwIDSAAwRQIgGkBw
        yFI3THC3aYLJe7lLFT7yJ4lmf67f5W4WzhW68TcCIQDlrIWIq7NaH5jQtaoRxF/s
        YY6DAlKs3AbtOJd+0DX+sA==
        -----END CERTIFICATE-----
    "};

    /// The loopback address that [`OCSP_LEAF_PEM`] names as its OCSP responder.
    #[cfg(target_os = "macos")]
    const OCSP_RESPONDER_ADDR: &str = "127.0.0.1:39254";

    fn write_pem(contents: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        file.flush().unwrap();
        file
    }

    fn cert(pem: &str) -> CertificateDer<'static> {
        CertificateDer::from_pem_slice(pem.as_bytes()).unwrap()
    }

    /// Answers one HTTPS request on a loopback port with `ok`, presenting [`LONG_LIVED_LEAF_PEM`]
    /// over `version`.
    fn serve_once(version: &'static rustls::SupportedProtocolVersion) -> SocketAddr {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[version])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert(LONG_LIVED_LEAF_PEM)],
                PrivateKeyDer::from_pem_slice(LONG_LIVED_LEAF_KEY_PEM.as_bytes()).unwrap(),
            )
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let conn = rustls::ServerConnection::new(Arc::new(config)).unwrap();
            let mut tls = rustls::StreamOwned::new(conn, tcp);
            for line in BufReader::new(&mut tls).lines() {
                if line.unwrap().is_empty() {
                    break;
                }
            }
            tls.write_all(indoc! {b"
                HTTP/1.1 200 OK\r
                content-length: 2\r
                connection: close\r
                \r
                ok"
            })
            .unwrap();
        });
        addr
    }

    /// A verifier whose platform side trusts `platform_ca_pem` and whose exempt anchors are the
    /// bundle `exempt_ca_pem`. Production verifiers trust the same bundle on both sides.
    #[cfg(target_os = "macos")]
    fn verifier(platform_ca_pem: &str, exempt_ca_pem: &str) -> ExtraCaVerifier {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        ExtraCaVerifier {
            platform: PlatformVerifier::new_with_extra_roots(
                vec![cert(platform_ca_pem)],
                Arc::clone(&provider),
            )
            .unwrap(),
            exempt: exempt_anchors(exempt_ca_pem),
        }
    }

    #[cfg(target_os = "macos")]
    fn exempt_anchors(ca_pem: &str) -> ExemptAnchors {
        ExemptAnchors::new(
            &[cert(ca_pem)],
            &rustls::crypto::aws_lc_rs::default_provider(),
        )
        .unwrap()
    }

    #[cfg(target_os = "macos")]
    fn verify_leaf(
        verifier: &ExtraCaVerifier,
        leaf_pem: &str,
        name: &str,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        verifier.verify_server_cert(
            &cert(leaf_pem),
            &[],
            &ServerName::try_from(name.to_owned()).unwrap(),
            ocsp_response,
            now,
        )
    }

    #[cfg(target_os = "macos")]
    fn unix_time(at: time::OffsetDateTime) -> UnixTime {
        UnixTime::since_unix_epoch(std::time::Duration::from_secs(
            u64::try_from(at.unix_timestamp()).unwrap(),
        ))
    }

    #[cfg(target_os = "macos")]
    fn revoked_leaf_ocsp() -> Vec<u8> {
        use base64::Engine;

        let base64: String = REVOKED_LEAF_OCSP_BASE64.lines().collect();
        base64::engine::general_purpose::STANDARD
            .decode(base64)
            .unwrap()
    }

    /// Whether `err` carries the Security framework `code` the way the platform verifier reports
    /// codes that rustls has no variant for.
    #[cfg(target_os = "macos")]
    fn has_os_status(err: &Error, code: i32) -> bool {
        matches!(
            err,
            Error::InvalidCertificate(CertificateError::Other(other))
                if other.to_string().ends_with(&format!(": {code}"))
        )
    }

    // ── apply_extra_ca_certs ──

    #[tokio::test]
    async fn apply_extra_ca_certs_connects_to_a_long_lived_leaf_from_the_bundle() {
        let bundle = write_pem(ISSUING_CA_PEM);
        for version in rustls::ALL_VERSIONS {
            let addr = serve_once(version);
            let builder = reqwest::Client::builder()
                .no_proxy()
                .resolve("localhost", addr);
            let client = apply_extra_ca_certs(builder, Some(bundle.path()))
                .unwrap()
                .build()
                .unwrap();

            let response = client
                .get(format!("https://localhost:{}/", addr.port()))
                .send()
                .await
                .unwrap_or_else(|e| panic!("{version:?}: {:#}", anyhow::Error::from(e)));

            assert_eq!(response.text().await.unwrap(), "ok", "{version:?}");
        }
    }

    #[test]
    fn apply_extra_ca_certs_surfaces_loader_error_with_path() {
        let missing = Path::new("/definitely/does/not/exist.pem");

        let err = apply_extra_ca_certs(reqwest::Client::builder(), Some(missing))
            .expect_err("missing path must error");

        assert_eq!(
            err.to_string(),
            "failed to read extra CA bundle at /definitely/does/not/exist.pem",
        );
    }

    // ── load_extra_ca_certs ──

    #[test]
    fn load_extra_ca_certs_parses_single_and_multi_block_bundles() {
        for (label, body, expected) in [
            ("single", TEST_CA_PEM.to_owned(), vec![cert(TEST_CA_PEM)]),
            (
                "bundle",
                format!("{TEST_CA_PEM}\n{ISSUING_CA_PEM}"),
                vec![cert(TEST_CA_PEM), cert(ISSUING_CA_PEM)],
            ),
        ] {
            let file = write_pem(&body);

            let certs =
                load_extra_ca_certs(file.path()).unwrap_or_else(|e| panic!("{label}: {e:#}"));

            assert_eq!(certs, expected, "{label}");
        }
    }

    #[test]
    fn load_extra_ca_certs_rejects_empty_bundle() {
        let file = write_pem("# comments only, no certificate blocks\n");

        let err = load_extra_ca_certs(file.path()).expect_err("empty bundle must error");

        assert_eq!(
            err.to_string(),
            format!(
                "no PEM certificates found in {} \
                 (expected one or more `-----BEGIN CERTIFICATE-----` blocks)",
                file.path().display(),
            ),
        );
    }

    #[test]
    fn load_extra_ca_certs_reports_filename_on_parse_failure() {
        let malformed = write_pem(indoc! {"
            -----BEGIN CERTIFICATE-----
            not base64 data
            -----END CERTIFICATE-----
        "});

        let err = load_extra_ca_certs(malformed.path()).expect_err("malformed PEM must error");

        assert_eq!(
            err.to_string(),
            format!(
                "failed to parse PEM bundle at {}",
                malformed.path().display()
            ),
        );
    }

    #[test]
    fn load_extra_ca_certs_rejects_a_block_that_is_not_a_certificate() {
        let file = write_pem(&format!(
            "{TEST_CA_PEM}{}",
            indoc! {"
                -----BEGIN CERTIFICATE-----
                AAAA
                -----END CERTIFICATE-----
            "}
        ));

        let err = load_extra_ca_certs(file.path()).expect_err("a malformed certificate must error");

        assert_eq!(
            format!("{err:#}"),
            format!(
                "certificate 2 in {} is not a valid X.509 certificate",
                file.path().display(),
            ),
        );
    }

    // ── ExtraCaVerifier::new ──

    #[cfg(target_os = "macos")]
    #[test]
    fn extra_ca_verifier_new_exempts_the_bundle_cas_except_apple_roots() {
        let apple_root = TrustSettings::new(Domain::System)
            .iter()
            .unwrap()
            .next()
            .unwrap()
            .to_der();
        let bundle = vec![CertificateDer::from(apple_root), cert(ISSUING_CA_PEM)];

        let verifier = ExtraCaVerifier::new(
            bundle,
            &Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
        )
        .unwrap();

        let exempt = &verifier.exempt;
        let apple: Vec<_> = exempt.apple.iter().map(SecCertificate::to_der).collect();
        assert_eq!(apple, [cert(ISSUING_CA_PEM).to_vec()]);
        assert_eq!(
            exempt.webpki.roots,
            [webpki::anchor_from_trusted_cert(&cert(ISSUING_CA_PEM))
                .unwrap()
                .to_owned()],
        );
    }

    // ── ExtraCaVerifier::verify_server_cert ──

    #[cfg(target_os = "macos")]
    #[test]
    fn verify_server_cert_accepts_a_leaf_within_the_validity_limit() {
        let verifier = verifier(ISSUING_CA_PEM, TEST_CA_PEM);
        let within_validity = unix_time(datetime!(2026-12-01 00:00 UTC));

        verify_leaf(
            &verifier,
            SHORT_LIVED_LEAF_PEM,
            "localhost",
            &[],
            within_validity,
        )
        .unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn verify_server_cert_keeps_the_validity_limit_for_a_ca_outside_the_bundle() {
        let verifier = verifier(ISSUING_CA_PEM, TEST_CA_PEM);

        let err = verify_leaf(
            &verifier,
            LONG_LIVED_LEAF_PEM,
            "localhost",
            &[],
            UnixTime::now(),
        )
        .unwrap_err();

        assert!(
            has_os_status(&err, os_status::CERTIFICATE_VALIDITY_PERIOD_TOO_LONG),
            "{err}",
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn verify_server_cert_accepts_a_long_lived_leaf_under_an_intermediate_without_usages() {
        let verifier = verifier(ISSUING_CA_PEM, ISSUING_CA_PEM);

        let verdict = verifier.verify_server_cert(
            &cert(INTERMEDIATE_LEAF_PEM),
            &[cert(INTERMEDIATE_CA_PEM)],
            &ServerName::try_from("localhost").unwrap(),
            &[],
            UnixTime::now(),
        );

        verdict.unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn verify_server_cert_rejects_a_bundle_leaf_for_another_name() {
        let verifier = verifier(ISSUING_CA_PEM, ISSUING_CA_PEM);

        let err = verify_leaf(
            &verifier,
            LONG_LIVED_LEAF_PEM,
            "example.com",
            &[],
            UnixTime::now(),
        )
        .unwrap_err();

        assert_eq!(
            err,
            Error::InvalidCertificate(CertificateError::NotValidForNameContext {
                expected: ServerName::try_from("example.com").unwrap(),
                presented: vec!["DnsName(\"localhost\")".to_owned()],
            }),
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn verify_server_cert_rejects_an_expired_bundle_leaf() {
        let verifier = verifier(ISSUING_CA_PEM, ISSUING_CA_PEM);
        let after_expiry = unix_time(datetime!(2126-09-24 00:00 UTC));

        let err = verify_leaf(
            &verifier,
            LONG_LIVED_LEAF_PEM,
            "localhost",
            &[],
            after_expiry,
        )
        .unwrap_err();

        assert_eq!(
            err,
            Error::InvalidCertificate(CertificateError::ExpiredContext {
                time: after_expiry,
                not_after: unix_time(datetime!(2126-09-23 05:40:17 UTC)),
            }),
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn verify_server_cert_rejects_a_bundle_leaf_without_the_server_auth_usage() {
        let verifier = verifier(ISSUING_CA_PEM, ISSUING_CA_PEM);

        let err = verify_leaf(
            &verifier,
            NO_USAGE_LEAF_PEM,
            "localhost",
            &[],
            UnixTime::now(),
        )
        .unwrap_err();

        assert_eq!(
            err,
            Error::InvalidCertificate(CertificateError::InvalidPurpose)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn verify_server_cert_rejects_a_revoked_bundle_leaf() {
        let verifier = verifier(ISSUING_CA_PEM, ISSUING_CA_PEM);

        let err = verify_leaf(
            &verifier,
            REVOKED_LEAF_PEM,
            "localhost",
            &revoked_leaf_ocsp(),
            UnixTime::now(),
        )
        .unwrap_err();

        assert_eq!(err, Error::InvalidCertificate(CertificateError::Revoked));
    }

    // ── ExemptAnchors::verify ──

    #[cfg(target_os = "macos")]
    #[test]
    fn exempt_anchors_verify_enforces_a_stapled_revocation() {
        let exempt = exempt_anchors(ISSUING_CA_PEM);

        let err = exempt
            .verify(
                &cert(REVOKED_LEAF_PEM),
                &[],
                &ServerName::try_from("localhost").unwrap(),
                &revoked_leaf_ocsp(),
                UnixTime::now(),
            )
            .unwrap_err();

        assert_eq!(err, Error::InvalidCertificate(CertificateError::Revoked));
    }

    // ── ExemptAnchors::evaluate_basic ──

    #[cfg(target_os = "macos")]
    #[test]
    fn exempt_anchors_evaluate_basic_rejects_a_chain_to_another_anchor() {
        let exempt = exempt_anchors(TEST_CA_PEM);

        let err = exempt
            .evaluate_basic(&cert(LONG_LIVED_LEAF_PEM), &[], &[], UnixTime::now())
            .unwrap_err();

        assert!(has_os_status(&err, -25_318), "{err}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn exempt_anchors_evaluate_basic_leaves_the_ocsp_responder_alone() {
        let responder = TcpListener::bind(OCSP_RESPONDER_ADDR).unwrap();
        responder.set_nonblocking(true).unwrap();
        let exempt = exempt_anchors(ISSUING_CA_PEM);

        exempt
            .evaluate_basic(&cert(OCSP_LEAF_PEM), &[], &[], UnixTime::now())
            .unwrap();

        let err = responder.accept().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
    }
}
