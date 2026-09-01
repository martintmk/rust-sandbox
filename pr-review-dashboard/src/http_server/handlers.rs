// Licensed under the MIT License.

//! HTML page and JSON API handlers.
//!
//! Read handlers load data through [`Storage`]. Mutations either submit an
//! analysis to the transient [`CopilotService`] queue or atomically update the
//! active configuration file.
//!
//! The dashboard's richer rows and the pull request detail page are assembled here from the stored
//! analysis blob: each successful analysis is persisted as a single JSON document, which these
//! handlers deserialize into the overview/interest/review view fields the templates display. Every
//! value ultimately flows through minijinja's HTML auto-escaping, so provider, model, title, and
//! rationale strings are escaped in whatever context they land in.

use std::cmp::Ordering;
use std::sync::Arc;

use analysis::{AnalysisOutput, Interest, Priority, Verdict};
use hyper::StatusCode;
use serde::Serialize;
use storage::{
    Analysis, AnalysisOutcome, PullRequest, PullRequestState, Repository, RepositoryId, Storage, StorageError, StorageErrorKind,
};
use tick::Clock;

use crate::config::{ActionMapping, AppConfig, ConfigUpdate, ConfigUpdateError};
use crate::copilot::{CopilotError, CopilotService};
use crate::prereqs::{Availability, PrerequisiteReport};
use crate::templates::Templates;

use super::responses::{self, Body};
use super::routes::RouteResolver;
use super::security::{CsrfToken, validate_external_https_link};

type Response = hyper::Response<Body>;

/// Application state shared by every request.
pub(super) struct ServerState {
    pub(super) storage: Storage,
    pub(super) copilot: CopilotService,
    pub(super) templates: Templates,
    pub(super) clock: Clock,
    pub(super) csrf: CsrfToken,
    pub(super) listen_port: u16,
    pub(super) resolver: RouteResolver,
    pub(super) config_update_lock: tokio::sync::Mutex<()>,
    pub(super) config: Arc<AppConfig>,
    pub(super) prerequisites: Arc<PrerequisiteReport>,
}

fn pull_request_state_str(state: PullRequestState) -> &'static str {
    match state {
        PullRequestState::Open => "open",
        PullRequestState::Closed => "closed",
    }
}

/// Current time as Unix milliseconds. The polling and Copilot stages persist their timestamps in
/// milliseconds (see `polling::now_millis`), so freshness/staleness comparisons use the same unit
/// as `pull_requests.refreshed_at`.
fn now_epoch_millis(clock: &Clock) -> i64 {
    let since_epoch = clock.system_time().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX)
}

/// Renders a Unix-millisecond timestamp as a coarse "… ago" string for at-a-glance freshness. The
/// exact instant is exposed separately (see [`format_timestamp_iso`]) for a hover title.
fn humanize_age_millis(now_ms: i64, then_ms: i64) -> String {
    let delta_seconds = now_ms.saturating_sub(then_ms).max(0) / 1000;
    if delta_seconds < 60 {
        return "just now".to_owned();
    }
    let minutes = delta_seconds / 60;
    if minutes < 60 {
        return format!("{minutes} minute{} ago", plural(minutes));
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours} hour{} ago", plural(hours));
    }
    let days = hours / 24;
    format!("{days} day{} ago", plural(days))
}

const fn plural(count: i64) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// Formats a Unix-millisecond timestamp as an RFC 3339 UTC string, or an empty string if it falls
/// outside the representable range.
fn format_timestamp_iso(millis: i64) -> String {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    OffsetDateTime::from_unix_timestamp(millis.div_euclid(1000))
        .ok()
        .and_then(|instant| instant.format(&Rfc3339).ok())
        .unwrap_or_default()
}

/// Whether a pull request's cached data is stale: last refreshed longer than three poll intervals
/// ago. Three intervals tolerates a single missed or slow poll before flagging the row.
fn is_stale(config: &AppConfig, now_ms: i64, refreshed_at_ms: i64) -> bool {
    let interval_ms = i64::try_from(config.poll_interval.as_millis()).unwrap_or(i64::MAX);
    let threshold = interval_ms.saturating_mul(3);
    now_ms.saturating_sub(refreshed_at_ms) > threshold
}

/// The interest priority as displayed and filtered: the raw priority when the change is
/// interesting, otherwise `ignore`. `unknown` is used when no analysis exists yet.
fn priority_key(interesting: Option<&Interest>) -> &'static str {
    match interesting {
        None => "unknown",
        Some(interest) if !interest.interesting => "ignore",
        Some(interest) => priority_value(interest.priority),
    }
}

fn priority_value(priority: Priority) -> &'static str {
    match priority {
        Priority::Low => "low",
        Priority::Medium => "medium",
        Priority::High => "high",
        Priority::Critical => "critical",
    }
}

fn verdict_value(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Approve => "approve",
        Verdict::Comment => "comment",
        Verdict::RequestChanges => "request_changes",
    }
}

fn title_case(value: &str) -> String {
    let mut characters = value.chars();
    characters
        .next()
        .map_or_else(String::new, |first| first.to_uppercase().collect::<String>() + characters.as_str())
}

fn verdict_label(verdict: &str) -> String {
    match verdict {
        "approve" => "Approve".to_owned(),
        "comment" => "Comment".to_owned(),
        "request_changes" => "Request changes".to_owned(),
        other => title_case(other),
    }
}

/// The parsed presentation of a pull request's newest analysis and its freshness.
struct AnalysisView {
    status: &'static str,
    overview: Option<String>,
    has_interest: bool,
    priority: &'static str,
    rationale: Option<String>,
    has_review: bool,
    verdict: String,
    review_summary: Option<String>,
    findings: Vec<FindingView>,
    error: Option<String>,
}

