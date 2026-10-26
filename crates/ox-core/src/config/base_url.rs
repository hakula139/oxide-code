//! The validated API base URL and the endpoint requests go to.

use std::fmt;

use anyhow::{Result, bail, ensure};
use url::Host;

/// A validated API base URL without userinfo, parsed once at startup and shown as configured.
#[derive(Clone)]
pub(crate) struct BaseUrl {
    raw: String,
    url: reqwest::Url,
}

impl BaseUrl {
    pub(super) fn parse(raw: &str) -> Result<Self> {
        let url = reqwest::Url::parse(raw)?;
        ensure!(
            url.username().is_empty() && url.password().is_none(),
            "a base URL cannot carry a username or password. \
             Remove them, and set the credential with ANTHROPIC_AUTH_TOKEN, \
             ANTHROPIC_API_KEY, or [client.auth]."
        );
        ensure!(
            url.query().is_none() && url.fragment().is_none(),
            "query strings and fragments are not supported. Remove them from the base URL."
        );
        match url.scheme() {
            "https" => {}
            "http" if is_loopback_url(&url) => {}
            "http" => bail!("http is allowed only for localhost"),
            scheme => bail!("expected http or https, got {scheme:?}"),
        }
        Ok(Self {
            raw: raw.to_owned(),
            url,
        })
    }

    /// Returns whether the URL points at Anthropic's production or staging host. Gates the
    /// stored-login fallback, body-side `cache_control.scope = "global"`, and the billing `cch` hash.
    pub(crate) fn is_first_party(&self) -> bool {
        // `Url` lowercases the host of an `http` or `https` URL.
        matches!(
            self.url.host_str(),
            Some("api.anthropic.com" | "api-staging.anthropic.com")
        )
    }

    /// The `/v1/messages` endpoint under this base URL, dropping a trailing slash as the Anthropic
    /// SDKs do, since routers that keep empty path segments answer `//v1/messages` with a 404.
    pub(crate) fn messages_url(&self) -> reqwest::Url {
        let mut url = self.url.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .expect("an http base URL has path segments");
            segments.pop_if_empty().push("v1").push("messages");
        }
        url.set_query(Some("beta=true"));
        url
    }
}

impl fmt::Debug for BaseUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("BaseUrl").field(&self.raw).finish()
    }
}

impl fmt::Display for BaseUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

/// `raw` with any userinfo removed, for an error that rejects it without echoing the password.
/// Works without a successful parse, since a malformed URL can carry userinfo too.
pub(super) fn redacted(raw: &str) -> String {
    let Some((scheme, rest)) = raw.split_once("://") else {
        return raw.to_owned();
    };
    let authority = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let Some(at) = rest[..authority].rfind('@') else {
        return raw.to_owned();
    };
    format!("{scheme}://{}", &rest[at + 1..])
}

fn is_loopback_url(url: &reqwest::Url) -> bool {
    match url.host() {
        Some(Host::Domain("localhost")) => true,
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        Some(Host::Domain(_)) | None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::testing::base_url;

    mod is_first_party {
        use super::*;

        #[test]
        fn accepts_official_hosts() {
            assert!(base_url("https://api.anthropic.com").is_first_party());
            assert!(base_url("https://api.anthropic.com/").is_first_party());
            assert!(base_url("https://api-staging.anthropic.com").is_first_party());
            assert!(base_url("https://API.ANTHROPIC.COM").is_first_party());
        }

        #[test]
        fn rejects_proxies_and_loopback_hosts() {
            assert!(!base_url("https://api.openai.com").is_first_party());
            assert!(!base_url("https://proxy.example.com").is_first_party());
            assert!(!base_url("https://anthropic.com.evil.io").is_first_party());
            assert!(!base_url("http://127.0.0.1:8080").is_first_party());
        }
    }

    mod messages_url {
        use super::*;

        #[test]
        fn joins_the_messages_path_under_a_path_prefix_and_trailing_slash() {
            for (raw, expected) in [
                (
                    "https://example.com",
                    "https://example.com/v1/messages?beta=true",
                ),
                (
                    "https://example.com/",
                    "https://example.com/v1/messages?beta=true",
                ),
                (
                    "https://example.com/anthropic",
                    "https://example.com/anthropic/v1/messages?beta=true",
                ),
                (
                    "https://example.com/anthropic/",
                    "https://example.com/anthropic/v1/messages?beta=true",
                ),
                (
                    "http://127.0.0.1:8080",
                    "http://127.0.0.1:8080/v1/messages?beta=true",
                ),
            ] {
                assert_eq!(base_url(raw).messages_url().as_str(), expected, "{raw}");
            }
        }
    }

    mod redacted {
        use super::*;

        #[test]
        fn cuts_userinfo_without_parsing_the_url() {
            for (raw, expected) in [
                (
                    "https://user:secret@gw.example:99999/anthropic",
                    "https://gw.example:99999/anthropic",
                ),
                (
                    "https://user@gw.example?tenant=x",
                    "https://gw.example?tenant=x",
                ),
                ("https://user@[::1]:8080/x", "https://[::1]:8080/x"),
                ("https://gw.example/x", "https://gw.example/x"),
                ("gw.example", "gw.example"),
            ] {
                assert_eq!(redacted(raw), expected, "{raw}");
            }
        }
    }
}
