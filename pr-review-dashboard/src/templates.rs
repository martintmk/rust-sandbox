// Licensed under the MIT License.

//! Server-rendered HTML template seam.
//!
//! Templates are registered once at startup and rendered per request through a single
//! [`minijinja::Environment`]. Every template name ends in `.html`, which selects minijinja's
//! HTML auto-escape mode by default: any `{{ value }}` interpolation is escaped for its context, so
//! callers never need to (and must not) hand-assemble HTML strings from untrusted data (pull
//! request titles, branch names, provider/model text, ...). Handlers pass already-typed,
//! serializable context values rather than pre-built HTML fragments, so escaping is always applied
//! by minijinja rather than left to ad hoc string formatting.
//!
//! The dashboard's styling and behavior live entirely in two same-origin static assets
//! (`/static/app.css` and `/static/app.js`, served by [`crate::http_server`]); the layout only
//! links to them. This keeps the response Content-Security-Policy strict — `script-src 'self'` and
//! `style-src 'self'` with no inline `<script>`/`<style>` or event-handler attributes — while still
//! allowing progressive enhancement.

use std::sync::Arc;

use minijinja::Environment;
use serde::Serialize;

/// A page shell shared by every HTML view: the document `<head>` (charset, viewport, the CSRF
/// token as a `<meta>` tag for same-origin JavaScript, and links to the static stylesheet/script),
/// a skip link and primary navigation, and a `<main>` landmark whose body each view fills in.
/// Keeping the shell in one template means every page inherits the same accessible scaffolding and
/// asset wiring without each handler re-declaring it.
const LAYOUT_TEMPLATE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="csrf-token" content="{{ csrf_token }}">
<title>{{ title }}</title>
<link rel="stylesheet" href="/static/app.css">
<script src="/static/app.js" defer></script>
</head>
<body>
<a class="skip-link" href="#main">Skip to main content</a>
<header class="app-header">
<nav class="app-nav" aria-label="Primary">
<a href="/">Dashboard</a>
<a href="/settings">Settings</a>
</nav>
</header>
<main id="main" class="app-main">
<h1>{{ title }}</h1>
<p id="action-status" class="action-status" role="status" aria-live="polite"></p>
{% block content %}{% endblock %}
</main>
</body>
</html>
"##;

/// The dashboard: a filter form plus one accessible table row per tracked pull request, showing
/// provider/repository, title/author, freshness, draft state, the AI overview, and the interest
/// priority and rationale. Every interactive control degrades to a plain link or full-page form.
const DASHBOARD_TEMPLATE: &str = r#"{% extends "layout.html" %}
{% block content %}
<div class="toolbar">
<a class="button" href="/" data-reload role="button">Reload data</a>
<span class="muted">{{ pull_requests | length }} of {{ total }} pull request(s) shown</span>
</div>

<form class="filters" method="get" action="/" aria-label="Filter pull requests">
<input type="hidden" name="sort" value="{{ sort_column }}">
<input type="hidden" name="direction" value="{{ sort_direction }}">
<div class="field">
<label for="filter-provider">Provider</label>
<select id="filter-provider" name="provider">
<option value=""{% if not filters.provider %} selected{% endif %}>All providers</option>
{% for provider in provider_options %}
<option value="{{ provider }}"{% if filters.provider == provider %} selected{% endif %}>{{ provider }}</option>
{% endfor %}
</select>
</div>
<div class="field">
<label for="filter-repository">Repository</label>
<select id="filter-repository" name="repository">
<option value=""{% if not filters.repository %} selected{% endif %}>All repositories</option>
{% for repository in repository_options %}
<option value="{{ repository.id }}"{% if filters.repository == repository.id %} selected{% endif %}>{{ repository.label }}</option>
{% endfor %}
</select>
</div>
<div class="field">
<label for="filter-priority">Priority</label>
<select id="filter-priority" name="priority">
<option value=""{% if not filters.priority %} selected{% endif %}>Any priority</option>
{% for priority in priority_options %}
<option value="{{ priority }}"{% if filters.priority == priority %} selected{% endif %}>{{ priority }}</option>
{% endfor %}
</select>
</div>
<div class="field checkbox">
<input type="checkbox" id="filter-draft" name="draft" value="1"{% if filters.draft %} checked{% endif %}>
<label for="filter-draft">Draft only</label>
</div>
<div class="field checkbox">
<input type="checkbox" id="filter-stale" name="stale" value="1"{% if filters.stale %} checked{% endif %}>
<label for="filter-stale">Stale only</label>
</div>
<div class="field">
<label for="filter-search">Search</label>
<input type="search" id="filter-search" name="q" value="{{ filters.query }}" placeholder="title, author, repository">
</div>
<div class="field actions">
<button type="submit">Apply filters</button>
<a class="button secondary" href="/" role="button">Clear</a>
</div>
</form>