impl Default for AnalysisView {
    fn default() -> Self {
        Self {
            status: "pending",
            overview: None,
            has_interest: false,
            priority: "unknown",
            rationale: None,
            has_review: false,
            verdict: String::new(),
            review_summary: None,
            findings: Vec::new(),
            error: None,
        }
    }
}

#[derive(Serialize)]
struct FindingView {
    severity: String,
    title: String,
    details: String,
    location: Option<String>,
}

impl AnalysisView {
    /// Interprets the newest analysis row, retaining older-revision output as outdated.
    fn from_row(analysis: Option<&Analysis>, pull_request: &PullRequest, repository: &Repository) -> Self {
        let Some(analysis) = analysis else {
            return Self::default();
        };
        let status = if analysis.revision_fingerprint != pull_request.revision_fingerprint
            || analysis.action_configuration_fingerprint != repository.action_configuration_fingerprint
        {
            "outdated"
        } else {
            "current"
        };
        if analysis.outcome == AnalysisOutcome::Failed {
            return Self {
                status,
                error: Some(
                    analysis
                        .diagnostic
                        .clone()
                        .unwrap_or_else(|| "The last analysis failed without a recorded reason.".to_owned()),
                ),
                ..Self::default()
            };
        }
        let Some(summary) = analysis.summary.as_deref() else {
            return Self {
                status,
                error: Some("The stored analysis has no output.".to_owned()),
                ..Self::default()
            };
        };
        match serde_json::from_str::<AnalysisOutput>(summary) {
            Ok(output) => Self {
                status,
                overview: Some(output.overview.summary),
                has_interest: true,
                priority: priority_key(Some(&output.interesting)),
                rationale: Some(output.interesting.rationale),
                has_review: true,
                verdict: verdict_value(output.review.verdict).to_owned(),
                review_summary: Some(output.review.summary),
                findings: output
                    .review
                    .findings
                    .into_iter()
                    .map(|finding| FindingView {
                        location: finding_location(finding.file.as_deref(), finding.line),
                        severity: priority_value(finding.severity).to_owned(),
                        title: finding.title,
                        details: finding.details,
                    })
                    .collect(),
                error: None,
            },
            Err(_) => Self {
                status,
                error: Some("The stored analysis could not be read.".to_owned()),
                ..Self::default()
            },
        }
    }
}

fn finding_location(file: Option<&str>, line: Option<u64>) -> Option<String> {
    match (file, line) {
        (Some(file), Some(line)) => Some(format!("{file}:{line}")),
        (Some(file), None) => Some(file.to_owned()),
        _ => None,
    }
}

// --- serialized template contexts -------------------------------------------------------------

#[derive(Serialize)]
pub(super) struct RepositorySummary {
    id: i64,
    provider: String,
    owner: String,
    name: String,
    active: bool,
}

impl From<&Repository> for RepositorySummary {
    fn from(repository: &Repository) -> Self {
        Self {
            id: repository.id.0,
            provider: repository.provider.clone(),
            owner: repository.owner.clone(),
            name: repository.name.clone(),
            active: repository.active,
        }
    }
}

#[derive(Serialize)]
struct PullRequestSummary {
    id: i64,
    number: i64,
    title: String,
    author: Option<String>,
    state: &'static str,
    web_url: String,
}

impl From<&PullRequest> for PullRequestSummary {
    fn from(pull_request: &PullRequest) -> Self {
        Self {
            id: pull_request.id.0,
            number: pull_request.number,
            title: pull_request.title.clone(),
            author: pull_request.author.clone(),
            state: pull_request_state_str(pull_request.state),
            web_url: pull_request.web_url.clone(),
        }
    }
}

/// A fully-assembled dashboard/list row: the pull request plus its derived freshness and its
/// parsed interest priority and overview.
#[derive(Clone, Serialize)]
struct PullRequestRow {
    repository_id: i64,
    provider: String,
    repository: String,
    number: i64,
    title: String,
    author: Option<String>,
    state: &'static str,
    is_draft: bool,
    detail_path: String,
    updated_age: String,
    updated_iso: String,
    updated_at: Option<i64>,
    stale: bool,
    priority: &'static str,
    priority_label: String,
    rationale: Option<String>,
    overview: Option<String>,
    analysis_status: &'static str,
    has_error: bool,
}

#[derive(Serialize)]
struct RepositoryOption {
    id: i64,
    label: String,
}

#[derive(Serialize)]
struct DashboardFilters {
    provider: Option<String>,
    repository: Option<i64>,
    priority: Option<String>,
    draft: bool,
    stale: bool,
    query: String,
}

#[derive(Serialize)]
struct DashboardContext<'a> {
    title: &'static str,
    csrf_token: &'a str,
    total: usize,
    pull_requests: Vec<PullRequestRow>,
    provider_options: Vec<String>,
    repository_options: Vec<RepositoryOption>,
    priority_options: [&'static str; 4],
    filters: DashboardFilters,
    sort: SortLinks,
    sort_column: &'static str,
    sort_direction: &'static str,
}

#[derive(Serialize)]
struct PullRequestListContext<'a> {
    title: &'static str,
    csrf_token: &'a str,
    repository_id: i64,
    pull_requests: Vec<PullRequestRow>,
    sort: SortLinks,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SortColumn {
    Repository,
    PullRequest,
    Author,
    Updated,
    State,
    Priority,
    Overview,
}

