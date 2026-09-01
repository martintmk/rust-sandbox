// Licensed under the MIT License.

//! Application configuration: TOML parsing and validation.
//!
//! Configuration is intentionally strict: anything that would leave the dashboard unable to
//! start safely (a non-loopback bind address, an empty storage path, a zero concurrency limit,
//! duplicate repositories, ...) is rejected while loading, before any resource is touched.

use std::fs;
use std::io::Write as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(crate) use analysis::{ActionMapping, AnalysisPrompts, DEFAULT_INTEREST_PROMPT, DEFAULT_OVERVIEW_PROMPT};
use serde::{Deserialize, Serialize};

const DEFAULT_REVIEW_SKILL: &str = "review-lens";

/// Fatal configuration error: the dashboard cannot start until it is fixed.
#[ohno::error]
#[display("{message}")]
pub(crate) struct ConfigError {
    pub(crate) message: String,
}

fn default_review_action() -> ActionMapping {
    ActionMapping {
        marketplace: String::new(),
        plugin: String::new(),
        skill: DEFAULT_REVIEW_SKILL.to_owned(),
    }
}

/// A single GitHub repository to track.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct GitHubRepository {
    pub(crate) url: String,
    #[serde(skip)]
    pub(crate) owner: String,
    #[serde(skip)]
    pub(crate) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) checkout_path: Option<PathBuf>,
}

/// A single Azure DevOps repository to track.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct AzureDevOpsRepository {
    pub(crate) url: String,
    #[serde(skip)]
    pub(crate) organization: String,
    #[serde(skip)]
    pub(crate) project: String,
    #[serde(skip)]
    pub(crate) repository: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) checkout_path: Option<PathBuf>,
}

#[derive(Debug)]
pub(crate) enum ConfigUpdate {
    Prompts { overview: String, interesting: String },
    AddRepository { url: String, checkout_path: Option<String> },
    RemoveRepository { url: String },
}

#[derive(Debug)]
pub(crate) enum ConfigUpdateError {
    Invalid(ConfigError),
    Persistence(ConfigError),
}

impl std::fmt::Display for ConfigUpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(error) => write!(f, "invalid settings update: {error}"),
            Self::Persistence(error) => write!(f, "failed to save settings: {error}"),
        }
    }
}

impl std::error::Error for ConfigUpdateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Invalid(error) | Self::Persistence(error) => Some(error),
        }
    }
}

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub(crate) config_path: Option<PathBuf>,
    pub(crate) config_file_loaded: bool,
    pub(crate) listen_address: SocketAddr,
    pub(crate) database_path: PathBuf,
    pub(crate) poll_interval: Duration,
    pub(crate) max_concurrent_polls: NonZeroUsize,
    pub(crate) max_concurrent_copilot_jobs: NonZeroUsize,
    pub(crate) github_repositories: Vec<GitHubRepository>,
    pub(crate) azure_devops_repositories: Vec<AzureDevOpsRepository>,
    pub(crate) prompts: AnalysisPrompts,
    pub(crate) review_action: ActionMapping,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            config_path: None,
            config_file_loaded: false,
            listen_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8787),
            database_path: PathBuf::from("pr-review-dashboard.sqlite3"),
            poll_interval: Duration::from_mins(5),
            max_concurrent_polls: NonZeroUsize::new(4).expect("4 is nonzero"),
            max_concurrent_copilot_jobs: NonZeroUsize::new(2).expect("2 is nonzero"),
            github_repositories: Vec::new(),
            azure_devops_repositories: Vec::new(),
            prompts: AnalysisPrompts::default(),
            review_action: default_review_action(),
        }
    }
}

impl AppConfig {
    /// Reads and validates configuration from a TOML file at `path`.
    pub(crate) fn load(path: &Path) -> Result<Self, ConfigError> {
        let source = fs::read_to_string(path)
            .map_err(|error| ConfigError::caused_by(format!("failed to read configuration file `{}`", path.display()), error))?;
        let config_path = absolute_path(path)?;
        let mut config = Self::from_toml_str(&source)?;
        config.config_path = Some(config_path);
        config.config_file_loaded = true;
        Ok(config)
    }

    /// Parses and validates configuration from an in-memory TOML document.
    pub(crate) fn from_toml_str(source: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig =
            toml::from_str(source).map_err(|error| ConfigError::caused_by(format!("invalid configuration: {error}"), error))?;
        raw.into_app_config()
    }

    pub(crate) fn defaults_at(path: &Path) -> Result<Self, ConfigError> {
        Ok(Self {
            config_path: Some(absolute_path(path)?),
            config_file_loaded: false,
            ..Self::default()
        })
    }

