# storage

`storage` owns the PR review dashboard's SQLite persistence API and schema.

It stores only domain data:

- `repositories`
- `pull_requests`, containing raw provider metadata and details
- `analyses`, containing results associated with a pull request revision

Polling runs, pending analyses, retries, and other background-work state are
intentionally process-local and are not persisted.

This initial version has no migration framework. Recreate the database after an
incompatible schema change during development.