impl SortColumn {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Repository => "repository",
            Self::PullRequest => "pull_request",
            Self::Author => "author",
            Self::Updated => "updated",
            Self::State => "state",
            Self::Priority => "priority",
            Self::Overview => "overview",
        }
    }

    const fn default_direction(self) -> SortDirection {
        match self {
            Self::Updated | Self::Priority => SortDirection::Descending,
            Self::Repository | Self::PullRequest | Self::Author | Self::State | Self::Overview => SortDirection::Ascending,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SortDirection {
    Ascending,
    Descending,
}

impl SortDirection {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ascending => "asc",
            Self::Descending => "desc",
        }
    }

    const fn aria_sort(self) -> &'static str {
        match self {
            Self::Ascending => "ascending",
            Self::Descending => "descending",
        }
    }

    const fn reverse(self) -> Self {
        match self {
            Self::Ascending => Self::Descending,
            Self::Descending => Self::Ascending,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SortSelection {
    column: SortColumn,
    direction: SortDirection,
}

#[derive(Serialize)]
struct SortLink {
    href: String,
    aria_sort: &'static str,
    indicator: &'static str,
}

#[derive(Serialize)]
struct SortLinks {
    repository: SortLink,
    pull_request: SortLink,
    author: SortLink,
    updated: SortLink,
    state: SortLink,
    priority: SortLink,
    overview: SortLink,
}

#[derive(Serialize)]
struct PullRequestDetailView {
    number: i64,
    title: String,
    author: Option<String>,
    state: &'static str,
    source_branch: String,
    target_branch: String,
    body: Option<String>,
    is_draft: bool,
}

impl From<&PullRequest> for PullRequestDetailView {
    fn from(pull_request: &PullRequest) -> Self {
        Self {
            number: pull_request.number,
            title: pull_request.title.clone(),
            author: pull_request.author.clone(),
            state: pull_request_state_str(pull_request.state),
            source_branch: pull_request.source_branch.clone(),
            target_branch: pull_request.target_branch.clone(),
            body: pull_request.details.as_ref().and_then(|details| details.body.clone()),
            is_draft: pull_request.details.as_ref().is_some_and(|details| details.is_draft),
        }
    }
}

#[derive(Serialize)]
struct DetailAnalysisContext {
    status: &'static str,
    overview: Option<String>,
    has_interest: bool,
    priority: &'static str,
    priority_label: String,
    rationale: Option<String>,
    has_review: bool,
    verdict: String,
    verdict_label: String,
    review_summary: Option<String>,
    findings: Vec<FindingView>,
    error: Option<String>,
}

impl From<AnalysisView> for DetailAnalysisContext {
    fn from(view: AnalysisView) -> Self {
        Self {
            status: view.status,
            priority_label: priority_label(view.priority),
            verdict_label: verdict_label(&view.verdict),
            overview: view.overview,
            has_interest: view.has_interest,
            priority: view.priority,
            rationale: view.rationale,
            has_review: view.has_review,
            verdict: view.verdict,
            review_summary: view.review_summary,
            findings: view.findings,
            error: view.error,
        }
    }
}

fn priority_label(priority: &str) -> String {
    match priority {
        "unknown" => "Not assessed".to_owned(),
        other => title_case(other),
    }
}

#[derive(Serialize)]
struct PullRequestDetailContext<'a> {
    title: String,
    csrf_token: &'a str,
    repository_id: i64,
    provider: String,
    pull_request: PullRequestDetailView,
    external_link: Option<String>,
    stale: bool,
    updated_age: String,
    updated_iso: String,
    analysis: DetailAnalysisContext,
}

#[derive(Serialize)]
struct SettingsToolRow {
    name: &'static str,
    status: &'static str,
    status_label: &'static str,
    message: Option<String>,
}

#[derive(Serialize)]
struct SettingsActionRow {
    name: &'static str,
    marketplace: String,
    plugin: String,
    skill: String,
    status: &'static str,
    status_label: &'static str,
    message: Option<String>,
}

#[derive(Serialize)]
struct SettingsPromptRow {
    name: &'static str,
    prompt: String,
}

#[derive(Serialize)]
struct SettingsRepositoryRow {
    provider: &'static str,
    url: String,
}

#[derive(Serialize)]
struct SettingsContext<'a> {
    title: &'static str,
    csrf_token: &'a str,
    configuration_source: String,
    configuration_loaded: bool,
    notice: Option<&'static str>,
    editable: bool,
    overview_prompt: String,
    interesting_prompt: String,
    tools: Vec<SettingsToolRow>,
    prompts: Vec<SettingsPromptRow>,
    actions: Vec<SettingsActionRow>,
    repositories: Vec<SettingsRepositoryRow>,
}

#[derive(Serialize)]
struct ErrorContext<'a> {
    title: &'static str,
    csrf_token: &'a str,
    message: &'a str,
}

// --- rendering helpers ------------------------------------------------------------------------

fn render_html(state: &ServerState, template: &str, context: impl Serialize) -> Response {
    match state.templates.render(template, context) {
        Ok(body) => responses::html(StatusCode::OK, body),
        Err(error) => {
            tracing::error!(template, %error, "failed to render template");
            render_error_page(
                state,
                StatusCode::INTERNAL_SERVER_ERROR,
                "something went wrong while rendering this page",
            )
        }
    }
}

