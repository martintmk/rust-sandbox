// Licensed under the MIT License.

//! A minimal secret wrapper that keeps credentials out of logs and diagnostics.

use std::fmt;

/// Holds a sensitive string (such as an access token) and prevents it from
/// being accidentally printed.
///
/// [`Debug`] and [`Display`](fmt::Display) render a fixed redaction marker
/// instead of the value, so a [`SecretString`] can be embedded in structs and
/// error contexts without risk of leaking. The plaintext is only reachable via
/// [`reveal`](Self::reveal), which is used solely to build an `Authorization`
/// header at request time.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SecretString(String);

impl SecretString {
    /// Wraps a plaintext secret.
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the plaintext. Callers must never log or persist the result.
    pub(crate) fn reveal(&self) -> &str {
        &self.0
    }

    /// Returns `true` when the secret is empty (e.g. an empty CLI response).
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(redacted)")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("redacted")
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::SecretString;

    #[test]
    fn debug_and_display_are_redacted() {
        let secret = SecretString::new("super-secret-token");
        assert_eq!(format!("{secret:?}"), "SecretString(redacted)");
        assert_eq!(format!("{secret}"), "redacted");
        assert!(!format!("{secret:?}").contains("super-secret-token"));
    }

    #[test]
    fn reveal_returns_plaintext() {
        let secret = SecretString::new("abc123");
        assert_eq!(secret.reveal(), "abc123");
        assert!(!secret.is_empty());
        assert!(SecretString::new("").is_empty());
    }
}
