// Licensed under the MIT License.

//! Pull request analysis through a restricted GitHub Copilot SDK client.
//!
//! This crate accepts normalized pull request context and returns a validated,
//! structured [`AnalysisOutput`]. It deliberately does not own provider
//! polling, background scheduling, retries, or persistence.

mod prompt;
mod result;
mod sdk;

use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::watch;

pub use result::{AnalysisOutput, Finding, Interest, Overview, Priority, Review, Verdict};

use sdk::SdkBackend;

/// Marketplace/plugin/skill coordinates for one analysis action.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ActionMapping {
    pub marketplace: String,
    pub plugin: String,
    pub skill: String,
}

impl fmt::Display for ActionMapping {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.marketplace, self.plugin, self.skill)
    }
}

/// Default prompt used to generate the factual overview.
pub const DEFAULT_OVERVIEW_PROMPT: &str =
    "Produce a concise factual overview of the pull request. Explain its intent, scope, and notable risk areas.";

/// Default prompt used to decide whether a pull request is interesting.
pub const DEFAULT_INTEREST_PROMPT: &str =
    "Decide whether this pull request deserves reviewer attention and assign an appropriate priority.";

/// Ordinary prompts used before the external review skill.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnalysisPrompts {
    pub overview: String,
    pub interesting: String,
}

impl Default for AnalysisPrompts {
    fn default() -> Self {
        Self {
            overview: DEFAULT_OVERVIEW_PROMPT.to_owned(),
            interesting: DEFAULT_INTEREST_PROMPT.to_owned(),
        }
    }
}

/// Repository information supplied to the analyzer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryContext {
    pub provider: String,
    pub owner: String,
    pub name: String,
}

/// Raw pull request information supplied to the analyzer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequestContext {
    pub number: i64,
    pub title: String,
    pub author: Option<String>,
    pub web_url: String,
    pub source_branch: String,
    pub target_branch: String,
    pub revision_fingerprint: String,
    pub body: Option<String>,
    pub is_draft: Option<bool>,
    pub mergeable: Option<bool>,
    pub additions: Option<i64>,
    pub deletions: Option<i64>,
    pub changed_files: Option<i64>,
}

/// Complete input for one pull request analysis.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnalysisRequest {
    pub repository: RepositoryContext,
    pub pull_request: PullRequestContext,
    pub checkout_path: Option<PathBuf>,
    pub prompts: AnalysisPrompts,
    pub review_action: ActionMapping,
}

/// Stable classification of an [`AnalysisError`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AnalysisErrorKind {
    /// Analysis was cancelled by the caller.
    Cancelled,
    /// Input or local checkout context was invalid.
    InvalidContext,
    /// The model output did not match the required schema.
    InvalidOutput,
    /// A configured marketplace, plugin, or skill was unavailable.
    MissingCapability,
    /// The Copilot SDK operation failed.
    Sdk,
}

/// An error produced while analyzing a pull request.
#[derive(Debug)]
pub struct AnalysisError {
    kind: AnalysisErrorKind,
    message: String,
}

impl AnalysisError {
    /// Returns the stable classification for this error.
    #[must_use]
    pub const fn kind(&self) -> AnalysisErrorKind {
        self.kind
    }

    fn cancelled() -> Self {
        Self {
            kind: AnalysisErrorKind::Cancelled,
            message: "analysis was cancelled".to_owned(),
        }
    }

    fn invalid_context(message: impl Into<String>) -> Self {
        Self {
            kind: AnalysisErrorKind::InvalidContext,
            message: message.into(),
        }
    }

    fn invalid_output(message: impl Into<String>) -> Self {
        Self {
            kind: AnalysisErrorKind::InvalidOutput,
            message: message.into(),
        }
    }

    fn missing_capability(message: impl Into<String>) -> Self {
        Self {
            kind: AnalysisErrorKind::MissingCapability,
            message: message.into(),
        }
    }

    fn sdk(message: impl Into<String>) -> Self {
        Self {
            kind: AnalysisErrorKind::Sdk,
            message: message.into(),
        }
    }
}

impl fmt::Display for AnalysisError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            AnalysisErrorKind::Cancelled => f.write_str("pull request analysis was cancelled"),
            AnalysisErrorKind::InvalidContext => write!(f, "invalid pull request analysis context: {}", self.message),
            AnalysisErrorKind::InvalidOutput => write!(f, "invalid pull request analysis output: {}", self.message),
            AnalysisErrorKind::MissingCapability => write!(f, "configured analysis capability is unavailable: {}", self.message),
            AnalysisErrorKind::Sdk => write!(f, "Copilot SDK analysis failed: {}", self.message),
        }
    }
}

impl Error for AnalysisError {}

/// Handle used to cancel an in-flight analysis.
#[derive(Debug)]
pub struct CancellationHandle {
    sender: watch::Sender<bool>,
}

/// Cancellation signal passed to an analyzer.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    receiver: watch::Receiver<bool>,
}

