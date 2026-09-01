// Licensed under the MIT License.

//! Transient scheduling and persistence around the `analysis` crate.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use analysis::{AnalysisError, AnalysisErrorKind, AnalysisOutput, AnalysisRequest, Analyzer, PullRequestContext, RepositoryContext};
use futures_util::StreamExt;
use storage::{AnalysisOutcome, NewAnalysis, PullRequest, PullRequestId, Repository, Storage, StorageError};
use tick::{Clock, PeriodicTimer};
use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio::task::JoinSet;

use crate::config::AppConfig;
use crate::prereqs::PrerequisiteReport;
use crate::shutdown::ShutdownListener;

const MAX_ATTEMPTS: u32 = 3;
const RETRY_BASE_MILLIS: i64 = 30_000;
const MAX_RETRY_MILLIS: i64 = 5 * 60_000;
const ANALYSIS_SCAN_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub(crate) enum CopilotError {
    Cancelled,
    Analysis(AnalysisError),
    QueueUnavailable,
    Serialization(serde_json::Error),
    Storage(StorageError),
}

impl fmt::Display for CopilotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("Copilot action was cancelled"),
            Self::Analysis(error) => write!(f, "pull request analysis failed: {error}"),
            Self::QueueUnavailable => f.write_str("the in-process Copilot analysis runner is not available"),
            Self::Serialization(error) => write!(f, "failed to serialize pull request analysis: {error}"),
            Self::Storage(error) => write!(f, "Copilot persistence failed: {error}"),
        }
    }
}

impl Error for CopilotError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Analysis(error) => Some(error),
            Self::Serialization(error) => Some(error),
            Self::Storage(error) => Some(error),
            Self::Cancelled | Self::QueueUnavailable => None,
        }
    }
}

impl CopilotError {
    fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled) || matches!(self, Self::Analysis(error) if error.kind() == AnalysisErrorKind::Cancelled)
    }

    fn is_retryable(&self) -> bool {
        match self {
            Self::Analysis(error) => matches!(error.kind(), AnalysisErrorKind::InvalidOutput | AnalysisErrorKind::Sdk),
            Self::Storage(_) => true,
            Self::Cancelled | Self::QueueUnavailable | Self::Serialization(_) => false,
        }
    }
}

impl From<AnalysisError> for CopilotError {
    fn from(error: AnalysisError) -> Self {
        Self::Analysis(error)
    }
}

impl From<StorageError> for CopilotError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct AnalysisKey {
    pull_request_id: PullRequestId,
    revision_fingerprint: String,
    action_configuration_fingerprint: String,
}

#[derive(Debug)]
struct QueuedAnalysis {
    key: AnalysisKey,
    repository: Repository,
    pull_request: PullRequest,
}

struct CopilotServiceInner {
    config: AppConfig,
    clock: Clock,
    storage: Storage,
    analyzer: Analyzer,
    concurrency: Arc<Semaphore>,
    sender: mpsc::UnboundedSender<QueuedAnalysis>,
    receiver: Mutex<Option<mpsc::UnboundedReceiver<QueuedAnalysis>>>,
    active: Mutex<HashSet<AnalysisKey>>,
}

#[derive(Clone)]
pub struct CopilotService {
    inner: Arc<CopilotServiceInner>,
}

impl CopilotService {
    pub(crate) fn new<D>(dependencies: &D) -> Self
    where
        D: AsRef<AppConfig> + AsRef<Clock> + AsRef<Storage>,
    {
        Self::with_parts(
            AsRef::<AppConfig>::as_ref(dependencies).clone(),
            AsRef::<Clock>::as_ref(dependencies).clone(),
            AsRef::<Storage>::as_ref(dependencies).clone(),
            Analyzer::new(),
        )
    }

    fn with_parts(config: AppConfig, clock: Clock, storage: Storage, analyzer: Analyzer) -> Self {
        let concurrency = Arc::new(Semaphore::new(config.max_concurrent_copilot_jobs.get()));
        let (sender, receiver) = mpsc::unbounded_channel();
        Self {
            inner: Arc::new(CopilotServiceInner {
                config,
                clock,
                storage,
                analyzer,
                concurrency,
                sender,
                receiver: Mutex::new(Some(receiver)),
                active: Mutex::new(HashSet::new()),
            }),
        }
    }

