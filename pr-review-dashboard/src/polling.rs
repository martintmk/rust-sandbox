// Licensed under the MIT License.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;

use futures_util::StreamExt;
use providers::{
    ListOutcome, ProviderError, ProviderKind, ProviderRegistry, PullRequestDetail, PullRequestSource, PullRequestState as ProviderState,
    RepositoryCoordinate, ReviewDecision,
};
use sha2::{Digest, Sha256};
use storage::{PullRequestDetails, PullRequestSnapshot, PullRequestState, Repository, RepositoryConfig, Storage};
use tick::{Clock, PeriodicTimer};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;

use crate::config::{ActionMapping, AppConfig};
use crate::shutdown::ShutdownListener;

trait ProviderSources: fmt::Debug + Send + Sync {
    fn source(&self, provider: ProviderKind) -> Result<Box<dyn PullRequestSource>, ProviderError>;
}

impl ProviderSources for ProviderRegistry {
    fn source(&self, provider: ProviderKind) -> Result<Box<dyn PullRequestSource>, ProviderError> {
        Self::source(self, provider)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct RepositoryKey {
    provider: ProviderKind,
    coordinate: RepositoryCoordinate,
}

#[derive(Clone, Debug)]
struct PollTarget {
    key: RepositoryKey,
    config: RepositoryConfig,
}

struct PollSchedulerInner {
    config: AppConfig,
    clock: Clock,
    storage: Storage,
    providers: Arc<dyn ProviderSources>,
    concurrency: Arc<Semaphore>,
    global_refresh: Mutex<()>,
    repository_refreshes: Mutex<HashMap<RepositoryKey, Arc<Mutex<()>>>>,
}

#[derive(Clone)]
pub struct PollScheduler {
    inner: Arc<PollSchedulerInner>,
}

impl PollScheduler {
    pub(crate) fn new<D>(dependencies: &D) -> Self
    where
        D: AsRef<AppConfig> + AsRef<Clock> + AsRef<Storage> + AsRef<ProviderRegistry>,
    {
        let config = AsRef::<AppConfig>::as_ref(dependencies).clone();
        Self {
            inner: Arc::new(PollSchedulerInner {
                concurrency: Arc::new(Semaphore::new(config.max_concurrent_polls.get())),
                config,
                clock: AsRef::<Clock>::as_ref(dependencies).clone(),
                storage: AsRef::<Storage>::as_ref(dependencies).clone(),
                providers: Arc::new(AsRef::<ProviderRegistry>::as_ref(dependencies).clone()),
                global_refresh: Mutex::new(()),
                repository_refreshes: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub(crate) async fn run(self, shutdown: ShutdownListener) {
        tracing::info!(
            interval_seconds = self.inner.config.poll_interval.as_secs(),
            "repository poller started; refreshing immediately"
        );
        self.refresh_all(shutdown.clone()).await;

        let mut timer = PeriodicTimer::new(&self.inner.clock, self.inner.config.poll_interval);
        loop {
            tokio::select! {
                () = shutdown.clone().cancelled() => break,
                tick = timer.next() => {
                    if tick.is_none() {
                        break;
                    }
                    self.refresh_all(shutdown.clone()).await;
                }
            }
            tracing::info!("repository poller stopped");
        }
    }

    async fn refresh_all(&self, shutdown: ShutdownListener) {
        let Ok(_global_refresh) = self.inner.global_refresh.try_lock() else {
            tracing::info!("skipped repository refresh because another refresh is still running");
            return;
        };
        let started_at = now_millis(&self.inner.clock);
        let targets = configured_targets(&self.inner.config);
        tracing::info!(repository_count = targets.len(), "repository refresh cycle started");
        let repositories = match self
            .inner
            .storage
            .reconcile_repositories(targets.iter().map(|target| target.config.clone()).collect(), started_at)
            .await
        {
            Ok(repositories) => repositories,
            Err(error) => {
                tracing::warn!(%error, "failed to reconcile configured repositories");
                return;
            }
        };

        let mut tasks = JoinSet::new();
        for target in targets {
            let Some(repository) = find_repository(&repositories, &target.config) else {
                tracing::warn!(
                    owner = %target.config.owner,
                    repository = %target.config.name,
                    "configured repository was not returned by storage"
                );
                continue;
            };
            let repository = repository.clone();
            let scheduler = self.clone();
            let task_shutdown = shutdown.clone();
            tasks.spawn(async move {
                scheduler.refresh_repository(repository, target, task_shutdown).await;
            });
        }

        while let Some(result) = tasks.join_next().await {
            if let Err(error) = result {
                tracing::warn!(%error, "repository polling task failed");
            }
        }
        tracing::info!(
            elapsed_ms = now_millis(&self.inner.clock).saturating_sub(started_at),
            "repository refresh cycle finished"
        );
    }

    async fn refresh_repository(&self, repository: Repository, target: PollTarget, shutdown: ShutdownListener) {
        let permit = tokio::select! {
            () = shutdown.clone().cancelled() => return,
            result = Arc::clone(&self.inner.concurrency).acquire_owned() => {
                match result {
                    Ok(permit) => permit,
                    Err(_) => return,
                }
            }
        };
        let repository_guard = {
            let mut guards = self.inner.repository_refreshes.lock().await;
            Arc::clone(guards.entry(target.key.clone()).or_insert_with(|| Arc::new(Mutex::new(()))))
        };
        let Ok(_repository_refresh) = repository_guard.try_lock_owned() else {
            return;
        };
        let _permit = permit;
        let refresh_started_at = now_millis(&self.inner.clock);
        tracing::info!(
            provider = %target.key.provider,
            owner = %repository.owner,
            repository = %repository.name,
            "repository refresh started"
        );

        let source = match self.inner.providers.source(target.key.provider) {
            Ok(source) => source,
            Err(error) => {
                tracing::warn!(
                    owner = %repository.owner,
                    repository = %repository.name,
                    %error,
                    "failed to initialize provider"
                );
                return;
            }
        };

        let snapshots = match load_snapshots(source.as_ref(), &target.key, shutdown).await {
            Ok(snapshots) => snapshots,
            Err(diagnostic) => {
                tracing::warn!(
                    owner = %repository.owner,
                    repository = %repository.name,
                    %diagnostic,
                    "provider refresh failed"
                );
                return;
            }
        };

        let open_pull_requests = snapshots.len();
        let completed_at = now_millis(&self.inner.clock);
        match self
            .inner
            .storage
            .reconcile_pull_requests(repository.id, snapshots, completed_at)
            .await
        {
            Ok(_) => tracing::info!(
                owner = %repository.owner,
                repository = %repository.name,
                open_pull_requests,
                elapsed_ms = completed_at.saturating_sub(refresh_started_at),
                "repository refresh completed"
            ),
            Err(error) => {
                tracing::warn!(
                    owner = %repository.owner,
                    repository = %repository.name,
                    %error,
                    "failed to persist refreshed pull requests"
                );
            }
        }
    }

    #[cfg(test)]
    fn with_sources(config: AppConfig, clock: Clock, storage: Storage, providers: Arc<dyn ProviderSources>) -> Self {
        Self {
            inner: Arc::new(PollSchedulerInner {
                concurrency: Arc::new(Semaphore::new(config.max_concurrent_polls.get())),
                config,
                clock,
                storage,
                providers,
                global_refresh: Mutex::new(()),
                repository_refreshes: Mutex::new(HashMap::new()),
            }),
        }
    }
}

impl fmt::Debug for PollScheduler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PollScheduler")
            .field("poll_interval", &self.inner.config.poll_interval)
            .field("max_concurrency", &self.inner.config.max_concurrent_polls)
            .finish_non_exhaustive()
    }
}

fn configured_targets(config: &AppConfig) -> Vec<PollTarget> {
    let action_configuration_fingerprint = action_configuration_fingerprint(config);
    config
        .github_repositories
        .iter()
        .map(|repository| PollTarget {
            key: RepositoryKey {
                provider: ProviderKind::GitHub,
                coordinate: RepositoryCoordinate::github(repository.owner.clone(), repository.name.clone()),
            },
            config: RepositoryConfig {
                provider: ProviderKind::GitHub.as_str().to_owned(),
                owner: repository.owner.clone(),
                name: repository.name.clone(),
                remote_id: None,
                action_configuration_fingerprint: action_configuration_fingerprint.clone(),
            },
        })
        .chain(config.azure_devops_repositories.iter().map(|repository| PollTarget {
            key: RepositoryKey {
                provider: ProviderKind::AzureDevOps,
                coordinate: RepositoryCoordinate::azure_devops(
                    repository.organization.clone(),
                    repository.project.clone(),
                    repository.repository.clone(),
                ),
            },
            config: RepositoryConfig {
                provider: ProviderKind::AzureDevOps.as_str().to_owned(),
                owner: repository.organization.clone(),
                name: repository.repository.clone(),
                remote_id: Some(repository.project.clone()),
                action_configuration_fingerprint: action_configuration_fingerprint.clone(),
            },
        }))
        .collect()
}

fn find_repository<'a>(repositories: &'a [Repository], config: &RepositoryConfig) -> Option<&'a Repository> {
    repositories.iter().find(|repository| {
        repository.provider == config.provider
            && repository.owner == config.owner
            && repository.name == config.name
            && repository.remote_id == config.remote_id
    })
}

fn validate_identity(summary: &providers::PullRequestSummary, target: &RepositoryKey) -> Result<(), String> {
    if summary.provider != target.provider {
        return Err(format!(
            "provider returned pull request {} from an unexpected provider",
            summary.number.0
        ));
    }
    if summary.repository != target.coordinate {
        return Err(format!(
            "provider returned pull request {} for an unexpected repository",
            summary.number.0
        ));
    }
    Ok(())
}

fn validate_summary(summary: &providers::PullRequestSummary, target: &RepositoryKey) -> Result<(), String> {
    validate_identity(summary, target)?;
    if summary.state != ProviderState::Open {
        return Err(format!(
            "provider open-pull-request listing returned non-open pull request {}",
            summary.number.0
        ));
    }
    Ok(())
}

fn validate_detail(detail: &PullRequestDetail, target: &RepositoryKey, expected_number: u64) -> Result<(), String> {
    validate_identity(&detail.summary, target)?;
    if detail.summary.number.0 != expected_number {
        return Err(format!(
            "provider returned pull request {} while fetching {expected_number}",
            detail.summary.number.0
        ));
    }
    Ok(())
}

fn snapshot_from_detail(detail: &PullRequestDetail) -> Result<PullRequestSnapshot, String> {
    let number = i64::try_from(detail.summary.number.0)
        .map_err(|_error| format!("pull request number {} exceeds SQLite's integer range", detail.summary.number.0))?;
    Ok(PullRequestSnapshot {
        provider_id: detail.summary.number.0.to_string(),
        number,
        title: detail.summary.title.clone(),
        author: detail
            .summary
            .author
            .as_ref()
            .and_then(|author| author.login.clone().or_else(|| author.display_name.clone())),
        web_url: detail.summary.url.clone().unwrap_or_default(),
        source_branch: detail.summary.source_branch.clone().unwrap_or_default(),
        target_branch: detail.summary.target_branch.clone().unwrap_or_default(),
        state: PullRequestState::Open,
        revision_fingerprint: revision_fingerprint(detail),
        provider_updated_at: provider_updated_at(detail)?,
        details: Some(PullRequestDetails {
            body: detail.body.clone(),
            is_draft: detail.summary.is_draft,
            mergeable: None,
            additions: None,
            deletions: None,
            changed_files: None,
        }),
    })
}

fn provider_updated_at(detail: &PullRequestDetail) -> Result<Option<i64>, String> {
    detail
        .summary
        .updated_at
        .as_deref()
        .or(detail.summary.created_at.as_deref())
        .map(|value| {
            let timestamp = OffsetDateTime::parse(value, &Rfc3339)
                .map_err(|error| format!("provider returned invalid pull request update timestamp `{value}`: {error}"))?;
            i64::try_from(timestamp.unix_timestamp_nanos().div_euclid(1_000_000))
                .map_err(|_error| format!("provider pull request update timestamp `{value}` is outside the supported range"))
        })
        .transpose()
}

async fn load_snapshots(
    source: &dyn PullRequestSource,
    target: &RepositoryKey,
    shutdown: ShutdownListener,
) -> Result<Vec<PullRequestSnapshot>, String> {
    let outcome = tokio::select! {
        () = shutdown.clone().cancelled() => return Err("poll cancelled during shutdown".to_owned()),
        result = source.list_open_pull_requests(&target.coordinate, None) => result,
    }
    .map_err(|error| error.to_string())?;
    let summaries = match outcome {
        ListOutcome::Fetched { pull_requests, .. } => pull_requests,
        ListOutcome::Unchanged => {
            return Err("provider returned an unchanged response without a conditional request".to_owned());
        }
    };

    let mut snapshots = Vec::with_capacity(summaries.len());
    for summary in summaries {
        validate_summary(&summary, target)?;
        if summary.is_draft {
            continue;
        }
        let detail = tokio::select! {
            () = shutdown.clone().cancelled() => return Err("poll cancelled during shutdown".to_owned()),
            result = source.fetch_pull_request(&target.coordinate, summary.number) => result,
        }
        .map_err(|error| error.to_string())?;
        validate_detail(&detail, target, summary.number.0)?;
        if detail.summary.state != ProviderState::Open || detail.summary.is_draft {
            continue;
        }
        snapshots.push(snapshot_from_detail(&detail)?);
    }
    Ok(snapshots)
}

fn action_configuration_fingerprint(config: &AppConfig) -> String {
    let mut fingerprint = Fingerprint::new("pr-review-dashboard/actions/v1");
    fingerprint.field("overview_prompt", &config.prompts.overview);
    fingerprint.field("interesting_prompt", &config.prompts.interesting);
    fingerprint.action("review", &config.review_action);
    fingerprint.finish()
}

fn revision_fingerprint(detail: &PullRequestDetail) -> String {
    let summary = &detail.summary;
    let mut fingerprint = Fingerprint::new("pr-review-dashboard/pull-request/v2");
    fingerprint.field("provider", summary.provider.as_str());
    fingerprint.field("repository_owner", &summary.repository.owner);
    fingerprint.optional_field("repository_project", summary.repository.project.as_deref());
    fingerprint.field("repository_name", &summary.repository.name);
    fingerprint.field("number", &summary.number.0.to_string());
    fingerprint.field("title", &summary.title);
    fingerprint.field("state", provider_state_name(summary.state));
    fingerprint.field("is_draft", if summary.is_draft { "true" } else { "false" });
    if let Some(author) = &summary.author {
        fingerprint.optional_field("author_login", author.login.as_deref());
        fingerprint.optional_field("author_display_name", author.display_name.as_deref());
    } else {
        fingerprint.optional_field("author_login", None);
        fingerprint.optional_field("author_display_name", None);
    }
    fingerprint.optional_field("source_branch", summary.source_branch.as_deref());
    fingerprint.optional_field("target_branch", summary.target_branch.as_deref());
    fingerprint.optional_field("source_commit_sha", summary.source_commit_sha.as_deref());
    fingerprint.optional_field("url", summary.url.as_deref());
    fingerprint.optional_field("created_at", summary.created_at.as_deref());
    fingerprint.optional_field("updated_at", summary.updated_at.as_deref());
    fingerprint.optional_field("body", detail.body.as_deref());

    let mut labels: Vec<&str> = detail.labels.iter().map(|label| label.name.as_str()).collect();
    labels.sort_unstable();
    for label in labels {
        fingerprint.field("label", label);
    }
    let mut reviewers = detail.reviewers.iter().collect::<Vec<_>>();
    reviewers.sort_unstable_by(|left, right| {
        (
            left.user.login.as_deref(),
            left.user.display_name.as_deref(),
            review_decision_name(left.decision),
        )
            .cmp(&(
                right.user.login.as_deref(),
                right.user.display_name.as_deref(),
                review_decision_name(right.decision),
            ))
    });
    for reviewer in reviewers {
        fingerprint.optional_field("reviewer_login", reviewer.user.login.as_deref());
        fingerprint.optional_field("reviewer_display_name", reviewer.user.display_name.as_deref());
        fingerprint.field("reviewer_decision", review_decision_name(reviewer.decision));
    }
    fingerprint.finish()
}

fn provider_state_name(state: ProviderState) -> &'static str {
    match state {
        ProviderState::Open => "open",
        ProviderState::Merged => "merged",
        ProviderState::Closed => "closed",
    }
}

fn review_decision_name(decision: ReviewDecision) -> &'static str {
    match decision {
        ReviewDecision::Approved => "approved",
        ReviewDecision::ChangesRequested => "changes_requested",
        ReviewDecision::Pending => "pending",
    }
}