/// Renders the shared error page. Deliberately does not call [`render_html`] (which would call
/// this on failure) to avoid recursion; if even the built-in error template fails to render, this
/// falls back to a plain-text body instead.
fn render_error_page(state: &ServerState, status: StatusCode, message: &str) -> Response {
    let context = ErrorContext {
        title: "Error",
        csrf_token: state.csrf.value(),
        message,
    };
    match state.templates.render("error.html", context) {
        Ok(body) => responses::html(status, body),
        Err(_) => responses::plain(status, "something went wrong"),
    }
}

fn storage_error_page(state: &ServerState, context_message: &str, error: &StorageError) -> Response {
    tracing::error!(context = context_message, %error, "storage operation failed");
    match error {
        error if error.kind() == StorageErrorKind::NotFound => render_error_page(state, StatusCode::NOT_FOUND, "not found"),
        _ => render_error_page(state, StatusCode::INTERNAL_SERVER_ERROR, "something went wrong"),
    }
}

fn storage_error_json(context_message: &str, error: &StorageError) -> Response {
    tracing::error!(context = context_message, %error, "storage operation failed");
    let status = match error {
        error if error.kind() == StorageErrorKind::NotFound => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    responses::json(
        status,
        &serde_json::json!({ "error": status.canonical_reason().unwrap_or("error") }),
    )
}

async fn find_repository(state: &ServerState, repository_id: RepositoryId) -> Result<Option<Repository>, StorageError> {
    let repositories = state.storage.list_repositories(false).await?;
    Ok(repositories.into_iter().find(|repository| repository.id == repository_id))
}

async fn find_pull_request(state: &ServerState, repository_id: RepositoryId, number: i64) -> Result<Option<PullRequest>, StorageError> {
    let pull_requests = state.storage.list_pull_requests(repository_id).await?;
    Ok(pull_requests.into_iter().find(|pull_request| pull_request.number == number))
}

/// Loads the newest analysis for a pull request, returning `None` (rather than
/// failing the whole page) when the lookup errors, so a single bad row cannot blank the dashboard.
async fn load_analysis(state: &ServerState, pull_request: &PullRequest) -> Option<Analysis> {
    match state.storage.newest_analysis(pull_request.id).await {
        Ok(analysis) => analysis,
        Err(error) => {
            tracing::error!(
                pull_request_id = pull_request.id.0,
                %error,
                "failed to load analysis"
            );
            None
        }
    }
}

/// Assembles a dashboard/list row from a pull request and its (optional) newest analysis.
fn build_row(
    state: &ServerState,
    now_ms: i64,
    repository: &Repository,
    pull_request: &PullRequest,
    analysis: Option<&Analysis>,
) -> PullRequestRow {
    let view = AnalysisView::from_row(analysis, pull_request, repository);
    let overview = if view.error.is_some() {
        view.error.clone()
    } else {
        view.overview.clone()
    };
    let updated_age = pull_request
        .provider_updated_at
        .map_or_else(|| "unknown".to_owned(), |updated_at| humanize_age_millis(now_ms, updated_at));
    let updated_iso = pull_request.provider_updated_at.map_or_else(String::new, format_timestamp_iso);
    PullRequestRow {
        repository_id: repository.id.0,
        provider: repository.provider.clone(),
        repository: format!("{}/{}", repository.owner, repository.name),
        number: pull_request.number,
        title: pull_request.title.clone(),
        author: pull_request.author.clone(),
        state: pull_request_state_str(pull_request.state),
        is_draft: pull_request.details.as_ref().is_some_and(|details| details.is_draft),
        detail_path: format!("/repositories/{}/pull-requests/{}", repository.id.0, pull_request.number),
        updated_age,
        updated_iso,
        updated_at: pull_request.provider_updated_at,
        stale: is_stale(&state.config, now_ms, pull_request.refreshed_at),
        priority: view.priority,
        priority_label: priority_label(view.priority),
        rationale: view.rationale,
        overview,
        analysis_status: view.status,
        has_error: view.error.is_some(),
    }
}

/// Whether a row's priority satisfies the selected priority filter. Selecting `high` also matches
/// `critical`, so the dashboard's four-way taxonomy still surfaces the most severe findings.
fn priority_matches(selected: &str, row_priority: &str) -> bool {
    match selected {
        "high" => row_priority == "high" || row_priority == "critical",
        other => other == row_priority,
    }
}

fn row_matches_search(row: &PullRequestRow, needle: &str) -> bool {
    let needle = needle.to_lowercase();
    row.title.to_lowercase().contains(&needle)
        || row.repository.to_lowercase().contains(&needle)
        || row.author.as_deref().is_some_and(|author| author.to_lowercase().contains(&needle))
        || format!("#{}", row.number).contains(&needle)
}

fn query_field(query: Option<&str>, field: &str) -> Option<String> {
    query.and_then(|raw| super::security::form_field(raw.as_bytes(), field))
}

fn sort_selection(query: Option<&str>) -> SortSelection {
    let column = match query_field(query, "sort").as_deref() {
        Some("repository") => SortColumn::Repository,
        Some("pull_request") => SortColumn::PullRequest,
        Some("author") => SortColumn::Author,
        Some("state") => SortColumn::State,
        Some("priority") => SortColumn::Priority,
        Some("overview") => SortColumn::Overview,
        _ => SortColumn::Updated,
    };
    let direction = match query_field(query, "direction").as_deref() {
        Some("asc") => SortDirection::Ascending,
        Some("desc") => SortDirection::Descending,
        Some(_) | None => column.default_direction(),
    };
    SortSelection { column, direction }
}

fn sort_rows(rows: &mut [PullRequestRow], selection: SortSelection) {
    rows.sort_by(|left, right| {
        let ordering = match selection.column {
            SortColumn::Repository => (&left.provider, &left.repository).cmp(&(&right.provider, &right.repository)),
            SortColumn::PullRequest => (&left.title, left.number).cmp(&(&right.title, right.number)),
            SortColumn::Author => left.author.cmp(&right.author),
            SortColumn::Updated => compare_updated(left.updated_at, right.updated_at, selection.direction),
            SortColumn::State => left.state.cmp(right.state),
            SortColumn::Priority => priority_rank(left.priority).cmp(&priority_rank(right.priority)),
            SortColumn::Overview => left.overview.cmp(&right.overview),
        };
        let directed = if selection.column == SortColumn::Updated {
            ordering
        } else if selection.direction == SortDirection::Descending {
            ordering.reverse()
        } else {
            ordering
        };
        directed.then_with(|| left.number.cmp(&right.number))
    });
}

fn compare_updated(left: Option<i64>, right: Option<i64>, direction: SortDirection) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) if direction == SortDirection::Descending => right.cmp(&left),
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn priority_rank(priority: &str) -> u8 {
    match priority {
        "critical" => 4,
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => 0,
    }
}

