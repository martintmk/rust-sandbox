// Licensed under the MIT License.

//! GitHub pull request adapter, built on the community-maintained [`octocrab`] SDK.
//!
//! A fresh token is minted from the GitHub CLI ([`CredentialSource`]) per
//! request and handed to a short-lived [`octocrab::Octocrab`] client; the SDK
//! owns transport, authorization, models, and single-resource fetches.
//!
//! # Documented raw-request gap
//!
//! `octocrab`'s high-level pager ([`octocrab::Octocrab::all_pages`]) does not
//! surface the `ETag`/`Link` response headers, so it cannot express GitHub's
//! conditional (`If-None-Match` → `304 Not Modified`) listing or `Link`-header
//! pagination. The list path therefore drives the SDK's own low-level transport
//! ([`octocrab::Octocrab::_get_with_headers`]) — still octocrab's HTTP stack,
//! auth, and [`octocrab::models::pulls::PullRequest`] models — purely to read
//! those headers. Everything else (`pulls().get(..)`, the reviews collection)
//! uses the SDK's typed methods. The GitHub reviews endpoint likewise has no
//! typed `octocrab` method, so it is fetched through the SDK's generic
//! [`octocrab::Octocrab::get`] into `octocrab` models.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use octocrab::models::IssueState;
use octocrab::models::pulls::{PullRequest as OctoPullRequest, Review, ReviewState};
use octocrab::{Octocrab, Page};
use tick::Clock;

use super::command::BoxFuture;
use super::credentials::CredentialSource;
use super::error::ProviderError;
use super::model::{
    Label, ProviderKind, PullRequestDetail, PullRequestNumber, PullRequestState, PullRequestSummary, RepositoryCoordinate, ReviewDecision,
    Reviewer, UserRef,
};
use super::secret::SecretString;
use super::transport::{EntityTag, ListOutcome, PullRequestSource, classify_http_status};

/// Page size used for listing pull requests (GitHub caps this at 100).
const PER_PAGE: u8 = 100;

/// A GitHub pull request source.
#[derive(Clone, Debug)]
pub(crate) struct GitHubProvider {
    credential: Arc<dyn CredentialSource>,
    clock: Clock,
    base_uri: Option<String>,
}

impl GitHubProvider {
    /// Creates a GitHub provider using the public API endpoint.
    pub(crate) fn new(credential: Arc<dyn CredentialSource>, clock: Clock) -> Self {
        Self {
            credential,
            clock,
            base_uri: None,
        }
    }

    /// Overrides the API base URL (for GitHub Enterprise or tests).
    #[cfg(test)]
    pub(crate) fn with_base_uri(mut self, base_uri: impl Into<String>) -> Self {
        self.base_uri = Some(trim_trailing_slash(base_uri.into()));
        self
    }

    /// Validates the coordinate and returns `(owner, name)`.
    fn resolve(repository: &RepositoryCoordinate) -> Result<(&str, &str), ProviderError> {
        if repository.owner.is_empty() || repository.name.is_empty() {
            return Err(ProviderError::configuration(
                "GitHub repository requires a non-empty owner and name",
            ));
        }
        Ok((&repository.owner, &repository.name))
    }

    /// Builds a short-lived, token-authorized `octocrab` client.
    fn client(&self, token: &SecretString) -> Result<Octocrab, ProviderError> {
        let mut builder = Octocrab::builder().personal_token(token.reveal().to_owned());
        if let Some(base) = &self.base_uri {
            builder = builder
                .base_uri(base.as_str())
                .map_err(|error| ProviderError::configuration(format!("invalid GitHub base URI: {error}")))?;
        }
        builder
            .build()
            .map_err(|error| ProviderError::configuration(format!("failed to build GitHub client: {error}")))
    }
}

