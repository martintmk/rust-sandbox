// Licensed under the MIT License.

//! Local pull request review dashboard.

mod application;
mod bootstrap;
mod config;
mod copilot;
mod error;
mod http_server;
mod polling;
mod prereqs;
mod shutdown;
mod templates;

use std::env;
use std::path::PathBuf;

use config::AppConfig;
use error::AppError;
use prereqs::{PrerequisiteError, SystemCommandRunner};
use tracing_subscriber::EnvFilter;

/// Environment variable used to point the dashboard at a non-default configuration file.
const CONFIG_PATH_ENV_VAR: &str = "PR_REVIEW_DASHBOARD_CONFIG";
const DEFAULT_CONFIG_PATH: &str = "pr-review-dashboard.toml";

#[tokio::main]
async fn main() -> Result<(), AppError> {
    init_tracing()?;
    let config = load_config()?;
    let config_source = config
        .config_path
        .as_ref()
        .map_or_else(|| "built-in defaults".to_owned(), |path| path.display().to_string());
    tracing::info!(
        source = %config_source,
        loaded = config.config_file_loaded,
        "configuration selected"
    );
    tracing::info!(path = %config.database_path.display(), "SQLite database selected");
    tracing::info!(
        github_repositories = config.github_repositories.len(),
        azure_devops_repositories = config.azure_devops_repositories.len(),
        poll_interval_seconds = config.poll_interval.as_secs(),
        max_concurrent_polls = config.max_concurrent_polls.get(),
        max_concurrent_analyses = config.max_concurrent_copilot_jobs.get(),
        "runtime configuration loaded"
    );

    let report = prereqs::discover(&config, &SystemCommandRunner);
    for warning in report.warnings() {
        tracing::warn!(%warning);
    }
    let fatal_reasons = report.fatal_reasons();
    if report.is_fatal() {
        return Err(AppError::caused_by(PrerequisiteError::new(fatal_reasons.join("; "))));
    }
    tracing::info!(
        enabled = report.analysis_available(),
        review_skill = %config.review_action.skill,
        "automatic AI analysis configured"
    );

    bootstrap::build(&config, report).run().await
}

fn init_tracing() -> Result<(), AppError> {
    let filter = match env::var("RUST_LOG") {
        Ok(value) => EnvFilter::try_new(value).map_err(AppError::caused_by)?,
        Err(env::VarError::NotPresent) => EnvFilter::new("info"),
        Err(error) => return Err(AppError::caused_by(error)),
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init()
        .map_err(AppError::caused_by)
}

/// Loads configuration from the path named by [`CONFIG_PATH_ENV_VAR`], falling back to
/// [`DEFAULT_CONFIG_PATH`], or to [`AppConfig::default`] when neither exists.
fn load_config() -> Result<AppConfig, AppError> {
    let path = config_path();
    if path.exists() {
        AppConfig::load(&path).map_err(AppError::caused_by)
    } else {
        AppConfig::defaults_at(&path).map_err(AppError::caused_by)
    }
}

fn config_path() -> PathBuf {
    env::var_os(CONFIG_PATH_ENV_VAR).map_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH), PathBuf::from)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::DEFAULT_CONFIG_PATH;

    #[test]
    fn default_config_path_is_relative() {
        assert!(Path::new(DEFAULT_CONFIG_PATH).is_relative());
    }
}