fn sort_links(base_path: &str, selection: SortSelection, filters: Option<&DashboardFilters>) -> SortLinks {
    SortLinks {
        repository: sort_link(base_path, SortColumn::Repository, selection, filters),
        pull_request: sort_link(base_path, SortColumn::PullRequest, selection, filters),
        author: sort_link(base_path, SortColumn::Author, selection, filters),
        updated: sort_link(base_path, SortColumn::Updated, selection, filters),
        state: sort_link(base_path, SortColumn::State, selection, filters),
        priority: sort_link(base_path, SortColumn::Priority, selection, filters),
        overview: sort_link(base_path, SortColumn::Overview, selection, filters),
    }
}

fn sort_link(base_path: &str, column: SortColumn, selection: SortSelection, filters: Option<&DashboardFilters>) -> SortLink {
    let next_direction = if selection.column == column {
        selection.direction.reverse()
    } else {
        column.default_direction()
    };
    let mut parameters = Vec::new();
    if let Some(filters) = filters {
        if let Some(provider) = &filters.provider {
            parameters.push(("provider", provider.clone()));
        }
        if let Some(repository) = filters.repository {
            parameters.push(("repository", repository.to_string()));
        }
        if let Some(priority) = &filters.priority {
            parameters.push(("priority", priority.clone()));
        }
        if filters.draft {
            parameters.push(("draft", "on".to_owned()));
        }
        if filters.stale {
            parameters.push(("stale", "on".to_owned()));
        }
        if !filters.query.is_empty() {
            parameters.push(("q", filters.query.clone()));
        }
    }
    parameters.push(("sort", column.as_str().to_owned()));
    parameters.push(("direction", next_direction.as_str().to_owned()));
    let query = parameters
        .into_iter()
        .map(|(name, value)| format!("{name}={}", encode_query_component(&value)))
        .collect::<Vec<_>>()
        .join("&");
    let active = selection.column == column;
    SortLink {
        href: format!("{base_path}?{query}"),
        aria_sort: if active { selection.direction.aria_sort() } else { "none" },
        indicator: if active {
            match selection.direction {
                SortDirection::Ascending => "(asc)",
                SortDirection::Descending => "(desc)",
            }
        } else {
            ""
        },
    }
}

fn encode_query_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => encoded.push(char::from(byte)),
            b' ' => encoded.push('+'),
            _ => {
                encoded.push('%');
                encoded.push(char::from(HEX[usize::from(byte >> 4)]));
                encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
            }
        }
    }
    encoded
}

// --- page handlers ----------------------------------------------------------------------------

pub(super) async fn dashboard_page(state: &ServerState, query: Option<&str>) -> Response {
    let repositories = match state.storage.list_repositories(false).await {
        Ok(repositories) => repositories,
        Err(error) => return storage_error_page(state, "failed to list repositories", &error),
    };

    let now_ms = now_epoch_millis(&state.clock);
    let mut rows = Vec::new();
    let mut provider_options: Vec<String> = Vec::new();
    let mut repository_options: Vec<RepositoryOption> = Vec::new();
    for repository in &repositories {
        if !provider_options.contains(&repository.provider) {
            provider_options.push(repository.provider.clone());
        }
        repository_options.push(RepositoryOption {
            id: repository.id.0,
            label: format!("{}/{}/{}", repository.provider, repository.owner, repository.name),
        });
        let pull_requests = match state.storage.list_pull_requests(repository.id).await {
            Ok(pull_requests) => pull_requests,
            Err(error) => return storage_error_page(state, "failed to list pull requests", &error),
        };
        for pull_request in &pull_requests {
            let analysis = load_analysis(state, pull_request).await;
            rows.push(build_row(state, now_ms, repository, pull_request, analysis.as_ref()));
        }
    }
    provider_options.sort();

    let total = rows.len();
    let filter_provider = query_field(query, "provider").filter(|value| !value.is_empty());
    let filter_repository = query_field(query, "repository").and_then(|value| value.parse::<i64>().ok());
    let filter_priority = query_field(query, "priority").filter(|value| !value.is_empty());
    let filter_draft = query_field(query, "draft").is_some();
    let filter_stale = query_field(query, "stale").is_some();
    let filter_search = query_field(query, "q")
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let sort_selection = sort_selection(query);

    rows.retain(|row| {
        filter_provider.as_deref().is_none_or(|provider| row.provider == provider)
            && filter_repository.is_none_or(|id| row.repository_id == id)
            && filter_priority
                .as_deref()
                .is_none_or(|priority| priority_matches(priority, row.priority))
            && (!filter_draft || row.is_draft)
            && (!filter_stale || row.stale)
            && filter_search.as_deref().is_none_or(|needle| row_matches_search(row, needle))
    });
    let filters = DashboardFilters {
        provider: filter_provider,
        repository: filter_repository,
        priority: filter_priority,
        draft: filter_draft,
        stale: filter_stale,
        query: filter_search.unwrap_or_default(),
    };
    sort_rows(&mut rows, sort_selection);
    let sort = sort_links("/", sort_selection, Some(&filters));

    let context = DashboardContext {
        title: "PR review dashboard",
        csrf_token: state.csrf.value(),
        total,
        pull_requests: rows,
        provider_options,
        repository_options,
        priority_options: ["high", "medium", "low", "ignore"],
        filters,
        sort,
        sort_column: sort_selection.column.as_str(),
        sort_direction: sort_selection.direction.as_str(),
    };
    render_html(state, "dashboard.html", context)
}

