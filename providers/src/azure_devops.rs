// Licensed under the MIT License.

//! Azure DevOps pull request adapter, built on Microsoft's generated
//! [`azure_devops_rust_api`] SDK (the `git` feature).
//!
//! Credentials come from the Azure CLI through
//! [`azure_identity::AzureCliCredential`], wrapped in the SDK's
//! [`azure_devops_rust_api::Credential`]; the token is minted lazily by the
//! SDK's transport per request and never stored or logged by this adapter. The
//! SDK owns transport, authorization, models, and single-resource fetches, and
//! `azure_core`'s pipeline provides the bounded retry/back-off used for
//! throttling and transient failures.
//!
//! # Pagination
//!
//! The Azure DevOps `git` "list pull requests" API is repository-scoped and
//! paginates with `$top`/`$skip` rather than a continuation token, so the SDK
//! exposes no continuation cursor. The list path therefore walks pages with a
//! bounded `$top`/`$skip` loop over the SDK's typed
//! [`get_pull_requests`](azure_devops_rust_api::git::pull_requests::Client::get_pull_requests)
//! builder, stopping when a short page is returned.
//!
//! # Conditional requests
//!
//! Unlike GitHub, the Azure DevOps REST surface does not offer `ETag`/
//! `If-None-Match` conditional listing for pull requests, so
//! [`list_open_pull_requests`](AzureDevOpsProvider::list_open_pull_requests)
//! ignores the caller's conditional tag and always reports a fresh listing with
//! no entity tag.

use std::fmt;

use azure_devops_rust_api::Credential;
use azure_devops_rust_api::git::{self, models};
use time::format_description::well_known::Rfc3339;

use super::command::BoxFuture;
use super::error::ProviderError;
use super::model::{
    Label, ProviderKind, PullRequestDetail, PullRequestNumber, PullRequestState, PullRequestSummary, RepositoryCoordinate, ReviewDecision,
    Reviewer, UserRef,
};
use super::transport::{EntityTag, ListOutcome, PullRequestSource, classify_http_status};

/// The Azure DevOps AAD scope used when acquiring a bearer token.
///
/// This well-known application id represents Azure DevOps; the `/.default`
/// suffix requests the app's statically-configured permissions.
const AZURE_DEVOPS_SCOPE: &str = "499b84ac-1321-427f-aa17-267ca6975798/.default";

/// Default page size for `$top`/`$skip` pagination (the API caps this at 100).
const DEFAULT_PAGE_SIZE: i32 = 100;

/// An Azure DevOps pull request source.
pub(crate) struct AzureDevOpsProvider {
    credential: Credential,
    endpoint: Option<String>,
    scopes: Vec<String>,
    page_size: i32,
}

impl AzureDevOpsProvider {
    /// Creates an Azure DevOps provider using the public `dev.azure.com`
    /// endpoint and the default Azure DevOps scope.
    pub(crate) fn new(credential: Credential) -> Self {
        Self {
            credential,
            endpoint: None,
            scopes: vec![AZURE_DEVOPS_SCOPE.to_owned()],
            page_size: DEFAULT_PAGE_SIZE,
        }
    }

    /// Overrides the API endpoint (for Azure DevOps Server or tests).
    #[cfg(test)]
    pub(crate) fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(trim_trailing_slash(endpoint.into()));
        self
    }

    /// Overrides the pagination page size (used by tests to exercise the
    /// `$top`/`$skip` loop without large fixtures).
    #[cfg(test)]
    pub(crate) fn with_page_size(mut self, page_size: i32) -> Self {
        self.page_size = page_size;
        self
    }

    /// Validates the coordinate and returns `(organization, project, name)`.
    fn resolve(repository: &RepositoryCoordinate) -> Result<(&str, &str, &str), ProviderError> {
        let Some(project) = repository.project.as_deref().filter(|project| !project.is_empty()) else {
            return Err(ProviderError::configuration("Azure DevOps repository requires a project"));
        };
        if repository.owner.is_empty() || repository.name.is_empty() {
            return Err(ProviderError::configuration(
                "Azure DevOps repository requires a non-empty organization and name",
            ));
        }
        Ok((&repository.owner, project, &repository.name))
    }

    /// Builds a token-authorized `azure_devops_rust_api` git client.
    fn client(&self) -> Result<git::Client, ProviderError> {
        let scope_refs: Vec<&str> = self.scopes.iter().map(String::as_str).collect();
        let mut builder = git::Client::builder(self.credential.clone()).scopes(&scope_refs);
        // Tests point at a local mock and must not wait on the SDK's default
        // exponential back-off when asserting throttling/transient classification.
        #[cfg(test)]
        {
            builder = builder.retry(azure_core::http::RetryOptions::none());
        }
        if let Some(endpoint) = &self.endpoint {
            let url = azure_core::http::Url::parse(endpoint)
                .map_err(|error| ProviderError::configuration(format!("invalid Azure DevOps endpoint: {error}")))?;
            builder = builder.endpoint(url);
        }
        Ok(builder.build())
    }
}

