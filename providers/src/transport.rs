// Licensed under the MIT License.

//! Provider-neutral transport contracts and SDK error classification.
//!
//! The concrete adapters talk to their forge through a vendor SDK (`octocrab`
//! for GitHub, `azure_devops_rust_api` for Azure DevOps), so this module no
//! longer owns an HTTP client. What stays provider-neutral lives here: the
//! [`PullRequestSource`] trait, the [`ListOutcome`]/[`EntityTag`] value types,
//! and the mapping from an SDK/HTTP status code to a typed [`ProviderError`]
//! (authentication, throttling, transient, permanent).

use std::fmt;
use std::time::Duration;

use super::command::BoxFuture;
use super::error::ProviderError;
use super::model::{PullRequestDetail, PullRequestNumber, PullRequestSummary, RepositoryCoordinate};

/// An HTTP entity tag captured from a `list` response and replayed on the next
/// poll via `If-None-Match` to detect "nothing changed".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityTag(String);

impl EntityTag {
    /// Wraps a raw `ETag` header value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the raw `ETag` value for use in an `If-None-Match` header.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The result of listing open pull requests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListOutcome {
    /// The provider confirmed (via `304 Not Modified`) that nothing changed
    /// since the [`EntityTag`] supplied by the caller.
    Unchanged,
    /// A fresh listing, with an optional new [`EntityTag`] to store for next time.
    Fetched {
        /// The normalized open pull requests, across all pages.
        pull_requests: Vec<PullRequestSummary>,
        /// A new entity tag, when the provider supplied one.
        etag: Option<EntityTag>,
    },
}

/// A provider-neutral source of pull request data.
///
/// Implementors acquire an ephemeral token per call, talk to their REST API via
/// the vendor SDK, and normalize the results.
pub trait PullRequestSource: fmt::Debug + Send + Sync {
    /// Lists open pull requests for `repository`, following pagination.
    ///
    /// When `conditional` is supplied and the provider reports the listing is
    /// unchanged, [`ListOutcome::Unchanged`] is returned without re-fetching.
    fn list_open_pull_requests<'a>(
        &'a self,
        repository: &'a RepositoryCoordinate,
        conditional: Option<&'a EntityTag>,
    ) -> BoxFuture<'a, Result<ListOutcome, ProviderError>>;

    /// Fetches the full detail of a single pull request.
    fn fetch_pull_request<'a>(
        &'a self,
        repository: &'a RepositoryCoordinate,
        number: PullRequestNumber,
    ) -> BoxFuture<'a, Result<PullRequestDetail, ProviderError>>;
}

/// Classifies an HTTP status code (surfaced by an SDK error or a raw response)
/// into a typed [`ProviderError`].
///
/// The `403`/throttling distinction mirrors GitHub, where an exhausted rate
/// limit is reported as `403` alongside rate-limit headers; when the caller can
/// see those headers it passes a `retry_after`, which promotes the `403` to a
/// [`ProviderErrorKind::Throttled`](super::error::ProviderErrorKind::Throttled).
pub(crate) fn classify_http_status(status: u16, retry_after: Option<Duration>) -> ProviderError {
    match status {
        401 => ProviderError::authentication("authentication rejected (401 Unauthorized); re-authenticate the local CLI"),
        403 if retry_after.is_some() => ProviderError::throttled("rate limited (403 Forbidden)", retry_after),
        403 => ProviderError::authentication("access forbidden (403 Forbidden); the token may lack the required scopes"),
        404 => ProviderError::not_found("resource not found (404 Not Found)"),
        408 => ProviderError::transient("request timed out (408 Request Timeout)"),
        429 => ProviderError::throttled("rate limited (429 Too Many Requests)", retry_after),
        400 => ProviderError::configuration("bad request (400)"),
        422 => ProviderError::configuration("unprocessable request (422)"),
        code if code >= 500 => ProviderError::transient(format!("server error ({code})")),
        code => ProviderError::response(format!("unexpected status ({code})")),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::time::Duration;

    use super::super::error::ProviderErrorKind;
    use super::classify_http_status;

    #[test]
    fn classifies_401_as_authentication() {
        assert_eq!(classify_http_status(401, None).kind(), ProviderErrorKind::Authentication);
    }

    #[test]
    fn classifies_403_without_retry_after_as_authentication() {
        assert_eq!(classify_http_status(403, None).kind(), ProviderErrorKind::Authentication);
    }

    #[test]
    fn classifies_403_with_retry_after_as_throttled() {
        let error = classify_http_status(403, Some(Duration::from_secs(42)));
        assert_eq!(error.kind(), ProviderErrorKind::Throttled);
        assert_eq!(error.retry_after(), Some(Duration::from_secs(42)));
    }

    #[test]
    fn classifies_429_as_throttled() {
        let error = classify_http_status(429, Some(Duration::from_secs(7)));
        assert_eq!(error.kind(), ProviderErrorKind::Throttled);
        assert_eq!(error.retry_after(), Some(Duration::from_secs(7)));
    }

    #[test]
    fn classifies_404_and_422_and_5xx() {
        assert_eq!(classify_http_status(404, None).kind(), ProviderErrorKind::NotFound);
        assert_eq!(classify_http_status(422, None).kind(), ProviderErrorKind::Configuration);
        assert_eq!(classify_http_status(502, None).kind(), ProviderErrorKind::Transient);
    }

    #[test]
    fn classifies_unexpected_status_as_response() {
        assert_eq!(classify_http_status(418, None).kind(), ProviderErrorKind::Response);
    }
}
