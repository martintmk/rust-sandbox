// Licensed under the MIT License.

//! SQLite-backed storage for repositories, pull request snapshots, and analyses.
//!
//! Operational concerns such as polling runs and analysis jobs are deliberately
//! not persisted. A successful provider refresh replaces one repository's pull
//! request snapshot atomically, while failed refreshes leave the last snapshot
//! untouched.

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params};

const MAX_DIAGNOSTIC_CHARS: usize = 4_096;
const SCHEMA: &str = include_str!("../schema.sql");

/// A cloneable handle to the `SQLite` database.
#[derive(Clone, Debug)]
pub struct Storage {
    database_path: Arc<PathBuf>,
    startup_error: Option<Arc<str>>,
}

/// Stable, dependency-free classification of a [`StorageError`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum StorageErrorKind {
    /// `SQLite` rejected an operation.
    Database,
    /// A blocking `SQLite` task could not complete.
    BlockingTask,
    /// Synchronous startup initialization failed.
    Initialization,
    /// A requested record does not exist.
    NotFound,
    /// A persisted enum-like value is invalid.
    CorruptData,
}

/// An error returned by a storage operation.
#[derive(Debug)]
pub struct StorageError {
    inner: StorageErrorInner,
}

#[derive(Debug)]
enum StorageErrorInner {
    Database(rusqlite::Error),
    BlockingTask(tokio::task::JoinError),
    Initialization { path: PathBuf, message: Arc<str> },
    NotFound { entity: &'static str, id: i64 },
    CorruptData { field: &'static str, value: String },
}

impl StorageError {
    /// Returns the stable classification for this error.
    #[must_use]
    pub const fn kind(&self) -> StorageErrorKind {
        match &self.inner {
            StorageErrorInner::Database(_) => StorageErrorKind::Database,
            StorageErrorInner::BlockingTask(_) => StorageErrorKind::BlockingTask,
            StorageErrorInner::Initialization { .. } => StorageErrorKind::Initialization,
            StorageErrorInner::NotFound { .. } => StorageErrorKind::NotFound,
            StorageErrorInner::CorruptData { .. } => StorageErrorKind::CorruptData,
        }
    }

    fn initialization(path: PathBuf, message: Arc<str>) -> Self {
        Self {
            inner: StorageErrorInner::Initialization { path, message },
        }
    }

    fn not_found(entity: &'static str, id: i64) -> Self {
        Self {
            inner: StorageErrorInner::NotFound { entity, id },
        }
    }

    fn corrupt_data(field: &'static str, value: String) -> Self {
        Self {
            inner: StorageErrorInner::CorruptData { field, value },
        }
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            StorageErrorInner::Database(error) => write!(f, "SQLite operation failed: {error}"),
            StorageErrorInner::BlockingTask(error) => write!(f, "SQLite blocking task failed: {error}"),
            StorageErrorInner::Initialization { path, message } => {
                write!(f, "failed to initialize {}: {message}", path.display())
            }
            StorageErrorInner::NotFound { entity, id } => write!(f, "{entity} {id} was not found"),
            StorageErrorInner::CorruptData { field, value } => {
                write!(f, "invalid persisted value for {field}: {value}")
            }
        }
    }
}

impl Error for StorageError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.inner {
            StorageErrorInner::Database(error) => Some(error),
            StorageErrorInner::BlockingTask(error) => Some(error),
            StorageErrorInner::Initialization { .. } | StorageErrorInner::NotFound { .. } | StorageErrorInner::CorruptData { .. } => None,
        }
    }
}

impl From<rusqlite::Error> for StorageError {
    fn from(error: rusqlite::Error) -> Self {
        Self {
            inner: StorageErrorInner::Database(error),
        }
    }
}

impl From<tokio::task::JoinError> for StorageError {
    fn from(error: tokio::task::JoinError) -> Self {
        Self {
            inner: StorageErrorInner::BlockingTask(error),
        }
    }
}

