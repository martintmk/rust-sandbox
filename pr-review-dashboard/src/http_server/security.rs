// Licensed under the MIT License.

//! Security seams shared by every request: request-target/body limits, a per-process CSRF token,
//! and Host/Origin loopback validation.
//!
//! The dashboard only ever binds to a loopback address ([`AppConfig`](crate::config::AppConfig)
//! rejects anything else at startup), but a loopback bind alone does not stop a malicious page
//! open in the operator's browser from directing requests at it: the browser is happy to send
//! cross-origin requests to `http://127.0.0.1:<port>`, and DNS rebinding can make an
//! attacker-controlled hostname resolve to a loopback address after the fact. This module layers
//! three independent checks against that:
//!
//! * **Host header validation** ([`is_loopback_authority`]) rejects any request whose `Host`
//!   header is not `localhost` or a loopback IP literal on the server's own port, closing the DNS
//!   rebinding gap.
//! * **CSRF token** ([`CsrfToken`]) is a random value minted once per process and required, via a
//!   custom header, on every state-changing request. A cross-origin page cannot read the token
//!   (same-origin policy blocks reading the response that carries it), so it cannot forge the
//!   header even if it can cause the browser to send a request.
//! * **Origin/Referer validation** ([`authorize_mutation`]) is defense in depth: ordinary web
//!   origins must name the loopback server. Opaque and application-scheme origins used by embedded
//!   browsers are accepted only after the per-process CSRF token has already been validated.

use std::collections::hash_map::RandomState;
use std::fmt::Write as _;
use std::hash::BuildHasher as _;
use std::net::IpAddr;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Limited};
use hyper::body::Incoming;
use hyper::http::uri::Authority;
use hyper::{HeaderMap, Uri};

/// Header carrying the per-process CSRF token on state-changing requests.
pub(crate) const CSRF_HEADER_NAME: &str = "x-csrf-token";

/// Upper bound on the raw request target (`path?query`) length, enforced before route
/// resolution — `routerama` itself does not impose one (see its README's "Securing route
/// resolution" section).
pub(crate) const MAX_REQUEST_TARGET_LEN: usize = 2048;

/// Upper bound on a request body, enforced while draining it via [`http_body_util::Limited`].
/// Settings mutations carry prompts and repository fields in the request body.
/// Every POST handler drains and bounds its body before acting.
pub(crate) const MAX_BODY_BYTES: usize = 64 * 1024;

/// Upper bound on the number of headers hyper will parse for one request.
pub(crate) const MAX_HEADER_COUNT: usize = 64;

/// How long hyper waits for a complete header block before giving up on a connection.
pub(crate) const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// A random value minted once when the server starts, required (via [`CSRF_HEADER_NAME`]) on
/// every state-changing request. Deliberately opaque in `Debug` so it never ends up in logs.
#[derive(Clone)]
pub(crate) struct CsrfToken(std::sync::Arc<str>);

impl CsrfToken {
    /// Generates a fresh, unpredictable token from the operating system's randomness, without
    /// depending on a cryptography crate: [`RandomState`] reseeds itself from OS entropy on every
    /// construction, so hashing an arbitrary value with a freshly constructed one yields a
    /// process-unpredictable 64-bit word. Four words (256 bits) give a comfortable margin over
    /// what a guessing attacker could exploit within a process lifetime.
    pub(crate) fn generate() -> Self {
        let mut token = String::with_capacity(64);
        for seed in 0u8..4 {
            let word = RandomState::new().hash_one(seed);
            write!(token, "{word:016x}").expect("writing hex digits into a String never fails");
        }
        Self(std::sync::Arc::from(token))
    }

    pub(crate) fn value(&self) -> &str {
        &self.0
    }