/// Creates a cancellation handle and its corresponding token.
#[must_use]
pub fn cancellation_pair() -> (CancellationHandle, CancellationToken) {
    let (sender, receiver) = watch::channel(false);
    (CancellationHandle { sender }, CancellationToken { receiver })
}

impl CancellationHandle {
    /// Cancels the associated analysis.
    pub fn cancel(self) {
        self.sender.send_replace(true);
    }
}

impl CancellationToken {
    async fn cancelled(mut self) {
        if *self.receiver.borrow() {
            return;
        }
        while self.receiver.changed().await.is_ok() {
            if *self.receiver.borrow() {
                return;
            }
        }
    }
}

#[async_trait]
trait AnalysisBackend: fmt::Debug + Send + Sync {
    async fn analyze(&self, request: AnalysisRequest, cancellation: CancellationToken) -> Result<AnalysisOutput, AnalysisError>;

    async fn shutdown(&self) -> Result<(), AnalysisError>;
}

#[cfg(any(test, feature = "test-util"))]
#[derive(Debug)]
struct ExampleBackend;

#[cfg(any(test, feature = "test-util"))]
#[async_trait]
impl AnalysisBackend for ExampleBackend {
    async fn analyze(&self, _request: AnalysisRequest, _cancellation: CancellationToken) -> Result<AnalysisOutput, AnalysisError> {
        Ok(AnalysisOutput::example())
    }

    async fn shutdown(&self) -> Result<(), AnalysisError> {
        Ok(())
    }
}

/// Cloneable pull request analyzer.
#[derive(Clone)]
pub struct Analyzer {
    backend: Arc<dyn AnalysisBackend>,
}

impl Analyzer {
    /// Creates an analyzer backed by the installed GitHub Copilot CLI.
    #[must_use]
    pub fn new() -> Self {
        Self {
            backend: Arc::new(SdkBackend::new()),
        }
    }

    /// Analyzes a pull request without external cancellation.
    ///
    /// # Errors
    ///
    /// Returns [`AnalysisError`] when input validation, capability discovery,
    /// SDK execution, or output validation fails.
    pub async fn analyze(&self, request: AnalysisRequest) -> Result<AnalysisOutput, AnalysisError> {
        let (_handle, cancellation) = cancellation_pair();
        self.backend.analyze(request, cancellation).await
    }

    /// Analyzes a pull request until completion or cancellation.
    ///
    /// # Errors
    ///
    /// Returns [`AnalysisError`] when the operation is cancelled or when input
    /// validation, capability discovery, SDK execution, or output validation
    /// fails.
    pub async fn analyze_with_cancellation(
        &self,
        request: AnalysisRequest,
        cancellation: CancellationToken,
    ) -> Result<AnalysisOutput, AnalysisError> {
        self.backend.analyze(request, cancellation).await
    }

    /// Stops the underlying SDK client, if it was started.
    ///
    /// # Errors
    ///
    /// Returns [`AnalysisError`] when the SDK client cannot stop cleanly.
    pub async fn shutdown(&self) -> Result<(), AnalysisError> {
        self.backend.shutdown().await
    }

    /// Creates a deterministic analyzer for integration tests.
    #[cfg(feature = "test-util")]
    #[must_use]
    pub fn example() -> Self {
        Self {
            backend: Arc::new(ExampleBackend),
        }
    }

    #[cfg(test)]
    fn with_backend(backend: Arc<dyn AnalysisBackend>) -> Self {
        Self { backend }
    }
}

impl Default for Analyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Analyzer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Analyzer").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> AnalysisRequest {
        AnalysisRequest {
            repository: RepositoryContext {
                provider: "github".to_owned(),
                owner: "octo".to_owned(),
                name: "repo".to_owned(),
            },
            pull_request: PullRequestContext {
                number: 1,
                title: "Improve parser".to_owned(),
                author: Some("octocat".to_owned()),
                web_url: "https://github.com/octo/repo/pull/1".to_owned(),
                source_branch: "feature".to_owned(),
                target_branch: "main".to_owned(),
                revision_fingerprint: "revision-1".to_owned(),
                body: Some("Body".to_owned()),
                is_draft: Some(false),
                mergeable: Some(true),
                additions: Some(10),
                deletions: Some(2),
                changed_files: Some(1),
            },
            checkout_path: None,
            prompts: AnalysisPrompts::default(),
            review_action: ActionMapping::default(),
        }
    }

    #[tokio::test]
    async fn analyzer_returns_structured_output() {
        let analyzer = Analyzer::with_backend(Arc::new(ExampleBackend));
        let output = analyzer.analyze(request()).await.expect("analysis should succeed");

        assert_eq!(output.overview.summary, "Updates parser behavior.");
        assert_eq!(output.interesting.priority, Priority::High);
    }

    #[tokio::test]
    async fn cancellation_pair_releases_waiter() {
        let (handle, token) = cancellation_pair();

        handle.cancel();
        token.cancelled().await;
    }
}
