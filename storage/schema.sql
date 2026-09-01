CREATE TABLE IF NOT EXISTS repositories (
    id INTEGER PRIMARY KEY,
    provider TEXT NOT NULL,
    owner TEXT NOT NULL,
    name TEXT NOT NULL,
    remote_id TEXT,
    action_configuration_fingerprint TEXT NOT NULL,
    active INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
    updated_at INTEGER NOT NULL,
    UNIQUE (provider, owner, name)
) STRICT;

CREATE TABLE IF NOT EXISTS pull_requests (
    id INTEGER PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id),
    provider_id TEXT NOT NULL,
    number INTEGER NOT NULL CHECK (number >= 0),
    title TEXT NOT NULL,
    author TEXT,
    web_url TEXT NOT NULL,
    source_branch TEXT NOT NULL,
    target_branch TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('open', 'closed')),
    revision_fingerprint TEXT NOT NULL,
    closed_at INTEGER,
    refreshed_at INTEGER NOT NULL,
    provider_updated_at INTEGER,
    details_present INTEGER NOT NULL DEFAULT 0 CHECK (details_present IN (0, 1)),
    body TEXT,
    is_draft INTEGER CHECK (is_draft IS NULL OR is_draft IN (0, 1)),
    mergeable INTEGER CHECK (mergeable IS NULL OR mergeable IN (0, 1)),
    additions INTEGER CHECK (additions IS NULL OR additions >= 0),
    deletions INTEGER CHECK (deletions IS NULL OR deletions >= 0),
    changed_files INTEGER CHECK (changed_files IS NULL OR changed_files >= 0),
    CHECK (
        (details_present = 0 AND body IS NULL AND is_draft IS NULL
            AND mergeable IS NULL AND additions IS NULL AND deletions IS NULL
            AND changed_files IS NULL)
        OR (details_present = 1 AND is_draft IS NOT NULL)
    ),
    UNIQUE (repository_id, provider_id),
    UNIQUE (repository_id, number)
) STRICT;

CREATE INDEX IF NOT EXISTS pull_requests_repository_state
    ON pull_requests(repository_id, state, number);

CREATE TABLE IF NOT EXISTS analyses (
    id INTEGER PRIMARY KEY,
    pull_request_id INTEGER NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE,
    revision_fingerprint TEXT NOT NULL,
    action_configuration_fingerprint TEXT NOT NULL,
    outcome TEXT NOT NULL CHECK (outcome IN ('succeeded', 'failed')),
    summary TEXT,
    diagnostic TEXT CHECK (diagnostic IS NULL OR length(diagnostic) <= 4096),
    completed_at INTEGER NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS analyses_newest
    ON analyses(pull_request_id, completed_at DESC, id DESC);

CREATE INDEX IF NOT EXISTS analyses_matching_newest
    ON analyses(
        pull_request_id,
        revision_fingerprint,
        action_configuration_fingerprint,
        completed_at DESC,
        id DESC
    );