impl fmt::Debug for AzureDevOpsProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Deliberately omit the credential: never risk surfacing token state.
        f.debug_struct("AzureDevOpsProvider")
            .field("endpoint", &self.endpoint)
            .field("scopes", &self.scopes)
            .field("page_size", &self.page_size)
            .finish_non_exhaustive()
    }
}

impl PullRequestSource for AzureDevOpsProvider {
    fn list_open_pull_requests<'a>(
        &'a self,
        repository: &'a RepositoryCoordinate,
        _conditional: Option<&'a EntityTag>,
    ) -> BoxFuture<'a, Result<ListOutcome, ProviderError>> {
        Box::pin(async move {
            let (organization, project, name) = Self::resolve(repository)?;
            let client = self.client()?;
            let pull_requests_client = client.pull_requests_client();

            let mut pull_requests = Vec::new();
            let mut skip = 0_i32;
            loop {
                let list = pull_requests_client
                    .get_pull_requests(organization, name, project)
                    .search_criteria_status(models::PullRequestStatus::Active)
                    .top(self.page_size)
                    .skip(skip)
                    .await
                    .map_err(|error| map_azure_error(&error))?;

                let page_len = list.value.len();
                pull_requests.extend(list.value.iter().map(|pull_request| summary_from(pull_request, repository)));

                if page_len < usize::try_from(self.page_size).unwrap_or(0) || page_len == 0 {
                    break;
                }
                skip = skip.saturating_add(self.page_size);
            }

            Ok(ListOutcome::Fetched { pull_requests, etag: None })
        })
    }

    fn fetch_pull_request<'a>(
        &'a self,
        repository: &'a RepositoryCoordinate,
        number: PullRequestNumber,
    ) -> BoxFuture<'a, Result<PullRequestDetail, ProviderError>> {
        Box::pin(async move {
            let (organization, project, name) = Self::resolve(repository)?;
            let pull_request_id = i32::try_from(number.0)
                .map_err(|_error| ProviderError::configuration(format!("pull request id {} is out of range for Azure DevOps", number.0)))?;

            let client = self.client()?;
            let pull_request = client
                .pull_requests_client()
                .get_pull_request(organization, name, pull_request_id, project)
                .await
                .map_err(|error| map_azure_error(&error))?;

            Ok(detail_from(&pull_request, repository))
        })
    }
}

/// Maps an [`azure_core::Error`] into a typed [`ProviderError`].
///
/// The SDK surfaces an HTTP status code for service responses; transport-level
/// failures (no response) carry no status and are treated as transient.
fn map_azure_error(error: &azure_core::Error) -> ProviderError {
    match error.http_status() {
        Some(status) => classify_http_status(u16::from(status), None),
        None => ProviderError::transient(format!("Azure DevOps request failed before a response was received: {error}")),
    }
}

/// Removes a single trailing slash so endpoint joins stay well-formed.
#[cfg(test)]
fn trim_trailing_slash(mut value: String) -> String {
    if value.ends_with('/') {
        value.pop();
    }
    value
}

/// Normalizes an Azure DevOps pull request into a provider-neutral summary.
fn summary_from(pull_request: &models::GitPullRequest, repository: &RepositoryCoordinate) -> PullRequestSummary {
    let state = match pull_request.status {
        git::models::git_pull_request::Status::Completed => PullRequestState::Merged,
        git::models::git_pull_request::Status::Abandoned => PullRequestState::Closed,
        _ => PullRequestState::Open,
    };

    let author = Some(identity_to_user(&pull_request.created_by));
    let updated_at = pull_request
        .last_merge_source_commit
        .as_ref()
        .and_then(|commit| {
            commit
                .committer
                .as_ref()
                .and_then(|committer| committer.date)
                .or_else(|| commit.author.as_ref().and_then(|author| author.date))
        })
        .and_then(|timestamp| timestamp.format(&Rfc3339).ok());

    PullRequestSummary {
        provider: ProviderKind::AzureDevOps,
        repository: repository.clone(),
        number: PullRequestNumber(pull_request.pull_request_id.unsigned_abs().into()),
        title: pull_request.title.clone().unwrap_or_default(),
        state,
        is_draft: pull_request.is_draft,
        author,
        source_branch: Some(short_branch(&pull_request.source_ref_name).to_owned()),
        target_branch: Some(short_branch(&pull_request.target_ref_name).to_owned()),
        source_commit_sha: pull_request
            .last_merge_source_commit
            .as_ref()
            .and_then(|commit| commit.commit_id.clone()),
        url: web_url(pull_request),
        created_at: pull_request.creation_date.format(&Rfc3339).ok(),
        updated_at,
    }
}

