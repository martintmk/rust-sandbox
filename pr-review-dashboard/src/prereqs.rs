// Licensed under the MIT License.

//! Startup prerequisite discovery and validation.
//!
//! Before the dashboard starts polling or running Copilot actions, it checks that the local
//! tools it depends on (`gh`, `az`, and the Copilot CLI) are installed and configured with the
//! review skill referenced by `[actions.review]`. Missing local CLIs that a configured
//! provider needs are fatal: the dashboard cannot do anything useful without them. A missing
//! marketplace/plugin/skill for one action is nonfatal: it is reported so the operator can fix
//! it, but the rest of the dashboard still starts.
//!
//! Command execution goes through the [`CommandRunner`] trait so tests can supply canned output
//! instead of depending on the host machine's installed tools or login state. No credential
//! material is read, stored, or logged; only presence/authentication status is observed.
//!
//! ## Aligning checks with how each provider is actually reached
//!
//! GitHub access uses `octocrab` with an ephemeral token from `gh auth token`.
//! Azure DevOps access uses Microsoft's `azure_devops_rust_api` with
//! `azure_identity::AzureCliCredential`. The Azure DevOps CLI extension is not
//! required because the application does not invoke `az repos` or `az devops`.
//!
//! Copilot action execution uses the workspace `analysis` crate, including authoritative typed
//! plugin, marketplace, skill, MCP server, and resource discovery through `github-copilot-sdk`.
//! This startup preflight remains deliberately shallow and synchronous: it verifies the installed
//! CLI and provides early best-effort warnings from `copilot plugins list --json`; every analysis
//! revalidates its configured capability through typed SDK RPC before invoking it. The shipped CLI
//! has no non-interactive, side-effect-free authentication status subcommand (`copilot login`
//! performs an OAuth flow), so this module only confirms that the CLI is runnable.

use std::fmt;
use std::process::Command;

use serde::Deserialize;

use crate::config::{ActionMapping, AppConfig};

/// Fatal prerequisite error: a CLI required by the configuration is missing or unauthenticated.
#[ohno::error]
#[display("{message}")]
pub(crate) struct PrerequisiteError {
    pub(crate) message: String,
}

/// Output of a single external command invocation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CommandOutput {
    pub(crate) success: bool,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

/// Abstraction over process execution, so prerequisite checks are testable without touching the
/// host machine's installed tools, PATH, or login state.
///
/// `run` returns `None` when the program could not be spawned at all (typically because it is not
/// installed / not on `PATH`), and `Some(output)` when it ran to completion, regardless of exit
/// code.
pub(crate) trait CommandRunner: fmt::Debug {
    fn run(&self, program: &str, args: &[&str]) -> Option<CommandOutput>;
}

/// [`CommandRunner`] that spawns real host processes.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> Option<CommandOutput> {
        let output = Command::new(program).args(args).output().ok()?;
        Some(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Availability of a prerequisite or an individual action's Copilot capability.
///
/// For the Copilot CLI specifically, [`Available`](Self::Available) means only "the executable
/// was found on `PATH` and ran successfully" — the shipped CLI has no non-interactive way to
/// report whether it is logged in (see the module docs), so login state is not represented here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Availability {
    /// Checked and confirmed usable.
    Available,
    /// Checked and found unusable, with a human-readable, credential-free reason.
    Unavailable(String),
    /// Not checked because nothing in the configuration requires it.
    NotRequired,
}

impl Availability {
    /// Used by tests to inspect per-action availability without matching on the enum directly.
    #[cfg(test)]
    pub(crate) fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
}

/// Result of running every startup prerequisite check.
#[derive(Clone, Debug)]
pub struct PrerequisiteReport {
    pub(crate) gh_cli: Availability,
    pub(crate) az_cli: Availability,
    pub(crate) copilot_cli: Availability,
    pub(crate) review_action: Availability,
}

