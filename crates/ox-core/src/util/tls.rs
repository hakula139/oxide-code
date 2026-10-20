//! The roots HTTP clients trust, including extra anchors so corporate and self-signed endpoints
//! whose CA the platform trust store lacks still verify.

use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::{cell::Cell, collections::HashSet, fmt, sync::Arc};

use anyhow::{Context, Result, bail};
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

// ── Trust ──

/// The roots an HTTP client verifies servers against. Tests substitute roots that read no system
/// store.
pub trait Trust: Send + Sync {
    /// Configures `builder` to verify servers against these roots, possibly replacing its TLS
    /// options. Set TLS options before calling this method only when they may be replaced.
    fn apply(&self, builder: ClientBuilder) -> Result<ClientBuilder>;
}

/// The platform trust store, plus the PEM bundle at the path when one is set.
pub struct PlatformTrust(pub Option<PathBuf>);

impl Trust for PlatformTrust {
    fn apply(&self, builder: ClientBuilder) -> Result<ClientBuilder> {
        apply_extra_ca_certs(builder, self.0.as_deref())
    }
}

// ── Client Builder ──

/// Trusts the PEM bundle at `path` on top of the platform trust store, or returns `builder`
/// unchanged for `None`.
fn apply_extra_ca_certs(builder: ClientBuilder, path: Option<&Path>) -> Result<ClientBuilder> {
    apply_extra_ca_certs_with(builder, path, trust_extra_cas)
}

fn apply_extra_ca_certs_with(
    builder: ClientBuilder,
    path: Option<&Path>,
    trust: impl FnOnce(ClientBuilder, Vec<CertificateDer<'static>>) -> Result<ClientBuilder>,
) -> Result<ClientBuilder> {
    let Some(path) = path else {
        return Ok(builder);
    };
    trust(builder, load_extra_ca_certs(path)?)
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
    let trust: Arc<dyn AppleTrust> = Arc::new(SecurityFramework);
    trust_extra_cas_with(builder, certs, &trust)
}

#[cfg(target_os = "macos")]
fn trust_extra_cas_with(
    builder: ClientBuilder,
    certs: Vec<CertificateDer<'static>>,
    trust: &Arc<dyn AppleTrust>,
) -> Result<ClientBuilder> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let verifier = ExtraCaVerifier::new(certs, &provider, trust)?;
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
    platform: Box<dyn ServerCertVerifier>,
    exempt: ExemptAnchors,
}