pub(super) async fn pull_request_list_page(state: &ServerState, repository_id: RepositoryId, query: Option<&str>) -> Response {
    let repository = match find_repository(state, repository_id).await {
        Ok(Some(repository)) => repository,
        Ok(None) => return render_error_page(state, StatusCode::NOT_FOUND, "repository not found"),
        Err(error) => return storage_error_page(state, "failed to load repository", &error),
    };
    let pull_requests = match state.storage.list_pull_requests(repository_id).await {
        Ok(pull_requests) => pull_requests,
        Err(error) => return storage_error_page(state, "failed to list pull requests", &error),
    };

    let now_ms = now_epoch_millis(&state.clock);
    let mut rows = Vec::with_capacity(pull_requests.len());
    for pull_request in &pull_requests {
        let analysis = load_analysis(state, pull_request).await;
        rows.push(build_row(state, now_ms, &repository, pull_request, analysis.as_ref()));
    }
    let sort_selection = sort_selection(query);
    sort_rows(&mut rows, sort_selection);
    let base_path = format!("/repositories/{}/pull-requests", repository_id.0);

    let context = PullRequestListContext {
        title: "Pull requests",
        csrf_token: state.csrf.value(),
        repository_id: repository_id.0,
        pull_requests: rows,
        sort: sort_links(&base_path, sort_selection, None),
    };
    render_html(state, "pull_request_list.html", context)
}

pub(super) async fn pull_request_detail_page(state: &ServerState, repository_id: RepositoryId, number: i64) -> Response {
    let repository = match find_repository(state, repository_id).await {
        Ok(Some(repository)) => repository,
        Ok(None) => return render_error_page(state, StatusCode::NOT_FOUND, "repository not found"),
        Err(error) => return storage_error_page(state, "failed to load repository", &error),
    };
    let pull_request = match find_pull_request(state, repository_id, number).await {
        Ok(Some(pull_request)) => pull_request,
        Ok(None) => return render_error_page(state, StatusCode::NOT_FOUND, "pull request not found"),
        Err(error) => return storage_error_page(state, "failed to load pull request", &error),
    };

    let analysis = load_analysis(state, &pull_request).await;
    let now_ms = now_epoch_millis(&state.clock);
    let external_link = validate_external_https_link(&pull_request.web_url).map(str::to_owned);
    let updated_age = pull_request
        .provider_updated_at
        .map_or_else(|| "unknown".to_owned(), |updated_at| humanize_age_millis(now_ms, updated_at));
    let updated_iso = pull_request.provider_updated_at.map_or_else(String::new, format_timestamp_iso);
    let context = PullRequestDetailContext {
        title: format!("Pull request #{}", pull_request.number),
        csrf_token: state.csrf.value(),
        repository_id: repository_id.0,
        provider: repository.provider.clone(),
        pull_request: PullRequestDetailView::from(&pull_request),
        external_link,
        stale: is_stale(&state.config, now_ms, pull_request.refreshed_at),
        updated_age,
        updated_iso,
        analysis: DetailAnalysisContext::from(AnalysisView::from_row(analysis.as_ref(), &pull_request, &repository)),
    };
    render_html(state, "pull_request_detail.html", context)
}

pub(super) async fn settings_page(state: &ServerState, query: Option<&str>) -> Response {
    let running_config = state.config.as_ref().clone();
    let display_config = match running_config.config_path.clone() {
        Some(path) => match tokio::task::spawn_blocking(move || {
            if path.exists() {
                AppConfig::load(&path)
            } else {
                Ok(running_config)
            }
        })
        .await
        {
            Ok(Ok(config)) => config,
            Ok(Err(error)) => {
                tracing::error!(%error, "failed to reload settings from disk");
                return render_error_page(
                    state,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "The configuration file could not be read.",
                );
            }
            Err(error) => {
                tracing::error!(%error, "settings reload task failed");
                return render_error_page(
                    state,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "The configuration file could not be read.",
                );
            }
        },
        None => running_config,
    };
    let report = state.prerequisites.as_ref();
    let tools = vec![
        tool_row("GitHub CLI (gh)", &report.gh_cli),
        tool_row("Azure CLI (az)", &report.az_cli),
        tool_row("Copilot CLI", &report.copilot_cli),
    ];
    let prompts = vec![
        SettingsPromptRow {
            name: "overview",
            prompt: display_config.prompts.overview.clone(),
        },
        SettingsPromptRow {
            name: "interesting",
            prompt: display_config.prompts.interesting.clone(),
        },
    ];
    let actions = vec![action_row("review", &display_config.review_action, &report.review_action)];

    let mut repositories = Vec::new();
    for repository in &display_config.github_repositories {
        repositories.push(SettingsRepositoryRow {
            provider: "github",
            url: repository.url.clone(),
        });
    }
    for repository in &display_config.azure_devops_repositories {
        repositories.push(SettingsRepositoryRow {
            provider: "azure_devops",
            url: repository.url.clone(),
        });
    }

    let context = SettingsContext {
        title: "Settings",
        csrf_token: state.csrf.value(),
        configuration_source: display_config.config_path.as_ref().map_or_else(
            || "Built-in defaults (no configuration file loaded)".to_owned(),
            |path| path.display().to_string(),
        ),
        configuration_loaded: state.config.config_file_loaded,
        notice: query_field(query, "saved")
            .is_some_and(|value| value == "1")
            .then_some("Settings saved. Restart the dashboard to apply the updated configuration."),
        editable: display_config.config_path.is_some(),
        overview_prompt: display_config.prompts.overview.clone(),
        interesting_prompt: display_config.prompts.interesting,
        tools,
        prompts,
        actions,
        repositories,
    };
    render_html(state, "settings.html", context)
}