    /// Queues an analysis only in this process. Queued work and deduplication
    /// state intentionally disappear when the process exits.
    pub(crate) async fn enqueue_analysis(&self, repository: &Repository, pull_request: &PullRequest) -> Result<bool, CopilotError> {
        let key = AnalysisKey {
            pull_request_id: pull_request.id,
            revision_fingerprint: pull_request.revision_fingerprint.clone(),
            action_configuration_fingerprint: repository.action_configuration_fingerprint.clone(),
        };
        let mut active = self.inner.active.lock().await;
        if !active.insert(key.clone()) {
            return Ok(false);
        }

        let queued = QueuedAnalysis {
            key: key.clone(),
            repository: repository.clone(),
            pull_request: pull_request.clone(),
        };
        if self.inner.sender.send(queued).is_err() {
            active.remove(&key);
            return Err(CopilotError::QueueUnavailable);
        }
        tracing::info!(
            provider = %repository.provider,
            owner = %repository.owner,
            repository = %repository.name,
            pull_request = pull_request.number,
            "AI analysis queued"
        );
        Ok(true)
    }

    pub(crate) async fn run(self, shutdown: ShutdownListener) {
        tracing::info!(
            concurrency = self.inner.config.max_concurrent_copilot_jobs.get(),
            "AI analysis runner started"
        );
        let Some(mut receiver) = self.inner.receiver.lock().await.take() else {
            tracing::warn!("AI analysis runner was started more than once");
            return;
        };
        let mut tasks = JoinSet::new();

        loop {
            while let Some(result) = tasks.try_join_next() {
                observe_worker_result(result);
            }

            let permit = tokio::select! {
                () = shutdown.clone().cancelled() => break,
                result = Arc::clone(&self.inner.concurrency).acquire_owned() => {
                    match result {
                        Ok(permit) => permit,
                        Err(_) => break,
                    }
                }
            };
            let queued = tokio::select! {
                () = shutdown.clone().cancelled() => {
                    drop(permit);
                    break;
                }
                next = receiver.recv() => next,
            };
            let Some(queued) = queued else {
                drop(permit);
                break;
            };

            let service = self.clone();
            let task_shutdown = shutdown.clone();
            tasks.spawn(async move {
                let _permit = permit;
                service.process_queued_analysis(queued, task_shutdown).await;
            });
        }

        while let Some(result) = tasks.join_next().await {
            observe_worker_result(result);
        }
        if let Err(error) = self.inner.analyzer.shutdown().await {
            tracing::warn!(%error, "failed to stop pull request analyzer");
        }
        tracing::info!("AI analysis runner stopped");
    }

    async fn process_queued_analysis(&self, queued: QueuedAnalysis, shutdown: ShutdownListener) {
        let key = queued.key.clone();
        let result = self.process_analysis(&queued, shutdown).await;
        self.inner.active.lock().await.remove(&key);
        if let Err(error) = result
            && !error.is_cancelled()
        {
            tracing::warn!(
                owner = %queued.repository.owner,
                repository = %queued.repository.name,
                pull_request = queued.pull_request.number,
                %error,
                "AI analysis failed"
            );
        }
    }

    async fn process_analysis(&self, queued: &QueuedAnalysis, shutdown: ShutdownListener) -> Result<(), CopilotError> {
        let request = analysis_request(&self.inner.config, &queued.repository, &queued.pull_request);

        for attempt in 1..=MAX_ATTEMPTS {
            let started_at = self.now_millis();
            tracing::info!(
                owner = %queued.repository.owner,
                repository = %queued.repository.name,
                pull_request = queued.pull_request.number,
                attempt,
                max_attempts = MAX_ATTEMPTS,
                "AI analysis attempt started"
            );
            let (cancellation_handle, cancellation) = analysis::cancellation_pair();
            let mut analysis_future = Box::pin(self.inner.analyzer.analyze_with_cancellation(request.clone(), cancellation));
            let analysis = tokio::select! {
                result = &mut analysis_future => result,
                () = shutdown.clone().cancelled() => {
                    cancellation_handle.cancel();
                    analysis_future.await
                }
            }
            .map_err(CopilotError::from);
            let completed_at = self.now_millis();
            let result = match analysis {
                Ok(analysis) => self.persist_success(queued, analysis, completed_at).await,
                Err(error) => Err(error),
            };
            match result {
                Ok(()) => {
                    tracing::info!(
                        owner = %queued.repository.owner,
                        repository = %queued.repository.name,
                        pull_request = queued.pull_request.number,
                        elapsed_ms = completed_at.saturating_sub(started_at),
                        "AI analysis completed"
                    );
                    return Ok(());
                }
                Err(error) if error.is_cancelled() => return Err(error),
                Err(error) => {
                    self.persist_failure(queued, &error, completed_at).await;
                    if attempt == MAX_ATTEMPTS || !error.is_retryable() {
                        return Err(error);
                    }
                    let delay = u64::try_from(retry_delay(attempt)).unwrap_or(u64::MAX);
                    tracing::warn!(
                        owner = %queued.repository.owner,
                        repository = %queued.repository.name,
                        pull_request = queued.pull_request.number,
                        retry_in_seconds = delay / 1_000,
                        %error,
                        "AI analysis attempt failed; retrying"
                    );
                    tokio::select! {
                        () = shutdown.clone().cancelled() => return Err(CopilotError::Cancelled),
                        () = tokio::time::sleep(Duration::from_millis(delay)) => {}
                    }
                }
            }
        }
        unreachable!("the bounded retry loop always returns")
    }