{% if pull_requests %}
<div class="table-scroll">
<table class="pr-table">
<caption>Tracked pull requests</caption>
<thead>
<tr>
<th scope="col" aria-sort="{{ sort.repository.aria_sort }}"><a class="sort-link" href="{{ sort.repository.href }}">Repository <span class="sort-indicator">{{ sort.repository.indicator }}</span></a></th>
<th scope="col" aria-sort="{{ sort.pull_request.aria_sort }}"><a class="sort-link" href="{{ sort.pull_request.href }}">Pull request <span class="sort-indicator">{{ sort.pull_request.indicator }}</span></a></th>
<th scope="col" aria-sort="{{ sort.author.aria_sort }}"><a class="sort-link" href="{{ sort.author.href }}">Author <span class="sort-indicator">{{ sort.author.indicator }}</span></a></th>
<th scope="col" aria-sort="{{ sort.updated.aria_sort }}"><a class="sort-link" href="{{ sort.updated.href }}">Updated <span class="sort-indicator">{{ sort.updated.indicator }}</span></a></th>
<th scope="col" aria-sort="{{ sort.state.aria_sort }}"><a class="sort-link" href="{{ sort.state.href }}">State <span class="sort-indicator">{{ sort.state.indicator }}</span></a></th>
<th scope="col" aria-sort="{{ sort.priority.aria_sort }}"><a class="sort-link" href="{{ sort.priority.href }}">Priority <span class="sort-indicator">{{ sort.priority.indicator }}</span></a></th>
<th scope="col" aria-sort="{{ sort.overview.aria_sort }}"><a class="sort-link" href="{{ sort.overview.href }}">Overview <span class="sort-indicator">{{ sort.overview.indicator }}</span></a></th>
</tr>
</thead>
<tbody>
{% for pr in pull_requests %}
<tr class="pr-row priority-{{ pr.priority }}{% if pr.stale %} is-stale{% endif %}{% if pr.has_error %} has-error{% endif %}{% if pr.analysis_status == "outdated" %} analysis-outdated{% endif %}">
<td>
<span class="provider-badge">{{ pr.provider }}</span>
<span class="repo-name">{{ pr.repository }}</span>
</td>
<td>
<a class="pr-title" href="{{ pr.detail_path }}" title="{{ pr.title }}">#{{ pr.number }} {{ pr.title }}</a>
{% if pr.is_draft %}<span class="badge draft">Draft</span>{% endif %}
</td>
<td>{% if pr.author %}{{ pr.author }}{% else %}<span class="muted">unknown</span>{% endif %}</td>
<td>
<span title="{{ pr.updated_iso }}">{{ pr.updated_age }}</span>
{% if pr.stale %}<span class="badge stale">Stale</span>{% endif %}
</td>
<td><span class="badge state-{{ pr.state }}">{{ pr.state }}</span></td>
<td>
{% if pr.analysis_status == "pending" %}<span class="badge analysis-pending">AI analysis pending</span>
{% else %}
<span class="badge priority-badge priority-{{ pr.priority }}">{{ pr.priority_label }}</span>
{% if pr.analysis_status == "outdated" %}<span class="badge analysis-outdated">Outdated</span>{% endif %}
{% if pr.rationale %}<span class="rationale" title="{{ pr.rationale }}">{{ pr.rationale }}</span>{% endif %}
{% endif %}
</td>
<td>
{% if pr.has_error %}<span class="badge error">Analysis error</span> <span class="overview-snippet muted" title="{{ pr.overview }}">{{ pr.overview }}</span>
{% elif pr.analysis_status == "pending" %}<span class="muted">AI pending</span>
{% elif pr.overview %}<span class="overview-snippet" title="{{ pr.overview }}">{{ pr.overview }}</span>
{% else %}<span class="muted">No AI overview available.</span>{% endif %}
</td>
</tr>
{% endfor %}
</tbody>
</table>
</div>
{% else %}
<p class="empty">No pull requests match the current filters.</p>
{% endif %}