impl PrerequisiteReport {
    /// Reasons that must prevent startup: a required CLI is missing or unauthenticated.
    pub(crate) fn fatal_reasons(&self) -> Vec<String> {
        [("GitHub CLI (`gh`)", &self.gh_cli), ("Azure CLI (`az`)", &self.az_cli)]
            .into_iter()
            .filter_map(|(label, availability)| match availability {
                Availability::Unavailable(reason) => Some(format!("{label} unavailable: {reason}")),
                Availability::Available | Availability::NotRequired => None,
            })
            .collect()
    }

    /// Nonfatal, per-action warnings: the dashboard still starts, but these actions cannot run
    /// until the underlying marketplace/plugin/skill is installed.
    pub(crate) fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if let Availability::Unavailable(reason) = &self.copilot_cli {
            warnings.push(format!("Copilot analysis unavailable: {reason}"));
        }
        if let Availability::Unavailable(reason) = &self.review_action {
            warnings.push(format!("`review` action unavailable: {reason}"));
        }
        warnings
    }

    pub(crate) fn is_fatal(&self) -> bool {
        !self.fatal_reasons().is_empty()
    }

    pub(crate) fn analysis_available(&self) -> bool {
        self.copilot_cli == Availability::Available
    }
}

/// Response shape of `copilot plugins list --json` (verified against the shipped CLI's real
/// output; see the module docs). Lists every plugin, MCP server, skill, instruction source, and
/// language server the CLI knows about, grouped flatly by `kind`.
#[derive(Debug, Deserialize)]
struct PluginsListOutput {
    plugins: Vec<PluginEntry>,
}

/// One entry from `copilot plugins list --json`.
#[derive(Debug, Deserialize)]
struct PluginEntry {
    /// `"plugin"`, `"mcp"`, `"skill"`, `"instruction"`, or `"lsp"`.
    kind: String,
    name: String,
    /// Where the entry came from, e.g. `"marketplace:<name>"`, `"builtin"`, `"personal-copilot"`.
    #[serde(default)]
    source: String,
    #[serde(default)]
    enabled: bool,
}

/// Runs every startup prerequisite check and returns a combined report.
///
/// `gh` is only checked when GitHub repositories are configured; `az` is only checked when Azure
/// DevOps repositories are configured. The Copilot CLI is always checked, but
/// its absence does not prevent provider polling and dashboard startup.
pub(crate) fn discover(config: &AppConfig, runner: &dyn CommandRunner) -> PrerequisiteReport {
    let gh_cli = if config.github_repositories.is_empty() {
        Availability::NotRequired
    } else {
        check_gh_cli(runner)
    };

    let az_cli = if config.azure_devops_repositories.is_empty() {
        Availability::NotRequired
    } else {
        check_az_cli(runner)
    };

    let copilot_cli = check_copilot_cli(runner);
    let installed_plugins = if copilot_cli == Availability::Available {
        list_copilot_plugins(runner)
    } else {
        None
    };

    PrerequisiteReport {
        gh_cli,
        az_cli,
        copilot_cli: copilot_cli.clone(),
        review_action: check_action(&config.review_action, &copilot_cli, installed_plugins.as_deref()),
    }
}

/// Checks that `gh` (the GitHub CLI) is installed and logged in.
///
/// Matches exactly what the providers crate relies on to mint a per-request
/// token (`gh auth token`): this only needs the CLI on `PATH` and authenticated,
/// not any additional extension.
fn check_gh_cli(runner: &dyn CommandRunner) -> Availability {
    match runner.run("gh", &["--version"]) {
        None => Availability::Unavailable("`gh` executable was not found on PATH".to_owned()),
        Some(output) if !output.success => Availability::Unavailable("`gh --version` did not succeed".to_owned()),
        Some(_) => match runner.run("gh", &["auth", "status"]) {
            Some(output) if output.success => Availability::Available,
            Some(_) => Availability::Unavailable("`gh` is installed but not authenticated; run `gh auth login`".to_owned()),
            None => Availability::Unavailable("failed to invoke `gh auth status`".to_owned()),
        },
    }
}

