# PR review dashboard

`pr-review-dashboard` is a local, server-rendered dashboard that polls configured
GitHub and Azure DevOps repositories, stores pull request snapshots in SQLite,
and uses GitHub Copilot to generate an overview, interest rating, and review.

Provider access goes through the workspace [`providers`](../providers) crate,
which wraps vendor SDKs rather than using a hand-written HTTP transport:

- GitHub: [`octocrab`](https://crates.io/crates/octocrab), authenticated with an
  ephemeral token from `gh auth token`.
- Azure DevOps: Microsoft's
  [`azure_devops_rust_api`](https://crates.io/crates/azure_devops_rust_api),
  authenticated through `azure_identity::AzureCliCredential`.
- Copilot: the workspace [`analysis`](../analysis) crate, which wraps the
  official [`github-copilot-sdk`](https://crates.io/crates/github-copilot-sdk)
  and uses the already-installed Copilot CLI.

## Install and build

Use a current stable Rust toolchain with edition 2024 support:

```powershell
rustup toolchain install stable
rustup default stable
cargo build --release -p pr-review-dashboard
```

The binary is written to `target\release\pr-review-dashboard.exe` on Windows
(`target/release/pr-review-dashboard` elsewhere). You can also install it from
the workspace:

```powershell
cargo install --path pr-review-dashboard
```

The workspace `.cargo\config.toml` sets `COPILOT_SKIP_CLI_DOWNLOAD=1`; the SDK
must use the Copilot CLI installed on `PATH`. When building the crate outside
this workspace, set that environment variable yourself. At runtime,
`COPILOT_CLI_PATH` may name an explicit Copilot executable.

## Authenticate local tools

Install the CLIs required by the repositories you configure, then authenticate
them as the same OS user that runs the dashboard:

```powershell
# Required for configured GitHub repositories
gh auth login
gh auth status

# Required for configured Azure DevOps repositories
az login
az account show

# Always required because every analysis uses Copilot
copilot login
copilot --version
copilot plugins list --json
```

The Azure DevOps CLI extension is not required; the app does not invoke
`az devops` or `az repos`. Startup can confirm that Copilot is installed, but
the CLI has no side-effect-free login-status command, so an expired Copilot
login is reported when the first analysis runs. Missing Copilot tooling does
not block provider polling or the read-only dashboard.

## Configure

Copy [`pr-review-dashboard.example.toml`](pr-review-dashboard.example.toml),
edit every placeholder, and either place it at
`pr-review-dashboard.toml` in the process working directory or set:

```powershell
$env:PR_REVIEW_DASHBOARD_CONFIG = "D:\path\to\pr-review-dashboard.toml"
```

Configuration is strict: unknown fields, duplicate repositories, shared
checkout paths, empty configured prompts or review mappings, zero concurrency,
port `0`, and non-loopback bind addresses are rejected. The settings page shows
the exact configuration file that was loaded. It can update the overview and
interest prompts and add or remove repositories by URL.
Edits are validated and atomically saved to that file; restart the dashboard to
apply them to polling and analysis services.

### Repositories and local checkouts

Each `[[repositories]]` entry requires only its HTTPS repository root URL. The
dashboard derives the provider and coordinates. Supported forms are:

- `https://github.com/OWNER/REPOSITORY`
- `https://dev.azure.com/ORGANIZATION/PROJECT/_git/REPOSITORY`
- `https://ORGANIZATION.visualstudio.com/PROJECT/_git/REPOSITORY`

GitHub URLs may end in `.git`; trailing slashes are accepted. Pull-request,
file, query-bearing, and non-HTTPS URLs are rejected.

`checkout_path` is optional. Without it, Copilot receives provider metadata,
the pull request body, and the validated pull request URL. With it, Copilot may
also use read-only `view`, `rg`, and `glob` tools inside the canonicalized
directory. The dashboard never clones, fetches, switches, or updates that
checkout; keep it at the revision you want analyzed. Writes, shell commands,
host git operations, and reads outside the checkout are denied.

### Analysis prompts and review skill

Overview and interest use ordinary prompts. Configure them with
`prompts.overview` and `prompts.interesting`, or omit either value to use its
built-in default. They do not require external skills.

Review defaults to the unique enabled `review-lens` skill from any source,
including personal skills. Override it with `[actions.review].skill`. Optional
`marketplace` and `plugin` fields can pin the skill to a specific installed
plugin. Inspect available skills with:

```powershell
copilot skill list --json
copilot plugins list --json
```

Marketplace plugins are installed with a spec such as:

```powershell
copilot plugins install PLUGIN@MARKETPLACE
```

The startup check is best-effort. Every analysis performs authoritative typed
skill discovery for the review stage through `github-copilot-sdk`. The
configured review skill must be enabled, user-invocable, and unambiguous; when
plugin provenance is configured, it must also match that plugin.

### Polling and concurrency

The scheduler polls once at startup and then every
`polling.interval_seconds`. Repository polls are bounded by
`max_concurrent_polls`, never overlap for the same repository, and retain the
last successful cache when a provider fails. Missing pull requests are marked
closed only after a successful complete poll. Draft pull requests are ignored;
their full details are not fetched and they are not stored or analyzed.

The dashboard's **Updated** column shows the provider-reported PR update time
(including a new source commit), not the time the dashboard last polled it.
Cache staleness is tracked separately from that displayed timestamp.

PR tables sort by **Updated** descending by default. Select any column heading
to sort by that column; selecting the active heading toggles ascending and
descending order while preserving dashboard filters.

Copilot analyses are separately bounded by `max_concurrent_copilot_jobs`.
Startup polling only fetches and stores pull requests. A delayed secondary
background scan queues an initial analysis for each open pull request with no
analysis history. The queue and its deduplication state are transient.

A later commit does not automatically spend more AI credits. The dashboard
continues showing the previous result with an **AI analysis outdated** badge and
offers a manual **Update AI analysis** action. Reload links only render persisted
data; they do not force a provider poll or an analysis.

## Data location

SQLite is created at `storage.sqlite_path`. Relative paths are resolved from
the process working directory, not from the configuration file's directory.
The parent directory must already exist. SQLite WAL sidecars (`-wal`, `-shm`)
may exist while the process is running.

The database contains exactly three domain tables: `repositories`,
`pull_requests` (raw provider metadata and bodies), and `analyses` (results for
PR revisions). Poll runs, queue state, and job history are not persisted. It
does **not** store GitHub, Azure, or Copilot tokens. Treat the database and its
backups as sensitive source and review data.

If no configuration file exists, built-in defaults bind to
`127.0.0.1:8787`, use `pr-review-dashboard.sqlite3`, and configure no
repositories. The settings editor can create the default configuration file;
restart afterward to apply it.

## Start

From the workspace:

```powershell
cargo run --release -p pr-review-dashboard
```

Or run the installed binary:

```powershell
pr-review-dashboard
```

The process prints its loopback URL after the listener is ready. Open `/` for
the dashboard, `/settings` for prerequisite/action status, or `/health` for
the health probe (`/healthz` remains an alias). Stop with Ctrl+C.

Structured `tracing` logs are enabled at `INFO` by default on stderr. They report the active
configuration and database, repository refresh cycles and PR counts, automatic
analysis queueing, analysis attempts/retries/completions, settings writes, and
shutdown. Prompt bodies, pull request bodies, credentials, and model output are
never logged. Set `RUST_LOG` to override the filter, for example
`RUST_LOG=warn` for quieter output or
`RUST_LOG=pr_review_dashboard=info` for an explicit app filter.

## Local HTTP security

This is deliberately a local-only plain-HTTP service:

- configuration accepts only loopback IP addresses;
- every request validates `Host` against loopback and the configured port;
- mutations are POST-only and require a per-process CSRF token;
- browser `Origin`/`Referer` values are checked when present;
- request targets, headers, and bodies are bounded;
- all responses receive restrictive CSP, framing, MIME, referrer, cache, and
  cross-origin headers; and
- templates auto-escape provider and AI text, while external links must be
  absolute HTTPS URLs.

There is no user authentication or TLS. Any local process running as the user
can connect to the service and read its pages. Do not bind through a proxy,
port-forward it, expose it from a container/VM, or publish it on a network.

## Privacy and AI credits

Provider requests send only normal API calls to the configured forge. Copilot
analysis sends the pull request context, configured prompts, and any
read-only evidence selected from an optional checkout to GitHub Copilot. Review
your organization's provider and Copilot policies before use.

One analysis runs the overview prompt, interest prompt, and configured review
skill sequentially, so it can consume multiple Copilot requests/credits. The UI
action queues one complete analysis document. Failed analyses retry up to three
total attempts with bounded exponential delay. Pending work is not recovered
after a process restart. Automatic analysis occurs only once for an open pull
request that has no analysis history; later revisions require manual updates.

## Troubleshooting

- **Configuration rejected:** compare it with the example. TOML field names are
  exact, `server.bind` must be a nonzero loopback socket, and action strings
  cannot be blank.
- **Database initialization failed:** create the parent directory and check
  permissions. During initial development, delete and recreate databases after
  an incompatible schema change.
- **Port already in use:** choose another loopback port. Listener failure stops
  the application instead of leaving the workers running without a UI.
- **GitHub unavailable:** run `gh auth status`, then `gh auth login`. Ensure the
  token can read every configured repository.
- **Azure DevOps unavailable:** run `az account show`, then `az login`; confirm
  the signed-in identity can read the organization, project, and repository.
- **Copilot analyses fail:** run `copilot login`, then
  `copilot plugins list --json`. Check `/settings` for missing or disabled
  marketplaces, plugins, or skills.
- **Checkout context fails:** verify `checkout_path` exists, is a directory, and
  is readable by the dashboard process.
- **Data is stale:** provider failures preserve the prior cache. Check process
  warnings and credentials, then wait for the next configured poll interval.

## Validate the workspace

```powershell
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
```