<section aria-labelledby="repos-heading" class="repo-list">
<h2 id="repos-heading">Repositories</h2>
{% if repository_options %}
<ul>
{% for repository in repository_options %}
<li>
<a href="/repositories/{{ repository.id }}/pull-requests">{{ repository.label }}</a>
<a class="button secondary" href="/repositories/{{ repository.id }}/pull-requests" data-reload role="button">Reload</a>
</li>
{% endfor %}
</ul>
{% else %}
<p class="muted">No repositories are configured yet. See <a href="/settings">Settings</a>.</p>
{% endif %}
</section>
{% endblock %}
"#;

/// A single repository's pull request list. Reuses the dashboard's row vocabulary but scoped to
/// one repository, with a repository-level reload control.
const PULL_REQUEST_LIST_TEMPLATE: &str = r#"{% extends "layout.html" %}
{% block content %}
<p><a href="/">&larr; Back to dashboard</a></p>
<div class="toolbar">
<a class="button" href="/repositories/{{ repository_id }}/pull-requests" data-reload role="button">Reload data</a>
</div>
{% if pull_requests %}
<div class="table-scroll">
<table class="pr-table">
<caption>Pull requests</caption>
<thead>
<tr>
<th scope="col" aria-sort="{{ sort.pull_request.aria_sort }}"><a class="sort-link" href="{{ sort.pull_request.href }}">Pull request <span class="sort-indicator">{{ sort.pull_request.indicator }}</span></a></th>
<th scope="col" aria-sort="{{ sort.author.aria_sort }}"><a class="sort-link" href="{{ sort.author.href }}">Author <span class="sort-indicator">{{ sort.author.indicator }}</span></a></th>
<th scope="col" aria-sort="{{ sort.updated.aria_sort }}"><a class="sort-link" href="{{ sort.updated.href }}">Updated <span class="sort-indicator">{{ sort.updated.indicator }}</span></a></th>
<th scope="col" aria-sort="{{ sort.state.aria_sort }}"><a class="sort-link" href="{{ sort.state.href }}">State <span class="sort-indicator">{{ sort.state.indicator }}</span></a></th>
<th scope="col" aria-sort="{{ sort.priority.aria_sort }}"><a class="sort-link" href="{{ sort.priority.href }}">Priority <span class="sort-indicator">{{ sort.priority.indicator }}</span></a></th>
</tr>
</thead>
<tbody>
{% for pr in pull_requests %}
<tr class="pr-row priority-{{ pr.priority }}{% if pr.stale %} is-stale{% endif %}">
<td>
<a class="pr-title" href="{{ pr.detail_path }}" title="{{ pr.title }}">#{{ pr.number }} {{ pr.title }}</a>
{% if pr.is_draft %}<span class="badge draft">Draft</span>{% endif %}
</td>
<td>{% if pr.author %}{{ pr.author }}{% else %}<span class="muted">unknown</span>{% endif %}</td>
<td><span title="{{ pr.updated_iso }}">{{ pr.updated_age }}</span>{% if pr.stale %} <span class="badge stale">Stale</span>{% endif %}</td>
<td><span class="badge state-{{ pr.state }}">{{ pr.state }}</span></td>
<td>
{% if pr.analysis_status == "pending" %}<span class="badge analysis-pending">AI analysis pending</span>
{% else %}<span class="badge priority-badge priority-{{ pr.priority }}">{{ pr.priority_label }}</span>
{% if pr.analysis_status == "outdated" %}<span class="badge analysis-outdated">Outdated</span>{% endif %}
{% endif %}
</td>
</tr>
{% endfor %}
</tbody>
</table>
</div>
{% else %}
<p class="empty">No pull requests have been recorded for this repository yet.</p>
{% endif %}
{% endblock %}
"#;