/// Database identifier for a repository.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RepositoryId(pub i64);

/// Database identifier for a pull request.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PullRequestId(pub i64);

/// Database identifier for an analysis.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AnalysisId(pub i64);

/// Repository data supplied by application configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryConfig {
    pub provider: String,
    pub owner: String,
    pub name: String,
    pub remote_id: Option<String>,
    pub action_configuration_fingerprint: String,
}

/// A persisted repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Repository {
    pub id: RepositoryId,
    pub provider: String,
    pub owner: String,
    pub name: String,
    pub remote_id: Option<String>,
    pub action_configuration_fingerprint: String,
    pub active: bool,
}

/// Persisted lifecycle state of a pull request.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PullRequestState {
    Open,
    Closed,
}

impl PullRequestState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }

    fn parse(value: String) -> Result<Self, StorageError> {
        match value.as_str() {
            "open" => Ok(Self::Open),
            "closed" => Ok(Self::Closed),
            _ => Err(StorageError::corrupt_data("pull_requests.state", value)),
        }
    }
}

/// Provider detail fields stored with a pull request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequestDetails {
    pub body: Option<String>,
    pub is_draft: bool,
    pub mergeable: Option<bool>,
    pub additions: Option<i64>,
    pub deletions: Option<i64>,
    pub changed_files: Option<i64>,
}

/// Raw provider data used to insert or refresh a pull request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequestSnapshot {
    pub provider_id: String,
    pub number: i64,
    pub title: String,
    pub author: Option<String>,
    pub web_url: String,
    pub source_branch: String,
    pub target_branch: String,
    pub state: PullRequestState,
    pub revision_fingerprint: String,
    pub provider_updated_at: Option<i64>,
    pub details: Option<PullRequestDetails>,
}

/// A persisted pull request and its raw provider data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequest {
    pub id: PullRequestId,
    pub repository_id: RepositoryId,
    pub provider_id: String,
    pub number: i64,
    pub title: String,
    pub author: Option<String>,
    pub web_url: String,
    pub source_branch: String,
    pub target_branch: String,
    pub state: PullRequestState,
    pub revision_fingerprint: String,
    pub closed_at: Option<i64>,
    pub refreshed_at: i64,
    pub provider_updated_at: Option<i64>,
    pub details: Option<PullRequestDetails>,
}

/// Result of an analysis attempt.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AnalysisOutcome {
    Succeeded,
    Failed,
}

impl AnalysisOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }

    fn parse(value: String) -> Result<Self, StorageError> {
        match value.as_str() {
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            _ => Err(StorageError::corrupt_data("analyses.outcome", value)),
        }
    }
}

/// Analysis data to persist.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewAnalysis {
    pub pull_request_id: PullRequestId,
    pub revision_fingerprint: String,
    pub action_configuration_fingerprint: String,
    pub outcome: AnalysisOutcome,
    pub summary: Option<String>,
    pub diagnostic: Option<String>,
    pub completed_at: i64,
}

/// A persisted pull request analysis.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Analysis {
    pub id: AnalysisId,
    pub pull_request_id: PullRequestId,
    pub revision_fingerprint: String,
    pub action_configuration_fingerprint: String,
    pub outcome: AnalysisOutcome,
    pub summary: Option<String>,
    pub diagnostic: Option<String>,
    pub completed_at: i64,
}