    async fn persist_success(&self, queued: &QueuedAnalysis, analysis: AnalysisOutput, completed_at: i64) -> Result<(), CopilotError> {
        let summary = serde_json::to_string(&analysis).map_err(CopilotError::Serialization)?;
        self.inner
            .storage
            .record_analysis(NewAnalysis {
                pull_request_id: queued.pull_request.id,
                revision_fingerprint: queued.key.revision_fingerprint.clone(),
                action_configuration_fingerprint: queued.key.action_configuration_fingerprint.clone(),
                outcome: AnalysisOutcome::Succeeded,
                summary: Some(summary),
                diagnostic: None,
                completed_at,
            })
            .await?;
        Ok(())
    }

    async fn persist_failure(&self, queued: &QueuedAnalysis, error: &CopilotError, completed_at: i64) {
        let persisted = self
            .inner
            .storage
            .record_analysis(NewAnalysis {
                pull_request_id: queued.pull_request.id,
                revision_fingerprint: queued.key.revision_fingerprint.clone(),
                action_configuration_fingerprint: queued.key.action_configuration_fingerprint.clone(),
                outcome: AnalysisOutcome::Failed,
                summary: None,
                diagnostic: Some(error.to_string()),
                completed_at,
            })
            .await;
        if let Err(storage_error) = persisted {
            tracing::warn!(
                pull_request_id = queued.pull_request.id.0,
                error = %storage_error,
                "failed to persist analysis failure"
            );
        }
    }