/// A pull request's detail page: metadata, the validated provider link, AI output and freshness,
/// and a manual analysis form enhanced by `app.js` with a full-page fallback.
const PULL_REQUEST_DETAIL_TEMPLATE: &str = r#"{% extends "layout.html" %}
{% block content %}
<p><a href="/repositories/{{ repository_id }}/pull-requests">&larr; Back to pull requests</a></p>
<h2>#{{ pull_request.number }} {{ pull_request.title }}</h2>
<p class="pr-meta">
<span class="provider-badge">{{ provider }}</span>
<span class="badge state-{{ pull_request.state }}">{{ pull_request.state }}</span>
{% if pull_request.is_draft %}<span class="badge draft">Draft</span>{% endif %}
{% if stale %}<span class="badge stale">Stale</span>{% else %}<span class="badge fresh">Fresh</span>{% endif %}
{% if analysis.status == "pending" %}<span class="badge analysis-pending">AI analysis pending</span>{% endif %}
{% if analysis.status == "outdated" %}<span class="badge analysis-outdated">AI analysis outdated</span>{% endif %}
{% if pull_request.author %}<span>opened by {{ pull_request.author }}</span>{% endif %}
<span title="{{ updated_iso }}">updated {{ updated_age }}</span>
</p>
<p>{{ pull_request.source_branch }} &rarr; {{ pull_request.target_branch }}</p>
{% if external_link %}<p><a href="{{ external_link }}" rel="noopener noreferrer" target="_blank">View pull request on {{ provider }}</a></p>{% endif %}
{% if pull_request.body %}<details class="pr-body"><summary>Description</summary><p>{{ pull_request.body }}</p></details>{% endif %}

<section class="actions-panel" aria-labelledby="actions-heading">
<h2 id="actions-heading">Actions</h2>
<div class="action-buttons">
<form class="action-form" method="post" action="/api/repositories/{{ repository_id }}/pull-requests/{{ pull_request.number }}/analysis">
<input type="hidden" name="csrf_token" value="{{ csrf_token }}">
<button type="submit">{% if analysis.status == "outdated" %}Update AI analysis{% elif analysis.status == "pending" %}Run AI analysis now{% else %}Re-run AI analysis{% endif %}</button>
</form>
</div>
</section>

{% if analysis.status == "outdated" %}
<p class="badge analysis-outdated">This analysis is for an earlier revision. Update it manually when you are ready.</p>
{% endif %}

<section aria-labelledby="overview-heading">
<h2 id="overview-heading">Overview</h2>
{% if analysis.error %}<p class="badge error">Analysis error</p><p>{{ analysis.error }}</p>
{% elif analysis.overview %}<p>{{ analysis.overview }}</p>
{% else %}<p class="muted">Waiting for background AI analysis.</p>{% endif %}
</section>