#[cfg(target_os = "macos")]
impl ExtraCaVerifier {
    /// Exempts the bundle CAs other than Apple's built-in roots, or none when those roots cannot
    /// be read.
    fn new(
        bundle: Vec<CertificateDer<'static>>,
        provider: &Arc<CryptoProvider>,
        trust: &Arc<dyn AppleTrust>,
    ) -> Result<Self> {
        let exempt = match trust.roots() {
            Ok(roots) => ExemptAnchors::new(
                bundle.iter().filter(|cert| !roots.contains(cert.as_ref())),
                provider,
                Arc::clone(trust),
            )?,
            Err(e) => {
                warn!("keeping Apple's validity limit for extra CAs: {e:#}");
                ExemptAnchors::new([], provider, Arc::clone(trust))?
            }
        };
        let platform = trust.verifier(bundle, Arc::clone(provider))?;
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

/// Returns whether the platform verifier rejected the chain with
/// `errSecCertificateValidityPeriodTooLong`, which it reports only in the message because rustls
/// has no variant for it. Apple reports this code ahead of untrusted anchor, hostname, extended key
/// usage, and expiry failures, but behind a revocation.
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

/// The bundle CAs whose chains may exceed the validity limit.
#[cfg(target_os = "macos")]
#[derive(Debug)]
struct ExemptAnchors {
    webpki: RootCertStore,
    apple: Vec<SecCertificate>,
    algorithms: WebPkiSupportedAlgorithms,
    trust: Arc<dyn AppleTrust>,
}

#[cfg(target_os = "macos")]
impl ExemptAnchors {
    fn new<'a>(
        anchors: impl IntoIterator<Item = &'a CertificateDer<'static>>,
        provider: &CryptoProvider,
        trust: Arc<dyn AppleTrust>,
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
            trust,
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
        self.trust.evaluate(&BasicEvaluation {
            end_entity,
            intermediates,
            anchors: &self.apple,
            anchors_only: true,
            revocation: RevocationPolicy::USE_ANY_METHOD_AVAILABLE
                | RevocationPolicy::NETWORK_ACCESS_DISABLED,
            ocsp_response,
            verify_date: now,
        })
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

// ── AppleTrust ──

/// `kCFAbsoluteTimeIntervalSince1970`, the Unix time of Core Foundation's 2001 epoch.
#[cfg(target_os = "macos")]
const CF_EPOCH_UNIX_SECS: u64 = 978_307_200;

/// The Security framework services behind [`ExtraCaVerifier`]. Tests fake them because the Nix
/// build sandbox blocks trustd, which they rely on.
#[cfg(target_os = "macos")]
trait AppleTrust: fmt::Debug + Send + Sync {
    /// Returns the DER encodings of Apple's built-in roots.
    fn roots(&self) -> Result<HashSet<Vec<u8>>>;

    /// Returns the platform verifier, trusting `extra_roots` on top of the system trust store.
    fn verifier(
        &self,
        extra_roots: Vec<CertificateDer<'static>>,
        provider: Arc<CryptoProvider>,
    ) -> Result<Box<dyn ServerCertVerifier>>;

    /// Evaluates the chain, failing with the rustls error that describes the rejection.
    fn evaluate(&self, evaluation: &BasicEvaluation<'_>) -> Result<(), Error>;
}

/// A chain for the Security framework to evaluate under the basic X.509 policy, which keeps Apple's
/// chain, expiry, trust setting, and revocation checks without the TLS policy that carries the
/// validity limit.
#[cfg(target_os = "macos")]
#[derive(Debug)]
struct BasicEvaluation<'a> {
    end_entity: &'a CertificateDer<'a>,
    intermediates: &'a [CertificateDer<'a>],
    anchors: &'a [SecCertificate],
    /// Trusts only `anchors`, ignoring the system roots and keychains.
    anchors_only: bool,
    /// Revocation relies on the stapled OCSP response and trustd's cache, since a network fetch
    /// would block the handshake on the responder.
    revocation: RevocationPolicy,
    ocsp_response: &'a [u8],
    verify_date: UnixTime,
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct SecurityFramework;

#[cfg(target_os = "macos")]
impl AppleTrust for SecurityFramework {
    /// Returns the roots in the system trust settings domain, since CAs installed in a keychain
    /// belong to the admin and user domains.
    fn roots(&self) -> Result<HashSet<Vec<u8>>> {
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

    fn verifier(
        &self,
        extra_roots: Vec<CertificateDer<'static>>,
        provider: Arc<CryptoProvider>,
    ) -> Result<Box<dyn ServerCertVerifier>> {
        let verifier = PlatformVerifier::new_with_extra_roots(extra_roots, provider)?;
        Ok(Box::new(verifier))
    }

    fn evaluate(&self, evaluation: &BasicEvaluation<'_>) -> Result<(), Error> {
        let general = |e: security_framework::base::Error| Error::General(e.to_string());
        let chain = std::iter::once(evaluation.end_entity)
            .chain(evaluation.intermediates)
            .map(|cert| SecCertificate::from_der(cert))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| Error::InvalidCertificate(CertificateError::BadEncoding))?;
        let revocation = SecPolicy::create_revocation(evaluation.revocation).map_err(general)?;
        let mut trust =
            SecTrust::create_with_certificates(&chain, &[SecPolicy::create_x509(), revocation])
                .map_err(general)?;
        let since_cf_epoch = evaluation
            .verify_date
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
        if !evaluation.ocsp_response.is_empty() {
            trust
                .set_trust_ocsp_response(std::iter::once(evaluation.ocsp_response))
                .map_err(general)?;
        }
        trust
            .set_anchor_certificates(evaluation.anchors)
            .map_err(general)?;
        trust
            .set_trust_anchor_certificates_only(evaluation.anchors_only)
            .map_err(general)?;

        trust
            .evaluate_with_error()
            .map_err(|e| evaluation_error(e.code(), &e.description().to_string()))
    }
}

/// Returns the rustls error for an evaluation that failed with the `OSStatus` `code`. A code other
/// than a revocation stays in the message, as the platform verifier reports codes rustls has no
/// variant for.
#[cfg(target_os = "macos")]
fn evaluation_error(code: isize, description: &str) -> Error {
    let reason = if code == os_status::CERTIFICATE_REVOKED as isize {
        CertificateError::Revoked
    } else {
        let message: Box<dyn std::error::Error + Send + Sync> =
            format!("{description}: {code}").into();
        CertificateError::Other(OtherError(Arc::from(message)))
    };
    Error::InvalidCertificate(reason)
}

// ── Testing ──

#[cfg(test)]
pub(crate) mod local_testing {
    use std::sync::Arc;

    use super::Trust;
    use super::testing::NoRoots;

    /// A self-signed P-256 CA, valid for 100 years. The CLI tests pin it as the trust store.
    pub(crate) const TEST_CA_PEM: &str = include_str!("../../tests/fixtures/test-ca.pem");

    /// [`NoRoots`] as a shared `Arc<dyn Trust>`.
    pub(crate) fn no_roots() -> Arc<dyn Trust> {
        Arc::new(NoRoots)
    }
}

#[cfg(any(test, feature = "testing"))]
pub(crate) mod testing {
    use anyhow::Result;
    use reqwest::ClientBuilder;

    use super::Trust;

    /// No roots, so building a client reads no system store. The client reaches only plain-HTTP
    /// servers, such as the mock servers.
    pub(crate) struct NoRoots;

    impl Trust for NoRoots {
        fn apply(&self, builder: ClientBuilder) -> Result<ClientBuilder> {
            Ok(builder.no_proxy().tls_certs_only([]))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::sync::Arc;
    #[cfg(target_os = "macos")]
    use std::sync::Mutex;

    use indoc::indoc;
    use rustls::pki_types::PrivateKeyDer;
    use tempfile::NamedTempFile;
    #[cfg(target_os = "macos")]
    use time::macros::datetime;

    use super::local_testing::TEST_CA_PEM;
    use super::*;

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

    fn write_pem(contents: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        file.flush().unwrap();
        file
    }

    fn cert(pem: &str) -> CertificateDer<'static> {
        CertificateDer::from_pem_slice(pem.as_bytes()).unwrap()
    }

    /// Apple trust services whose built-in roots are `roots`, or unreadable when `None`, whose
    /// platform verifier gives `platform_verdict`, and whose basic evaluation gives
    /// `evaluation_verdict`. They record the extra roots and evaluations they receive.
    #[cfg(target_os = "macos")]
    #[derive(Debug)]
    struct FakeAppleTrust {
        roots: Option<HashSet<Vec<u8>>>,
        platform_verdict: Result<(), Error>,
        evaluation_verdict: Result<(), Error>,
        extra_roots: Mutex<Vec<CertificateDer<'static>>>,
        evaluations: Mutex<Vec<RecordedEvaluation>>,
    }

    #[cfg(target_os = "macos")]
    impl FakeAppleTrust {
        /// Returns services with no built-in roots and basic evaluations that pass.
        fn new(platform_verdict: Result<(), Error>) -> Self {
            Self {
                roots: Some(HashSet::new()),
                platform_verdict,
                evaluation_verdict: Ok(()),
                extra_roots: Mutex::default(),
                evaluations: Mutex::default(),
            }
        }

        fn extra_roots(&self) -> Vec<CertificateDer<'static>> {
            self.extra_roots.lock().unwrap().clone()
        }

        fn evaluations(&self) -> Vec<RecordedEvaluation> {
            self.evaluations.lock().unwrap().clone()
        }
    }

    #[cfg(target_os = "macos")]
    impl AppleTrust for FakeAppleTrust {
        fn roots(&self) -> Result<HashSet<Vec<u8>>> {
            self.roots
                .clone()
                .context("the system trust settings are unreadable")
        }

        fn verifier(
            &self,
            extra_roots: Vec<CertificateDer<'static>>,
            provider: Arc<CryptoProvider>,
        ) -> Result<Box<dyn ServerCertVerifier>> {
            *self.extra_roots.lock().unwrap() = extra_roots;
            Ok(Box::new(FakePlatformVerifier {
                verdict: self.platform_verdict.clone(),
                algorithms: provider.signature_verification_algorithms,
            }))
        }

        fn evaluate(&self, evaluation: &BasicEvaluation<'_>) -> Result<(), Error> {
            self.evaluations.lock().unwrap().push(evaluation.into());
            self.evaluation_verdict.clone()
        }
    }

    /// An owned copy of a [`BasicEvaluation`], with the leaf leading the chain and the anchors
    /// in DER.
    #[cfg(target_os = "macos")]
    #[derive(Clone, Debug, PartialEq)]
    struct RecordedEvaluation {
        chain: Vec<CertificateDer<'static>>,
        anchors: Vec<CertificateDer<'static>>,
        anchors_only: bool,
        revocation: RevocationPolicy,
        ocsp_response: Vec<u8>,
        verify_date: UnixTime,
    }

    #[cfg(target_os = "macos")]
    impl From<&BasicEvaluation<'_>> for RecordedEvaluation {
        fn from(evaluation: &BasicEvaluation<'_>) -> Self {
            Self {
                chain: std::iter::once(evaluation.end_entity)
                    .chain(evaluation.intermediates)
                    .map(|cert| cert.clone().into_owned())
                    .collect(),
                anchors: evaluation
                    .anchors
                    .iter()
                    .map(|anchor| anchor.to_der().into())
                    .collect(),
                anchors_only: evaluation.anchors_only,
                revocation: evaluation.revocation,
                ocsp_response: evaluation.ocsp_response.to_vec(),
                verify_date: evaluation.verify_date,
            }
        }
    }

    /// A platform verifier that gives `verdict` for every chain and checks handshake signatures
    /// with webpki.
    #[cfg(target_os = "macos")]
    #[derive(Debug)]
    struct FakePlatformVerifier {
        verdict: Result<(), Error>,
        algorithms: WebPkiSupportedAlgorithms,
    }

    #[cfg(target_os = "macos")]
    impl ServerCertVerifier for FakePlatformVerifier {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            self.verdict
                .clone()
                .map(|()| ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.algorithms.supported_schemes()
        }
    }

    /// Returns the rejection that the platform verifier gave [`LONG_LIVED_LEAF_PEM`] under a
    /// trusted [`ISSUING_CA_PEM`], recorded on macOS 27.0.1 with rustls-platform-verifier 0.7.1.
    #[cfg(target_os = "macos")]
    fn validity_limit_rejection() -> Error {
        other_error("\u{201c}localhost\u{201d} certificate is not standards compliant: -67901")
    }

    /// Returns an error carrying `message` the way the platform verifier reports a code that
    /// rustls has no variant for.
    #[cfg(target_os = "macos")]
    fn other_error(message: &str) -> Error {
        let message: Box<dyn std::error::Error + Send + Sync> = message.into();
        Error::InvalidCertificate(CertificateError::Other(OtherError(Arc::from(message))))
    }

    /// Returns the message of an error that carries a code rustls has no variant for.
    /// `OtherError` never compares equal, so tests compare this.
    #[cfg(target_os = "macos")]
    fn other_message(err: &Error) -> String {
        match err {
            Error::InvalidCertificate(CertificateError::Other(other)) => other.to_string(),
            err => panic!("expected a code without a rustls variant: {err:?}"),
        }
    }

    mod apply_extra_ca_certs {
        use super::*;

        /// The throwaway private key of [`LONG_LIVED_LEAF_PEM`].
        const LONG_LIVED_LEAF_KEY_PEM: &str = indoc! {"
            -----BEGIN EC PRIVATE KEY-----
            MHcCAQEEINmz7sOs20KgX0/M7R5/avsn9FXV266iOrOi4ABp1YpDoAoGCCqGSM49
            AwEHoUQDQgAEnasGs4XIF2Eer/xbi0+ASrJ2oJ0ol6qjVMQ/8/F4atf4sxl70Ivd
            ppr9cqydr+0vdpCk1H0tbv0yTBYz/Kg5iA==
            -----END EC PRIVATE KEY-----
        "};

        /// Answers one HTTPS request on a loopback port with `ok`, presenting
        /// [`LONG_LIVED_LEAF_PEM`] over `version`.
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

        /// Returns a client for [`serve_once`] at `addr`, which it reaches as `localhost`.
        fn localhost_client(addr: SocketAddr) -> ClientBuilder {
            reqwest::Client::builder()
                .no_proxy()
                .resolve("localhost", addr)
        }

        async fn fetch_root(builder: ClientBuilder, addr: SocketAddr) -> anyhow::Result<String> {
            let response = builder
                .build()?
                .get(format!("https://localhost:{}/", addr.port()))
                .send()
                .await?;
            Ok(response.text().await?)
        }

        #[cfg(not(target_os = "macos"))]
        fn trust_bundle(
            builder: ClientBuilder,
            bundle: Vec<CertificateDer<'static>>,
        ) -> Result<ClientBuilder> {
            trust_extra_cas(builder, bundle)
        }

        /// Calls [`trust_extra_cas_with`] on Apple trust services whose platform verifier rejects
        /// every chain the way Apple's rejects [`LONG_LIVED_LEAF_PEM`].
        #[cfg(target_os = "macos")]
        fn trust_bundle(
            builder: ClientBuilder,
            bundle: Vec<CertificateDer<'static>>,
        ) -> Result<ClientBuilder> {
            let trust: Arc<dyn AppleTrust> =
                Arc::new(FakeAppleTrust::new(Err(validity_limit_rejection())));
            trust_extra_cas_with(builder, bundle, &trust)
        }

        #[test]
        fn surfaces_loader_error_with_path() {
            let missing = Path::new("/definitely/does/not/exist.pem");

            let err = apply_extra_ca_certs(reqwest::Client::builder(), Some(missing))
                .expect_err("missing path must error");

            assert_eq!(
                err.to_string(),
                "failed to read extra CA bundle at /definitely/does/not/exist.pem",
            );
        }

        #[tokio::test]
        async fn connects_to_a_long_lived_leaf_from_the_bundle() {
            let bundle = write_pem(ISSUING_CA_PEM);
            for version in rustls::ALL_VERSIONS {
                let addr = serve_once(version);
                let builder = apply_extra_ca_certs_with(
                    localhost_client(addr),
                    Some(bundle.path()),
                    trust_bundle,
                )
                .unwrap();

                let body = fetch_root(builder, addr)
                    .await
                    .unwrap_or_else(|e| panic!("{version:?}: {e:#}"));

                assert_eq!(body, "ok", "{version:?}");
            }
        }
    }

    mod load_extra_ca_certs {
        use super::*;

        #[test]
        fn parses_single_and_multi_block_bundles() {
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
        fn rejects_empty_bundle() {
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
        fn reports_filename_on_parse_failure() {
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
        fn rejects_a_block_that_is_not_a_certificate() {
            let file = write_pem(&format!(
                "{TEST_CA_PEM}{}",
                indoc! {"
                    -----BEGIN CERTIFICATE-----
                    AAAA
                    -----END CERTIFICATE-----
                "}
            ));

            let err =
                load_extra_ca_certs(file.path()).expect_err("a malformed certificate must error");

            assert_eq!(
                format!("{err:#}"),
                format!(
                    "certificate 2 in {} is not a valid X.509 certificate",
                    file.path().display(),
                ),
            );
        }
    }

    #[cfg(target_os = "macos")]
    mod extra_ca_verifier {
        use super::*;

        /// Returns an [`ExtraCaVerifier`] on `trust` for the PEM certificates in `bundle`.
        fn verifier(trust: &Arc<FakeAppleTrust>, bundle: &[&str]) -> ExtraCaVerifier {
            let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
            let bundle = bundle.iter().map(|pem| cert(pem)).collect();
            let trust: Arc<dyn AppleTrust> = Arc::<FakeAppleTrust>::clone(trust);
            ExtraCaVerifier::new(bundle, &provider, &trust).unwrap()
        }

        fn verify_leaf(
            verifier: &ExtraCaVerifier,
            leaf_pem: &str,
            name: &str,
            now: UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            verifier.verify_server_cert(
                &cert(leaf_pem),
                &[],
                &ServerName::try_from(name.to_owned()).unwrap(),
                &[],
                now,
            )
        }

        mod new {
            use super::*;

            #[test]
            fn exempts_the_bundle_cas_outside_apple_roots() {
                let trust = Arc::new(FakeAppleTrust {
                    roots: Some(HashSet::from([cert(TEST_CA_PEM).to_vec()])),
                    ..FakeAppleTrust::new(Err(validity_limit_rejection()))
                });
                let verifier = verifier(&trust, &[TEST_CA_PEM, ISSUING_CA_PEM]);

                verify_leaf(&verifier, LONG_LIVED_LEAF_PEM, "localhost", UnixTime::now()).unwrap();

                assert_eq!(
                    trust.extra_roots(),
                    [cert(TEST_CA_PEM), cert(ISSUING_CA_PEM)]
                );
                let anchors: Vec<_> = trust
                    .evaluations()
                    .into_iter()
                    .map(|evaluation| evaluation.anchors)
                    .collect();
                assert_eq!(anchors, [vec![cert(ISSUING_CA_PEM)]]);
            }

            #[test]
            fn exempts_no_ca_when_apple_roots_cannot_be_read() {
                let trust = Arc::new(FakeAppleTrust {
                    roots: None,
                    ..FakeAppleTrust::new(Err(validity_limit_rejection()))
                });
                let verifier = verifier(&trust, &[ISSUING_CA_PEM]);

                let err = verify_leaf(&verifier, LONG_LIVED_LEAF_PEM, "localhost", UnixTime::now())
                    .unwrap_err();

                assert_eq!(
                    other_message(&err),
                    other_message(&validity_limit_rejection())
                );
                assert_eq!(trust.extra_roots(), [cert(ISSUING_CA_PEM)]);
                assert_eq!(trust.evaluations(), Vec::new());
            }
        }

        mod verify_server_cert {
            use super::*;

            /// [`LONG_LIVED_LEAF_PEM`] without the extended key usage extension.
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

            fn unix_time(at: time::OffsetDateTime) -> UnixTime {
                UnixTime::since_unix_epoch(std::time::Duration::from_secs(
                    u64::try_from(at.unix_timestamp()).unwrap(),
                ))
            }

            #[test]
            fn keeps_a_platform_verdict_other_than_the_validity_limit() {
                for verdict in [
                    Ok(()),
                    Err(Error::InvalidCertificate(CertificateError::Revoked)),
                ] {
                    let trust = Arc::new(FakeAppleTrust::new(verdict.clone()));
                    let verifier = verifier(&trust, &[ISSUING_CA_PEM]);

                    let result =
                        verify_leaf(&verifier, LONG_LIVED_LEAF_PEM, "localhost", UnixTime::now());

                    assert_eq!(result.map(drop), verdict);
                    assert_eq!(trust.evaluations(), Vec::new(), "{verdict:?}");
                }
            }

            #[test]
            fn keeps_the_validity_limit_for_a_ca_outside_the_bundle() {
                let trust = Arc::new(FakeAppleTrust::new(Err(validity_limit_rejection())));
                let verifier = verifier(&trust, &[TEST_CA_PEM]);

                let err = verify_leaf(&verifier, LONG_LIVED_LEAF_PEM, "localhost", UnixTime::now())
                    .unwrap_err();

                assert_eq!(
                    other_message(&err),
                    other_message(&validity_limit_rejection())
                );
                assert_eq!(trust.evaluations(), Vec::new());
            }

            #[test]
            fn rejects_a_bundle_leaf_for_another_name() {
                let trust = Arc::new(FakeAppleTrust::new(Err(validity_limit_rejection())));
                let verifier = verifier(&trust, &[ISSUING_CA_PEM]);

                let err = verify_leaf(
                    &verifier,
                    LONG_LIVED_LEAF_PEM,
                    "example.com",
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

            #[test]
            fn rejects_an_expired_bundle_leaf() {
                let trust = Arc::new(FakeAppleTrust::new(Err(validity_limit_rejection())));
                let verifier = verifier(&trust, &[ISSUING_CA_PEM]);
                let after_expiry = unix_time(datetime!(2126-09-24 00:00 UTC));

                let err = verify_leaf(&verifier, LONG_LIVED_LEAF_PEM, "localhost", after_expiry)
                    .unwrap_err();

                assert_eq!(
                    err,
                    Error::InvalidCertificate(CertificateError::ExpiredContext {
                        time: after_expiry,
                        not_after: unix_time(datetime!(2126-09-23 05:40:17 UTC)),
                    }),
                );
            }

            #[test]
            fn rejects_a_bundle_leaf_without_the_server_auth_usage() {
                let trust = Arc::new(FakeAppleTrust::new(Err(validity_limit_rejection())));
                let verifier = verifier(&trust, &[ISSUING_CA_PEM]);

                let err = verify_leaf(&verifier, NO_USAGE_LEAF_PEM, "localhost", UnixTime::now())
                    .unwrap_err();

                assert_eq!(
                    err,
                    Error::InvalidCertificate(CertificateError::InvalidPurpose)
                );
            }

            #[test]
            fn rejects_a_bundle_leaf_the_basic_evaluation_rejects() {
                let revoked = Error::InvalidCertificate(CertificateError::Revoked);
                let trust = Arc::new(FakeAppleTrust {
                    evaluation_verdict: Err(revoked.clone()),
                    ..FakeAppleTrust::new(Err(validity_limit_rejection()))
                });
                let verifier = verifier(&trust, &[ISSUING_CA_PEM]);

                let err = verify_leaf(&verifier, LONG_LIVED_LEAF_PEM, "localhost", UnixTime::now())
                    .unwrap_err();

                assert_eq!(err, revoked);
            }
        }
    }

    #[cfg(target_os = "macos")]
    mod exceeds_validity_limit {
        use super::*;

        #[test]
        fn matches_only_the_validity_limit_code() {
            let others = [
                other_error("\u{201c}localhost\u{201d} certificate is not trusted: -25318"),
                Error::InvalidCertificate(CertificateError::Revoked),
                Error::General("validity period too long: -67901".to_owned()),
            ];

            assert!(exceeds_validity_limit(&validity_limit_rejection()));
            for other in others {
                assert!(!exceeds_validity_limit(&other), "{other:?}");
            }
        }
    }

    #[cfg(target_os = "macos")]
    mod exempt_anchors {
        use super::*;

        mod verify {
            use super::*;

            /// An intermediate CA under [`ISSUING_CA_PEM`] without the extended key usage
            /// extension, as private CAs often issue them.
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

            #[test]
            fn hands_an_intermediate_chain_and_staple_to_the_basic_evaluation() {
                let trust = Arc::new(FakeAppleTrust::new(Ok(())));
                let provider = rustls::crypto::aws_lc_rs::default_provider();
                let exempt = ExemptAnchors::new(
                    [&cert(ISSUING_CA_PEM)],
                    &provider,
                    Arc::<FakeAppleTrust>::clone(&trust),
                )
                .unwrap();
                let now = UnixTime::now();

                exempt
                    .verify(
                        &cert(INTERMEDIATE_LEAF_PEM),
                        &[cert(INTERMEDIATE_CA_PEM)],
                        &ServerName::try_from("localhost").unwrap(),
                        b"stapled OCSP response",
                        now,
                    )
                    .unwrap();

                assert_eq!(
                    trust.evaluations(),
                    [RecordedEvaluation {
                        chain: vec![cert(INTERMEDIATE_LEAF_PEM), cert(INTERMEDIATE_CA_PEM)],
                        anchors: vec![cert(ISSUING_CA_PEM)],
                        anchors_only: true,
                        revocation: RevocationPolicy::USE_ANY_METHOD_AVAILABLE
                            | RevocationPolicy::NETWORK_ACCESS_DISABLED,
                        ocsp_response: b"stapled OCSP response".to_vec(),
                        verify_date: now,
                    }],
                );
            }
        }
    }

    #[cfg(target_os = "macos")]
    mod evaluation_error {
        use super::*;

        #[test]
        fn maps_a_revocation_and_keeps_other_codes_in_the_message() {
            let revoked = evaluation_error(-67_820, "revoked");
            let untrusted = evaluation_error(
                -25_318,
                "\u{201c}localhost\u{201d} certificate is not trusted",
            );

            assert_eq!(
                revoked,
                Error::InvalidCertificate(CertificateError::Revoked)
            );
            assert_eq!(
                other_message(&untrusted),
                "\u{201c}localhost\u{201d} certificate is not trusted: -25318",
            );
        }
    }
}