impl Storage {
    /// Creates a handle and initializes its database synchronously.
    ///
    /// Initialization errors are retained and returned by [`Self::ensure_ready`]
    /// and subsequent operations. Use [`Self::open`] when construction itself
    /// can return a `Result`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let database_path = path.into();
        match initialize_path(&database_path) {
            Ok(()) => Self {
                database_path: Arc::new(database_path),
                startup_error: None,
            },
            Err(error) => Self {
                database_path: Arc::new(database_path),
                startup_error: Some(Arc::from(error.to_string())),
            },
        }
    }

    /// Opens and initializes a database without blocking the async executor.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the database cannot be opened or
    /// initialized by the blocking task.
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self, StorageError> {
        let database_path = path.into();
        let initialization_path = database_path.clone();
        tokio::task::spawn_blocking(move || initialize_path(&initialization_path)).await??;
        Ok(Self {
            database_path: Arc::new(database_path),
            startup_error: None,
        })
    }

    /// Reports a construction-time initialization error, if one occurred.
    ///
    /// # Errors
    ///
    /// Returns [`StorageErrorKind::Initialization`] when [`Self::new`] could not
    /// initialize the database.
    pub fn ensure_ready(&self) -> Result<(), StorageError> {
        if let Some(message) = &self.startup_error {
            return Err(StorageError::initialization(
                self.database_path.as_ref().clone(),
                Arc::clone(message),
            ));
        }
        Ok(())
    }

    /// Replaces the active repository configuration atomically.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the database operation fails.
    pub async fn reconcile_repositories(
        &self,
        repositories: Vec<RepositoryConfig>,
        updated_at: i64,
    ) -> Result<Vec<Repository>, StorageError> {
        self.run(move |connection| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute("UPDATE repositories SET active = 0", [])?;
            for repository in repositories {
                transaction.execute(
                    "INSERT INTO repositories (
                        provider, owner, name, remote_id,
                        action_configuration_fingerprint, active, updated_at
                    ) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6)
                    ON CONFLICT(provider, owner, name) DO UPDATE SET
                        remote_id = excluded.remote_id,
                        action_configuration_fingerprint =
                            excluded.action_configuration_fingerprint,
                        active = 1,
                        updated_at = excluded.updated_at",
                    params![
                        repository.provider,
                        repository.owner,
                        repository.name,
                        repository.remote_id,
                        repository.action_configuration_fingerprint,
                        updated_at,
                    ],
                )?;
            }
            transaction.commit()?;
            list_repositories(connection, true)
        })
        .await
    }

    /// Lists repositories, optionally excluding inactive configuration entries.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the database operation fails or persisted
    /// data is invalid.
    pub async fn list_repositories(&self, active_only: bool) -> Result<Vec<Repository>, StorageError> {
        self.run(move |connection| list_repositories(connection, active_only)).await
    }

    /// Atomically replaces the raw pull request snapshot for one repository.
    ///
    /// Previously open pull requests omitted from `snapshots` are marked
    /// closed. Callers should invoke this only after a complete successful
    /// provider refresh; an error before this call leaves the prior data intact.
    ///
    /// # Errors
    ///
    /// Returns [`StorageErrorKind::NotFound`] when `repository_id` does not exist,
    /// or another [`StorageError`] when the transaction fails.
    pub async fn reconcile_pull_requests(
        &self,
        repository_id: RepositoryId,
        snapshots: Vec<PullRequestSnapshot>,
        refreshed_at: i64,
    ) -> Result<Vec<PullRequest>, StorageError> {
        self.run(move |connection| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            ensure_repository_exists(&transaction, repository_id)?;
            transaction.execute(
                "UPDATE pull_requests
                 SET state = 'closed', closed_at = ?1, refreshed_at = ?1
                 WHERE repository_id = ?2 AND state = 'open'",
                params![refreshed_at, repository_id.0],
            )?;
            for snapshot in snapshots {
                upsert_pull_request(&transaction, repository_id, snapshot, refreshed_at)?;
            }
            transaction.commit()?;
            list_pull_requests(connection, repository_id)
        })
        .await
    }

    /// Lists all pull requests stored for a repository.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the database operation fails or persisted
    /// data is invalid.
    pub async fn list_pull_requests(&self, repository_id: RepositoryId) -> Result<Vec<PullRequest>, StorageError> {
        self.run(move |connection| list_pull_requests(connection, repository_id)).await
    }

    /// Persists an analysis result.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the database operation fails, including
    /// when the referenced pull request does not exist.
    pub async fn record_analysis(&self, analysis: NewAnalysis) -> Result<AnalysisId, StorageError> {
        let diagnostic = analysis.diagnostic.as_deref().map(bounded_diagnostic);
        self.run(move |connection| {
            connection.execute(
                "INSERT INTO analyses (
                    pull_request_id, revision_fingerprint,
                    action_configuration_fingerprint, outcome, summary,
                    diagnostic, completed_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    analysis.pull_request_id.0,
                    analysis.revision_fingerprint,
                    analysis.action_configuration_fingerprint,
                    analysis.outcome.as_str(),
                    analysis.summary,
                    diagnostic,
                    analysis.completed_at,
                ],
            )?;
            Ok(AnalysisId(connection.last_insert_rowid()))
        })
        .await
    }

    /// Loads the newest analysis matching the PR revision and action settings.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the database operation fails or persisted
    /// data is invalid.
    pub async fn newest_matching_analysis(
        &self,
        pull_request_id: PullRequestId,
        revision_fingerprint: String,
        action_configuration_fingerprint: String,
    ) -> Result<Option<Analysis>, StorageError> {
        self.run(move |connection| {
            connection
                .query_row(
                    "SELECT
                        id, pull_request_id, revision_fingerprint,
                        action_configuration_fingerprint, outcome, summary,
                        diagnostic, completed_at
                     FROM analyses
                     WHERE pull_request_id = ?1
                       AND revision_fingerprint = ?2
                       AND action_configuration_fingerprint = ?3
                     ORDER BY completed_at DESC, id DESC
                     LIMIT 1",
                    params![pull_request_id.0, revision_fingerprint, action_configuration_fingerprint,],
                    analysis_from_row,
                )
                .optional()
                .map_err(StorageError::from)
        })
        .await
    }

    /// Loads the newest analysis for a pull request, regardless of revision.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the database operation fails or persisted
    /// data is invalid.
    pub async fn newest_analysis(&self, pull_request_id: PullRequestId) -> Result<Option<Analysis>, StorageError> {
        self.run(move |connection| {
            connection
                .query_row(
                    "SELECT
                        id, pull_request_id, revision_fingerprint,
                        action_configuration_fingerprint, outcome, summary,
                        diagnostic, completed_at
                     FROM analyses
                     WHERE pull_request_id = ?1
                     ORDER BY completed_at DESC, id DESC
                     LIMIT 1",
                    [pull_request_id.0],
                    analysis_from_row,
                )
                .optional()
                .map_err(StorageError::from)
        })
        .await
    }

    async fn run<T, F>(&self, operation: F) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, StorageError> + Send + 'static,
    {
        self.ensure_ready()?;
        let database_path = Arc::clone(&self.database_path);
        tokio::task::spawn_blocking(move || {
            let mut connection = open_connection(&database_path)?;
            operation(&mut connection)
        })
        .await?
    }
}

