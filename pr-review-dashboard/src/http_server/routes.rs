// Licensed under the MIT License.

//! Route/query resolution table.
//!
//! The route set is described once as a typed enum through [`routerama::resolver`], which
//! compiles every `#[route(METHOD, "path")]` template into a static trie at build time. Dispatch
//! resolves an incoming method and path straight to a typed [`Route`] variant with already-parsed
//! captures — no hand-rolled path splitting or manual integer parsing in the request handler.
//! Every capture here is an owned integer, so the generated enum needs no
//! borrowed-capture lifetime parameter.

use routerama::resolver;

/// The dashboard's full route table. Static-only (every variant carries a `#[route]`), so the
/// generated `RouteResolver` has an infallible `Route::resolver()` constructor.
#[resolver]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Route {
    /// Repository overview page.
    #[route(GET, "/")]
    Dashboard,

    /// Liveness/readiness probe; never touches storage.
    #[route(GET, "/health")]
    Health,

    /// Backward-compatible alias for the liveness/readiness probe.
    #[route(GET, "/healthz")]
    Healthz,

    /// Settings editor and prerequisite report.
    #[route(GET, "/settings")]
    Settings,

    /// Persists a settings-page update to the active configuration file.
    #[route(POST, "/settings")]
    UpdateSettings,

    /// Embedded dashboard stylesheet.
    #[route(GET, "/static/app.css")]
    StaticCss,

    /// Embedded dashboard script.
    #[route(GET, "/static/app.js")]
    StaticJs,

    /// HTML pull request list for one repository.
    #[route(GET, "/repositories/{repository_id}/pull-requests")]
    PullRequestList { repository_id: i64 },

    /// HTML pull request detail page.
    #[route(GET, "/repositories/{repository_id}/pull-requests/{number}")]
    PullRequestDetail { repository_id: i64, number: i64 },

    /// JSON repository listing.
    #[route(GET, "/api/repositories")]
    ApiRepositoryList,

    /// JSON pull request listing for one repository.
    #[route(GET, "/api/repositories/{repository_id}/pull-requests")]
    ApiPullRequestList { repository_id: i64 },

    /// Queues an in-process Copilot analysis for a pull request.
    #[route(POST, "/api/repositories/{repository_id}/pull-requests/{number}/analysis")]
    EnqueueAnalysis { repository_id: i64, number: i64 },
}

#[cfg(test)]
mod tests {
    use routerama::ResolveError;

    use super::Route;

    #[test]
    fn resolves_dashboard_and_health() {
        let resolver = Route::resolver();
        assert_eq!(resolver.resolve("GET", "/"), Ok(Route::Dashboard));
        assert_eq!(resolver.resolve("GET", "/health"), Ok(Route::Health));
        assert_eq!(resolver.resolve("GET", "/healthz"), Ok(Route::Healthz));
    }

    #[test]
    fn resolves_settings_and_static_assets() {
        let resolver = Route::resolver();
        assert_eq!(resolver.resolve("GET", "/settings"), Ok(Route::Settings));
        assert_eq!(resolver.resolve("POST", "/settings"), Ok(Route::UpdateSettings));
        assert_eq!(resolver.resolve("GET", "/static/app.css"), Ok(Route::StaticCss));
        assert_eq!(resolver.resolve("GET", "/static/app.js"), Ok(Route::StaticJs));
    }

    #[test]
    fn resolves_pull_request_routes_with_parsed_captures() {
        let resolver = Route::resolver();
        assert_eq!(
            resolver.resolve("GET", "/repositories/7/pull-requests"),
            Ok(Route::PullRequestList { repository_id: 7 })
        );
        assert_eq!(
            resolver.resolve("GET", "/repositories/7/pull-requests/42"),
            Ok(Route::PullRequestDetail {
                repository_id: 7,
                number: 42
            })
        );
    }

    #[test]
    fn resolves_json_api_routes() {
        let resolver = Route::resolver();
        assert_eq!(resolver.resolve("GET", "/api/repositories"), Ok(Route::ApiRepositoryList));
        assert_eq!(
            resolver.resolve("GET", "/api/repositories/3/pull-requests"),
            Ok(Route::ApiPullRequestList { repository_id: 3 })
        );
    }

    #[test]
    fn resolves_enqueue_analysis_only_for_post() {
        let resolver = Route::resolver();
        assert_eq!(
            resolver.resolve("POST", "/api/repositories/1/pull-requests/2/analysis"),
            Ok(Route::EnqueueAnalysis {
                repository_id: 1,
                number: 2
            })
        );
        assert!(matches!(
            resolver.resolve("GET", "/api/repositories/1/pull-requests/2/analysis"),
            Err(ResolveError::NotFound(_))
        ));
    }

    #[test]
    fn unknown_path_is_not_found() {
        let resolver = Route::resolver();
        assert!(matches!(resolver.resolve("GET", "/nope"), Err(ResolveError::NotFound(_))));
    }

    #[test]
    fn non_numeric_id_is_a_bad_request() {
        let resolver = Route::resolver();
        assert!(matches!(
            resolver.resolve("GET", "/repositories/not-a-number/pull-requests"),
            Err(ResolveError::InvalidCapture("repository_id"))
        ));
    }
}