pub(super) async fn update_settings(state: &ServerState, body: &[u8]) -> Response {
    let update = match parse_config_update(body) {
        Ok(update) => update,
        Err(message) => return render_error_page(state, StatusCode::BAD_REQUEST, message),
    };
    let update_name = config_update_name(&update);
    let config_path = state
        .config
        .config_path
        .as_ref()
        .map_or_else(|| "unavailable path".to_owned(), |path| path.display().to_string());
    tracing::info!(update = update_name, path = %config_path, "saving settings");
    let _update_guard = state.config_update_lock.lock().await;
    let config = state.config.as_ref().clone();
    match tokio::task::spawn_blocking(move || config.persist_update(update)).await {
        Ok(Ok(())) => {
            tracing::info!(update = update_name, "settings saved; restart required to apply them");
            responses::see_other("/settings?saved=1")
        }
        Ok(Err(ConfigUpdateError::Invalid(error))) => {
            tracing::warn!(%error, "settings update rejected");
            render_error_page(state, StatusCode::BAD_REQUEST, &error.to_string())
        }
        Ok(Err(ConfigUpdateError::Persistence(error))) => {
            tracing::error!(%error, "failed to persist settings update");
            render_error_page(
                state,
                StatusCode::INTERNAL_SERVER_ERROR,
                "The configuration file could not be updated.",
            )
        }
        Err(error) => {
            tracing::error!(%error, "settings update task failed");
            render_error_page(
                state,
                StatusCode::INTERNAL_SERVER_ERROR,
                "The configuration file could not be updated.",
            )
        }
    }
}

fn config_update_name(update: &ConfigUpdate) -> &'static str {
    match update {
        ConfigUpdate::Prompts { .. } => "analysis prompt",
        ConfigUpdate::AddRepository { .. } => "repository addition",
        ConfigUpdate::RemoveRepository { .. } => "repository removal",
    }
}

fn parse_config_update(body: &[u8]) -> Result<ConfigUpdate, &'static str> {
    let operation = required_form_field(body, "operation")?;
    match operation.as_str() {
        "update-prompts" => Ok(ConfigUpdate::Prompts {
            overview: required_form_field(body, "overview")?,
            interesting: required_form_field(body, "interesting")?,
        }),
        "add-repository" => Ok(ConfigUpdate::AddRepository {
            url: required_form_field(body, "url")?,
            checkout_path: optional_form_field(body, "checkout_path"),
        }),
        "remove-repository" => Ok(ConfigUpdate::RemoveRepository {
            url: required_form_field(body, "url")?,
        }),
        _ => Err("Unknown settings operation."),
    }
}

fn required_form_field(body: &[u8], name: &str) -> Result<String, &'static str> {
    super::security::form_field(body, name)
        .filter(|value| !value.trim().is_empty())
        .ok_or("A required settings field is missing.")
}

fn optional_form_field(body: &[u8], name: &str) -> Option<String> {
    super::security::form_field(body, name).and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    })
}

fn availability_status(availability: &Availability) -> (&'static str, &'static str, Option<String>) {
    match availability {
        Availability::Available => ("available", "Available", None),
        Availability::Unavailable(reason) => ("unavailable", "Unavailable", Some(reason.clone())),
        Availability::NotRequired => ("not_required", "Not required", None),
    }
}

fn tool_row(name: &'static str, availability: &Availability) -> SettingsToolRow {
    let (status, status_label, message) = availability_status(availability);
    SettingsToolRow {
        name,
        status,
        status_label,
        message,
    }
}

fn action_row(name: &'static str, mapping: &ActionMapping, availability: &Availability) -> SettingsActionRow {
    let (status, status_label, message) = availability_status(availability);
    SettingsActionRow {
        name,
        marketplace: if mapping.marketplace.is_empty() {
            "Any enabled source".to_owned()
        } else {
            mapping.marketplace.clone()
        },
        plugin: if mapping.plugin.is_empty() {
            "Any enabled source".to_owned()
        } else {
            mapping.plugin.clone()
        },
        skill: mapping.skill.clone(),
        status,
        status_label,
        message,
    }
}

// --- JSON API handlers ------------------------------------------------------------------------

pub(super) async fn api_repository_list(state: &ServerState) -> Response {
    match state.storage.list_repositories(false).await {
        Ok(repositories) => responses::json(
            StatusCode::OK,
            &repositories.iter().map(RepositorySummary::from).collect::<Vec<_>>(),
        ),
        Err(error) => storage_error_json("failed to list repositories", &error),
    }
}

