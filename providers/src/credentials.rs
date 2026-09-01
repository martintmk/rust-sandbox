// Licensed under the MIT License.

//! Ephemeral credential acquisition from local CLIs.
//!
//! Tokens are never stored on disk or held in long-lived state by the adapters.
//! The GitHub adapter asks `gh auth token` for a fresh token per request, uses
//! it to authorize a single client, and drops it; the returned [`SecretString`]
//! keeps the value out of logs. (Azure DevOps credentials are handled by
//! `azure_identity::AzureCliCredential`, which the SDK invokes on demand.)

use std::sync::Arc;

use super::command::{BoxFuture, CommandRequest, CommandRunner};
use super::error::ProviderError;
use super::secret::SecretString;

/// Acquires an ephemeral bearer token for a provider.
pub(crate) trait CredentialSource: std::fmt::Debug + Send + Sync {
    /// Returns a freshly-minted access token, or a permanent authentication
    /// error when the local CLI is missing or not logged in.
    fn access_token(&self) -> BoxFuture<'_, Result<SecretString, ProviderError>>;
}

/// Reads a GitHub token from the GitHub CLI via `gh auth token`.
#[derive(Clone, Debug)]
pub(crate) struct GitHubCliCredential {
    runner: Arc<dyn CommandRunner>,
}

impl GitHubCliCredential {
    /// Creates a credential source backed by `runner`.
    pub(crate) fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }
}

impl CredentialSource for GitHubCliCredential {
    fn access_token(&self) -> BoxFuture<'_, Result<SecretString, ProviderError>> {
        Box::pin(async move {
            let output = self
                .runner
                .run(CommandRequest::new("gh", &["auth", "token"]))
                .await
                .map_err(|error| {
                    ProviderError::authentication_caused_by(
                        "unable to run `gh auth token`; is the GitHub CLI installed and on PATH?",
                        error,
                    )
                })?;

            if !output.success {
                return Err(ProviderError::authentication(format!(
                    "`gh auth token` failed; run `gh auth login` to authenticate ({})",
                    first_line(&output.stderr)
                )));
            }

            let token = SecretString::new(output.stdout.trim());
            if token.is_empty() {
                return Err(ProviderError::authentication(
                    "`gh auth token` returned an empty token; run `gh auth login` to authenticate",
                ));
            }

            Ok(token)
        })
    }
}

/// Returns the first non-empty line of `text`, trimmed, for compact diagnostics.
fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no error output")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::Arc;

    use super::super::command::CommandRunner;
    use super::super::command::testing::ScriptedCommandRunner;
    use super::super::error::ProviderErrorKind;
    use super::{CredentialSource, GitHubCliCredential};

    #[tokio::test]
    async fn github_token_is_trimmed() {
        let runner = Arc::new(ScriptedCommandRunner::new());
        runner.push_stdout("gh", "gho_exampletoken\n");
        let credential = GitHubCliCredential::new(Arc::clone(&runner) as Arc<dyn CommandRunner>);

        let token = credential.access_token().await.expect("token should be returned");
        assert_eq!(token.reveal(), "gho_exampletoken");

        let calls = runner.recorded_calls();
        assert_eq!(calls[0].program, "gh");
        assert_eq!(calls[0].args, vec!["auth", "token"]);
    }

    #[tokio::test]
    async fn github_login_failure_is_authentication_error() {
        let runner = Arc::new(ScriptedCommandRunner::new());
        runner.push_failure("gh", "gh: To get started with GitHub CLI, please run: gh auth login");
        let credential = GitHubCliCredential::new(runner);

        let error = credential.access_token().await.expect_err("should fail");
        assert_eq!(error.kind(), ProviderErrorKind::Authentication);
        assert!(error.to_string().contains("gh auth login"));
    }

    #[tokio::test]
    async fn github_empty_token_is_authentication_error() {
        let runner = Arc::new(ScriptedCommandRunner::new());
        runner.push_stdout("gh", "   \n");
        let credential = GitHubCliCredential::new(runner);

        let error = credential.access_token().await.expect_err("should fail");
        assert_eq!(error.kind(), ProviderErrorKind::Authentication);
    }

    #[tokio::test]
    async fn github_missing_cli_is_authentication_error() {
        let runner = Arc::new(ScriptedCommandRunner::new());
        runner.push_launch_error("gh", "No such file or directory");
        let credential = GitHubCliCredential::new(runner);

        let error = credential.access_token().await.expect_err("should fail");
        assert_eq!(error.kind(), ProviderErrorKind::Authentication);
        assert!(error.to_string().contains("GitHub CLI"));
    }
}