    fn now_millis(&self) -> i64 {
        self.inner
            .clock
            .system_time()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map_or(0, |duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
    }

    #[cfg(test)]
    pub(crate) fn with_example_backend(config: AppConfig, clock: Clock, storage: Storage) -> Self {
        Self::with_parts(config, clock, storage, Analyzer::example())
    }

    #[cfg(test)]
    pub(crate) async fn process_next_analysis_for_test(&self) -> Result<PullRequestId, CopilotError> {
        let queued = {
            let mut receiver = self.inner.receiver.lock().await;
            receiver
                .as_mut()
                .ok_or(CopilotError::QueueUnavailable)?
                .recv()
                .await
                .ok_or(CopilotError::QueueUnavailable)?
        };
        let pull_request_id = queued.pull_request.id;
        let key = queued.key.clone();
        let (_trigger, listener) = crate::shutdown::channel();
        let result = self.process_analysis(&queued, listener).await;
        self.inner.active.lock().await.remove(&key);
        result?;
        Ok(pull_request_id)
    }

    #[cfg(test)]
    pub(crate) async fn active_analysis_count_for_test(&self) -> usize {
        self.inner.active.lock().await.len()
    }
}

fn observe_worker_result(result: Result<(), tokio::task::JoinError>) {
    if let Err(error) = result {
        if error.is_panic() {
            tracing::warn!(%error, "AI analysis worker panicked");
        } else {
            tracing::warn!(%error, "AI analysis worker failed");
        }
    }
}

fn retry_delay(attempts: u32) -> i64 {
    let exponent = attempts.saturating_sub(1).min(8);
    RETRY_BASE_MILLIS
        .saturating_mul(2_i64.saturating_pow(exponent))
        .min(MAX_RETRY_MILLIS)
}

fn analysis_request(config: &AppConfig, repository: &Repository, pull_request: &PullRequest) -> AnalysisRequest {
    let details = pull_request.details.as_ref();
    AnalysisRequest {
        repository: RepositoryContext {
            provider: repository.provider.clone(),
            owner: repository.owner.clone(),
            name: repository.name.clone(),
        },
        pull_request: PullRequestContext {
            number: pull_request.number,
            title: pull_request.title.clone(),
            author: pull_request.author.clone(),
            web_url: pull_request.web_url.clone(),
            source_branch: pull_request.source_branch.clone(),
            target_branch: pull_request.target_branch.clone(),
            revision_fingerprint: pull_request.revision_fingerprint.clone(),
            body: details.and_then(|value| value.body.clone()),
            is_draft: details.map(|value| value.is_draft),
            mergeable: details.and_then(|value| value.mergeable),
            additions: details.and_then(|value| value.additions),
            deletions: details.and_then(|value| value.deletions),
            changed_files: details.and_then(|value| value.changed_files),
        },
        checkout_path: checkout_path(config, repository),
        prompts: config.prompts.clone(),
        review_action: config.review_action.clone(),
    }
}

fn checkout_path(config: &AppConfig, repository: &Repository) -> Option<PathBuf> {
    match repository.provider.as_str() {
        "github" => config
            .github_repositories
            .iter()
            .find(|candidate| {
                candidate.owner.eq_ignore_ascii_case(&repository.owner) && candidate.name.eq_ignore_ascii_case(&repository.name)
            })
            .and_then(|candidate| candidate.checkout_path.clone()),
        "azure_devops" => {
            let mut candidates = config.azure_devops_repositories.iter().filter(|candidate| {
                candidate.organization.eq_ignore_ascii_case(&repository.owner)
                    && candidate.repository.eq_ignore_ascii_case(&repository.name)
            });
            let first = candidates.next()?;
            if candidates.next().is_some() {
                None
            } else {
                first.checkout_path.clone()
            }
        }
        _ => None,
    }
}

impl fmt::Debug for CopilotService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CopilotService")
            .field("available_concurrency", &self.inner.concurrency.available_permits())
            .finish_non_exhaustive()
    }
}

struct AnalysisSchedulerInner {
    clock: Clock,
    storage: Storage,
    copilot: CopilotService,
    enabled: bool,
}

/// Delayed background scan for open pull requests that have never been analyzed.
#[derive(Clone)]
pub struct AnalysisScheduler {
    inner: Arc<AnalysisSchedulerInner>,
}

impl AnalysisScheduler {
    pub(crate) fn new<D>(dependencies: &D) -> Self
    where
        D: AsRef<Clock> + AsRef<Storage> + AsRef<CopilotService> + AsRef<PrerequisiteReport>,
    {
        Self {
            inner: Arc::new(AnalysisSchedulerInner {
                clock: AsRef::<Clock>::as_ref(dependencies).clone(),
                storage: AsRef::<Storage>::as_ref(dependencies).clone(),
                copilot: AsRef::<CopilotService>::as_ref(dependencies).clone(),
                enabled: AsRef::<PrerequisiteReport>::as_ref(dependencies).analysis_available(),
            }),
        }
    }

    pub(crate) async fn run(self, shutdown: ShutdownListener) {
        if !self.inner.enabled {
            tracing::info!("automatic AI analysis scheduler disabled because the Copilot CLI is unavailable");
            return;
        }
        tracing::info!(
            first_scan_in_seconds = ANALYSIS_SCAN_INTERVAL.as_secs(),
            "automatic AI analysis scheduler started"
        );
        let mut timer = PeriodicTimer::new(&self.inner.clock, ANALYSIS_SCAN_INTERVAL);
        loop {
            tokio::select! {
                () = shutdown.clone().cancelled() => break,
                tick = timer.next() => {
                    if tick.is_none() {
                        break;
                    }
                    self.scan_once().await;
                }
            }
        }
        tracing::info!("automatic AI analysis scheduler stopped");
    }