fn initialize_path(path: &Path) -> Result<(), StorageError> {
    let mut connection = open_connection(path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA)?;
    transaction.commit()?;
    Ok(())
}

fn open_connection(path: &Path) -> Result<Connection, StorageError> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_FULL_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "foreign_keys", true)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(connection)
}

fn list_repositories(connection: &Connection, active_only: bool) -> Result<Vec<Repository>, StorageError> {
    let query = if active_only {
        "SELECT id, provider, owner, name, remote_id,
                action_configuration_fingerprint, active
         FROM repositories WHERE active = 1 ORDER BY provider, owner, name"
    } else {
        "SELECT id, provider, owner, name, remote_id,
                action_configuration_fingerprint, active
         FROM repositories ORDER BY provider, owner, name"
    };
    let mut statement = connection.prepare(query)?;
    let rows = statement.query_map([], |row| {
        Ok(Repository {
            id: RepositoryId(row.get(0)?),
            provider: row.get(1)?,
            owner: row.get(2)?,
            name: row.get(3)?,
            remote_id: row.get(4)?,
            action_configuration_fingerprint: row.get(5)?,
            active: row.get(6)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(StorageError::from)
}

fn ensure_repository_exists(transaction: &Transaction<'_>, repository_id: RepositoryId) -> Result<(), StorageError> {
    let exists = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM repositories WHERE id = ?1)",
        [repository_id.0],
        |row| row.get::<_, bool>(0),
    )?;
    if exists {
        Ok(())
    } else {
        Err(StorageError::not_found("repository", repository_id.0))
    }
}

fn upsert_pull_request(
    transaction: &Transaction<'_>,
    repository_id: RepositoryId,
    snapshot: PullRequestSnapshot,
    refreshed_at: i64,
) -> Result<(), StorageError> {
    let closed_at = (snapshot.state == PullRequestState::Closed).then_some(refreshed_at);
    let (details_present, body, is_draft, mergeable, additions, deletions, changed_files) = match snapshot.details {
        Some(details) => (
            true,
            details.body,
            Some(details.is_draft),
            details.mergeable,
            details.additions,
            details.deletions,
            details.changed_files,
        ),
        None => (false, None, None, None, None, None, None),
    };
    transaction.execute(
        "INSERT INTO pull_requests (
            repository_id, provider_id, number, title, author, web_url,
            source_branch, target_branch, state, revision_fingerprint,
            closed_at, refreshed_at, provider_updated_at, details_present, body,
            is_draft, mergeable, additions, deletions, changed_files
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
            ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20
         )
         ON CONFLICT(repository_id, provider_id) DO UPDATE SET
            number = excluded.number,
            title = excluded.title,
            author = excluded.author,
            web_url = excluded.web_url,
            source_branch = excluded.source_branch,
            target_branch = excluded.target_branch,
            state = excluded.state,
            revision_fingerprint = excluded.revision_fingerprint,
            closed_at = excluded.closed_at,
            refreshed_at = excluded.refreshed_at,
            provider_updated_at = excluded.provider_updated_at,
            details_present = CASE
                WHEN excluded.details_present = 1 THEN 1
                ELSE pull_requests.details_present
            END,
            body = CASE
                WHEN excluded.details_present = 1 THEN excluded.body
                ELSE pull_requests.body
            END,
            is_draft = CASE
                WHEN excluded.details_present = 1 THEN excluded.is_draft
                ELSE pull_requests.is_draft
            END,
            mergeable = CASE
                WHEN excluded.details_present = 1 THEN excluded.mergeable
                ELSE pull_requests.mergeable
            END,
            additions = CASE
                WHEN excluded.details_present = 1 THEN excluded.additions
                ELSE pull_requests.additions
            END,
            deletions = CASE
                WHEN excluded.details_present = 1 THEN excluded.deletions
                ELSE pull_requests.deletions
            END,
            changed_files = CASE
                WHEN excluded.details_present = 1 THEN excluded.changed_files
                ELSE pull_requests.changed_files
            END",
        params![
            repository_id.0,
            snapshot.provider_id,
            snapshot.number,
            snapshot.title,
            snapshot.author,
            snapshot.web_url,
            snapshot.source_branch,
            snapshot.target_branch,
            snapshot.state.as_str(),
            snapshot.revision_fingerprint,
            closed_at,
            refreshed_at,
            snapshot.provider_updated_at,
            details_present,
            body,
            is_draft,
            mergeable,
            additions,
            deletions,
            changed_files,
        ],
    )?;
    Ok(())
}

fn list_pull_requests(connection: &Connection, repository_id: RepositoryId) -> Result<Vec<PullRequest>, StorageError> {
    let mut statement = connection.prepare(
        "SELECT
            id, repository_id, provider_id, number, title, author, web_url,
            source_branch, target_branch, state, revision_fingerprint,
            closed_at, refreshed_at, provider_updated_at, details_present, body,
            is_draft, mergeable, additions, deletions, changed_files
         FROM pull_requests
         WHERE repository_id = ?1
         ORDER BY number",
    )?;
    let rows = statement.query_map([repository_id.0], pull_request_from_row)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(StorageError::from)
}

fn pull_request_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PullRequest> {
    let state = PullRequestState::parse(row.get(9)?).map_err(to_sql_conversion_error)?;
    let details = if row.get::<_, bool>(14)? {
        Some(PullRequestDetails {
            body: row.get(15)?,
            is_draft: row.get(16)?,
            mergeable: row.get(17)?,
            additions: row.get(18)?,
            deletions: row.get(19)?,
            changed_files: row.get(20)?,
        })
    } else {
        None
    };
    Ok(PullRequest {
        id: PullRequestId(row.get(0)?),
        repository_id: RepositoryId(row.get(1)?),
        provider_id: row.get(2)?,
        number: row.get(3)?,
        title: row.get(4)?,
        author: row.get(5)?,
        web_url: row.get(6)?,
        source_branch: row.get(7)?,
        target_branch: row.get(8)?,
        state,
        revision_fingerprint: row.get(10)?,
        closed_at: row.get(11)?,
        refreshed_at: row.get(12)?,
        provider_updated_at: row.get(13)?,
        details,
    })
}

fn analysis_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Analysis> {
    let outcome = AnalysisOutcome::parse(row.get(4)?).map_err(to_sql_conversion_error)?;
    Ok(Analysis {
        id: AnalysisId(row.get(0)?),
        pull_request_id: PullRequestId(row.get(1)?),
        revision_fingerprint: row.get(2)?,
        action_configuration_fingerprint: row.get(3)?,
        outcome,
        summary: row.get(5)?,
        diagnostic: row.get(6)?,
        completed_at: row.get(7)?,
    })
}

fn to_sql_conversion_error(error: StorageError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

fn bounded_diagnostic(text: &str) -> String {
    text.chars().take(MAX_DIAGNOSTIC_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_DATABASE: AtomicU64 = AtomicU64::new(1);

    struct TestDatabase {
        path: PathBuf,
    }

    impl TestDatabase {
        async fn open() -> (Self, Storage) {
            let database = Self::uninitialized();
            let storage = Storage::open(database.path.clone()).await.expect("test database should open");
            (database, storage)
        }

        fn uninitialized() -> Self {
            let id = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::current_dir()
                .expect("current directory should be available")
                .join("target")
                .join("storage-tests");
            fs::create_dir_all(&directory).expect("test database directory should be created");
            Self {
                path: directory.join(format!("storage-{id}.sqlite3")),
            }
        }
    }

    impl Drop for TestDatabase {
        fn drop(&mut self) {
            remove_sqlite_files(&self.path);
        }
    }

    fn remove_sqlite_files(path: &Path) {
        for suffix in ["", "-shm", "-wal"] {
            let candidate = PathBuf::from(format!("{}{suffix}", path.display()));
            match fs::remove_file(candidate) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("test database should be removable: {error}"),
            }
        }
    }

    fn repository_config(fingerprint: &str) -> RepositoryConfig {
        RepositoryConfig {
            provider: "github".to_owned(),
            owner: "octo".to_owned(),
            name: "repo".to_owned(),
            remote_id: None,
            action_configuration_fingerprint: fingerprint.to_owned(),
        }
    }

    fn snapshot(provider_id: &str, number: i64, title: &str) -> PullRequestSnapshot {
        PullRequestSnapshot {
            provider_id: provider_id.to_owned(),
            number,
            title: title.to_owned(),
            author: Some("octocat".to_owned()),
            web_url: format!("https://example.test/pulls/{number}"),
            source_branch: "feature".to_owned(),
            target_branch: "main".to_owned(),
            state: PullRequestState::Open,
            revision_fingerprint: format!("revision-{number}"),
            provider_updated_at: Some(number * 1_000),
            details: Some(PullRequestDetails {
                body: Some(format!("Body {number}")),
                is_draft: false,
                mergeable: Some(true),
                additions: Some(10),
                deletions: Some(2),
                changed_files: Some(1),
            }),
        }
    }

    async fn configured_repository(storage: &Storage) -> Repository {
        storage
            .reconcile_repositories(vec![repository_config("actions-v1")], 10)
            .await
            .expect("repository should persist")
            .remove(0)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn schema_contains_only_domain_tables() {
        let (database, _storage) = TestDatabase::open().await;
        let connection = Connection::open(&database.path).expect("database should reopen");
        let mut statement = connection
            .prepare(
                "SELECT name FROM sqlite_schema
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
                 ORDER BY name",
            )
            .expect("schema should be queryable");
        let tables = statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("table names should be queryable")
            .collect::<Result<Vec<_>, _>>()
            .expect("table names should be readable");

        assert_eq!(tables, ["analyses", "pull_requests", "repositories"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pull_request_snapshot_is_atomic_and_closes_missing_items() {
        let (_database, storage) = TestDatabase::open().await;
        let repository = configured_repository(&storage).await;
        storage
            .reconcile_pull_requests(repository.id, vec![snapshot("PR_1", 1, "First"), snapshot("PR_2", 2, "Second")], 20)
            .await
            .expect("initial snapshot should persist");

        let mut refreshed = snapshot("PR_2", 2, "Updated");
        refreshed.details = None;
        let pull_requests = storage
            .reconcile_pull_requests(repository.id, vec![refreshed], 30)
            .await
            .expect("replacement snapshot should persist");

        assert_eq!(pull_requests[0].state, PullRequestState::Closed);
        assert_eq!(pull_requests[0].closed_at, Some(30));
        assert_eq!(pull_requests[1].state, PullRequestState::Open);
        assert_eq!(pull_requests[1].title, "Updated");
        assert_eq!(
            pull_requests[1].details.as_ref().and_then(|details| details.body.as_deref()),
            Some("Body 2"),
            "a summary-only refresh should preserve the last complete raw details"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn analyses_match_both_fingerprints_and_persist_across_reopen() {
        let (database, storage) = TestDatabase::open().await;
        let repository = configured_repository(&storage).await;
        let pull_request = storage
            .reconcile_pull_requests(repository.id, vec![snapshot("PR_1", 1, "First")], 20)
            .await
            .expect("pull request should persist")
            .remove(0);
        storage
            .record_analysis(NewAnalysis {
                pull_request_id: pull_request.id,
                revision_fingerprint: "revision-1".to_owned(),
                action_configuration_fingerprint: "actions-v1".to_owned(),
                outcome: AnalysisOutcome::Succeeded,
                summary: Some("summary".to_owned()),
                diagnostic: None,
                completed_at: 30,
            })
            .await
            .expect("analysis should persist");
        storage
            .record_analysis(NewAnalysis {
                pull_request_id: pull_request.id,
                revision_fingerprint: "revision-2".to_owned(),
                action_configuration_fingerprint: "actions-v1".to_owned(),
                outcome: AnalysisOutcome::Succeeded,
                summary: Some("new summary".to_owned()),
                diagnostic: None,
                completed_at: 40,
            })
            .await
            .expect("new analysis should persist");

        assert!(
            storage
                .newest_matching_analysis(pull_request.id, "revision-3".to_owned(), "actions-v1".to_owned())
                .await
                .expect("lookup should succeed")
                .is_none()
        );
        let reopened = Storage::open(database.path.clone()).await.expect("database should reopen");
        let analysis = reopened
            .newest_matching_analysis(pull_request.id, "revision-1".to_owned(), "actions-v1".to_owned())
            .await
            .expect("lookup should succeed")
            .expect("matching analysis should exist");
        assert_eq!(analysis.summary.as_deref(), Some("summary"));
        let newest = reopened
            .newest_analysis(pull_request.id)
            .await
            .expect("latest lookup should succeed")
            .expect("latest analysis should exist");
        assert_eq!(newest.summary.as_deref(), Some("new summary"));
    }
}