    pub(crate) fn persist_update(&self, update: ConfigUpdate) -> Result<(), ConfigUpdateError> {
        let path = self
            .config_path
            .as_ref()
            .ok_or_else(|| ConfigUpdateError::Persistence(ConfigError::new("no configuration file path is available".to_owned())))?;
        let mut updated = if path.exists() {
            Self::load(path).map_err(ConfigUpdateError::Invalid)?
        } else {
            self.clone()
        };
        updated.apply_update(update).map_err(ConfigUpdateError::Invalid)?;
        let source = updated.to_toml().map_err(ConfigUpdateError::Persistence)?;
        Self::from_toml_str(&source).map_err(ConfigUpdateError::Invalid)?;
        persist_atomically(path, source.as_bytes()).map_err(ConfigUpdateError::Persistence)
    }

    fn apply_update(&mut self, update: ConfigUpdate) -> Result<(), ConfigError> {
        match update {
            ConfigUpdate::Prompts { overview, interesting } => {
                self.prompts = AnalysisPrompts { overview, interesting };
            }
            ConfigUpdate::AddRepository { url, checkout_path } => match parse_repository_url(&url)? {
                ParsedRepository::GitHub { url, owner, name } => self.github_repositories.push(GitHubRepository {
                    url,
                    owner,
                    name,
                    checkout_path: checkout_path.map(PathBuf::from),
                }),
                ParsedRepository::AzureDevOps {
                    url,
                    organization,
                    project,
                    repository,
                } => self.azure_devops_repositories.push(AzureDevOpsRepository {
                    url,
                    organization,
                    project,
                    repository,
                    checkout_path: checkout_path.map(PathBuf::from),
                }),
            },
            ConfigUpdate::RemoveRepository { url } => {
                match parse_repository_url(&url)? {
                    ParsedRepository::GitHub { owner, name, .. } => {
                        let Some(index) = self.github_repositories.iter().position(|candidate| {
                            candidate.owner.eq_ignore_ascii_case(&owner) && candidate.name.eq_ignore_ascii_case(&name)
                        }) else {
                            return Err(ConfigError::new(format!("GitHub repository `{url}` is not configured")));
                        };
                        self.github_repositories.remove(index);
                    }
                    ParsedRepository::AzureDevOps {
                        organization,
                        project,
                        repository,
                        ..
                    } => {
                        let Some(index) = self.azure_devops_repositories.iter().position(|candidate| {
                            candidate.organization.eq_ignore_ascii_case(&organization)
                                && candidate.project.eq_ignore_ascii_case(&project)
                                && candidate.repository.eq_ignore_ascii_case(&repository)
                        }) else {
                            return Err(ConfigError::new(format!("Azure DevOps repository `{url}` is not configured")));
                        };
                        self.azure_devops_repositories.remove(index);
                    }
                }
            }
        }
        let source = self.to_toml()?;
        let validated = Self::from_toml_str(&source)?;
        self.prompts = validated.prompts;
        self.github_repositories = validated.github_repositories;
        self.azure_devops_repositories = validated.azure_devops_repositories;
        Ok(())
    }

    fn to_toml(&self) -> Result<String, ConfigError> {
        let review =
            (!self.review_action.marketplace.is_empty() || !self.review_action.plugin.is_empty() || !self.review_action.skill.is_empty())
                .then(|| WritableActions {
                    review: WritableActionMapping {
                        marketplace: (!self.review_action.marketplace.is_empty()).then_some(self.review_action.marketplace.as_str()),
                        plugin: (!self.review_action.plugin.is_empty()).then_some(self.review_action.plugin.as_str()),
                        skill: &self.review_action.skill,
                    },
                });
        let repositories = self
            .github_repositories
            .iter()
            .map(|repository| WritableRepository {
                url: &repository.url,
                checkout_path: repository.checkout_path.as_deref(),
            })
            .chain(self.azure_devops_repositories.iter().map(|repository| WritableRepository {
                url: &repository.url,
                checkout_path: repository.checkout_path.as_deref(),
            }))
            .collect::<Vec<_>>();
        toml::to_string_pretty(&WritableConfig {
            server: WritableServer {
                bind: self.listen_address.to_string(),
            },
            storage: WritableStorage {
                sqlite_path: &self.database_path,
            },
            polling: WritablePolling {
                interval_seconds: self.poll_interval.as_secs(),
                max_concurrent_polls: self.max_concurrent_polls.get(),
                max_concurrent_copilot_jobs: self.max_concurrent_copilot_jobs.get(),
            },
            prompts: WritablePrompts {
                overview: &self.prompts.overview,
                interesting: &self.prompts.interesting,
            },
            repositories: &repositories,
            actions: review,
        })
        .map_err(|error| ConfigError::caused_by("failed to serialize configuration", error))
    }
}

