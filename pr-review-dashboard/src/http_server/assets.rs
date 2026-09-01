// Licensed under the MIT License.

//! Static, same-origin dashboard assets: one stylesheet and one script, embedded in the binary
//! and served verbatim from `/static/app.css` and `/static/app.js`.
//!
//! Serving both from the server's own origin is what lets the response Content-Security-Policy
//! stay strict (`script-src 'self'; style-src 'self'` with no inline scripts, styles, or
//! event-handler attributes): the browser loads exactly these two files and nothing else. The
//! script is deliberately small vanilla JavaScript — it reads the per-process CSRF token from the
//! `<meta>` tag and submits the action forms as `fetch` POSTs with that token. With scripting
//! disabled the same forms fall back to a full-page POST (the server accepts the CSRF token from a
//! hidden form field and redirects back).

/// The dashboard stylesheet. Plain, dependency-free CSS: a readable layout, accessible focus
/// styles, and color-coded badges for priority, freshness, and status.
pub(super) const APP_CSS: &str = r#":root {
  color-scheme: light dark;
  --fg: #1b1b1f;
  --muted: #5c5f66;
  --bg: #ffffff;
  --surface: #f5f6f8;
  --border: #d5d8de;
  --accent: #0b62d6;
  --high: #b3261e;
  --medium: #a15c00;
  --low: #3a6a00;
  --ignore: #5c5f66;
  --error: #b3261e;
}
* { box-sizing: border-box; }
body {
  margin: 0;
  font-family: system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
  color: var(--fg);
  background: var(--bg);
  line-height: 1.45;
}
a { color: var(--accent); }
.skip-link {
  position: absolute;
  left: -999px;
  top: 0;
  background: var(--accent);
  color: #fff;
  padding: 0.5rem 1rem;
  z-index: 10;
}
.skip-link:focus { left: 0; }
:focus-visible { outline: 3px solid var(--accent); outline-offset: 2px; }
.app-header { border-bottom: 1px solid var(--border); background: var(--surface); }
.app-nav { display: flex; gap: 1rem; padding: 0.75rem 1.5rem; }
.app-main { max-width: 92rem; margin: 0 auto; padding: 1rem 1.25rem; }
h1, h2 { line-height: 1.2; }
.muted { color: var(--muted); }
.toolbar { display: flex; align-items: center; gap: 1rem; margin: 1rem 0; flex-wrap: wrap; }
.action-status:empty { display: none; }
.action-status {
  padding: 0.5rem 0.75rem;
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: 0.375rem;
}
.button, button {
  display: inline-block;
  padding: 0.4rem 0.85rem;
  border: 1px solid var(--accent);
  border-radius: 0.375rem;
  background: var(--accent);
  color: #fff;
  font: inherit;
  text-decoration: none;
  cursor: pointer;
}
.button.secondary, .button.secondary:link { background: transparent; color: var(--accent); }
.filters {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(12rem, 1fr));
  gap: 0.5rem 0.75rem;
  padding: 0.65rem 0.75rem;
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: 0.5rem;
  margin-bottom: 0.75rem;
}
.field { display: flex; flex-direction: column; gap: 0.25rem; }
.field.checkbox { flex-direction: row; align-items: center; gap: 0.5rem; }
.field.actions { flex-direction: row; align-items: end; gap: 0.5rem; }
.field label { font-weight: 600; }
.field input, .field select, .field textarea { padding: 0.4rem; font: inherit; border: 1px solid var(--border); border-radius: 0.375rem; }
.field textarea { min-height: 6rem; resize: vertical; }
.settings-form {
  display: grid;
  gap: 0.75rem;
  padding: 1rem;
  margin: 1rem 0;
  border: 1px solid var(--border);
  border-radius: 0.5rem;
  background: var(--surface);
}
.settings-form-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(14rem, 1fr)); gap: 0.75rem; }
.settings-notice { padding: 0.75rem; border: 1px solid var(--low); border-radius: 0.375rem; }
.inline-form { margin: 0; }
table { width: 100%; border-collapse: collapse; margin: 1rem 0; }
caption { text-align: left; font-weight: 600; margin-bottom: 0.5rem; }
th, td { text-align: left; padding: 0.5rem 0.6rem; border-bottom: 1px solid var(--border); vertical-align: top; }
thead th { background: var(--surface); }
.pr-row.is-stale { background: rgba(161, 92, 0, 0.06); }
.pr-row.has-error { background: rgba(179, 38, 30, 0.06); }
.table-scroll { overflow-x: auto; }
.pr-table { min-width: 64rem; margin: 0.5rem 0; font-size: 0.8rem; line-height: 1.2; }
.pr-table caption { margin-bottom: 0.25rem; }
.pr-table th, .pr-table td { padding: 0.28rem 0.4rem; }
.pr-table thead th { position: sticky; top: 0; z-index: 1; }
.sort-link { color: inherit; text-decoration: none; white-space: nowrap; }
.sort-link:hover { text-decoration: underline; }
.sort-indicator { color: var(--muted); font-size: 0.68rem; }
.pr-table td:nth-child(3), .pr-table td:nth-child(4), .pr-table td:nth-child(5) { white-space: nowrap; }
.pr-table .badge { padding: 0.05rem 0.35rem; font-size: 0.68rem; }
.pr-table .provider-badge { font-size: 0.65rem; }
.provider-badge { display: inline-block; font-size: 0.75rem; text-transform: uppercase; letter-spacing: 0.03em; color: var(--muted); }
.repo-name { display: inline; margin-left: 0.25rem; font-weight: 600; }
.pr-title, .rationale, .overview-snippet {
  display: block;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.pr-title { max-width: 20rem; }
.rationale { max-width: 14rem; color: var(--muted); font-size: 0.75rem; }
.overview-snippet { max-width: 24rem; }
.badge {
  display: inline-block;
  padding: 0.1rem 0.5rem;
  border-radius: 999px;
  font-size: 0.78rem;
  font-weight: 600;
  border: 1px solid var(--border);
  background: var(--surface);
}
.badge.draft { color: #fff; background: var(--muted); border-color: var(--muted); }
.badge.stale { color: #fff; background: var(--medium); border-color: var(--medium); }
.badge.fresh { color: var(--low); }
.badge.error, .badge.status-unavailable { color: #fff; background: var(--error); border-color: var(--error); }
.badge.analysis-pending { color: #fff; background: var(--medium); border-color: var(--medium); }
.badge.analysis-outdated { color: #fff; background: var(--muted); border-color: var(--muted); }
.badge.status-available { color: var(--low); }
.badge.status-not_required { color: var(--muted); }
.priority-badge.priority-critical, .priority-badge.priority-high { color: #fff; background: var(--high); border-color: var(--high); }
.priority-badge.priority-medium { color: #fff; background: var(--medium); border-color: var(--medium); }
.priority-badge.priority-low { color: #fff; background: var(--low); border-color: var(--low); }
.priority-badge.priority-ignore, .priority-badge.priority-unknown { color: var(--muted); }
.verdict-request_changes { color: #fff; background: var(--high); border-color: var(--high); }
.verdict-comment { color: var(--medium); }
.verdict-approve { color: var(--low); }
.actions-panel { margin: 1.5rem 0; padding: 1rem; border: 1px solid var(--border); border-radius: 0.5rem; }
.action-buttons { display: flex; gap: 0.75rem; flex-wrap: wrap; }
.action-buttons form { margin: 0; }
.findings { list-style: none; padding: 0; }
.findings li { padding: 0.75rem; border: 1px solid var(--border); border-radius: 0.375rem; margin-bottom: 0.5rem; }
.repo-list ul { list-style: none; padding: 0; }
.repo-list li { display: flex; align-items: center; gap: 1rem; padding: 0.4rem 0; }
.empty { padding: 1rem; color: var(--muted); }
.pr-body summary { cursor: pointer; font-weight: 600; }
"#;

/// The dashboard script. Same-origin, no inline handlers: it enhances the action forms with
/// `fetch`-based submission (CSRF token from the `<meta>` tag), while `data-reload` links reload
/// the current view.
pub(super) const APP_JS: &str = r#""use strict";
(function () {
  var meta = document.querySelector('meta[name="csrf-token"]');
  var csrf = meta ? meta.getAttribute("content") : "";
  var status = document.getElementById("action-status");

  function announce(message) {
    if (status) {
      status.textContent = message;
    }
  }

  function submitAction(form) {
    form.addEventListener("submit", function (event) {
      event.preventDefault();
      announce("Submitting\u2026");
      fetch(form.getAttribute("action"), {
        method: "POST",
        headers: { "X-Csrf-Token": csrf, Accept: "application/json" }
      })
        .then(function (response) {
          if (response.status === 200 || response.status === 202) {
            return response.json();
          }
          throw new Error("status " + response.status);
        })
        .then(function (result) {
          if (result.status === "already_queued") {
            announce("Analysis is already queued. Reload later to see the result.");
          } else {
            announce("Analysis queued. Reload later to see the result.");
          }
        })
        .catch(function () {
          announce("The request was rejected.");
        });
    });
  }

  var forms = document.querySelectorAll("form.action-form");
  for (var i = 0; i < forms.length; i += 1) {
    submitAction(forms[i]);
  }

  var reloaders = document.querySelectorAll("[data-reload]");
  for (var j = 0; j < reloaders.length; j += 1) {
    reloaders[j].addEventListener("click", function (event) {
      event.preventDefault();
      announce("Reloading\u2026");
      window.location.reload();
    });
  }
})();
"#;
