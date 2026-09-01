// Licensed under the MIT License.

//! Provider-neutral, normalized representations of pull requests.
//!
//! These types are the common currency between the concrete provider adapters
//! (GitHub, Azure DevOps) and the rest of the application. Every adapter maps
//! its wire format into these structures so downstream code never needs to know
//! which forge a pull request came from.

use std::fmt;

/// Identifies which hosting provider a pull request originates from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    /// GitHub (github.com or GitHub Enterprise).
    GitHub,
    /// Azure DevOps.
    AzureDevOps,
}

impl ProviderKind {
    /// Returns a stable, lowercase identifier for the provider.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::AzureDevOps => "azure_devops",
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A provider-neutral coordinate that locates a repository.
///
/// GitHub repositories are addressed by `owner` and `name`; Azure DevOps
/// repositories additionally require a `project`. The optional `project`
/// field keeps a single type usable for both providers while allowing each
/// adapter to validate the fields it requires.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RepositoryCoordinate {
    /// GitHub owner (user or organization), or the Azure DevOps organization.
    pub owner: String,
    /// Azure DevOps project. Not used by GitHub.
    pub project: Option<String>,
    /// Repository name.
    pub name: String,
}

impl RepositoryCoordinate {
    /// Creates a GitHub-style coordinate (`owner/name`).
    pub fn github(owner: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            owner: owner.into(),
            project: None,
            name: name.into(),
        }
    }

    /// Creates an Azure DevOps coordinate (`organization/project/name`).
    pub fn azure_devops(organization: impl Into<String>, project: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            owner: organization.into(),
            project: Some(project.into()),
            name: name.into(),
        }
    }
}

impl fmt::Display for RepositoryCoordinate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.project {
            Some(project) => write!(f, "{}/{}/{}", self.owner, project, self.name),
            None => write!(f, "{}/{}", self.owner, self.name),
        }
    }
}

/// A pull request number as reported by the originating provider.
///
/// GitHub calls this the pull request "number" and Azure DevOps the
/// "pull request id"; both are stable positive integers within a repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PullRequestNumber(pub u64);

impl fmt::Display for PullRequestNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Normalized lifecycle state of a pull request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PullRequestState {
    /// The pull request is open and awaiting completion.
    Open,
    /// The pull request was merged into its target branch.
    Merged,
    /// The pull request was closed/abandoned without merging.
    Closed,
}

/// A person referenced by a pull request (author or reviewer).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserRef {
    /// Stable login/handle when available (e.g. GitHub login).
    pub login: Option<String>,
    /// Human-friendly display name when available.
    pub display_name: Option<String>,
}

/// A label/tag attached to a pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    /// The label text.
    pub name: String,
}

/// A reviewer and their current vote/decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reviewer {
    /// The reviewing user.
    pub user: UserRef,
    /// Normalized decision for this reviewer.
    pub decision: ReviewDecision,
}

/// A normalized reviewer decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewDecision {
    /// The reviewer approved the changes.
    Approved,
    /// The reviewer requested changes / rejected.
    ChangesRequested,
    /// The reviewer is waiting or has an unknown/neutral state.
    Pending,
}

/// A lightweight, provider-neutral summary of a pull request.
///
/// Summaries are what list/pagination operations yield. They carry enough to
/// render a dashboard row and to detect changes between polls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestSummary {
    /// Originating provider.
    pub provider: ProviderKind,
    /// Repository the pull request belongs to.
    pub repository: RepositoryCoordinate,
    /// Provider-assigned number/id.
    pub number: PullRequestNumber,
    /// Title text.
    pub title: String,
    /// Normalized lifecycle state.
    pub state: PullRequestState,
    /// `true` when the pull request is a draft/work-in-progress.
    pub is_draft: bool,
    /// Author, when known.
    pub author: Option<UserRef>,
    /// Source branch reference (short name when available).
    pub source_branch: Option<String>,
    /// Target branch reference (short name when available).
    pub target_branch: Option<String>,
    /// The commit id (SHA) at the tip of the source branch.
    ///
    /// GitHub reports this as `head.sha`; Azure DevOps reports it as
    /// `last_merge_source_commit.commit_id` (the commit last observed at the
    /// tip of the source ref, refreshed by the service on every push). This
    /// is the only reliable signal that a source branch was pushed to when
    /// no other summary/detail field changed, so it must be included in the
    /// revision fingerprint used to detect new work.
    pub source_commit_sha: Option<String>,
    /// Browser URL for the pull request.
    pub url: Option<String>,
    /// ISO-8601 creation timestamp as reported by the provider.
    pub created_at: Option<String>,
    /// ISO-8601 last-updated timestamp as reported by the provider.
    pub updated_at: Option<String>,
}

/// A fully-detailed, provider-neutral pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestDetail {
    /// The summary fields shared with list results.
    pub summary: PullRequestSummary,
    /// Description/body text.
    pub body: Option<String>,
    /// Labels attached to the pull request.
    pub labels: Vec<Label>,
    /// Reviewers and their decisions.
    pub reviewers: Vec<Reviewer>,
}