    async fn scan_once(&self) {
        let mut queued_count = 0_usize;
        let repositories = match self.inner.storage.list_repositories(true).await {
            Ok(repositories) => repositories,
            Err(error) => {
                tracing::warn!(%error, "failed to load repositories for background analysis");
                return;
            }
        };
        for repository in repositories {
            let pull_requests = match self.inner.storage.list_pull_requests(repository.id).await {
                Ok(pull_requests) => pull_requests,
                Err(error) => {
                    tracing::warn!(
                        owner = %repository.owner,
                        repository = %repository.name,
                        %error,
                        "failed to load pull requests for background analysis"
                    );
                    continue;
                }
            };
            for pull_request in pull_requests
                .iter()
                .filter(|pull_request| pull_request.state == storage::PullRequestState::Open)
            {
                match self.inner.storage.newest_analysis(pull_request.id).await {
                    Ok(Some(analysis))
                        if analysis.outcome == AnalysisOutcome::Failed
                            && analysis.action_configuration_fingerprint != repository.action_configuration_fingerprint =>
                    {
                        match self.inner.copilot.enqueue_analysis(&repository, pull_request).await {
                            Ok(true) => queued_count += 1,
                            Ok(false) => {}
                            Err(error) => {
                                tracing::warn!(
                                    owner = %repository.owner,
                                    repository = %repository.name,
                                    pull_request = pull_request.number,
                                    %error,
                                    "failed to retry initial analysis after configuration changed"
                                );
                            }
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => match self.inner.copilot.enqueue_analysis(&repository, pull_request).await {
                        Ok(true) => queued_count += 1,
                        Ok(false) => {}
                        Err(error) => {
                            tracing::warn!(
                                owner = %repository.owner,
                                repository = %repository.name,
                                pull_request = pull_request.number,
                                %error,
                                "failed to queue initial analysis"
                            );
                        }
                    },
                    Err(error) => {
                        tracing::warn!(
                            owner = %repository.owner,
                            repository = %repository.name,
                            pull_request = pull_request.number,
                            %error,
                            "failed to inspect analysis history"
                        );
                    }
                }
            }
            if queued_count != 0 {
                tracing::info!(queued_pull_requests = queued_count, "automatic AI analysis scan completed");
            }
        }
    }

    #[cfg(test)]
    pub(crate) async fn scan_once_for_test(&self) {
        self.scan_once().await;
    }

    #[cfg(test)]
    pub(crate) fn for_test(clock: Clock, storage: Storage, copilot: CopilotService) -> Self {
        Self {
            inner: Arc::new(AnalysisSchedulerInner {
                clock,
                storage,
                copilot,
                enabled: true,
            }),
        }
    }
}

impl fmt::Debug for AnalysisScheduler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnalysisScheduler")
            .field("scan_interval", &ANALYSIS_SCAN_INTERVAL)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use storage::{PullRequestDetails, PullRequestSnapshot, PullRequestState, RepositoryConfig, RepositoryId};
    use tick::ClockControl;

    use super::*;
    use crate::config::{ActionMapping, AzureDevOpsRepository, GitHubRepository};

    static NEXT_DATABASE: AtomicU64 = AtomicU64::new(1);

    struct TestDatabase(PathBuf);

    impl TestDatabase {
        async fn open() -> (Self, Storage) {
            let id = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::current_dir()
                .expect("current directory should be available")
                .join("target")
                .join("copilot-tests");
            fs::create_dir_all(&directory).expect("test database directory should be created");
            let path = directory.join(format!("copilot-{id}.sqlite3"));
            let storage = Storage::open(path.clone()).await.expect("storage should open");
            (Self(path), storage)
        }
    }

    impl Drop for TestDatabase {
        fn drop(&mut self) {
            for suffix in ["", "-shm", "-wal"] {
                let _ = fs::remove_file(format!("{}{suffix}", self.0.display()));
            }
        }
    }

    fn mapping(skill: &str) -> ActionMapping {
        ActionMapping {
            marketplace: "market".to_owned(),
            plugin: "plugin".to_owned(),
            skill: skill.to_owned(),
        }
    }

    fn config() -> AppConfig {
        AppConfig {
            prompts: analysis::AnalysisPrompts {
                overview: "Summarize the change.".to_owned(),
                interesting: "Prioritize correctness and public API impact.".to_owned(),
            },
            review_action: mapping("review"),
            github_repositories: vec![GitHubRepository {
                url: "https://github.com/octo/repo".to_owned(),
                owner: "octo".to_owned(),
                name: "repo".to_owned(),
                checkout_path: None,
            }],
            azure_devops_repositories: Vec::<AzureDevOpsRepository>::new(),
            ..AppConfig::default()
        }
    }

    async fn persisted_pull_request(storage: &Storage) -> (Repository, PullRequest) {
        let repository = storage
            .reconcile_repositories(
                vec![RepositoryConfig {
                    provider: "github".to_owned(),
                    owner: "octo".to_owned(),
                    name: "repo".to_owned(),
                    remote_id: None,
                    action_configuration_fingerprint: "actions-v1".to_owned(),
                }],
                1,
            )
            .await
            .expect("repository should persist")
            .remove(0);
        let pull_request = storage
            .reconcile_pull_requests(
                repository.id,
                vec![PullRequestSnapshot {
                    provider_id: "PR_1".to_owned(),
                    number: 1,
                    title: "Improve parser".to_owned(),
                    author: Some("octocat".to_owned()),
                    web_url: "https://github.com/octo/repo/pull/1".to_owned(),
                    source_branch: "feature".to_owned(),
                    target_branch: "main".to_owned(),
                    state: PullRequestState::Open,
                    revision_fingerprint: "revision-1".to_owned(),
                    provider_updated_at: Some(1_000),
                    details: Some(PullRequestDetails {
                        body: Some("Body".to_owned()),
                        is_draft: false,
                        mergeable: Some(true),
                        additions: Some(10),
                        deletions: Some(2),
                        changed_files: Some(1),
                    }),
                }],
                3,
            )
            .await
            .expect("pull request should persist")
            .remove(0);
        (repository, pull_request)
    }

    #[tokio::test]
    async fn analysis_crate_output_is_persisted_and_active_work_is_deduplicated() {
        let (_database, storage) = TestDatabase::open().await;
        let service = CopilotService::with_example_backend(
            config(),
            Clock::new_frozen_at(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1)),
            storage.clone(),
        );
        let (repository, pull_request) = persisted_pull_request(&storage).await;
        let request = analysis_request(&config(), &repository, &pull_request);
        assert_eq!(request.repository.owner, "octo");
        assert_eq!(request.pull_request.body.as_deref(), Some("Body"));
        assert_eq!(request.review_action.skill, "review");
        assert_eq!(request.prompts.overview, "Summarize the change.");

        assert!(
            service
                .enqueue_analysis(&repository, &pull_request)
                .await
                .expect("analysis should enqueue")
        );
        assert!(
            !service
                .enqueue_analysis(&repository, &pull_request)
                .await
                .expect("duplicate should resolve")
        );
        let pull_request_id = service.process_next_analysis_for_test().await.expect("analysis should complete");

        let stored_analysis = storage
            .newest_matching_analysis(pull_request_id, "revision-1".to_owned(), "actions-v1".to_owned())
            .await
            .expect("analysis lookup should work")
            .expect("analysis should exist");
        let output = serde_json::from_str::<AnalysisOutput>(stored_analysis.summary.as_deref().expect("summary should exist"))
            .expect("summary should be structured");
        assert_eq!(output.overview.summary, "Updates parser behavior.");
        assert_eq!(output.interesting.priority, analysis::Priority::High);
        assert!(
            service
                .enqueue_analysis(&repository, &pull_request)
                .await
                .expect("completed analysis should be enqueueable again")
        );
    }