    /// Constant-time comparison: a CSRF token is a secret, so comparing it with `==` (which can
    /// short-circuit on the first differing byte) would leak timing information about how much of
    /// a guess was correct.
    pub(crate) fn is_valid(&self, candidate: &str) -> bool {
        let expected = self.0.as_bytes();
        let actual = candidate.as_bytes();
        if expected.len() != actual.len() {
            return false;
        }
        let mut mismatch = 0u8;
        for (left, right) in expected.iter().zip(actual) {
            mismatch |= left ^ right;
        }
        mismatch == 0
    }
}

impl std::fmt::Debug for CsrfToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CsrfToken").field(&"..").finish()
    }
}

/// A request body could not be read within the enforced limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BodyReadError {
    /// The body exceeded [`MAX_BODY_BYTES`].
    TooLarge,
    /// The connection failed while streaming the body.
    Invalid,
}

/// A mutation request failed CSRF or Origin/Referer validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MutationRejected {
    MissingOrInvalidCsrfToken,
    CrossOriginRequest,
}

/// Parses an HTTP `Host` header (or an `Origin`/`Referer` authority) and reports whether it names
/// the loopback address this server is bound to, on the expected port. Accepts `localhost` and
/// any IP literal for which [`IpAddr::is_loopback`] holds (`127.0.0.0/8`, `::1`); rejects anything
/// else, including a bare hostname that merely happens to resolve to a loopback address today.
pub(crate) fn is_loopback_authority(raw: &str, expected_port: u16) -> bool {
    let Ok(authority) = raw.parse::<Authority>() else {
        return false;
    };
    if authority.port_u16() != Some(expected_port) {
        return false;
    }
    let host = authority.host();
    // `Authority::host()` keeps the surrounding brackets for IPv6 literals (e.g. `[::1]`), which
    // `IpAddr::from_str` rejects, so strip them before parsing.
    let bare_host = host.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')).unwrap_or(host);
    host.eq_ignore_ascii_case("localhost") || bare_host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Parses an absolute `Origin` or `Referer` header value and validates its scheme and authority
/// against the loopback server, ignoring any path/query the header may carry.
fn is_loopback_url(raw: &str, expected_port: u16) -> bool {
    let Ok(uri) = raw.parse::<Uri>() else {
        return false;
    };
    if uri.scheme_str() != Some("http") {
        return false;
    }
    let Some(authority) = uri.authority() else {
        return false;
    };
    is_loopback_authority(authority.as_str(), expected_port)
}

/// Accepts normal loopback HTTP origins and opaque/custom application origins.
///
/// VS Code webviews and other embedded browsers can expose a local HTTP page through an opaque
/// (`null`) or custom-scheme origin. Those origins cannot be compared to the loopback listener,
/// so the already-validated per-process CSRF token remains the authority. Ordinary `http`/`https`
/// web origins are still rejected unless they identify this loopback listener.
fn mutation_source_is_allowed(raw: &str, expected_port: u16) -> bool {
    if raw.eq_ignore_ascii_case("null") {
        return true;
    }
    let Ok(uri) = raw.parse::<Uri>() else {
        return false;
    };
    match uri.scheme_str() {
        Some("http") => is_loopback_url(raw, expected_port),
        Some("https") | None => false,
        Some(_) => true,
    }
}

/// Validates the CSRF token and, when present, the Origin/Referer header on a state-changing
/// request, reading the token from the request header. The production request path instead reads
/// the token explicitly (header or form field) and calls [`authorize_mutation_with_token`]; this
/// header-only convenience remains for focused unit tests of that shared check.
#[cfg(test)]
pub(crate) fn authorize_mutation(headers: &HeaderMap, csrf: &CsrfToken, expected_port: u16) -> Result<(), MutationRejected> {
    let token = headers
        .get(CSRF_HEADER_NAME)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    authorize_mutation_with_token(headers, csrf, expected_port, token)
}

/// Like [`authorize_mutation`], but validates an explicitly-supplied token instead of reading it
/// from the request header. Used by the no-JavaScript form fallback, which carries the CSRF token
/// in a hidden form field (a browser cannot set a custom request header without scripting), while
/// still applying the same Origin/Referer defense-in-depth check against the request headers.
pub(crate) fn authorize_mutation_with_token(
    headers: &HeaderMap,
    csrf: &CsrfToken,
    expected_port: u16,
    token: &str,
) -> Result<(), MutationRejected> {
    if !csrf.is_valid(token) {
        return Err(MutationRejected::MissingOrInvalidCsrfToken);
    }

    if let Some(origin) = headers.get(hyper::header::ORIGIN).and_then(|value| value.to_str().ok()) {
        if !mutation_source_is_allowed(origin, expected_port) {
            return Err(MutationRejected::CrossOriginRequest);
        }
    } else if let Some(referer) = headers.get(hyper::header::REFERER).and_then(|value| value.to_str().ok())
        && !mutation_source_is_allowed(referer, expected_port)
    {
        return Err(MutationRejected::CrossOriginRequest);
    }

    Ok(())
}

/// Reports whether the client prefers an HTML response (a full-page browser navigation, e.g. a
/// no-JavaScript form submit) rather than the JSON the `fetch`-based path requests via
/// `Accept: application/json`. A request with no `Accept` header is treated as an API caller.
pub(crate) fn request_prefers_html(headers: &HeaderMap) -> bool {
    headers
        .get(hyper::header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.contains("text/html"))
}

/// Extracts a single field's value from an `application/x-www-form-urlencoded` request body,
/// applying the minimal percent- and `+`-decoding those bodies use. Returns `None` when the field
/// is absent, malformed, or not valid UTF-8.
pub(crate) fn form_field(body: &[u8], name: &str) -> Option<String> {
    let body = std::str::from_utf8(body).ok()?;
    body.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        let key = decode_form_component(key)?;
        if key == name { decode_form_component(value) } else { None }
    })
}