<section aria-labelledby="interest-heading">
<h2 id="interest-heading">Interest</h2>
{% if analysis.has_interest %}
<p><span class="badge priority-badge priority-{{ analysis.priority }}">{{ analysis.priority_label }}</span></p>
<p>{{ analysis.rationale }}</p>
{% else %}<p class="muted">Waiting for background AI analysis.</p>{% endif %}
</section>

<section aria-labelledby="review-heading">
<h2 id="review-heading">Review</h2>
{% if analysis.has_review %}
<p><span class="badge verdict-{{ analysis.verdict }}">{{ analysis.verdict_label }}</span></p>
<p>{{ analysis.review_summary }}</p>
{% if analysis.findings %}
<ul class="findings">
{% for finding in analysis.findings %}
<li class="priority-{{ finding.severity }}">
<span class="badge priority-badge priority-{{ finding.severity }}">{{ finding.severity }}</span>
<strong>{{ finding.title }}</strong>
{% if finding.location %}<span class="muted">{{ finding.location }}</span>{% endif %}
<p>{{ finding.details }}</p>
</li>
{% endfor %}
</ul>
{% endif %}
{% else %}<p class="muted">Waiting for background AI analysis.</p>{% endif %}
</section>
{% endblock %}
"#;

/// The settings editor: configuration source, prompts, repositories, review skill mapping, and
/// runtime prerequisites. No secrets, tokens, or credentials are ever surfaced here.
const SETTINGS_TEMPLATE: &str = r#"{% extends "layout.html" %}
{% block content %}
<p><a href="/">&larr; Back to dashboard</a></p>

<section aria-labelledby="config-source-heading">
<h2 id="config-source-heading">Configuration</h2>
<p>Configuration file: <code>{{ configuration_source }}</code></p>
{% if configuration_loaded %}
<p class="muted">The running process loaded this file at startup.</p>
{% elif editable %}
<p class="muted">Built-in defaults are active. Saving a setting creates this file.</p>
{% endif %}
{% if notice %}<p class="settings-notice" role="status">{{ notice }}</p>{% endif %}
{% if not editable %}<p class="muted">This process has no writable configuration path, so editing is disabled.</p>{% endif %}
</section>

<section aria-labelledby="cli-heading">
<h2 id="cli-heading">Command-line tools</h2>
<table class="settings-table">
<caption>Required CLI availability</caption>
<thead><tr><th scope="col">Tool</th><th scope="col">Status</th><th scope="col">Details</th></tr></thead>
<tbody>
{% for tool in tools %}
<tr class="status-{{ tool.status }}">
<th scope="row">{{ tool.name }}</th>
<td><span class="badge status-{{ tool.status }}">{{ tool.status_label }}</span></td>
<td>{% if tool.message %}{{ tool.message }}{% else %}<span class="muted">&mdash;</span>{% endif %}</td>
</tr>
{% endfor %}
</tbody>
</table>
</section>

<section aria-labelledby="prompts-heading">
<h2 id="prompts-heading">Analysis prompts</h2>
{% if editable %}
<form class="settings-form" method="post" action="/settings">
<input type="hidden" name="csrf_token" value="{{ csrf_token }}">
<input type="hidden" name="operation" value="update-prompts">
<div class="field">
<label for="overview-prompt">Overview prompt</label>
<textarea id="overview-prompt" name="overview" required>{{ overview_prompt }}</textarea>
</div>
<div class="field">
<label for="interesting-prompt">Is interesting prompt</label>
<textarea id="interesting-prompt" name="interesting" required>{{ interesting_prompt }}</textarea>
</div>
<div><button type="submit">Save prompts</button></div>
</form>
{% else %}
<table class="settings-table">
<caption>Configurable prompts used directly by Copilot</caption>
<thead><tr><th scope="col">Stage</th><th scope="col">Prompt</th></tr></thead>
<tbody>
{% for prompt in prompts %}
<tr><th scope="row">{{ prompt.name }}</th><td>{{ prompt.prompt }}</td></tr>
{% endfor %}
</tbody>
</table>
{% endif %}
</section>

