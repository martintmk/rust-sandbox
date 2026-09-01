# analysis

`analysis` turns normalized pull request context into a validated structured
analysis using the GitHub Copilot SDK.

The crate owns the analysis request and output models, configurable overview and
interest prompts (with built-in defaults), review-skill capability validation,
restricted SDK permissions, and cancellation behavior. Overview and interest
are ordinary Copilot prompts; only review invokes an external skill. The crate
does not poll providers, schedule work, or persist results.