    #[tokio::test]
    async fn background_scheduler_only_queues_the_initial_analysis() {
        let (_database, storage) = TestDatabase::open().await;
        let clock = Clock::new_frozen_at(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1));
        let service = CopilotService::with_example_backend(config(), clock.clone(), storage.clone());
        let (repository, pull_request) = persisted_pull_request(&storage).await;
        let scheduler = AnalysisScheduler::for_test(clock, storage.clone(), service.clone());

        scheduler.scan_once_for_test().await;
        assert_eq!(service.active_analysis_count_for_test().await, 1);
        service
            .process_next_analysis_for_test()
            .await
            .expect("initial analysis should complete");

        let pull_request_id = pull_request.id;
        let updated = PullRequestSnapshot {
            provider_id: pull_request.provider_id,
            number: pull_request.number,
            title: "Improve parser again".to_owned(),
            author: pull_request.author,
            web_url: pull_request.web_url,
            source_branch: pull_request.source_branch,
            target_branch: pull_request.target_branch,
            state: PullRequestState::Open,
            revision_fingerprint: "revision-2".to_owned(),
            provider_updated_at: Some(2_000),
            details: pull_request.details,
        };
        storage
            .reconcile_pull_requests(repository.id, vec![updated], 5)
            .await
            .expect("new revision should persist");