impl PullRequestSource for GitHubProvider {
    fn list_open_pull_requests<'a>(
        &'a self,
        repository: &'a RepositoryCoordinate,
        conditional: Option<&'a EntityTag>,
    ) -> BoxFuture<'a, Result<ListOutcome, ProviderError>> {
        Box::pin(async move {
            let (owner, name) = Self::resolve(repository)?;
            let token = self.credential.access_token().await?;
            let client = self.client(&token)?;

            let mut route = Some(format!("/repos/{owner}/{name}/pulls?state=open&per_page={PER_PAGE}&page=1"));
            let mut pull_requests = Vec::new();
            let mut captured_etag = None;
            let mut is_first_page = true;

            while let Some(uri) = route.take() {
                let mut headers = http::HeaderMap::new();
                if is_first_page && let Some(tag) = conditional {
                    let value = http::HeaderValue::from_str(tag.as_str())
                        .map_err(|error| ProviderError::configuration(format!("invalid ETag value: {error}")))?;
                    headers.insert(http::header::IF_NONE_MATCH, value);
                }
                let request_headers = if headers.is_empty() { None } else { Some(headers) };

                let response = client
                    ._get_with_headers(uri, request_headers)
                    .await
                    .map_err(|error| map_octocrab_error(&error))?;

                let status = response.status();
                if is_first_page && status == http::StatusCode::NOT_MODIFIED {
                    return Ok(ListOutcome::Unchanged);
                }
                if !status.is_success() {
                    let retry_after = retry_after_from_headers(response.headers(), &self.clock);
                    return Err(classify_http_status(status.as_u16(), retry_after));
                }

                if is_first_page {
                    captured_etag = response
                        .headers()
                        .get(http::header::ETAG)
                        .and_then(|value| value.to_str().ok())
                        .map(EntityTag::new);
                }
                let next = next_page_link(response.headers());

                let body = client.body_to_string(response).await.map_err(|error| map_octocrab_error(&error))?;
                let page: Vec<OctoPullRequest> = serde_json::from_str(&body)
                    .map_err(|error| ProviderError::response_caused_by("failed to parse GitHub pull request list", error))?;
                pull_requests.extend(page.iter().map(|pull_request| summary_from(pull_request, repository)));

                route = next;
                is_first_page = false;
            }

            Ok(ListOutcome::Fetched {
                pull_requests,
                etag: captured_etag,
            })
        })
    }

    fn fetch_pull_request<'a>(
        &'a self,
        repository: &'a RepositoryCoordinate,
        number: PullRequestNumber,
    ) -> BoxFuture<'a, Result<PullRequestDetail, ProviderError>> {
        Box::pin(async move {
            let (owner, name) = Self::resolve(repository)?;
            let token = self.credential.access_token().await?;
            let client = self.client(&token)?;

            let pull_request = client
                .pulls(owner, name)
                .get(number.0)
                .await
                .map_err(|error| map_octocrab_error(&error))?;

            // The reviews endpoint has no typed `octocrab` method; fetch it via
            // the SDK's generic getter into `octocrab` models (documented gap).
            let reviews_route = format!("/repos/{owner}/{name}/pulls/{}/reviews?per_page=100", number.0);
            let reviews: Page<Review> = client
                .get(reviews_route, None::<&()>)
                .await
                .map_err(|error| map_octocrab_error(&error))?;

            Ok(detail_from(&pull_request, &reviews.items, repository))
        })
    }
}

/// Maps an [`octocrab::Error`] into a typed [`ProviderError`].
///
/// GitHub API errors carry an HTTP status code but not their response headers,
/// so throttling `retry_after` is unavailable here; the list path recovers it
/// from the raw response instead.
fn map_octocrab_error(error: &octocrab::Error) -> ProviderError {
    match error {
        octocrab::Error::GitHub { source, .. } => classify_http_status(source.status_code.as_u16(), None),
        octocrab::Error::Service { .. } | octocrab::Error::Hyper { .. } | octocrab::Error::Http { .. } => {
            ProviderError::transient(format!("GitHub request failed before a response was received: {error}"))
        }
        other => ProviderError::response(format!("GitHub request failed: {other}")),
    }
}

/// Removes a single trailing slash so URL joins stay well-formed.
#[cfg(test)]
fn trim_trailing_slash(mut value: String) -> String {
    if value.ends_with('/') {
        value.pop();
    }
    value
}

/// Extracts the `rel="next"` URL from a GitHub `Link` header, if present.
fn next_page_link(headers: &http::HeaderMap) -> Option<String> {
    let link = headers.get("link").and_then(|value| value.to_str().ok())?;
    for segment in link.split(',') {
        let mut parts = segment.split(';');
        let Some(url_part) = parts.next() else {
            continue;
        };
        let is_next = parts.any(|param| param.trim() == "rel=\"next\"");
        if is_next {
            let url = url_part.trim().trim_start_matches('<').trim_end_matches('>');
            return Some(url.to_owned());
        }
    }
    None
}

/// Computes a backoff delay from `Retry-After` or `x-ratelimit-reset` headers.
fn retry_after_from_headers(headers: &http::HeaderMap, clock: &Clock) -> Option<Duration> {
    if let Some(seconds) = headers
        .get(http::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
    {
        return Some(Duration::from_secs(seconds));
    }

    let reset_epoch = headers
        .get("x-ratelimit-reset")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())?;
    let reset_at = SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(reset_epoch))?;
    reset_at.duration_since(clock.system_time()).ok()
}