#[derive(Serialize)]
struct WritableConfig<'a> {
    server: WritableServer,
    storage: WritableStorage<'a>,
    polling: WritablePolling,
    prompts: WritablePrompts<'a>,
    repositories: &'a [WritableRepository<'a>],
    #[serde(skip_serializing_if = "Option::is_none")]
    actions: Option<WritableActions<'a>>,
}

#[derive(Serialize)]
struct WritableServer {
    bind: String,
}

#[derive(Serialize)]
struct WritableStorage<'a> {
    sqlite_path: &'a Path,
}

#[derive(Serialize)]
struct WritablePolling {
    interval_seconds: u64,
    max_concurrent_polls: usize,
    max_concurrent_copilot_jobs: usize,
}

#[derive(Serialize)]
struct WritablePrompts<'a> {
    overview: &'a str,
    interesting: &'a str,
}

#[derive(Serialize)]
struct WritableRepository<'a> {
    url: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    checkout_path: Option<&'a Path>,
}

#[derive(Serialize)]
struct WritableActions<'a> {
    review: WritableActionMapping<'a>,
}

#[derive(Serialize)]
struct WritableActionMapping<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    marketplace: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plugin: Option<&'a str>,
    skill: &'a str,
}

fn absolute_path(path: &Path) -> Result<PathBuf, ConfigError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|directory| directory.join(path))
        .map_err(|error| ConfigError::caused_by("failed to determine the configuration file's absolute path", error))
}

fn persist_atomically(path: &Path, contents: &[u8]) -> Result<(), ConfigError> {
    let parent = path
        .parent()
        .ok_or_else(|| ConfigError::new(format!("configuration path `{}` has no parent directory", path.display())))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|error| {
        ConfigError::caused_by(
            format!("failed to create a temporary configuration file beside `{}`", path.display()),
            error,
        )
    })?;
    match fs::metadata(path) {
        Ok(metadata) => temporary
            .as_file()
            .set_permissions(metadata.permissions())
            .map_err(|error| ConfigError::caused_by("failed to preserve configuration file permissions", error))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(ConfigError::caused_by("failed to inspect configuration file permissions", error)),
    }
    temporary
        .write_all(contents)
        .map_err(|error| ConfigError::caused_by("failed to write the updated configuration", error))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| ConfigError::caused_by("failed to flush the updated configuration", error))?;
    temporary
        .persist(path)
        .map_err(|error| ConfigError::caused_by(format!("failed to replace configuration file `{}`", path.display()), error.error))?;
    Ok(())
}