struct Fingerprint(Sha256);

impl Fingerprint {
    fn new(domain: &str) -> Self {
        let mut this = Self(Sha256::new());
        this.field("domain", domain);
        this
    }

    fn action(&mut self, name: &str, action: &ActionMapping) {
        self.field("action", name);
        self.field("marketplace", &action.marketplace);
        self.field("plugin", &action.plugin);
        self.field("skill", &action.skill);
    }

    fn optional_field(&mut self, name: &str, value: Option<&str>) {
        if let Some(value) = value {
            self.field("presence", "some");
            self.field(name, value);
        } else {
            self.field("presence", "none");
            self.field(name, "");
        }
    }

    fn field(&mut self, name: &str, value: &str) {
        Self::write_component(&mut self.0, name.as_bytes());
        Self::write_component(&mut self.0, value.as_bytes());
    }

    fn write_component(hasher: &mut Sha256, value: &[u8]) {
        hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(value);
    }

    fn finish(self) -> String {
        format!("{:x}", self.0.finalize())
    }
}

fn now_millis(clock: &Clock) -> i64 {
    clock
        .system_time()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::num::NonZeroUsize;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::time::Duration;

    use tick::ClockControl;
    use tokio::sync::Notify;

    use super::*;
    use crate::config::GitHubRepository;
    use crate::copilot::{AnalysisScheduler, CopilotService};
    use crate::shutdown;
    use providers::{BoxFuture, EntityTag, Label, PullRequestNumber, PullRequestSummary, Reviewer, UserRef};

    static NEXT_DATABASE: AtomicU64 = AtomicU64::new(1);

    struct TestDatabase {
        path: PathBuf,
    }

    impl TestDatabase {
        async fn open() -> (Self, Storage) {
            let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::current_dir()
                .expect("current directory should be available")
                .join(format!("polling-test-{}-{sequence}.sqlite3", std::process::id()));
            remove_sqlite_files(&path);
            let storage = Storage::open(path.clone()).await.expect("test database should open");
            (Self { path }, storage)
        }
    }

    impl Drop for TestDatabase {
        fn drop(&mut self) {
            remove_sqlite_files(&self.path);
        }
    }

    fn remove_sqlite_files(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let candidate = PathBuf::from(format!("{}{suffix}", path.display()));
            if let Err(error) = fs::remove_file(&candidate) {
                assert_eq!(
                    error.kind(),
                    std::io::ErrorKind::NotFound,
                    "failed to remove {}: {error}",
                    candidate.display()
                );
            }
        }
    }

    #[derive(Clone, Debug)]
    enum MockMode {
        Success(Vec<PullRequestDetail>),
        Failure(String),
    }

    #[derive(Debug)]
    struct MockSource {
        mode: StdMutex<MockMode>,
        list_calls: AtomicUsize,
        fetch_calls: AtomicUsize,
        active_lists: Arc<AtomicUsize>,
        max_active_lists: Arc<AtomicUsize>,
        list_started: Notify,
        gate: Option<Arc<Semaphore>>,
    }

    impl MockSource {
        fn new(details: Vec<PullRequestDetail>) -> Arc<Self> {
            Arc::new(Self {
                mode: StdMutex::new(MockMode::Success(details)),
                list_calls: AtomicUsize::new(0),
                fetch_calls: AtomicUsize::new(0),
                active_lists: Arc::new(AtomicUsize::new(0)),
                max_active_lists: Arc::new(AtomicUsize::new(0)),
                list_started: Notify::new(),
                gate: None,
            })
        }

        fn blocked(details: Vec<PullRequestDetail>) -> (Arc<Self>, Arc<Semaphore>) {
            let gate = Arc::new(Semaphore::new(0));
            (
                Arc::new(Self {
                    mode: StdMutex::new(MockMode::Success(details)),
                    list_calls: AtomicUsize::new(0),
                    fetch_calls: AtomicUsize::new(0),
                    active_lists: Arc::new(AtomicUsize::new(0)),
                    max_active_lists: Arc::new(AtomicUsize::new(0)),
                    list_started: Notify::new(),
                    gate: Some(Arc::clone(&gate)),
                }),
                gate,
            )
        }

        fn set_success(&self, details: Vec<PullRequestDetail>) {
            *self.mode.lock().expect("mock mode mutex should not be poisoned") = MockMode::Success(details);
        }

        fn set_failure(&self, diagnostic: &str) {
            *self.mode.lock().expect("mock mode mutex should not be poisoned") = MockMode::Failure(diagnostic.to_owned());
        }

        async fn wait_for_list_calls(&self, expected: usize) {
            while self.list_calls.load(Ordering::SeqCst) < expected {
                self.list_started.notified().await;
            }
        }
    }

    #[derive(Debug)]
    struct ActiveList {
        active: Arc<AtomicUsize>,
    }

    impl Drop for ActiveList {
        fn drop(&mut self) {
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[derive(Debug)]
    struct SharedSource(Arc<MockSource>);

    impl PullRequestSource for SharedSource {
        fn list_open_pull_requests<'a>(
            &'a self,
            repository: &'a RepositoryCoordinate,
            _conditional: Option<&'a EntityTag>,
        ) -> BoxFuture<'a, Result<ListOutcome, ProviderError>> {
            let source = Arc::clone(&self.0);
            Box::pin(async move {
                source.list_calls.fetch_add(1, Ordering::SeqCst);
                let active = source.active_lists.fetch_add(1, Ordering::SeqCst) + 1;
                source.max_active_lists.fetch_max(active, Ordering::SeqCst);
                source.list_started.notify_waiters();
                let _active = ActiveList {
                    active: Arc::clone(&source.active_lists),
                };
                if let Some(gate) = &source.gate {
                    let permit = Arc::clone(gate).acquire_owned().await.expect("mock gate should stay open");
                    permit.forget();
                }
                let mode = source.mode.lock().expect("mock mode mutex should not be poisoned").clone();
                match mode {
                    MockMode::Success(details) => Ok(ListOutcome::Fetched {
                        pull_requests: details
                            .into_iter()
                            .map(|mut detail| {
                                detail.summary.repository = repository.clone();
                                detail.summary
                            })
                            .collect(),
                        etag: None,
                    }),
                    MockMode::Failure(diagnostic) => Err(ProviderError::transient(diagnostic)),
                }
            })
        }

        fn fetch_pull_request<'a>(
            &'a self,
            repository: &'a RepositoryCoordinate,
            number: PullRequestNumber,
        ) -> BoxFuture<'a, Result<PullRequestDetail, ProviderError>> {
            let source = Arc::clone(&self.0);
            Box::pin(async move {
                source.fetch_calls.fetch_add(1, Ordering::SeqCst);
                let mode = source.mode.lock().expect("mock mode mutex should not be poisoned").clone();
                match mode {
                    MockMode::Success(details) => details
                        .into_iter()
                        .find(|detail| detail.summary.number == number)
                        .map(|mut detail| {
                            detail.summary.repository = repository.clone();
                            detail
                        })
                        .ok_or_else(|| ProviderError::not_found(format!("pull request {number} was not scripted"))),
                    MockMode::Failure(diagnostic) => Err(ProviderError::transient(diagnostic)),
                }
            })
        }
    }

    #[derive(Debug)]
    struct ClosingBetweenListAndDetail {
        listed: PullRequestSummary,
        detail: PullRequestDetail,
    }

    impl PullRequestSource for ClosingBetweenListAndDetail {
        fn list_open_pull_requests<'a>(
            &'a self,
            repository: &'a RepositoryCoordinate,
            _conditional: Option<&'a EntityTag>,
        ) -> BoxFuture<'a, Result<ListOutcome, ProviderError>> {
            let mut listed = self.listed.clone();
            listed.repository = repository.clone();
            Box::pin(async move {
                Ok(ListOutcome::Fetched {
                    pull_requests: vec![listed],
                    etag: None,
                })
            })
        }

        fn fetch_pull_request<'a>(
            &'a self,
            repository: &'a RepositoryCoordinate,
            _number: PullRequestNumber,
        ) -> BoxFuture<'a, Result<PullRequestDetail, ProviderError>> {
            let mut detail = self.detail.clone();
            detail.summary.repository = repository.clone();
            Box::pin(async move { Ok(detail) })
        }
    }

    #[derive(Debug)]
    struct MockRegistry {
        github: Arc<MockSource>,
        azure_devops: Option<Arc<MockSource>>,
    }

    impl ProviderSources for MockRegistry {
        fn source(&self, provider: ProviderKind) -> Result<Box<dyn PullRequestSource>, ProviderError> {
            match provider {
                ProviderKind::GitHub => Ok(Box::new(SharedSource(Arc::clone(&self.github)))),
                ProviderKind::AzureDevOps => self
                    .azure_devops
                    .as_ref()
                    .map(|source| Box::new(SharedSource(Arc::clone(source))) as Box<dyn PullRequestSource>)
                    .ok_or_else(|| ProviderError::configuration("Azure DevOps was not scripted")),
            }
        }
    }

    fn config(repository_count: usize, poll_interval: Duration, max_concurrent_polls: usize) -> AppConfig {
        AppConfig {
            poll_interval,
            max_concurrent_polls: NonZeroUsize::new(max_concurrent_polls).expect("test concurrency is nonzero"),
            github_repositories: (0..repository_count)
                .map(|index| GitHubRepository {
                    url: format!("https://github.com/octo/repo-{index}"),
                    owner: "octo".to_owned(),
                    name: format!("repo-{index}"),
                    checkout_path: None,
                })
                .collect(),
            prompts: analysis::AnalysisPrompts {
                overview: "summarize the change".to_owned(),
                interesting: "prioritize public API changes".to_owned(),
            },
            review_action: action("review"),
            ..AppConfig::default()
        }
    }

    fn action(skill: &str) -> ActionMapping {
        ActionMapping {
            marketplace: "marketplace".to_owned(),
            plugin: "plugin".to_owned(),
            skill: skill.to_owned(),
        }
    }

    fn detail(title: &str, body: &str) -> PullRequestDetail {
        PullRequestDetail {
            summary: PullRequestSummary {
                provider: ProviderKind::GitHub,
                repository: RepositoryCoordinate::github("octo", "repo-0"),
                number: PullRequestNumber(7),
                title: title.to_owned(),
                state: ProviderState::Open,
                is_draft: false,
                author: Some(UserRef {
                    login: Some("developer".to_owned()),
                    display_name: Some("Developer".to_owned()),
                }),
                source_branch: Some("feature".to_owned()),
                target_branch: Some("main".to_owned()),
                source_commit_sha: Some("sha-1".to_owned()),
                url: Some("https://github.com/octo/repo-0/pull/7".to_owned()),
                created_at: Some("2026-08-01T00:00:00Z".to_owned()),
                updated_at: Some("2026-08-02T00:00:00Z".to_owned()),
            },
            body: Some(body.to_owned()),
            labels: vec![Label { name: "rust".to_owned() }],
            reviewers: vec![Reviewer {
                user: UserRef {
                    login: Some("reviewer".to_owned()),
                    display_name: None,
                },
                decision: ReviewDecision::Approved,
            }],
        }
    }

    /// Builds an Azure DevOps-flavored [`PullRequestDetail`] for regression
    /// tests that must prove the source-head commit id is the only thing
    /// that changed between polls.
    fn ado_detail(title: &str, body: &str, source_commit_sha: &str) -> PullRequestDetail {
        PullRequestDetail {
            summary: PullRequestSummary {
                provider: ProviderKind::AzureDevOps,
                repository: RepositoryCoordinate::azure_devops("contoso", "Contoso", "widgets"),
                number: PullRequestNumber(42),
                title: title.to_owned(),
                state: ProviderState::Open,
                is_draft: false,
                author: Some(UserRef {
                    login: Some("ada@contoso.com".to_owned()),
                    display_name: Some("Ada Lovelace".to_owned()),
                }),
                source_branch: Some("feature/retry".to_owned()),
                target_branch: Some("main".to_owned()),
                source_commit_sha: Some(source_commit_sha.to_owned()),
                url: Some("https://dev.azure.com/contoso/Contoso/_git/widgets/pullrequest/42".to_owned()),
                created_at: Some("2026-08-01T00:00:00Z".to_owned()),
                updated_at: None,
            },
            body: Some(body.to_owned()),
            labels: vec![Label {
                name: "enhancement".to_owned(),
            }],
            reviewers: vec![Reviewer {
                user: UserRef {
                    login: Some("grace@contoso.com".to_owned()),
                    display_name: Some("Grace Hopper".to_owned()),
                },
                decision: ReviewDecision::Approved,
            }],
        }
    }

    fn build_scheduler(config: AppConfig, clock: Clock, storage: Storage, source: Arc<MockSource>) -> PollScheduler {
        PollScheduler::with_sources(
            config,
            clock,
            storage,
            Arc::new(MockRegistry {
                github: source,
                azure_devops: None,
            }),
        )
    }

    #[tokio::test]
    async fn refreshes_immediately_then_on_periodic_timer() {
        let (database, storage) = TestDatabase::open().await;
        let control = ClockControl::new_at(SystemTime::UNIX_EPOCH + Duration::from_secs(10)).auto_advance_timers(true);
        let clock = control.to_clock();
        let (source, gate) = MockSource::blocked(vec![detail("Initial", "Body")]);
        let scheduler = build_scheduler(config(1, Duration::from_mins(1), 1), clock, storage.clone(), Arc::clone(&source));
        let (trigger, listener) = shutdown::channel();

        let task = tokio::spawn(scheduler.clone().run(listener));
        source.wait_for_list_calls(1).await;
        assert_eq!(
            source.list_calls.load(Ordering::SeqCst),
            1,
            "startup refresh should run immediately"
        );
        gate.add_permits(1);
        source.wait_for_list_calls(2).await;

        trigger.trigger();
        task.await.expect("poll scheduler should stop cleanly");
        drop(scheduler);
        drop(storage);
        drop(database);
    }

    #[tokio::test]
    async fn reconciles_changes_and_preserves_stale_cache() {
        let (database, storage) = TestDatabase::open().await;
        let clock = ClockControl::new_at(SystemTime::UNIX_EPOCH + Duration::from_secs(10)).to_clock();
        let source = MockSource::new(vec![detail("Initial", "Body")]);
        let scheduler = build_scheduler(config(1, Duration::from_mins(1), 1), clock, storage.clone(), Arc::clone(&source));
        let (_trigger, listener) = shutdown::channel();

        scheduler.refresh_all(listener.clone()).await;
        let repository = storage
            .list_repositories(true)
            .await
            .expect("repositories should load")
            .into_iter()
            .next()
            .expect("repository should exist");
        let first_pull_request = storage
            .list_pull_requests(repository.id)
            .await
            .expect("pull requests should load")
            .into_iter()
            .next()
            .expect("pull request should exist");
        scheduler.refresh_all(listener.clone()).await;

        source.set_success(vec![detail("Changed", "Changed body")]);
        scheduler.refresh_all(listener.clone()).await;
        let changed_pull_request = storage
            .list_pull_requests(repository.id)
            .await
            .expect("pull requests should load")
            .into_iter()
            .next()
            .expect("pull request should exist");
        assert_ne!(first_pull_request.revision_fingerprint, changed_pull_request.revision_fingerprint);

        source.set_failure("provider unavailable");
        scheduler.refresh_all(listener.clone()).await;
        let stale_pull_request = storage
            .list_pull_requests(repository.id)
            .await
            .expect("stale pull requests should remain cached")
            .into_iter()
            .next()
            .expect("cached pull request should remain");
        assert_eq!(stale_pull_request.state, PullRequestState::Open);
        assert_eq!(stale_pull_request.refreshed_at, changed_pull_request.refreshed_at);
        source.set_success(Vec::new());
        scheduler.refresh_all(listener).await;
        let closed_pull_request = storage
            .list_pull_requests(repository.id)
            .await
            .expect("pull requests should load")
            .into_iter()
            .next()
            .expect("closed pull request should remain");
        assert_eq!(closed_pull_request.state, PullRequestState::Closed);

        drop(scheduler);
        drop(storage);
        drop(database);
    }

    #[tokio::test]
    async fn initial_poll_is_fetch_only_then_background_scan_analyzes_and_renders() {
        let (database, storage) = TestDatabase::open().await;
        let clock = ClockControl::new_at(SystemTime::UNIX_EPOCH + Duration::from_secs(10)).to_clock();
        let source = MockSource::new(vec![detail("Integrated title", "Integrated body")]);
        let app_config = config(1, Duration::from_mins(1), 1);
        let copilot = CopilotService::with_example_backend(app_config.clone(), clock.clone(), storage.clone());
        let scheduler = PollScheduler::with_sources(
            app_config.clone(),
            clock.clone(),
            storage.clone(),
            Arc::new(MockRegistry {
                github: source,
                azure_devops: None,
            }),
        );
        let (_trigger, listener) = shutdown::channel();

        scheduler.refresh_all(listener).await;
        assert_eq!(
            copilot.active_analysis_count_for_test().await,
            0,
            "initial provider loading must not queue AI work"
        );
        let pending_html = crate::http_server::render_dashboard_for_test(app_config.clone(), clock.clone(), storage.clone()).await;
        assert!(pending_html.contains("AI analysis pending"));

        let analysis_scheduler = AnalysisScheduler::for_test(clock.clone(), storage.clone(), copilot.clone());
        analysis_scheduler.scan_once_for_test().await;
        assert_eq!(copilot.active_analysis_count_for_test().await, 1);
        let pull_request_id = copilot
            .process_next_analysis_for_test()
            .await
            .expect("background scan should queue one analyzable pull request");
        let repository = storage.list_repositories(true).await.expect("repositories should load").remove(0);
        assert!(
            storage
                .newest_matching_analysis(
                    pull_request_id,
                    storage
                        .list_pull_requests(repository.id)
                        .await
                        .expect("pull requests should load")
                        .remove(0)
                        .revision_fingerprint,
                    repository.action_configuration_fingerprint.clone(),
                )
                .await
                .expect("analysis lookup should work")
                .is_some()
        );

        let html = crate::http_server::render_dashboard_for_test(app_config, clock, storage.clone()).await;
        assert!(html.contains("Integrated title"));
        assert!(html.contains("Updates parser behavior."));
        assert!(html.contains("High"));

        drop(scheduler);
        drop(analysis_scheduler);
        drop(copilot);
        drop(storage);
        drop(database);
    }

    #[tokio::test]
    async fn pull_request_closed_during_poll_does_not_fail_repository_refresh() {
        let mut closed = detail("Closed while polling", "Body");
        let mut listed = closed.summary.clone();
        listed.state = ProviderState::Open;
        closed.summary.state = ProviderState::Closed;
        let source = ClosingBetweenListAndDetail { listed, detail: closed };
        let key = RepositoryKey {
            provider: ProviderKind::GitHub,
            coordinate: RepositoryCoordinate::github("octo", "repo-0"),
        };
        let (_trigger, listener) = shutdown::channel();

        let snapshots = load_snapshots(&source, &key, listener)
            .await
            .expect("a normal list/detail race should not fail the poll");
        assert!(snapshots.is_empty(), "the no-longer-open pull request should be omitted");
    }

    #[tokio::test]
    async fn draft_summary_is_skipped_before_fetching_details() {
        let mut draft = detail("Draft change", "Body");
        draft.summary.is_draft = true;
        let source = MockSource::new(vec![draft]);
        let key = RepositoryKey {
            provider: ProviderKind::GitHub,
            coordinate: RepositoryCoordinate::github("octo", "repo-0"),
        };
        let shared = SharedSource(Arc::clone(&source));
        let (_trigger, listener) = shutdown::channel();

        let snapshots = load_snapshots(&shared, &key, listener)
            .await
            .expect("draft filtering should succeed");

        assert!(snapshots.is_empty());
        assert_eq!(
            source.fetch_calls.load(Ordering::SeqCst),
            0,
            "draft summaries must not trigger detail requests"
        );
    }

    #[tokio::test]
    async fn pull_request_becoming_draft_during_poll_is_omitted() {
        let mut draft = detail("Became draft", "Body");
        let listed = draft.summary.clone();
        draft.summary.is_draft = true;
        let source = ClosingBetweenListAndDetail { listed, detail: draft };
        let key = RepositoryKey {
            provider: ProviderKind::GitHub,
            coordinate: RepositoryCoordinate::github("octo", "repo-0"),
        };
        let (_trigger, listener) = shutdown::channel();

        let snapshots = load_snapshots(&source, &key, listener)
            .await
            .expect("draft list/detail race should not fail");

        assert!(snapshots.is_empty());
    }

    #[tokio::test]
    async fn bounds_repository_concurrency_and_skips_overlapping_global_refreshes() {
        let (database, storage) = TestDatabase::open().await;
        let clock = ClockControl::new_at(SystemTime::UNIX_EPOCH + Duration::from_secs(10)).to_clock();
        let (source, gate) = MockSource::blocked(vec![detail("Initial", "Body")]);
        let scheduler = build_scheduler(config(3, Duration::from_mins(1), 2), clock, storage.clone(), Arc::clone(&source));
        let (_trigger, listener) = shutdown::channel();

        let first_scheduler = scheduler.clone();
        let first_listener = listener.clone();
        let first = tokio::spawn(async move { first_scheduler.refresh_all(first_listener).await });
        source.wait_for_list_calls(2).await;
        let second_scheduler = scheduler.clone();
        let second = tokio::spawn(async move { second_scheduler.refresh_all(listener).await });
        second.await.expect("overlapping refresh should return");

        assert_eq!(source.max_active_lists.load(Ordering::SeqCst), 2);
        gate.add_permits(2);
        source.wait_for_list_calls(3).await;
        gate.add_permits(1);
        first.await.expect("first refresh should complete");
        assert_eq!(source.list_calls.load(Ordering::SeqCst), 3);

        drop(scheduler);
        drop(storage);
        drop(database);
    }

    #[test]
    fn revision_fingerprint_is_stable_and_order_independent() {
        let first = detail("Title", "Body");
        let mut reordered = first.clone();
        reordered.labels.insert(0, Label { name: "api".to_owned() });
        let mut equivalent = reordered.clone();
        equivalent.labels.reverse();
        equivalent.reviewers.reverse();

        assert_eq!(revision_fingerprint(&reordered), revision_fingerprint(&equivalent));
        assert_ne!(revision_fingerprint(&first), revision_fingerprint(&reordered));
        assert_eq!(revision_fingerprint(&first).len(), 64);
    }

    /// An Azure DevOps push updates `lastMergeSourceCommit` while leaving
    /// title, branches, labels, reviewers, and timestamps untouched (Azure
    /// DevOps does not bump `updatedDate` the way GitHub does). The
    /// fingerprint must still change so the new commit is analyzed.
    #[test]
    fn revision_fingerprint_changes_when_only_ado_source_commit_changes() {
        let before = ado_detail("Add retry policy", "Body", "abc123sourcecommit");
        let mut after = before.clone();
        after.summary.source_commit_sha = Some("def456sourcecommit".to_owned());

        assert_ne!(
            revision_fingerprint(&before),
            revision_fingerprint(&after),
            "a changed source commit with otherwise identical metadata must change the fingerprint"
        );
    }

    #[test]
    fn snapshot_uses_provider_update_timestamp_with_creation_fallback() {
        let github = snapshot_from_detail(&detail("Title", "Body")).expect("GitHub snapshot should build");
        let azure = snapshot_from_detail(&ado_detail("Title", "Body", "commit")).expect("Azure snapshot should build");
        let github_expected = OffsetDateTime::parse("2026-08-02T00:00:00Z", &Rfc3339)
            .expect("timestamp should parse")
            .unix_timestamp_nanos()
            .div_euclid(1_000_000);
        let azure_expected = OffsetDateTime::parse("2026-08-01T00:00:00Z", &Rfc3339)
            .expect("timestamp should parse")
            .unix_timestamp_nanos()
            .div_euclid(1_000_000);

        assert_eq!(github.provider_updated_at.map(i128::from), Some(github_expected));
        assert_eq!(azure.provider_updated_at.map(i128::from), Some(azure_expected));
    }

    #[test]
    fn analysis_configuration_fingerprint_tracks_prompts_and_review_skill() {
        let original = config(1, Duration::from_mins(1), 1);
        let original_fingerprint = action_configuration_fingerprint(&original);
        let mut prompt_changed = original.clone();
        prompt_changed.prompts.interesting = "prioritize security changes".to_owned();
        let mut review_changed = original;
        review_changed.review_action.skill = "another-review".to_owned();

        assert_ne!(original_fingerprint, action_configuration_fingerprint(&prompt_changed));
        assert_ne!(original_fingerprint, action_configuration_fingerprint(&review_changed));
    }

    #[tokio::test]
    async fn ado_pull_request_with_new_source_commit_updates_stored_revision() {
        let (database, storage) = TestDatabase::open().await;
        let clock = ClockControl::new_at(SystemTime::UNIX_EPOCH + Duration::from_secs(10)).to_clock();
        let source = MockSource::new(vec![ado_detail("Add retry policy", "Body", "abc123sourcecommit")]);
        let mut app_config = config(0, Duration::from_mins(1), 1);
        app_config.azure_devops_repositories.push(crate::config::AzureDevOpsRepository {
            url: "https://dev.azure.com/contoso/Contoso/_git/widgets".to_owned(),
            organization: "contoso".to_owned(),
            project: "Contoso".to_owned(),
            repository: "widgets".to_owned(),
            checkout_path: None,
        });
        let scheduler = PollScheduler::with_sources(
            app_config,
            clock,
            storage.clone(),
            Arc::new(MockRegistry {
                github: MockSource::new(Vec::new()),
                azure_devops: Some(Arc::clone(&source)),
            }),
        );
        let (_trigger, listener) = shutdown::channel();

        scheduler.refresh_all(listener.clone()).await;
        let repository = storage
            .list_repositories(true)
            .await
            .expect("repositories should load")
            .into_iter()
            .next()
            .expect("the Azure DevOps repository should be registered");
        let first_pull_request = storage
            .list_pull_requests(repository.id)
            .await
            .expect("pull requests should load")
            .into_iter()
            .next()
            .expect("the Azure DevOps pull request should be cached");
        // Re-polling with byte-for-byte identical provider data keeps the same
        // stored revision.
        scheduler.refresh_all(listener.clone()).await;

        // Only the source-head commit id changes (as after a push); every
        // other summary/detail field is byte-for-byte identical.
        source.set_success(vec![ado_detail("Add retry policy", "Body", "def456sourcecommit")]);
        scheduler.refresh_all(listener).await;

        let second_pull_request = storage
            .list_pull_requests(repository.id)
            .await
            .expect("pull requests should load")
            .into_iter()
            .next()
            .expect("the Azure DevOps pull request should still be cached");
        assert_ne!(
            first_pull_request.revision_fingerprint, second_pull_request.revision_fingerprint,
            "the stored revision fingerprint must change when only the source commit changes"
        );

        drop(scheduler);
        drop(storage);
        drop(database);
    }
}