/// Normalizes an Azure DevOps pull request into a detail record.
fn detail_from(pull_request: &models::GitPullRequest, repository: &RepositoryCoordinate) -> PullRequestDetail {
    let summary = summary_from(pull_request, repository);
    let labels = pull_request
        .labels
        .iter()
        .filter_map(|label| label.name.clone())
        .map(|name| Label { name })
        .collect();
    let reviewers = pull_request
        .reviewers
        .iter()
        .map(|reviewer| Reviewer {
            user: identity_to_user(&reviewer.identity_ref),
            decision: decision_from_vote(reviewer.vote.unwrap_or(0)),
        })
        .collect();

    PullRequestDetail {
        summary,
        body: pull_request.description.clone(),
        labels,
        reviewers,
    }
}

/// Maps an Azure DevOps identity into a provider-neutral user reference.
fn identity_to_user(identity: &models::IdentityRef) -> UserRef {
    UserRef {
        login: identity.unique_name.clone(),
        display_name: identity.graph_subject_base.display_name.clone(),
    }
}

/// Maps an Azure DevOps reviewer vote to a normalized decision.
///
/// Azure DevOps votes are `10` (approved), `5` (approved with suggestions),
/// `0` (no vote), `-5` (waiting for author), and `-10` (rejected).
const fn decision_from_vote(vote: i64) -> ReviewDecision {
    if vote >= 5 {
        ReviewDecision::Approved
    } else if vote <= -5 {
        ReviewDecision::ChangesRequested
    } else {
        ReviewDecision::Pending
    }
}

/// Strips the `refs/heads/` prefix from a branch ref for display.
fn short_branch(ref_name: &str) -> &str {
    ref_name.strip_prefix("refs/heads/").unwrap_or(ref_name)
}