pub(super) async fn api_pull_request_list(state: &ServerState, repository_id: RepositoryId) -> Response {
    match state.storage.list_pull_requests(repository_id).await {
        Ok(pull_requests) => responses::json(
            StatusCode::OK,
            &pull_requests.iter().map(PullRequestSummary::from).collect::<Vec<_>>(),
        ),
        Err(error) => storage_error_json("failed to list pull requests", &error),
    }
}

/// Queues a fresh in-process analysis and returns immediately. All three action
/// buttons recompute the same complete overview/interest/review document.
pub(super) async fn enqueue_analysis(state: &ServerState, repository_id: RepositoryId, number: i64, wants_html: bool) -> Response {
    let repository = match find_repository(state, repository_id).await {
        Ok(Some(repository)) => repository,
        Ok(None) => return action_error(state, wants_html, StatusCode::NOT_FOUND, "repository not found"),
        Err(error) => return action_storage_error(state, wants_html, "failed to load repository", &error),
    };
    let pull_request = match find_pull_request(state, repository_id, number).await {
        Ok(Some(pull_request)) => pull_request,
        Ok(None) => return action_error(state, wants_html, StatusCode::NOT_FOUND, "pull request not found"),
        Err(error) => return action_storage_error(state, wants_html, "failed to load pull request", &error),
    };

    match state.copilot.enqueue_analysis(&repository, &pull_request).await {
        Ok(queued) => {
            if wants_html {
                return responses::see_other(&format!("/repositories/{}/pull-requests/{number}", repository_id.0));
            }
            let (status, queue_status) = if queued {
                (StatusCode::ACCEPTED, "queued")
            } else {
                (StatusCode::OK, "already_queued")
            };
            responses::json(status, &serde_json::json!({ "status": queue_status }))
        }
        Err(error) => action_copilot_error(state, wants_html, &error),
    }
}

fn action_error(state: &ServerState, wants_html: bool, status: StatusCode, message: &'static str) -> Response {
    if wants_html {
        return render_error_page(state, status, message);
    }
    responses::json(status, &serde_json::json!({ "error": message }))
}

fn action_storage_error(state: &ServerState, wants_html: bool, context_message: &str, error: &StorageError) -> Response {
    if wants_html {
        return storage_error_page(state, context_message, error);
    }
    storage_error_json(context_message, error)
}

fn action_copilot_error(state: &ServerState, wants_html: bool, error: &CopilotError) -> Response {
    tracing::error!(%error, "failed to queue analysis");
    action_error(state, wants_html, StatusCode::SERVICE_UNAVAILABLE, "analysis runner is unavailable")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(number: i64, updated_at: Option<i64>) -> PullRequestRow {
        let (repository, title, author, state, priority, overview) = match number {
            1 => ("alpha", "Alpha", "Ada", "closed", "high", "Alpha"),
            2 => ("zeta", "Zeta", "Zoe", "open", "low", "Zeta"),
            _ => ("middle", "Middle", "Grace", "open", "medium", "Middle"),
        };
        PullRequestRow {
            repository_id: 1,
            provider: "github".to_owned(),
            repository: repository.to_owned(),
            number,
            title: title.to_owned(),
            author: Some(author.to_owned()),
            state,
            is_draft: false,
            detail_path: String::new(),
            updated_age: String::new(),
            updated_iso: String::new(),
            updated_at,
            stale: false,
            priority,
            priority_label: String::new(),
            rationale: None,
            overview: Some(overview.to_owned()),
            analysis_status: "current",
            has_error: false,
        }
    }

    #[test]
    fn defaults_to_most_recent_provider_update_with_unknown_times_last() {
        let mut rows = vec![row(1, Some(20)), row(2, None), row(3, Some(10))];
        let selection = sort_selection(None);

        sort_rows(&mut rows, selection);

        assert_eq!(selection.column, SortColumn::Updated);
        assert_eq!(selection.direction, SortDirection::Descending);
        assert_eq!(rows.iter().map(|row| row.number).collect::<Vec<_>>(), [1, 3, 2]);
    }

    #[test]
    fn every_dashboard_column_is_sortable() {
        let first = row(2, Some(10));
        let second = row(1, Some(20));
        for query in [
            "sort=repository&direction=asc",
            "sort=pull_request&direction=asc",
            "sort=author&direction=asc",
            "sort=updated&direction=desc",
            "sort=state&direction=asc",
            "sort=priority&direction=desc",
            "sort=overview&direction=asc",
        ] {
            let mut rows = vec![first.clone(), second.clone()];
            sort_rows(&mut rows, sort_selection(Some(query)));
            assert_eq!(rows[0].number, 1, "unexpected order for `{query}`");
        }
    }

    #[test]
    fn sort_links_toggle_direction_and_preserve_filters() {
        let filters = DashboardFilters {
            provider: Some("github".to_owned()),
            repository: Some(7),
            priority: Some("high".to_owned()),
            draft: true,
            stale: false,
            query: "parser & api".to_owned(),
        };
        let links = sort_links(
            "/",
            SortSelection {
                column: SortColumn::Updated,
                direction: SortDirection::Descending,
            },
            Some(&filters),
        );

        assert!(links.updated.href.contains("direction=asc"));
        assert!(links.updated.href.contains("q=parser+%26+api"));
        assert!(links.updated.href.contains("provider=github"));
        assert!(links.updated.href.contains("draft=on"));
        assert_eq!(links.updated.aria_sort, "descending");
        assert!(links.repository.href.contains("direction=asc"));
    }
}