        scheduler.scan_once_for_test().await;
        assert_eq!(
            service.active_analysis_count_for_test().await,
            0,
            "an existing analysis must prevent automatic re-analysis after a new commit"
        );
        let existing = storage
            .newest_analysis(pull_request_id)
            .await
            .expect("analysis history should load")
            .expect("initial analysis should remain");
        assert_eq!(existing.revision_fingerprint, "revision-1");
    }

    #[tokio::test]
    async fn background_scheduler_skips_closed_pull_requests() {
        let (_database, storage) = TestDatabase::open().await;
        let clock = Clock::new_frozen();
        let service = CopilotService::with_example_backend(config(), clock.clone(), storage.clone());
        let (repository, _pull_request) = persisted_pull_request(&storage).await;
        storage
            .reconcile_pull_requests(repository.id, Vec::new(), 5)
            .await
            .expect("pull request should close");
        let scheduler = AnalysisScheduler::for_test(clock, storage, service.clone());

        scheduler.scan_once_for_test().await;

        assert_eq!(service.active_analysis_count_for_test().await, 0);
    }

    #[tokio::test]
    async fn background_scheduler_retries_failed_initial_analysis_after_configuration_changes() {
        let (_database, storage) = TestDatabase::open().await;
        let clock = Clock::new_frozen();
        let service = CopilotService::with_example_backend(config(), clock.clone(), storage.clone());
        let (repository, pull_request) = persisted_pull_request(&storage).await;
        storage
            .record_analysis(NewAnalysis {
                pull_request_id: pull_request.id,
                revision_fingerprint: pull_request.revision_fingerprint.clone(),
                action_configuration_fingerprint: "broken-actions".to_owned(),
                outcome: AnalysisOutcome::Failed,
                summary: None,
                diagnostic: Some("review skill was unavailable".to_owned()),
                completed_at: 4,
            })
            .await
            .expect("failed analysis should persist");
        storage
            .reconcile_repositories(
                vec![RepositoryConfig {
                    provider: repository.provider,
                    owner: repository.owner,
                    name: repository.name,
                    remote_id: repository.remote_id,
                    action_configuration_fingerprint: "fixed-actions".to_owned(),
                }],
                5,
            )
            .await
            .expect("updated analysis configuration should persist");
        let scheduler = AnalysisScheduler::for_test(clock, storage, service.clone());

        scheduler.scan_once_for_test().await;

        assert_eq!(service.active_analysis_count_for_test().await, 1);
    }

    #[tokio::test]
    async fn background_scheduler_waits_before_initial_scan() {
        let (_database, storage) = TestDatabase::open().await;
        let control = ClockControl::new();
        let clock = control.to_clock();
        let service = CopilotService::with_example_backend(config(), clock.clone(), storage.clone());
        persisted_pull_request(&storage).await;
        let scheduler = AnalysisScheduler::for_test(clock, storage, service.clone());
        let (trigger, listener) = crate::shutdown::channel();
        let task = tokio::spawn(scheduler.run(listener));

        tokio::task::yield_now().await;
        assert_eq!(service.active_analysis_count_for_test().await, 0);

        control.advance(ANALYSIS_SCAN_INTERVAL);
        tokio::time::timeout(Duration::from_secs(2), async {
            while service.active_analysis_count_for_test().await == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background scan should run after its delay");

        trigger.trigger();
        task.await.expect("analysis scheduler should stop cleanly");
    }

    #[test]
    fn retries_are_exponential_and_bounded() {
        assert_eq!(retry_delay(1), 30_000);
        assert_eq!(retry_delay(2), 60_000);
        assert_eq!(retry_delay(20), MAX_RETRY_MILLIS);
    }

    #[test]
    fn checkout_lookup_is_provider_specific() {
        let mut config = config();
        config.github_repositories[0].checkout_path = Some(PathBuf::from("checkout"));
        let repository = Repository {
            id: RepositoryId(1),
            provider: "github".to_owned(),
            owner: "OCTO".to_owned(),
            name: "REPO".to_owned(),
            remote_id: None,
            action_configuration_fingerprint: "actions-v1".to_owned(),
            active: true,
        };

        assert_eq!(checkout_path(&config, &repository), Some(PathBuf::from("checkout")));
    }
}