<section aria-labelledby="actions-map-heading">
<h2 id="actions-map-heading">Review capability</h2>
<table class="settings-table">
<caption>External skill used for the review stage</caption>
<thead><tr><th scope="col">Action</th><th scope="col">Marketplace</th><th scope="col">Plugin</th><th scope="col">Skill</th><th scope="col">Status</th><th scope="col">Details</th></tr></thead>
<tbody>
{% for action in actions %}
<tr class="status-{{ action.status }}">
<th scope="row">{{ action.name }}</th>
<td>{{ action.marketplace }}</td>
<td>{{ action.plugin }}</td>
<td>{{ action.skill }}</td>
<td><span class="badge status-{{ action.status }}">{{ action.status_label }}</span></td>
<td>{% if action.message %}{{ action.message }}{% else %}<span class="muted">&mdash;</span>{% endif %}</td>
</tr>
{% endfor %}
</tbody>
</table>
</section>

<section aria-labelledby="repos-config-heading">
<h2 id="repos-config-heading">Configured repositories</h2>
{% if repositories %}
<table class="settings-table">
<caption>Repositories tracked by this dashboard</caption>
<thead><tr><th scope="col">Provider</th><th scope="col">Repository</th>{% if editable %}<th scope="col">Action</th>{% endif %}</tr></thead>
<tbody>
{% for repository in repositories %}
<tr>
<td><span class="provider-badge">{{ repository.provider }}</span></td>
<td><a href="{{ repository.url }}" rel="noopener noreferrer" target="_blank">{{ repository.url }}</a></td>
{% if editable %}
<td>
<form class="inline-form" method="post" action="/settings">
<input type="hidden" name="csrf_token" value="{{ csrf_token }}">
<input type="hidden" name="operation" value="remove-repository">
<input type="hidden" name="url" value="{{ repository.url }}">
<button type="submit">Remove</button>
</form>
</td>
{% endif %}
</tr>
{% endfor %}
</tbody>
</table>
{% else %}
<p class="muted">No repositories are configured.</p>
{% endif %}
{% if editable %}
<h3>Add repository</h3>
<form class="settings-form" method="post" action="/settings">
<input type="hidden" name="csrf_token" value="{{ csrf_token }}">
<input type="hidden" name="operation" value="add-repository">
<div class="settings-form-grid">
<div class="field"><label for="repository-url">GitHub or Azure DevOps repository URL</label><input id="repository-url" name="url" type="url" placeholder="https://github.com/owner/repository" required></div>
<div class="field"><label for="repository-checkout">Checkout path (optional)</label><input id="repository-checkout" name="checkout_path"></div>
</div>
<div><button type="submit">Add repository</button></div>
</form>
<p class="muted">Saved repository and prompt changes take effect after the dashboard restarts.</p>
{% endif %}
</section>
{% endblock %}
"#;

const ERROR_TEMPLATE: &str = r#"{% extends "layout.html" %}
{% block content %}
<p>{{ message }}</p>
{% endblock %}
"#;

#[ohno::error]
#[display("template rendering failed")]
pub(crate) struct TemplateError;

#[derive(Clone, Debug)]
pub struct Templates {
    environment: Arc<Environment<'static>>,
}

impl Templates {
    pub(crate) fn new() -> Self {
        let mut environment = Environment::new();
        environment
            .add_template("layout.html", LAYOUT_TEMPLATE)
            .expect("built-in layout template is valid");
        environment
            .add_template("dashboard.html", DASHBOARD_TEMPLATE)
            .expect("built-in dashboard template is valid");
        environment
            .add_template("pull_request_list.html", PULL_REQUEST_LIST_TEMPLATE)
            .expect("built-in pull request list template is valid");
        environment
            .add_template("pull_request_detail.html", PULL_REQUEST_DETAIL_TEMPLATE)
            .expect("built-in pull request detail template is valid");
        environment
            .add_template("settings.html", SETTINGS_TEMPLATE)
            .expect("built-in settings template is valid");
        environment
            .add_template("error.html", ERROR_TEMPLATE)
            .expect("built-in error template is valid");
        Self {
            environment: Arc::new(environment),
        }
    }