fn decode_form_component(component: &str) -> Option<String> {
    let bytes = component.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' => {
                if index + 2 >= bytes.len() {
                    return None;
                }
                let high = (bytes[index + 1] as char).to_digit(16);
                let low = (bytes[index + 2] as char).to_digit(16);
                if let (Some(high), Some(low)) = (high, low) {
                    #[expect(clippy::cast_possible_truncation, reason = "a single hex nibble pair fits in one byte")]
                    decoded.push((high * 16 + low) as u8);
                    index += 3;
                } else {
                    return None;
                }
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded).ok()
}

/// Drains an incoming body up to [`MAX_BODY_BYTES`], returning the bytes read.
pub(crate) async fn read_limited_body(body: Incoming) -> Result<Bytes, BodyReadError> {
    match Limited::new(body, MAX_BODY_BYTES).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(error) if error.is::<http_body_util::LengthLimitError>() => Err(BodyReadError::TooLarge),
        Err(_) => Err(BodyReadError::Invalid),
    }
}

/// Applies the dashboard's fixed set of restrictive security headers to every response. Centralized
/// so no response path can forget them: no inline/remote scripts or styles, no framing, no
/// cross-origin reads, and no caching of what is always live, local data.
pub(crate) fn apply_security_headers(headers: &mut HeaderMap) {
    use hyper::header::{self, HeaderName, HeaderValue};

    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'; \
             script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self'",
        ),
    );
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );
}

/// Validates that an external link the dashboard is about to render is `https://` with a
/// non-empty host, so a provider-sourced value (a pull request's `web_url`, ...) can never smuggle
/// a `javascript:`/`data:` URI into a rendered `href`, even though minijinja's HTML auto-escaping
/// already neutralizes it as a quoting/injection vector. Returns the validated URL unchanged so
/// callers can render it directly, or `None` when it should be omitted from the page instead.
pub(crate) fn validate_external_https_link(candidate: &str) -> Option<&str> {
    let uri = candidate.parse::<Uri>().ok()?;
    if uri.scheme_str() != Some("https") {
        return None;
    }
    let authority = uri.authority()?;
    if authority.host().is_empty() {
        return None;
    }
    Some(candidate)
}

#[cfg(test)]
mod tests {
    use super::{CsrfToken, MutationRejected, authorize_mutation, form_field, is_loopback_authority, validate_external_https_link};