/// Normalizes an `octocrab` pull request into a provider-neutral summary.
fn summary_from(pull_request: &OctoPullRequest, repository: &RepositoryCoordinate) -> PullRequestSummary {
    let state = if pull_request.merged_at.is_some() {
        PullRequestState::Merged
    } else {
        match pull_request.state {
            Some(IssueState::Closed) => PullRequestState::Closed,
            _ => PullRequestState::Open,
        }
    };

    let author = pull_request.user.as_ref().map(|user| UserRef {
        login: Some(user.login.clone()),
        display_name: user.name.clone().or_else(|| Some(user.login.clone())),
    });

    PullRequestSummary {
        provider: ProviderKind::GitHub,
        repository: repository.clone(),
        number: PullRequestNumber(pull_request.number),
        title: pull_request.title.clone().unwrap_or_default(),
        state,
        is_draft: pull_request.draft.unwrap_or(false),
        author,
        source_branch: Some(pull_request.head.ref_field.clone()),
        target_branch: Some(pull_request.base.ref_field.clone()),
        source_commit_sha: Some(pull_request.head.sha.clone()),
        url: pull_request.html_url.as_ref().map(ToString::to_string),
        created_at: pull_request.created_at.map(|timestamp| timestamp.to_rfc3339()),
        updated_at: pull_request.updated_at.map(|timestamp| timestamp.to_rfc3339()),
    }
}

/// Normalizes an `octocrab` pull request plus its reviews into a detail record.
fn detail_from(pull_request: &OctoPullRequest, reviews: &[Review], repository: &RepositoryCoordinate) -> PullRequestDetail {
    let summary = summary_from(pull_request, repository);
    let labels = pull_request
        .labels
        .iter()
        .flatten()
        .map(|label| Label { name: label.name.clone() })
        .collect();
    let reviewers = reviews
        .iter()
        .map(|review| Reviewer {
            user: UserRef {
                login: review.user.as_ref().map(|user| user.login.clone()),
                display_name: review
                    .user
                    .as_ref()
                    .map(|user| user.name.clone().unwrap_or_else(|| user.login.clone())),
            },
            decision: decision_from_state(review.state),
        })
        .collect();

    PullRequestDetail {
        summary,
        body: pull_request.body.clone(),
        labels,
        reviewers,
    }
}