/// Extracts the browser URL from the pull request's `_links.web.href`, falling
/// back to the API URL when the web link is absent.
fn web_url(pull_request: &models::GitPullRequest) -> Option<String> {
    pull_request
        .links
        .as_ref()
        .and_then(|links| links.get("web"))
        .and_then(|web| web.get("href"))
        .and_then(|href| href.as_str())
        .map(ToOwned::to_owned)
        .or_else(|| Some(pull_request.url.clone()))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use azure_devops_rust_api::Credential;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::super::error::ProviderErrorKind;
    use super::super::model::{ProviderKind, PullRequestNumber, PullRequestState, RepositoryCoordinate, ReviewDecision};
    use super::super::transport::{ListOutcome, PullRequestSource};
    use super::AzureDevOpsProvider;

    const PAGE1: &str = include_str!("fixtures/ado_pulls_page1.json");
    const PAGE2: &str = include_str!("fixtures/ado_pulls_page2.json");
    const DETAIL: &str = include_str!("fixtures/ado_pull_detail.json");

    fn provider(endpoint: &str) -> AzureDevOpsProvider {
        AzureDevOpsProvider::new(Credential::from_pat("fake-pat")).with_endpoint(endpoint)
    }

    fn repo() -> RepositoryCoordinate {
        RepositoryCoordinate::azure_devops("contoso", "Contoso", "widgets")
    }

    /// Builds the request path wiremock actually observes.
    ///
    /// The generated SDK constructs request URLs with
    /// `format!("{}/{}/...", endpoint, organization, ...)`, and an
    /// authority-only `Url` (e.g. `http://127.0.0.1:port`) renders with a
    /// trailing slash, so the emitted path begins with a literal `//`. Real
    /// Azure DevOps tolerates the collapsed double slash; wiremock matches the
    /// raw path, so tests assert the exact string the SDK sends.
    fn ado_path(suffix: &str) -> String {
        format!("//contoso/Contoso/_apis/git/repositories/widgets/{suffix}")
    }

    #[tokio::test]
    async fn lists_and_normalizes_across_pages() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(ado_path("pullrequests")))
            .and(query_param("$skip", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(PAGE1, "application/json"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(ado_path("pullrequests")))
            .and(query_param("$skip", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(PAGE2, "application/json"))
            .mount(&server)
            .await;

        let outcome = provider(&server.uri())
            .with_page_size(2)
            .list_open_pull_requests(&repo(), None)
            .await
            .expect("list should succeed");

        match outcome {
            ListOutcome::Fetched { pull_requests, etag } => {
                assert_eq!(pull_requests.len(), 3);
                assert_eq!(pull_requests[0].number, PullRequestNumber(42));
                assert_eq!(pull_requests[0].provider, ProviderKind::AzureDevOps);
                assert_eq!(pull_requests[0].state, PullRequestState::Open);
                assert!(!pull_requests[0].is_draft);
                assert_eq!(pull_requests[0].source_branch.as_deref(), Some("feature/retry"));
                assert_eq!(pull_requests[0].target_branch.as_deref(), Some("main"));
                assert_eq!(pull_requests[0].source_commit_sha.as_deref(), Some("abc123sourcecommit"));
                assert_eq!(
                    pull_requests[0].author.as_ref().and_then(|user| user.display_name.as_deref()),
                    Some("Ada Lovelace")
                );
                assert!(pull_requests[1].is_draft);
                assert_eq!(
                    pull_requests[1].source_commit_sha, None,
                    "missing lastMergeSourceCommit maps to None"
                );
                assert_eq!(pull_requests[2].number, PullRequestNumber(44));
                assert_eq!(etag, None);
            }
            ListOutcome::Unchanged => panic!("Azure DevOps never reports unchanged"),
        }
    }

    #[tokio::test]
    async fn fetch_detail_normalizes_labels_and_reviewers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(ado_path("pullrequests/42")))
            .respond_with(ResponseTemplate::new(200).set_body_raw(DETAIL, "application/json"))
            .mount(&server)
            .await;

        let detail = provider(&server.uri())
            .fetch_pull_request(&repo(), PullRequestNumber(42))
            .await
            .expect("detail should succeed");

        assert_eq!(detail.summary.number, PullRequestNumber(42));
        assert_eq!(detail.summary.source_commit_sha.as_deref(), Some("abc123sourcecommit"));
        assert_eq!(detail.summary.updated_at.as_deref(), Some("2024-05-02T15:30:00Z"));
        assert_eq!(
            detail.body.as_deref(),
            Some("Adds a bounded retry policy with exponential backoff.")
        );
        assert_eq!(
            detail.summary.url.as_deref(),
            Some("https://dev.azure.com/contoso/Contoso/_git/widgets/pullrequest/42")
        );
        assert_eq!(detail.labels.len(), 2);
        assert_eq!(detail.labels[0].name, "enhancement");
        assert_eq!(detail.reviewers.len(), 2);
        assert_eq!(detail.reviewers[0].user.display_name.as_deref(), Some("Grace Hopper"));
        assert_eq!(detail.reviewers[0].decision, ReviewDecision::Approved);
        assert_eq!(detail.reviewers[1].decision, ReviewDecision::ChangesRequested);
    }

    #[tokio::test]
    async fn not_found_is_classified() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(ado_path("pullrequests/999")))
            .respond_with(ResponseTemplate::new(404).set_body_raw(r#"{"message":"not found"}"#, "application/json"))
            .mount(&server)
            .await;

        let error = provider(&server.uri())
            .fetch_pull_request(&repo(), PullRequestNumber(999))
            .await
            .expect_err("should fail");
        assert_eq!(error.kind(), ProviderErrorKind::NotFound);
    }

    #[tokio::test]
    async fn throttling_is_classified() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(ado_path("pullrequests")))
            .respond_with(ResponseTemplate::new(429).set_body_raw(r#"{"message":"too many"}"#, "application/json"))
            .mount(&server)
            .await;

        let error = provider(&server.uri())
            .list_open_pull_requests(&repo(), None)
            .await
            .expect_err("should be throttled");
        assert_eq!(error.kind(), ProviderErrorKind::Throttled);
    }

    #[tokio::test]
    async fn missing_project_is_configuration_error() {
        let error = provider("http://127.0.0.1:1")
            .list_open_pull_requests(&RepositoryCoordinate::github("contoso", "widgets"), None)
            .await
            .expect_err("should fail");
        assert_eq!(error.kind(), ProviderErrorKind::Configuration);
    }

    #[tokio::test]
    async fn server_error_is_transient() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(ado_path("pullrequests")))
            .respond_with(ResponseTemplate::new(503).set_body_raw("down", "text/plain"))
            .mount(&server)
            .await;

        let error = provider(&server.uri())
            .list_open_pull_requests(&repo(), None)
            .await
            .expect_err("should fail");
        assert_eq!(error.kind(), ProviderErrorKind::Transient);
        assert!(error.kind().is_retryable());
    }
}