    #[test]
    fn csrf_token_round_trips_and_rejects_mismatch() {
        let token = CsrfToken::generate();
        assert!(token.is_valid(token.value()));
        assert!(!token.is_valid("not-the-token"));
        assert!(!token.is_valid(""));
    }

    #[test]
    fn two_generated_tokens_differ() {
        assert_ne!(CsrfToken::generate().value(), CsrfToken::generate().value());
    }

    #[test]
    fn form_fields_decode_valid_utf8_and_reject_malformed_encoding() {
        assert_eq!(
            form_field(b"prompt=security+%26+correctness", "prompt").as_deref(),
            Some("security & correctness")
        );
        assert_eq!(form_field(b"prompt=%ZZ", "prompt"), None);
        assert_eq!(form_field(b"prompt=%FF", "prompt"), None);
    }

    #[test]
    fn accepts_localhost_and_loopback_ips_on_the_right_port() {
        assert!(is_loopback_authority("127.0.0.1:8787", 8787));
        assert!(is_loopback_authority("localhost:8787", 8787));
        assert!(is_loopback_authority("LOCALHOST:8787", 8787));
        assert!(is_loopback_authority("[::1]:8787", 8787));
    }

    #[test]
    fn rejects_wrong_port_or_non_loopback_host() {
        assert!(!is_loopback_authority("127.0.0.1:9999", 8787));
        assert!(!is_loopback_authority("evil.example:8787", 8787));
        assert!(!is_loopback_authority("attacker.test", 8787));
        assert!(!is_loopback_authority("203.0.113.5:8787", 8787));
    }

    #[test]
    fn authorize_mutation_requires_a_valid_token() {
        let csrf = CsrfToken::generate();
        let headers = hyper::HeaderMap::new();
        assert_eq!(
            authorize_mutation(&headers, &csrf, 8787),
            Err(MutationRejected::MissingOrInvalidCsrfToken)
        );
    }

    #[test]
    fn authorize_mutation_accepts_matching_token_with_same_origin_header() {
        let csrf = CsrfToken::generate();
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            super::CSRF_HEADER_NAME,
            csrf.value().parse().expect("token is a valid header value"),
        );
        headers.insert(hyper::header::ORIGIN, "http://127.0.0.1:8787".parse().expect("valid header value"));
        assert_eq!(authorize_mutation(&headers, &csrf, 8787), Ok(()));
    }

    #[test]
    fn authorize_mutation_accepts_opaque_and_vscode_origins_with_valid_csrf() {
        for origin in ["null", "vscode-webview://2d547f9b/settings"] {
            let csrf = CsrfToken::generate();
            let mut headers = hyper::HeaderMap::new();
            headers.insert(
                super::CSRF_HEADER_NAME,
                csrf.value().parse().expect("token is a valid header value"),
            );
            headers.insert(hyper::header::ORIGIN, origin.parse().expect("valid header value"));

            assert_eq!(authorize_mutation(&headers, &csrf, 8787), Ok(()));
        }
    }

    #[test]
    fn authorize_mutation_rejects_cross_origin_header() {
        let csrf = CsrfToken::generate();
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            super::CSRF_HEADER_NAME,
            csrf.value().parse().expect("token is a valid header value"),
        );
        headers.insert(hyper::header::ORIGIN, "http://evil.example".parse().expect("valid header value"));
        assert_eq!(authorize_mutation(&headers, &csrf, 8787), Err(MutationRejected::CrossOriginRequest));
    }

    #[test]
    fn validates_https_links_only() {
        assert_eq!(
            validate_external_https_link("https://github.com/octo/widgets/pull/1"),
            Some("https://github.com/octo/widgets/pull/1")
        );
        assert_eq!(validate_external_https_link("javascript:alert(1)"), None);
        assert_eq!(validate_external_https_link("http://github.com/octo/widgets/pull/1"), None);
        assert_eq!(validate_external_https_link("data:text/html,<script>1</script>"), None);
    }
}