/// Maps a GitHub review state to a normalized decision.
fn decision_from_state(state: Option<ReviewState>) -> ReviewDecision {
    match state {
        Some(ReviewState::Approved) => ReviewDecision::Approved,
        Some(ReviewState::ChangesRequested) => ReviewDecision::ChangesRequested,
        _ => ReviewDecision::Pending,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::Arc;

    use tick::Clock;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::super::command::testing::ScriptedCommandRunner;
    use super::super::credentials::GitHubCliCredential;
    use super::super::error::ProviderErrorKind;
    use super::super::model::{ProviderKind, PullRequestNumber, PullRequestState, RepositoryCoordinate};
    use super::super::transport::{EntityTag, ListOutcome, PullRequestSource};
    use super::GitHubProvider;

    const PAGE1: &str = include_str!("fixtures/github_pulls_page1.json");
    const PAGE2: &str = include_str!("fixtures/github_pulls_page2.json");
    const DETAIL: &str = include_str!("fixtures/github_pull_detail.json");
    const REVIEWS: &str = include_str!("fixtures/github_reviews.json");

    fn provider(base_uri: &str) -> GitHubProvider {
        let runner = Arc::new(ScriptedCommandRunner::new());
        for _ in 0..8 {
            runner.push_stdout("gh", "gho_test_token\n");
        }
        let credential = Arc::new(GitHubCliCredential::new(runner));
        GitHubProvider::new(credential, Clock::new_frozen()).with_base_uri(base_uri)
    }

    fn repo() -> RepositoryCoordinate {
        RepositoryCoordinate::github("acme", "widgets")
    }

    #[tokio::test]
    async fn lists_and_normalizes_across_pages() {
        let server = MockServer::start().await;
        let next_link = format!(
            "<{}/repos/acme/widgets/pulls?state=open&per_page=100&page=2>; rel=\"next\"",
            server.uri()
        );
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls"))
            .and(query_param("page", "1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("ETag", "\"etag-v1\"")
                    .insert_header("Link", next_link.as_str())
                    .set_body_raw(PAGE1, "application/json"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(PAGE2, "application/json"))
            .mount(&server)
            .await;

        let outcome = provider(&server.uri())
            .list_open_pull_requests(&repo(), None)
            .await
            .expect("list should succeed");

        match outcome {
            ListOutcome::Fetched { pull_requests, etag } => {
                assert_eq!(pull_requests.len(), 3);
                assert_eq!(pull_requests[0].number, PullRequestNumber(101));
                assert_eq!(pull_requests[0].provider, ProviderKind::GitHub);
                assert_eq!(pull_requests[0].state, PullRequestState::Open);
                assert!(!pull_requests[0].is_draft);
                assert_eq!(pull_requests[0].source_branch.as_deref(), Some("feature/retry"));
                assert_eq!(pull_requests[0].target_branch.as_deref(), Some("main"));
                assert_eq!(
                    pull_requests[0].source_commit_sha.as_deref(),
                    Some("aaaa111100002222333344445555666677778888")
                );
                assert!(pull_requests[1].is_draft);
                assert_eq!(pull_requests[2].number, PullRequestNumber(103));
                assert_eq!(etag, Some(EntityTag::new("\"etag-v1\"")));
            }
            ListOutcome::Unchanged => panic!("expected a fresh listing"),
        }
    }

    #[tokio::test]
    async fn conditional_request_returns_unchanged_on_304() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls"))
            .and(header("if-none-match", "\"etag-v1\""))
            .respond_with(ResponseTemplate::new(304))
            .expect(1)
            .mount(&server)
            .await;

        let tag = EntityTag::new("\"etag-v1\"");
        let outcome = provider(&server.uri())
            .list_open_pull_requests(&repo(), Some(&tag))
            .await
            .expect("list should succeed");

        assert_eq!(outcome, ListOutcome::Unchanged);
    }

    #[tokio::test]
    async fn fetch_detail_normalizes_labels_and_reviewers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls/101"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(DETAIL, "application/json"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls/101/reviews"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(REVIEWS, "application/json"))
            .mount(&server)
            .await;

        let detail = provider(&server.uri())
            .fetch_pull_request(&repo(), PullRequestNumber(101))
            .await
            .expect("detail should succeed");

        assert_eq!(detail.summary.number, PullRequestNumber(101));
        assert_eq!(
            detail.body.as_deref(),
            Some("Adds a bounded retry policy with exponential backoff.")
        );
        assert_eq!(detail.labels.len(), 2);
        assert_eq!(detail.labels[0].name, "enhancement");
        assert_eq!(detail.reviewers.len(), 2);
        assert_eq!(detail.reviewers[0].user.login.as_deref(), Some("reviewer-one"));
        assert_eq!(detail.reviewers[0].decision, super::ReviewDecision::Approved);
        assert_eq!(detail.reviewers[1].decision, super::ReviewDecision::ChangesRequested);
    }

    #[tokio::test]
    async fn not_found_is_classified() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls/999"))
            .respond_with(ResponseTemplate::new(404).set_body_raw(r#"{"message":"Not Found"}"#, "application/json"))
            .mount(&server)
            .await;

        let error = provider(&server.uri())
            .fetch_pull_request(&repo(), PullRequestNumber(999))
            .await
            .expect_err("should fail");
        assert_eq!(error.kind(), ProviderErrorKind::NotFound);
    }

    #[tokio::test]
    async fn rate_limit_is_classified_as_throttled() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("x-ratelimit-remaining", "0")
                    .insert_header("retry-after", "30")
                    .set_body_raw(r#"{"message":"API rate limit exceeded"}"#, "application/json"),
            )
            .mount(&server)
            .await;

        let error = provider(&server.uri())
            .list_open_pull_requests(&repo(), None)
            .await
            .expect_err("should be throttled");
        assert_eq!(error.kind(), ProviderErrorKind::Throttled);
        assert_eq!(error.retry_after(), Some(std::time::Duration::from_secs(30)));
    }

    #[tokio::test]
    async fn empty_coordinate_is_configuration_error() {
        let error = provider("http://127.0.0.1:1")
            .list_open_pull_requests(&RepositoryCoordinate::github("", "widgets"), None)
            .await
            .expect_err("should fail");
        assert_eq!(error.kind(), ProviderErrorKind::Configuration);
    }

    #[tokio::test]
    async fn server_error_is_transient() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls"))
            .respond_with(ResponseTemplate::new(503).set_body_raw("still down", "text/plain"))
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
