# providers

`providers` exposes a provider-neutral API for listing and fetching pull
requests from GitHub and Azure DevOps.

The crate owns vendor SDK integration, pagination, normalized pull request
models, error classification, and ephemeral credential acquisition. Provider
tokens are acquired from local CLIs for individual requests and are never
persisted.