    /// Renders the named template with an auto-escaped HTML context. Callers pass already-typed,
    /// serializable context values rather than pre-built HTML fragments, so escaping is always
    /// applied by minijinja rather than left to ad hoc string formatting.
    pub(crate) fn render(&self, name: &str, context: impl Serialize) -> Result<String, TemplateError> {
        let template = self.environment.get_template(name).map_err(TemplateError::caused_by)?;
        template.render(context).map_err(TemplateError::caused_by)
    }
}

#[cfg(test)]
mod tests {
    use serde::Serialize;

    use super::Templates;

    #[derive(Serialize)]
    struct Context {
        title: &'static str,
        csrf_token: &'static str,
        message: &'static str,
    }

    #[test]
    fn escapes_untrusted_content_by_default() {
        let templates = Templates::new();
        let rendered = templates
            .render(
                "error.html",
                Context {
                    title: "Error",
                    csrf_token: "token",
                    message: "<script>alert(1)</script>",
                },
            )
            .expect("error.html should render");
        assert!(!rendered.contains("<script>alert(1)</script>"));
        assert!(rendered.contains("&lt;script&gt;"));
    }

    #[test]
    fn layout_links_static_assets_and_exposes_csrf_meta() {
        let templates = Templates::new();
        let rendered = templates
            .render(
                "error.html",
                Context {
                    title: "Error",
                    csrf_token: "abc123",
                    message: "hello",
                },
            )
            .expect("error.html should render");
        assert!(rendered.contains(r#"<link rel="stylesheet" href="/static/app.css">"#));
        assert!(rendered.contains(r#"<script src="/static/app.js" defer></script>"#));
        assert!(rendered.contains(r#"<meta name="csrf-token" content="abc123">"#));
        assert!(rendered.contains(r##"<a class="skip-link" href="#main">"##));
        assert!(rendered.contains(r#"aria-live="polite""#));
    }

    #[test]
    fn renders_every_registered_template_with_minimal_context() {
        let templates = Templates::new();
        let contexts = [
            (
                "dashboard.html",
                minijinja::context! {
                    title => "t",
                    csrf_token => "c",
                    total => 0,
                    pull_requests => Vec::<minijinja::Value>::new(),
                    provider_options => Vec::<minijinja::Value>::new(),
                    repository_options => Vec::<minijinja::Value>::new(),
                    priority_options => Vec::<minijinja::Value>::new(),
                    filters => minijinja::context! {
                        provider => minijinja::Value::UNDEFINED,
                        repository => minijinja::Value::UNDEFINED,
                        priority => minijinja::Value::UNDEFINED,
                        draft => false,
                        stale => false,
                        query => "",
                    },
                },
            ),
            (
                "pull_request_list.html",
                minijinja::context! {
                    title => "t",
                    csrf_token => "c",
                    repository_id => 1,
                    pull_requests => Vec::<minijinja::Value>::new(),
                },
            ),
            (
                "settings.html",
                minijinja::context! {
                    title => "t",
                    csrf_token => "c",
                    configuration_source => "defaults",
                    configuration_loaded => false,
                    tools => Vec::<minijinja::Value>::new(),
                    prompts => Vec::<minijinja::Value>::new(),
                    actions => Vec::<minijinja::Value>::new(),
                    repositories => Vec::<minijinja::Value>::new(),
                },
            ),
            (
                "error.html",
                minijinja::context! { title => "t", csrf_token => "c", message => "m" },
            ),
        ];
        for (name, context) in contexts {
            templates
                .render(name, context)
                .unwrap_or_else(|error| panic!("{name} should render with a minimal context: {error}"));
        }
    }
}
