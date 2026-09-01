// Licensed under the MIT License.

//! Typed provider errors with recovery classification.
//!
//! Provider operations fail for very different reasons, and callers (polling,
//! reconciliation) need to react differently: throttling should back off,
//! transient failures may retry, and permanent authentication/configuration
//! problems must surface clearly instead of being retried forever.

use std::fmt;
use std::time::Duration;

/// Classification of a [`ProviderError`], used to decide how callers react.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderErrorKind {
    /// The local CLI could not provide a token, or the provider rejected it
    /// (`401`, non-throttling `403`). Permanent: retrying will not help until
    /// the operator re-authenticates.
    Authentication,
    /// The request was well-formed but the configuration is wrong (missing
    /// project, unknown repository shape, `400`/`422`). Permanent.
    Configuration,
    /// The requested resource does not exist (`404`). Permanent.
    NotFound,
    /// The provider is rate-limiting or throttling the client (`429`, or a
    /// `403` with an exhausted rate-limit). Transient; honor [`retry_after`].
    ///
    /// [`retry_after`]: ProviderError::retry_after
    Throttled,
    /// A transient failure (`5xx`, transport/timeout error). Safe to retry.
    Transient,
    /// The provider returned a response that could not be understood
    /// (unexpected status or malformed body). Permanent.
    Response,
}

impl ProviderErrorKind {
    /// Returns `true` when a caller may reasonably retry the operation.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::Throttled | Self::Transient)
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Authentication => "authentication",
            Self::Configuration => "configuration",
            Self::NotFound => "not_found",
            Self::Throttled => "throttled",
            Self::Transient => "transient",
            Self::Response => "response",
        }
    }
}

impl fmt::Display for ProviderErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An error raised by a provider adapter.
///
/// The [`kind`](Self::kind) drives retry/backoff decisions; the message
/// (never containing credentials) explains what happened.
#[derive(ohno::Error)]
#[no_constructors]
#[display("{kind} provider error: {message}")]
pub struct ProviderError {
    kind: ProviderErrorKind,
    message: String,
    retry_after: Option<Duration>,
    inner: ohno::OhnoCore,
}

impl ProviderError {
    fn build(kind: ProviderErrorKind, message: impl Into<String>, retry_after: Option<Duration>, inner: ohno::OhnoCore) -> Self {
        Self {
            kind,
            message: message.into(),
            retry_after,
            inner,
        }
    }

    /// Creates an [`ProviderErrorKind::Authentication`] error.
    #[must_use]
    pub fn authentication(message: impl Into<String>) -> Self {
        Self::build(ProviderErrorKind::Authentication, message, None, ohno::OhnoCore::new())
    }

    /// Creates an [`ProviderErrorKind::Authentication`] error wrapping a cause.
    #[must_use]
    pub fn authentication_caused_by(
        message: impl Into<String>,
        cause: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Self {
        Self::build(ProviderErrorKind::Authentication, message, None, ohno::OhnoCore::from(cause))
    }

    /// Creates a [`ProviderErrorKind::Configuration`] error.
    #[must_use]
    pub fn configuration(message: impl Into<String>) -> Self {
        Self::build(ProviderErrorKind::Configuration, message, None, ohno::OhnoCore::new())
    }

    /// Creates a [`ProviderErrorKind::NotFound`] error.
    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::build(ProviderErrorKind::NotFound, message, None, ohno::OhnoCore::new())
    }

    /// Creates a [`ProviderErrorKind::Throttled`] error with an optional delay.
    #[must_use]
    pub fn throttled(message: impl Into<String>, retry_after: Option<Duration>) -> Self {
        Self::build(ProviderErrorKind::Throttled, message, retry_after, ohno::OhnoCore::new())
    }

    /// Creates a [`ProviderErrorKind::Transient`] error.
    #[must_use]
    pub fn transient(message: impl Into<String>) -> Self {
        Self::build(ProviderErrorKind::Transient, message, None, ohno::OhnoCore::new())
    }

    /// Creates a [`ProviderErrorKind::Response`] error.
    #[must_use]
    pub fn response(message: impl Into<String>) -> Self {
        Self::build(ProviderErrorKind::Response, message, None, ohno::OhnoCore::new())
    }

    /// Creates a [`ProviderErrorKind::Response`] error wrapping a cause.
    #[must_use]
    pub fn response_caused_by(message: impl Into<String>, cause: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>) -> Self {
        Self::build(ProviderErrorKind::Response, message, None, ohno::OhnoCore::from(cause))
    }

    /// Returns the error classification.
    #[must_use]
    pub const fn kind(&self) -> ProviderErrorKind {
        self.kind
    }

    /// Returns the provider-suggested backoff delay, when known.
    #[must_use]
    pub const fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }
}