// --- TOML document shape ----------------------------------------------------------------------
//
// Deserialized as-written from the TOML source, then converted and validated into `AppConfig`.
// Kept deliberately dumb (plain strings/integers) so validation failures can be reported with a
// single, comprehensive message rather than as a maze of serde/toml diagnostics.

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    server: RawServer,
    storage: RawStorage,
    polling: RawPolling,
    #[serde(default)]
    prompts: RawPrompts,
    #[serde(default)]
    repositories: Vec<RawRepository>,
    #[serde(default)]
    actions: RawActions,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    bind: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStorage {
    sqlite_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPolling {
    interval_seconds: NonZeroU64,
    max_concurrent_polls: NonZeroUsize,
    max_concurrent_copilot_jobs: NonZeroUsize,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawPrompts {
    overview: Option<String>,
    interesting: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRepository {
    url: String,
    #[serde(default)]
    checkout_path: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawActions {
    review: Option<RawActionMapping>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawActionMapping {
    #[serde(default)]
    marketplace: String,
    #[serde(default)]
    plugin: String,
    skill: String,
}

impl RawConfig {
    fn into_app_config(self) -> Result<AppConfig, ConfigError> {
        let mut problems = Vec::new();

        let listen_address = validate_bind_address(&self.server.bind, &mut problems);
        let database_path = validate_sqlite_path(&self.storage.sqlite_path, &mut problems);
        let prompts = validate_prompts(self.prompts, &mut problems);
        let (github_repositories, azure_devops_repositories) = validate_repositories(self.repositories, &mut problems);
        validate_unique_checkout_paths(&github_repositories, &azure_devops_repositories, &mut problems);
        let review_action = self.actions.review.as_ref().map_or_else(default_review_action, |review| {
            validate_action_mapping(review, "review", &mut problems)
        });

        if !problems.is_empty() {
            return Err(ConfigError::new(problems.join("; ")));
        }

        Ok(AppConfig {
            config_path: None,
            config_file_loaded: false,
            listen_address,
            database_path,
            poll_interval: Duration::from_secs(self.polling.interval_seconds.get()),
            max_concurrent_polls: self.polling.max_concurrent_polls,
            max_concurrent_copilot_jobs: self.polling.max_concurrent_copilot_jobs,
            github_repositories,
            azure_devops_repositories,
            prompts,
            review_action,
        })
    }
}

fn validate_bind_address(bind: &str, problems: &mut Vec<String>) -> SocketAddr {
    let fallback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

    match bind.parse::<SocketAddr>() {
        Ok(address) if !address.ip().is_loopback() => {
            problems.push(format!("server.bind `{address}` must be a loopback address (127.0.0.0/8 or ::1)"));
            fallback
        }
        Ok(address) if address.port() == 0 => {
            problems.push("server.bind port must be non-zero".to_owned());
            fallback
        }
        Ok(address) => address,
        Err(error) => {
            problems.push(format!("server.bind `{bind}` is not a valid socket address: {error}"));
            fallback
        }
    }
}

fn validate_sqlite_path(sqlite_path: &str, problems: &mut Vec<String>) -> PathBuf {
    let trimmed = sqlite_path.trim();
    if trimmed.is_empty() {
        problems.push("storage.sqlite_path must not be empty".to_owned());
        return PathBuf::new();
    }

    PathBuf::from(trimmed)
}

fn validate_prompts(raw: RawPrompts, problems: &mut Vec<String>) -> AnalysisPrompts {
    AnalysisPrompts {
        overview: validate_prompt(raw.overview, DEFAULT_OVERVIEW_PROMPT, "prompts.overview", problems),
        interesting: validate_prompt(raw.interesting, DEFAULT_INTEREST_PROMPT, "prompts.interesting", problems),
    }
}

fn validate_prompt(configured: Option<String>, default: &str, field: &str, problems: &mut Vec<String>) -> String {
    let Some(configured) = configured else {
        return default.to_owned();
    };
    let trimmed = configured.trim();
    if trimmed.is_empty() {
        problems.push(format!("{field} must not be empty when configured"));
        return default.to_owned();
    }
    trimmed.to_owned()
}

fn validate_checkout_path(checkout_path: Option<String>, context: &str, problems: &mut Vec<String>) -> Option<PathBuf> {
    let checkout_path = checkout_path?;
    let trimmed = checkout_path.trim();
    if trimmed.is_empty() {
        problems.push(format!("{context} checkout_path must not be empty when present"));
        return None;
    }

    Some(PathBuf::from(trimmed))
}

enum ParsedRepository {
    GitHub {
        url: String,
        owner: String,
        name: String,
    },
    AzureDevOps {
        url: String,
        organization: String,
        project: String,
        repository: String,
    },
}

fn validate_repositories(
    repositories: Vec<RawRepository>,
    problems: &mut Vec<String>,
) -> (Vec<GitHubRepository>, Vec<AzureDevOpsRepository>) {
    let mut seen = std::collections::HashSet::new();
    let mut github = Vec::new();
    let mut azure_devops = Vec::new();

    for repository in repositories {
        let parsed = match parse_repository_url(&repository.url) {
            Ok(parsed) => parsed,
            Err(error) => {
                problems.push(error.to_string());
                continue;
            }
        };
        let checkout_path = validate_checkout_path(
            repository.checkout_path,
            &format!("repository `{}`", repository.url.trim()),
            problems,
        );
        match parsed {
            ParsedRepository::GitHub { url, owner, name } => {
                let key = format!("github/{}/{}", owner.to_lowercase(), name.to_lowercase());
                if !seen.insert(key) {
                    problems.push(format!("duplicate GitHub repository `{owner}/{name}`"));
                    continue;
                }
                github.push(GitHubRepository {
                    url,
                    owner,
                    name,
                    checkout_path,
                });
            }
            ParsedRepository::AzureDevOps {
                url,
                organization,
                project,
                repository,
            } => {
                let key = format!(
                    "azure_devops/{}/{}/{}",
                    organization.to_lowercase(),
                    project.to_lowercase(),
                    repository.to_lowercase()
                );
                if !seen.insert(key) {
                    problems.push(format!("duplicate Azure DevOps repository `{organization}/{project}/{repository}`"));
                    continue;
                }
                azure_devops.push(AzureDevOpsRepository {
                    url,
                    organization,
                    project,
                    repository,
                    checkout_path,
                });
            }
        }
    }

    (github, azure_devops)
}

fn parse_repository_url(raw: &str) -> Result<ParsedRepository, ConfigError> {
    let trimmed = raw.trim();
    let uri = trimmed
        .parse::<http::Uri>()
        .map_err(|error| ConfigError::caused_by(format!("repository URL `{trimmed}` is invalid"), error))?;
    if uri.scheme_str() != Some("https") || uri.query().is_some() {
        return Err(ConfigError::new(format!(
            "repository URL `{trimmed}` must be an HTTPS repository root URL without a query"
        )));
    }
    let authority = uri
        .authority()
        .ok_or_else(|| ConfigError::new(format!("repository URL `{trimmed}` has no host")))?;
    if authority.as_str().contains('@') || authority.port_u16().is_some_and(|port| port != 443) {
        return Err(ConfigError::new(format!(
            "repository URL `{trimmed}` must not contain credentials or a non-HTTPS port"
        )));
    }
    let host = authority.host().to_ascii_lowercase();
    let segments = uri
        .path()
        .trim_matches('/')
        .split('/')
        .map(decode_url_segment)
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| ConfigError::new(format!("repository URL `{trimmed}` contains invalid path encoding")))?;
    if segments.iter().any(|segment| !valid_repository_segment(segment)) {
        return Err(ConfigError::new(format!(
            "repository URL `{trimmed}` contains an invalid repository coordinate"
        )));
    }
    let normalized_url = normalize_repository_url(trimmed);

    if host == "github.com" && segments.len() == 2 {
        let name = segments[1].strip_suffix(".git").unwrap_or(&segments[1]).to_owned();
        if !valid_repository_segment(&name) {
            return Err(ConfigError::new(format!(
                "repository URL `{trimmed}` has an invalid repository name"
            )));
        }
        return Ok(ParsedRepository::GitHub {
            url: normalized_url,
            owner: segments[0].clone(),
            name,
        });
    }

    if host == "dev.azure.com" && segments.len() == 4 && segments[2].eq_ignore_ascii_case("_git") {
        let repository = strip_git_suffix(&segments[3]);
        if !valid_repository_segment(&repository) {
            return Err(ConfigError::new(format!(
                "repository URL `{trimmed}` has an invalid repository name"
            )));
        }
        return Ok(ParsedRepository::AzureDevOps {
            url: normalized_url,
            organization: segments[0].clone(),
            project: segments[1].clone(),
            repository,
        });
    }

    if let Some(organization) = host.strip_suffix(".visualstudio.com")
        && segments.len() == 3
        && segments[1].eq_ignore_ascii_case("_git")
        && valid_repository_segment(organization)
    {
        let repository = strip_git_suffix(&segments[2]);
        if !valid_repository_segment(&repository) {
            return Err(ConfigError::new(format!(
                "repository URL `{trimmed}` has an invalid repository name"
            )));
        }
        return Ok(ParsedRepository::AzureDevOps {
            url: normalized_url,
            organization: organization.to_owned(),
            project: segments[0].clone(),
            repository,
        });
    }

    Err(ConfigError::new(format!(
        "repository URL `{trimmed}` must be `https://github.com/owner/repository` or \
         `https://dev.azure.com/organization/project/_git/repository`"
    )))
}

fn strip_git_suffix(value: &str) -> String {
    value.strip_suffix(".git").unwrap_or(value).to_owned()
}

fn normalize_repository_url(value: &str) -> String {
    let without_slash = value.trim_end_matches('/');
    without_slash.strip_suffix(".git").unwrap_or(without_slash).to_owned()
}

fn valid_repository_segment(value: &str) -> bool {
    !value.is_empty() && value != "." && value != ".." && !value.contains(['/', '\\', '\0'])
}

fn decode_url_segment(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return None;
            }
            let high = (bytes[index + 1] as char).to_digit(16)?;
            let low = (bytes[index + 2] as char).to_digit(16)?;
            #[expect(clippy::cast_possible_truncation, reason = "a pair of hexadecimal nibbles fits in one byte")]
            decoded.push((high * 16 + low) as u8);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// Two repositories writing into the same checkout directory would corrupt each other's working
/// tree, so configured checkout paths must be pairwise distinct across both providers.
fn validate_unique_checkout_paths(
    github_repositories: &[GitHubRepository],
    azure_devops_repositories: &[AzureDevOpsRepository],
    problems: &mut Vec<String>,
) {
    let mut seen = std::collections::HashSet::new();

    let all_paths = github_repositories
        .iter()
        .filter_map(|repository| repository.checkout_path.as_ref())
        .chain(
            azure_devops_repositories
                .iter()
                .filter_map(|repository| repository.checkout_path.as_ref()),
        );

    for path in all_paths {
        if !seen.insert(path) {
            problems.push(format!("checkout_path `{}` is used by more than one repository", path.display()));
        }
    }
}

fn validate_action_mapping(raw: &RawActionMapping, label: &str, problems: &mut Vec<String>) -> ActionMapping {
    let marketplace = raw.marketplace.trim().to_owned();
    let plugin = raw.plugin.trim().to_owned();
    let skill = raw.skill.trim().to_owned();

    if marketplace.is_empty() != plugin.is_empty() {
        problems.push(format!(
            "actions.{label}.marketplace and actions.{label}.plugin must either both be configured or both be omitted"
        ));
    }
    if skill.is_empty() {
        problems.push(format!("actions.{label}.skill must not be empty"));
    }

    ActionMapping {
        marketplace,
        plugin,
        skill,
    }
}

#[cfg(test)]
mod tests {
    use analysis::AnalysisPrompts;

    use super::{AppConfig, ConfigError, ConfigUpdate, ConfigUpdateError, ParsedRepository, parse_repository_url};

    fn valid_config() -> String {
        r#"
            [server]
            bind = "127.0.0.1:8787"

            [storage]
            sqlite_path = "dashboard.sqlite3"

            [polling]
            interval_seconds = 300
            max_concurrent_polls = 4
            max_concurrent_copilot_jobs = 2

            [prompts]
            overview = "summarize the change"
            interesting = "flag anything security related"

            [[repositories]]
            url = "https://github.com/octocat/hello-world"

            [[repositories]]
            url = "https://github.com/octocat/second-repo"
            checkout_path = "/checkouts/second-repo"

            [[repositories]]
            url = "https://dev.azure.com/contoso/widgets/_git/widgets-api"

            [actions.review]
            marketplace = "github"
            plugin = "review-lens"
            skill = "review"
        "#
        .to_owned()
    }

    fn assert_rejected(source: &str, expected_fragment: &str) {
        let error = AppConfig::from_toml_str(source).expect_err("expected validation to fail");
        assert!(
            error.to_string().contains(expected_fragment),
            "expected error to contain `{expected_fragment}`, got `{error}`"
        );
    }

    #[test]
    fn valid_document_parses_and_validates() {
        let config = AppConfig::from_toml_str(&valid_config()).expect("valid configuration should parse");

        assert_eq!(config.listen_address.to_string(), "127.0.0.1:8787");
        assert_eq!(config.database_path.to_str(), Some("dashboard.sqlite3"));
        assert_eq!(config.poll_interval.as_secs(), 300);
        assert_eq!(config.max_concurrent_polls.get(), 4);
        assert_eq!(config.max_concurrent_copilot_jobs.get(), 2);
        assert_eq!(config.github_repositories.len(), 2);
        assert_eq!(config.azure_devops_repositories.len(), 1);
        assert_eq!(config.github_repositories[0].owner, "octocat");
        assert_eq!(config.azure_devops_repositories[0].project, "widgets");
        assert_eq!(config.prompts.overview, "summarize the change");
        assert_eq!(config.prompts.interesting, "flag anything security related");
        assert_eq!(config.review_action.plugin, "review-lens");
    }

    #[test]
    fn documented_example_config_stays_valid() {
        let config = AppConfig::from_toml_str(include_str!("../pr-review-dashboard.example.toml"))
            .expect("documented example configuration should parse");
        assert_eq!(config.github_repositories.len(), 1);
        assert_eq!(config.azure_devops_repositories.len(), 1);
        assert_eq!(config.review_action.skill, "review-lens");
        assert!(config.review_action.plugin.is_empty());
    }

    #[test]
    fn repository_urls_determine_provider_and_coordinates() {
        let github = parse_repository_url("https://github.com/rust-lang/rust.git/").expect("GitHub URL should parse");
        let azure = parse_repository_url("https://dev.azure.com/contoso/My%20Project/_git/widgets").expect("Azure DevOps URL should parse");
        let legacy = parse_repository_url("https://contoso.visualstudio.com/My%20Project/_git/widgets")
            .expect("legacy Azure DevOps URL should parse");

        assert!(matches!(
            github,
            ParsedRepository::GitHub { owner, name, url }
                if owner == "rust-lang" && name == "rust" && url == "https://github.com/rust-lang/rust"
        ));
        assert!(matches!(
            azure,
            ParsedRepository::AzureDevOps {
                organization,
                project,
                repository,
                ..
            } if organization == "contoso" && project == "My Project" && repository == "widgets"
        ));
        assert!(matches!(
            legacy,
            ParsedRepository::AzureDevOps {
                organization,
                project,
                repository,
                ..
            } if organization == "contoso" && project == "My Project" && repository == "widgets"
        ));
    }

    #[test]
    fn rejects_non_repository_or_insecure_urls() {
        for url in [
            "http://github.com/octo/widgets",
            "https://github.com/octo/widgets/pull/1",
            "https://dev.azure.com/contoso/project/_git/repo?version=main",
            "https://example.com/octo/widgets",
            "not-a-url",
        ] {
            assert!(parse_repository_url(url).is_err(), "`{url}` should be rejected");
        }
    }

    #[test]
    fn loading_a_file_records_its_absolute_source_path() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("pr-review-dashboard.example.toml");
        let config = AppConfig::load(&path).expect("documented configuration should load");

        assert_eq!(config.config_path.as_deref(), Some(path.as_path()));
        assert!(config.config_file_loaded);
    }

    #[test]
    fn settings_updates_preserve_unrelated_configuration_and_compose() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("dashboard.toml");
        std::fs::write(&path, valid_config()).expect("test configuration should be written");
        let config = AppConfig::load(&path).expect("test configuration should load");

        config
            .persist_update(ConfigUpdate::Prompts {
                overview: "Explain intent and scope.".to_owned(),
                interesting: "Prioritize security changes.".to_owned(),
            })
            .expect("prompt update should persist");
        config
            .persist_update(ConfigUpdate::AddRepository {
                url: "https://github.com/rust-lang/rust".to_owned(),
                checkout_path: None,
            })
            .expect("repository addition should preserve the prompt update");
        config
            .persist_update(ConfigUpdate::RemoveRepository {
                url: "https://github.com/octocat/hello-world".to_owned(),
            })
            .expect("repository removal should preserve earlier updates");

        let updated = AppConfig::load(&path).expect("updated configuration should remain valid");
        assert_eq!(updated.listen_address.to_string(), "127.0.0.1:8787");
        assert_eq!(updated.review_action.plugin, "review-lens");
        assert_eq!(updated.prompts.overview, "Explain intent and scope.");
        assert_eq!(updated.prompts.interesting, "Prioritize security changes.");
        assert!(updated.github_repositories.iter().any(|repository| repository.owner == "rust-lang"));
        assert!(
            !updated
                .github_repositories
                .iter()
                .any(|repository| repository.name == "hello-world")
        );
        assert_eq!(
            updated
                .github_repositories
                .iter()
                .find(|repository| repository.name == "second-repo")
                .and_then(|repository| repository.checkout_path.as_deref()),
            Some(std::path::Path::new("/checkouts/second-repo"))
        );
    }

    #[test]
    fn settings_can_create_the_default_configuration_file() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("dashboard.toml");
        let config = AppConfig::defaults_at(&path).expect("default config path should resolve");
        assert!(!config.config_file_loaded);

        config
            .persist_update(ConfigUpdate::AddRepository {
                url: "https://dev.azure.com/contoso/widgets/_git/widgets-api".to_owned(),
                checkout_path: None,
            })
            .expect("settings should create a missing config file");

        let updated = AppConfig::load(&path).expect("created configuration should load");
        assert!(updated.config_file_loaded);
        assert_eq!(updated.azure_devops_repositories.len(), 1);
        assert_eq!(updated.prompts, AnalysisPrompts::default());
        assert_eq!(updated.review_action.skill, "review-lens");
    }

    #[test]
    fn invalid_settings_update_does_not_replace_the_file() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("dashboard.toml");
        let original = valid_config();
        std::fs::write(&path, &original).expect("test configuration should be written");
        let config = AppConfig::load(&path).expect("test configuration should load");

        let error = config
            .persist_update(ConfigUpdate::AddRepository {
                url: "https://github.com/OCTOCAT/HELLO-WORLD".to_owned(),
                checkout_path: None,
            })
            .expect_err("duplicate repository should be rejected");

        assert!(matches!(error, ConfigUpdateError::Invalid(_)));
        assert_eq!(
            std::fs::read_to_string(path).expect("configuration should remain readable"),
            original
        );
    }

    #[test]
    fn rejects_non_loopback_bind_address() {
        let source = valid_config().replace("127.0.0.1:8787", "0.0.0.0:8787");
        assert_rejected(&source, "must be a loopback address");
    }

    #[test]
    fn rejects_malformed_bind_address() {
        let source = valid_config().replace("127.0.0.1:8787", "not-an-address");
        assert_rejected(&source, "not a valid socket address");
    }

    #[test]
    fn rejects_zero_bind_port() {
        let source = valid_config().replace("127.0.0.1:8787", "127.0.0.1:0");
        assert_rejected(&source, "port must be non-zero");
    }

    #[test]
    fn rejects_empty_sqlite_path() {
        let source = valid_config().replace(r#"sqlite_path = "dashboard.sqlite3""#, r#"sqlite_path = "  ""#);
        assert_rejected(&source, "storage.sqlite_path must not be empty");
    }

    #[test]
    fn rejects_zero_polling_interval() {
        let source = valid_config().replace("interval_seconds = 300", "interval_seconds = 0");
        AppConfig::from_toml_str(&source).unwrap_err();
    }

    #[test]
    fn rejects_zero_concurrency_limit() {
        let source = valid_config().replace("max_concurrent_polls = 4", "max_concurrent_polls = 0");
        AppConfig::from_toml_str(&source).unwrap_err();
    }

    #[test]
    fn rejects_duplicate_github_repository() {
        let source = valid_config().replace("octocat/second-repo", "octocat/hello-world");
        assert_rejected(&source, "duplicate GitHub repository");
    }

    #[test]
    fn duplicate_repository_check_is_case_insensitive() {
        // The second GitHub repository already has owner "octocat"; renaming it to a
        // differently-cased spelling of the first repository's name makes the two collide.
        let source = valid_config().replace("octocat/second-repo", "OCTOCAT/Hello-World");
        assert_rejected(&source, "duplicate GitHub repository");
    }

    #[test]
    fn rejects_duplicate_azure_devops_repository() {
        let source = format!(
            "{}\n{}",
            valid_config(),
            r#"
            [[repositories]]
            url = "https://dev.azure.com/contoso/widgets/_git/widgets-api"
            "#
        );
        assert_rejected(&source, "duplicate Azure DevOps repository");
    }

    #[test]
    fn rejects_shared_checkout_path() {
        // Point the Azure DevOps repository's checkout at the same directory the second GitHub
        // repository already uses.
        let source = valid_config().replace(
            r#"url = "https://dev.azure.com/contoso/widgets/_git/widgets-api""#,
            "url = \"https://dev.azure.com/contoso/widgets/_git/widgets-api\"\n            checkout_path = \"/checkouts/second-repo\"",
        );
        assert_rejected(&source, "is used by more than one repository");
    }

    #[test]
    fn omitted_prompts_use_defaults() {
        let source = valid_config().replace(
            "            [prompts]\n            overview = \"summarize the change\"\n            interesting = \"flag anything security related\"\n\n",
            "",
        );
        let config = AppConfig::from_toml_str(&source).expect("configuration should use prompt defaults");
        assert_eq!(config.prompts, AnalysisPrompts::default());
    }

    #[test]
    fn omitted_review_action_uses_review_lens_from_any_source() {
        let source = valid_config().replace(
            "            [actions.review]\n            marketplace = \"github\"\n            plugin = \"review-lens\"\n            skill = \"review\"\n",
            "",
        );
        let config = AppConfig::from_toml_str(&source).expect("configuration should use review default");

        assert_eq!(config.review_action.skill, "review-lens");
        assert!(config.review_action.marketplace.is_empty());
        assert!(config.review_action.plugin.is_empty());
    }

    #[test]
    fn rejects_empty_configured_prompt() {
        let source = valid_config().replace(r#"interesting = "flag anything security related""#, r#"interesting = "   ""#);
        assert_rejected(&source, "prompts.interesting must not be empty");
    }

    #[test]
    fn rejects_incomplete_action_mapping() {
        let source = valid_config().replace(r#"skill = "review""#, r#"skill = "  ""#);
        assert_rejected(&source, "actions.review.skill must not be empty");
    }

    #[test]
    fn rejects_unknown_fields() {
        let source = format!("{}\n[server]\nbogus = true\n", valid_config());
        AppConfig::from_toml_str(&source).unwrap_err();
    }

    #[test]
    fn accumulates_multiple_problems_in_one_error() {
        let source = valid_config()
            .replace("127.0.0.1:8787", "0.0.0.0:8787")
            .replace(r#"interesting = "flag anything security related""#, r#"interesting = "   ""#);

        let error = AppConfig::from_toml_str(&source).expect_err("expected validation to fail");
        let message = error.to_string();
        assert!(message.contains("must be a loopback address"), "got `{message}`");
        assert!(message.contains("prompts.interesting must not be empty"), "got `{message}`");
    }

    #[test]
    fn config_error_display_is_the_message() {
        let error = ConfigError::new("boom".to_owned());
        assert_eq!(error.to_string(), "boom");
    }

    #[test]
    fn default_config_is_loopback_only() {
        assert!(AppConfig::default().listen_address.ip().is_loopback());
    }
}
