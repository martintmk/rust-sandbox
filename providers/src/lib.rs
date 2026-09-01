// Licensed under the MIT License.

//! Provider adapters for pull request data.
//!
//! This module defines a provider-neutral model ([`model`]) and an async
//! [`PullRequestSource`] interface ([`transport`]), plus concrete adapters for
//! GitHub ([`github`]) and Azure DevOps ([`azure_devops`]). Each adapter acquires
//! ephemeral credentials from a local CLI at request time and delegates
//! transport, pagination, models, and authorization to a vendor-maintained SDK,
//! so tests can run against local mock servers without touching real accounts or
//! persisting tokens.
//!
//! # Vendor SDKs
//!
//! * **GitHub** uses [`octocrab`], the community's de-facto GitHub SDK (there is
//!   no GitHub-official Rust client). A fresh token from `gh auth token`
//!   ([`credentials::GitHubCliCredential`], via the injectable
//!   [`command::CommandRunner`]) is handed to a short-lived client.
//! * **Azure DevOps** uses Microsoft's generated [`azure_devops_rust_api`]
//!   (`git` feature) with [`azure_identity::AzureCliCredential`], which mints a
//!   token from the Azure CLI on demand. `azure_core`'s pipeline supplies the
//!   bounded retry/back-off for throttling and transient failures.
//!
//! # Documented raw-request gaps
//!
//! The adapters lean on the SDKs' typed methods everywhere possible. The only
//! raw/low-level calls are in [`github`], and each is documented at its call
//! site: `octocrab`'s high-level pager hides the `ETag`/`Link` response headers,
//! so conditional (`If-None-Match` → `304`) listing and `Link` pagination drive
//! the SDK's own low-level transport, and the GitHub reviews endpoint (which has
//! no typed `octocrab` method) is fetched through the SDK's generic getter.
//! Azure DevOps exposes no `ETag` conditional listing and paginates with
//! `$top`/`$skip` rather than a continuation token, both handled via the SDK's
//! typed builders (see [`azure_devops`]).
//!
//! # Dependency-governance notes
//!
//! `octocrab` is pulled with `default-features = false` and a curated feature
//! set (rustls/ring transport, retry, timeout, follow-redirect, tracing, and the
//! `jwt-aws-lc-rs` JWT backend). This deliberately avoids the default
//! `jwt-rust-crypto` backend, which would drag in `rsa` (RUSTSEC-2023-0071)
//! purely for GitHub App auth this adapter never uses (it authenticates with a
//! `gh` PAT), so `rsa` is absent from the dependency tree entirely.
//!
//! `azure_devops_rust_api` is taken with only the `git` feature, but its
//! `reqwest` transport still pulls `webpki-root-certs` (CDLA-Permissive-2.0)
//! transitively via `rustls-platform-verifier`; that license is not in the
//! shared allow-list and cannot be dropped without abandoning the official SDK.
//! A narrowly scoped `deny.toml` exception (pinned to that one crate/license
//! pair, placed outside the anvil-managed sentinel) covers it.

mod azure_devops;
mod command;
mod credentials;
mod error;
mod github;
mod model;
mod secret;
mod transport;

use std::fmt;
use std::sync::Arc;

use azure_devops_rust_api::Credential;
use tick::Clock;

use self::azure_devops::AzureDevOpsProvider;
pub use self::command::BoxFuture;
use self::command::{CommandRunner, TokioCommandRunner};
use self::credentials::{CredentialSource, GitHubCliCredential};
pub use self::error::{ProviderError, ProviderErrorKind};
use self::github::GitHubProvider;
pub use self::model::{
    Label, ProviderKind, PullRequestDetail, PullRequestNumber, PullRequestState, PullRequestSummary, RepositoryCoordinate, ReviewDecision,
    Reviewer, UserRef,
};
pub use self::transport::{EntityTag, ListOutcome, PullRequestSource};

/// Builds provider adapters, holding the shared, injectable seams (command
/// execution and clock) that every adapter needs.
///
/// The registry is cheap to clone and carries no credentials: tokens are only
/// acquired transiently, per request, inside the adapters.
#[derive(Clone)]
pub struct ProviderRegistry {
    command_runner: Arc<dyn CommandRunner>,
    clock: Clock,
}

impl ProviderRegistry {
    /// Creates a registry using the real process runner and runtime clock.
    #[must_use]
    pub fn new() -> Self {
        Self {
            command_runner: Arc::new(TokioCommandRunner),
            clock: Clock::new_tokio(),
        }
    }

    /// Creates a registry with an explicit command runner (used by tests to
    /// inject a scripted runner instead of shelling out to real CLIs).
    #[cfg(test)]
    pub(crate) fn with_command_runner(command_runner: Arc<dyn CommandRunner>, clock: Clock) -> Self {
        Self { command_runner, clock }
    }

    /// Builds a GitHub pull request source that authorizes each request with a
    /// token freshly minted by the GitHub CLI.
    fn github(&self) -> GitHubProvider {
        let credential: Arc<dyn CredentialSource> = Arc::new(GitHubCliCredential::new(Arc::clone(&self.command_runner)));
        GitHubProvider::new(credential, self.clock.clone())
    }

    /// Builds an Azure DevOps pull request source that authorizes each request
    /// with a token freshly minted by the Azure CLI.
    ///
    /// # Errors
    ///
    /// Returns an authentication error when the Azure CLI credential cannot be
    /// initialized (for example, the Azure CLI is not installed).
    #[expect(
        clippy::unused_self,
        reason = "kept as an inherent method for symmetry with `github` and to allow future per-registry Azure config"
    )]
    fn azure_devops(&self) -> Result<AzureDevOpsProvider, ProviderError> {
        let cli = azure_identity::AzureCliCredential::new(None).map_err(|error| {
            ProviderError::authentication_caused_by(
                "unable to initialize the Azure CLI credential; is the Azure CLI installed and are you logged in with `az login`?",
                error,
            )
        })?;
        Ok(AzureDevOpsProvider::new(Credential::from_token_credential(cli)))
    }

    /// Returns a boxed [`PullRequestSource`] for `provider`.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider's credential cannot be initialized.
    pub fn source(&self, provider: ProviderKind) -> Result<Box<dyn PullRequestSource>, ProviderError> {
        match provider {
            ProviderKind::GitHub => Ok(Box::new(self.github()) as Box<dyn PullRequestSource>),
            ProviderKind::AzureDevOps => Ok(Box::new(self.azure_devops()?) as Box<dyn PullRequestSource>),
        }
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Deliberately opaque: never risk surfacing credential-adjacent state.
        f.debug_struct("ProviderRegistry").finish_non_exhaustive()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::Arc;

    use tick::Clock;

    use super::ProviderRegistry;
    use super::command::testing::ScriptedCommandRunner;
    use super::model::ProviderKind;

    fn registry() -> ProviderRegistry {
        let runner = Arc::new(ScriptedCommandRunner::new());
        ProviderRegistry::with_command_runner(runner, Clock::new_frozen())
    }

    #[test]
    fn registry_is_debug_opaque() {
        let debug = format!("{:?}", registry());
        assert!(debug.contains("ProviderRegistry"));
        assert!(!debug.contains("token"));
    }

    #[test]
    fn source_dispatches_to_github() {
        registry().source(ProviderKind::GitHub).expect("github source builds");
    }

    #[test]
    fn source_dispatches_to_azure_devops() {
        registry().source(ProviderKind::AzureDevOps).expect("azure source builds");
    }
}