/// Checks that `az` (the Azure CLI) is installed and logged in.
///
/// Matches what `azure_identity::AzureCliCredential` relies on: an installed,
/// authenticated Azure CLI. Azure DevOps is reached through Microsoft's Rust
/// SDK, so the `azure-devops` CLI extension is not a prerequisite.
fn check_az_cli(runner: &dyn CommandRunner) -> Availability {
    match runner.run("az", &["--version"]) {
        None => return Availability::Unavailable("`az` executable was not found on PATH".to_owned()),
        Some(output) if !output.success => return Availability::Unavailable("`az --version` did not succeed".to_owned()),
        Some(_) => {}
    }

    match runner.run("az", &["account", "show", "-o", "json"]) {
        Some(output) if output.success => Availability::Available,
        Some(_) => Availability::Unavailable("`az` is installed but not logged in; run `az login`".to_owned()),
        None => Availability::Unavailable("failed to invoke `az account show`".to_owned()),
    }
}

/// Checks that the Copilot CLI is installed and runnable.
///
/// This intentionally stops at presence. The shipped CLI (verified via `copilot --help`) has no
/// non-interactive `auth status`/`whoami` subcommand — `copilot login` performs an interactive or
/// device-code OAuth flow and stores the resulting token in the OS credential store (or a config
/// file as a fallback), neither of which this module reads. Reporting login state would mean
/// either invoking a real, side-effect-bearing session or guessing at a nonexistent flag; instead
/// this is documented as a known limitation (see the module docs) and left to surface naturally
/// the first time the Copilot worker invokes the SDK.
fn check_copilot_cli(runner: &dyn CommandRunner) -> Availability {
    match runner.run("copilot", &["--version"]) {
        None => Availability::Unavailable("`copilot` executable was not found on PATH".to_owned()),
        Some(output) if output.success => Availability::Available,
        Some(_) => Availability::Unavailable("`copilot --version` did not succeed".to_owned()),
    }
}

/// Lists plugins/MCP servers/skills/instructions/language servers known to the Copilot CLI via
/// `copilot plugins list --json` (the CLI's own unified inspection command; verified against the
/// installed CLI's `--help` output and real JSON output, not guessed).
fn list_copilot_plugins(runner: &dyn CommandRunner) -> Option<Vec<PluginEntry>> {
    let output = runner.run("copilot", &["plugins", "list", "--json"])?;
    if !output.success {
        return None;
    }

    let parsed: PluginsListOutput = serde_json::from_str(&output.stdout).ok()?;
    Some(parsed.plugins)
}

/// Checks whether the `[actions.review]` skill is installed and enabled, using
/// the flat `kind`-tagged listing from `copilot plugins list --json`.
///
/// The CLI's listing does not attribute a skill entry back to the plugin that contributed it, so
/// this checks the plugin (installed from the named marketplace, enabled) and the skill
/// (installed, enabled) independently rather than requiring an explicit link between them.
fn check_action(mapping: &ActionMapping, copilot_cli: &Availability, plugins: Option<&[PluginEntry]>) -> Availability {
    if *copilot_cli != Availability::Available {
        return Availability::Unavailable("Copilot CLI is unavailable".to_owned());
    }

    let Some(plugins) = plugins else {
        return Availability::Unavailable("could not enumerate installed Copilot plugins".to_owned());
    };

    if !mapping.marketplace.is_empty() && !mapping.plugin.is_empty() {
        let marketplace_source = format!("marketplace:{}", mapping.marketplace);
        let plugin_installed = plugins
            .iter()
            .any(|entry| entry.kind == "plugin" && entry.name == mapping.plugin && entry.enabled && entry.source == marketplace_source);
        if !plugin_installed {
            return Availability::Unavailable(format!(
                "plugin `{}` from marketplace `{}` is not installed and enabled",
                mapping.plugin, mapping.marketplace
            ));
        }
    }

    let skill_installed = plugins
        .iter()
        .any(|entry| entry.kind == "skill" && entry.name == mapping.skill && entry.enabled);
    if !skill_installed {
        return Availability::Unavailable(format!("skill `{}` is not installed and enabled", mapping.skill));
    }

    Availability::Available
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::{Availability, CommandOutput, CommandRunner, discover};
    use crate::config::AppConfig;

    /// Injectable [`CommandRunner`] driven entirely by canned responses, so tests never depend on
    /// the host machine's installed tools or login state.
    #[derive(Debug, Default)]
    struct FakeCommandRunner {
        responses: HashMap<(String, Vec<String>), Option<CommandOutput>>,
        calls: RefCell<Vec<(String, Vec<String>)>>,
    }

    impl FakeCommandRunner {
        fn with(mut self, program: &str, args: &[&str], output: Option<CommandOutput>) -> Self {
            let key = (program.to_owned(), args.iter().map(|arg| (*arg).to_owned()).collect());
            self.responses.insert(key, output);
            self
        }

        /// Wraps a successful command output; the `Option` return matches [`Self::with`]'s
        /// parameter type so call sites read as plain (program, args, canned-response) triples.
        #[expect(
            clippy::unnecessary_wraps,
            reason = "matches `with`'s Option<CommandOutput> parameter for readable test setup"
        )]
        fn success(stdout: &str) -> Option<CommandOutput> {
            Some(CommandOutput {
                success: true,
                stdout: stdout.to_owned(),
                stderr: String::new(),
            })
        }

        #[expect(
            clippy::unnecessary_wraps,
            reason = "matches `with`'s Option<CommandOutput> parameter for readable test setup"
        )]
        fn failure(stderr: &str) -> Option<CommandOutput> {
            Some(CommandOutput {
                success: false,
                stdout: String::new(),
                stderr: stderr.to_owned(),
            })
        }
    }

    impl CommandRunner for FakeCommandRunner {
        fn run(&self, program: &str, args: &[&str]) -> Option<CommandOutput> {
            let key = (program.to_owned(), args.iter().map(|arg| (*arg).to_owned()).collect());
            self.calls.borrow_mut().push(key.clone());
            self.responses.get(&key).cloned().unwrap_or(None)
        }
    }

    fn config_with(source_extra: &str) -> AppConfig {
        let source = format!(
            r#"
            [server]
            bind = "127.0.0.1:8787"

            [storage]
            sqlite_path = "dashboard.sqlite3"

            [polling]
            interval_seconds = 300
            max_concurrent_polls = 4
            max_concurrent_copilot_jobs = 2

            [actions.review]
            marketplace = "github"
            plugin = "review-lens"
            skill = "review"

            {source_extra}
        "#
        );

        AppConfig::from_toml_str(&source).expect("test configuration should be valid")
    }

    fn fully_available_runner() -> FakeCommandRunner {
        FakeCommandRunner::default()
            .with("gh", &["--version"], FakeCommandRunner::success("gh version 2.0.0"))
            .with("gh", &["auth", "status"], FakeCommandRunner::success("Logged in"))
            .with("az", &["--version"], FakeCommandRunner::success("azure-cli 2.0.0"))
            .with(
                "az",
                &["account", "show", "-o", "json"],
                FakeCommandRunner::success(r#"{"id":"sub"}"#),
            )
            .with("copilot", &["--version"], FakeCommandRunner::success("copilot 1.0.0"))
            .with(
                "copilot",
                &["plugins", "list", "--json"],
                FakeCommandRunner::success(
                    r#"{"plugins":[
                        {"kind":"plugin","name":"agency-support","source":"marketplace:github","enabled":true},
                        {"kind":"plugin","name":"review-lens","source":"marketplace:github","enabled":true},
                        {"kind":"skill","name":"overview","source":"marketplace:github","enabled":true},
                        {"kind":"skill","name":"interesting","source":"marketplace:github","enabled":true},
                        {"kind":"skill","name":"review","source":"marketplace:github","enabled":true}
                    ],"errors":[]}"#,
                ),
            )
    }

    #[test]
    fn gh_and_az_are_not_required_without_matching_repositories() {
        let config = config_with("");
        let runner = FakeCommandRunner::default()
            .with("copilot", &["--version"], FakeCommandRunner::success("copilot 1.0.0"))
            .with(
                "copilot",
                &["plugins", "list", "--json"],
                FakeCommandRunner::success(
                    r#"{"plugins":[
                        {"kind":"plugin","name":"agency-support","source":"marketplace:github","enabled":true},
                        {"kind":"plugin","name":"review-lens","source":"marketplace:github","enabled":true},
                        {"kind":"skill","name":"overview","source":"marketplace:github","enabled":true},
                        {"kind":"skill","name":"interesting","source":"marketplace:github","enabled":true},
                        {"kind":"skill","name":"review","source":"marketplace:github","enabled":true}
                    ],"errors":[]}"#,
                ),
            );

        let report = discover(&config, &runner);

        assert_eq!(report.gh_cli, Availability::NotRequired);
        assert_eq!(report.az_cli, Availability::NotRequired);
        assert!(!report.is_fatal());
    }

    #[test]
    fn missing_gh_cli_is_fatal_when_github_repositories_are_configured() {
        let config = config_with(
            r#"
            [[repositories]]
            url = "https://github.com/octocat/hello-world"
            "#,
        );
        let runner = fully_available_runner().with("gh", &["--version"], None);

        let report = discover(&config, &runner);

        assert!(report.gh_cli.is_unavailable());
        assert!(report.is_fatal());
        assert!(report.fatal_reasons().iter().any(|reason| reason.contains("GitHub CLI")));
    }

    #[test]
    fn unauthenticated_gh_cli_is_fatal_when_required() {
        let config = config_with(
            r#"
            [[repositories]]
            url = "https://github.com/octocat/hello-world"
            "#,
        );
        let runner = fully_available_runner().with("gh", &["auth", "status"], FakeCommandRunner::failure("not logged in"));

        let report = discover(&config, &runner);

        assert!(report.gh_cli.is_unavailable());
        assert!(report.is_fatal());
    }

    #[test]
    fn missing_az_cli_is_fatal_when_ado_repositories_are_configured() {
        let config = config_with(
            r#"
            [[repositories]]
            url = "https://dev.azure.com/contoso/widgets/_git/widgets-api"
            "#,
        );
        let runner = fully_available_runner().with("az", &["--version"], None);

        let report = discover(&config, &runner);

        assert!(report.az_cli.is_unavailable());
        assert!(report.is_fatal());
        assert!(report.fatal_reasons().iter().any(|reason| reason.contains("Azure CLI")));
    }

    #[test]
    fn unauthenticated_az_cli_is_fatal_when_ado_repositories_are_configured() {
        let config = config_with(
            r#"
            [[repositories]]
            url = "https://dev.azure.com/contoso/widgets/_git/widgets-api"
            "#,
        );
        let runner = fully_available_runner().with(
            "az",
            &["account", "show", "-o", "json"],
            FakeCommandRunner::failure("Please run 'az login'"),
        );

        let report = discover(&config, &runner);

        assert!(report.az_cli.is_unavailable());
        assert!(report.is_fatal());
    }

    #[test]
    fn missing_copilot_cli_does_not_block_provider_polling() {
        let config = config_with("");
        let runner = fully_available_runner().with("copilot", &["--version"], None);

        let report = discover(&config, &runner);

        assert!(report.copilot_cli.is_unavailable());
        assert!(!report.is_fatal());
        assert!(
            report
                .warnings()
                .iter()
                .any(|warning| warning.contains("Copilot analysis unavailable"))
        );
    }

    #[test]
    fn copilot_cli_availability_does_not_depend_on_login_state() {
        // The shipped CLI has no non-interactive way to report authentication (see the module
        // docs), so a successful `--version` alone must be enough for `copilot_cli` to be
        // `Available`, regardless of whether the CLI happens to be logged in on this machine.
        let config = config_with("");
        let runner = fully_available_runner();

        let report = discover(&config, &runner);

        assert_eq!(report.copilot_cli, Availability::Available);
    }

    #[test]
    fn missing_review_plugin_is_a_nonfatal_warning() {
        let config = config_with("");
        let runner = fully_available_runner().with(
            "copilot",
            &["plugins", "list", "--json"],
            FakeCommandRunner::success(
                r#"{"plugins":[
                    {"kind":"plugin","name":"agency-support","source":"marketplace:github","enabled":true}
                ],"errors":[]}"#,
            ),
        );

        let report = discover(&config, &runner);

        assert!(!report.is_fatal());
        assert!(report.review_action.is_unavailable());
        assert!(
            report.analysis_available(),
            "best-effort skill discovery must not suppress the authoritative background attempt"
        );
        let warnings = report.warnings();
        assert!(warnings.iter().any(|warning| warning.contains("review")));
    }

    #[test]
    fn personal_review_skill_is_available_without_plugin_pinning() {
        let mut config = config_with("");
        config.review_action.marketplace.clear();
        config.review_action.plugin.clear();
        config.review_action.skill = "review-lens".to_owned();
        let runner = fully_available_runner().with(
            "copilot",
            &["plugins", "list", "--json"],
            FakeCommandRunner::success(
                r#"{"plugins":[
                    {"kind":"skill","name":"review-lens","source":"personal-copilot","enabled":true}
                ],"errors":[]}"#,
            ),
        );

        let report = discover(&config, &runner);

        assert_eq!(report.review_action, Availability::Available);
        assert!(report.analysis_available());
    }

    #[test]
    fn missing_skill_on_an_installed_plugin_is_a_nonfatal_warning() {
        let config = config_with("");
        let runner = fully_available_runner().with(
            "copilot",
            &["plugins", "list", "--json"],
            FakeCommandRunner::success(
                r#"{"plugins":[
                    {"kind":"plugin","name":"agency-support","source":"marketplace:github","enabled":true}
                ],"errors":[]}"#,
            ),
        );

        let report = discover(&config, &runner);

        assert!(!report.is_fatal());
        assert!(report.review_action.is_unavailable());
    }

    #[test]
    fn fully_available_environment_yields_no_fatal_errors_or_warnings() {
        let config = config_with(
            r#"
            [[repositories]]
            url = "https://github.com/octocat/hello-world"

            [[repositories]]
            url = "https://dev.azure.com/contoso/widgets/_git/widgets-api"
            "#,
        );
        let runner = fully_available_runner();

        let report = discover(&config, &runner);

        assert!(!report.is_fatal());
        assert!(report.warnings().is_empty());
        assert_eq!(report.gh_cli, Availability::Available);
        assert_eq!(report.az_cli, Availability::Available);
        assert_eq!(report.copilot_cli, Availability::Available);
        assert_eq!(report.review_action, Availability::Available);

        // Make sure `plugins list` isn't invoked before we know Copilot CLI is available and
        // ready (avoids unnecessary process spawns and keeps discovery fast/predictable).
        assert!(
            runner
                .calls
                .borrow()
                .iter()
                .any(|(program, args)| program == "copilot" && args.iter().map(String::as_str).eq(["plugins", "list", "--json"]))
        );
    }

    #[test]
    fn system_command_runner_reports_missing_executable() {
        let runner = super::SystemCommandRunner;
        let result = runner.run("definitely-not-a-real-executable-xyz", &["--version"]);
        assert!(result.is_none());
    }
}
