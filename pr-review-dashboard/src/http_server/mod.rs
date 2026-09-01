// Licensed under the MIT License.

//! Loopback-only plain-HTTP server.
//!
//! [`HttpServer::run`] accepts TCP connections on the configured loopback address, resolves each
//! request through [`routes::Route`], and dispatches to a [`handlers`] function. Every response
//! passes through [`responses`], which centrally applies [`security::apply_security_headers`], so
//! no response path can accidentally skip them. The server never talks to a provider adapter or
//! provider adapters directly. Read paths use [`storage::Storage`], while the
//! mutating action route only submits transient work to the Copilot runner.
//!
//! Three independent layers guard the loopback boundary on every request (see [`security`] for
//! the rationale): a request-target length cap enforced before route resolution (`routerama`
//! itself does not impose one), a `Host` header check that must name the loopback address on the
//! server's own port, and — for every mutating route — a per-process CSRF token plus an
//! Origin/Referer check when either header is present.

mod assets;
mod handlers;
mod responses;
mod routes;
mod security;

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyspawn::Spawner;
use bytes::Bytes;
use hyper::body::Incoming;
use hyper::header::HOST;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use routerama::ResolveError;
use storage::{RepositoryId, Storage};
use tick::Clock;
use tokio::net::TcpListener;

use crate::config::AppConfig;
use crate::copilot::CopilotService;
use crate::prereqs::PrerequisiteReport;
use crate::shutdown::ShutdownListener;
use crate::templates::Templates;

use handlers::ServerState;
use responses::Body;
use routes::Route;
use security::{
    BodyReadError, CsrfToken, HEADER_READ_TIMEOUT, MAX_HEADER_COUNT, MAX_REQUEST_TARGET_LEN, MutationRejected,
    authorize_mutation_with_token, form_field, is_loopback_authority, read_limited_body, request_prefers_html,
};

/// How long a connection is given to finish in-flight requests after shutdown is triggered before
/// it is dropped unconditionally.
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// The dashboard's loopback-only HTTP server. Cheap to clone: every field is either `Copy` or
/// reference-counted, matching how [`crate::application::Application`] holds and spawns it.
#[derive(Clone)]
pub struct HttpServer {
    listen_address: SocketAddr,
    spawner: Spawner,
    state: Arc<ServerState>,
}

impl HttpServer {
    pub(crate) fn new<D>(dependencies: &D) -> Self
    where
        D: AsRef<AppConfig>
            + AsRef<Spawner>
            + AsRef<Storage>
            + AsRef<Templates>
            + AsRef<Clock>
            + AsRef<PrerequisiteReport>
            + AsRef<CopilotService>,
    {
        let config = AsRef::<AppConfig>::as_ref(dependencies);
        Self {
            listen_address: config.listen_address,
            spawner: AsRef::<Spawner>::as_ref(dependencies).clone(),
            state: Arc::new(ServerState {
                storage: AsRef::<Storage>::as_ref(dependencies).clone(),
                copilot: AsRef::<CopilotService>::as_ref(dependencies).clone(),
                templates: AsRef::<Templates>::as_ref(dependencies).clone(),
                clock: AsRef::<Clock>::as_ref(dependencies).clone(),
                csrf: CsrfToken::generate(),
                listen_port: config.listen_address.port(),
                resolver: Route::resolver(),
                config_update_lock: tokio::sync::Mutex::new(()),
                config: Arc::new(config.clone()),
                prerequisites: Arc::new(AsRef::<PrerequisiteReport>::as_ref(dependencies).clone()),
            }),
        }
    }

    /// Accepts connections until `shutdown` fires, then gives in-flight connections up to
    /// [`GRACEFUL_SHUTDOWN_TIMEOUT`] to finish before returning.
    pub(crate) async fn run(self, shutdown: ShutdownListener) -> Result<(), std::io::Error> {
        let listener = TcpListener::bind(self.listen_address).await?;
        tracing::info!(address = %self.listen_address, "PR review dashboard listening");

        let graceful = GracefulShutdown::new();
        let mut shutdown_future = std::pin::pin!(shutdown.cancelled());

        loop {
            tokio::select! {
                () = &mut shutdown_future => break,
                accepted = listener.accept() => {
                    let (stream, _peer_addr) = match accepted {
                        Ok(accepted) => accepted,
                        Err(error) => {
                            tracing::error!(%error, "failed to accept a connection");
                            continue;
                        }
                    };
                    self.spawn_connection(stream, &graceful);
                }
            }
        }

        tokio::select! {
            () = graceful.shutdown() => {}
            () = tokio::time::sleep(GRACEFUL_SHUTDOWN_TIMEOUT) => {
                tracing::warn!("graceful shutdown timed out; some connections may have been dropped");
            }
        }
        Ok(())
    }

    fn spawn_connection(&self, stream: tokio::net::TcpStream, graceful: &GracefulShutdown) {
        let io = TokioIo::new(stream);
        let state = Arc::clone(&self.state);
        let watcher = graceful.watcher();

        let mut builder = hyper::server::conn::http1::Builder::new();
        builder
            .max_headers(MAX_HEADER_COUNT)
            .header_read_timeout(HEADER_READ_TIMEOUT)
            .timer(TokioTimer::default());

        self.spawner.spawn(async move {
            let service = service_fn(move |request| {
                let state = Arc::clone(&state);
                async move { Ok::<_, std::convert::Infallible>(route_request(&state, request).await) }
            });
            let connection = builder.serve_connection(io, service);
            if let Err(error) = watcher.watch(connection).await {
                if error.is_timeout() || error.is_incomplete_message() {
                    tracing::debug!(%error, "client connection closed before sending a complete request");
                } else {
                    tracing::error!(%error, "connection failed");
                }
            }
        });
    }
}

impl fmt::Debug for HttpServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpServer")
            .field("listen_address", &self.listen_address)
            .finish_non_exhaustive()
    }
}

/// The request-target length, as sent on the wire (path + query), before route resolution ever
/// sees it. `routerama` deliberately does not enforce a limit itself (see its README), so this
/// must happen first.
fn request_target_len(request: &Request<Incoming>) -> usize {
    request.uri().path_and_query().map_or(0, |target| target.as_str().len())
}

/// Resolves and dispatches one request, applying the request-target-length, Host, and (for the
/// mutating routes) CSRF/Origin checks before any handler runs.
async fn route_request(state: &ServerState, request: Request<Incoming>) -> Response<Body> {
    if request_target_len(&request) > MAX_REQUEST_TARGET_LEN {
        return responses::uri_too_long();
    }

    let Some(host) = request.headers().get(HOST).and_then(|value| value.to_str().ok()) else {
        return responses::bad_request("missing Host header");
    };
    if !is_loopback_authority(host, state.listen_port) {
        return responses::forbidden("Host header does not name the loopback server");
    }

    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    let query = request.uri().query().map(str::to_owned);
    let route = match state.resolver.resolve(&method, &path) {
        Ok(route) => route,
        Err(ResolveError::NotFound(_)) => return responses::not_found(),
        Err(_) => return responses::bad_request("request path is invalid"),
    };

    dispatch_route(state, route, request, query.as_deref()).await
}

async fn dispatch_route(state: &ServerState, route: Route, request: Request<Incoming>, query: Option<&str>) -> Response<Body> {
    match route {
        Route::Dashboard => handlers::dashboard_page(state, query).await,
        Route::Health | Route::Healthz => responses::plain(StatusCode::OK, "ok"),
        Route::Settings => handlers::settings_page(state, query).await,
        Route::StaticCss => responses::static_asset("text/css; charset=utf-8", assets::APP_CSS),
        Route::StaticJs => responses::static_asset("text/javascript; charset=utf-8", assets::APP_JS),
        Route::PullRequestList { repository_id } => handlers::pull_request_list_page(state, RepositoryId(repository_id), query).await,
        Route::PullRequestDetail { repository_id, number } => {
            handlers::pull_request_detail_page(state, RepositoryId(repository_id), number).await
        }
        Route::ApiRepositoryList => handlers::api_repository_list(state).await,
        Route::ApiPullRequestList { repository_id } => handlers::api_pull_request_list(state, RepositoryId(repository_id)).await,
        Route::UpdateSettings => {
            let body = match read_authorized_mutation(state, request).await {
                Ok(body) => body,
                Err(response) => return response,
            };
            handlers::update_settings(state, &body).await
        }
        Route::EnqueueAnalysis { repository_id, number } => {
            let wants_html = request_prefers_html(request.headers());
            if let Err(response) = read_authorized_mutation(state, request).await {
                return response;
            }
            handlers::enqueue_analysis(state, RepositoryId(repository_id), number, wants_html).await
        }
    }
}

async fn read_authorized_mutation(state: &ServerState, request: Request<Incoming>) -> Result<Bytes, Response<Body>> {
    let headers = request.headers().clone();
    let header_token = headers
        .get(security::CSRF_HEADER_NAME)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = match read_limited_body(request.into_body()).await {
        Ok(bytes) => bytes,
        Err(BodyReadError::TooLarge) => return Err(responses::payload_too_large()),
        Err(BodyReadError::Invalid) => return Err(responses::bad_request("failed to read request body")),
    };
    let token = header_token.unwrap_or_else(|| form_field(&body, "csrf_token").unwrap_or_default());
    if let Err(rejection) = authorize_mutation_with_token(&headers, &state.csrf, state.listen_port, &token) {
        return Err(match rejection {
            MutationRejected::MissingOrInvalidCsrfToken => responses::forbidden("missing or invalid CSRF token"),
            MutationRejected::CrossOriginRequest => responses::forbidden("cross-origin request rejected"),
        });
    }
    Ok(body)
}

#[cfg(test)]
pub(crate) async fn render_dashboard_for_test(config: AppConfig, clock: Clock, storage: Storage) -> String {
    use http_body_util::BodyExt as _;

    let copilot = CopilotService::with_example_backend(config.clone(), clock.clone(), storage.clone());
    let state = ServerState {
        storage,
        copilot,
        templates: Templates::new(),
        clock,
        csrf: CsrfToken::generate(),
        listen_port: config.listen_address.port(),
        resolver: Route::resolver(),
        config_update_lock: tokio::sync::Mutex::new(()),
        config: Arc::new(config),
        prerequisites: Arc::new(PrerequisiteReport {
            gh_cli: crate::prereqs::Availability::NotRequired,
            az_cli: crate::prereqs::Availability::NotRequired,
            copilot_cli: crate::prereqs::Availability::Available,
            review_action: crate::prereqs::Availability::Available,
        }),
    };
    let response = handlers::dashboard_page(&state, None).await;
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("dashboard body should collect")
        .to_bytes();
    String::from_utf8(bytes.to_vec()).expect("dashboard body should be UTF-8")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpStream;
    use tokio::time::Duration as TokioDuration;

    use super::HttpServer;
    use crate::config::AppConfig;
    use crate::copilot::CopilotService;
    use crate::prereqs::{Availability, PrerequisiteReport};
    use crate::shutdown;
    use crate::templates::Templates;
    use storage::AnalysisOutcome;
    use storage::{
        NewAnalysis, PullRequestDetails, PullRequestId, PullRequestSnapshot, PullRequestState, Repository, RepositoryConfig, Storage,
    };

    static NEXT_DATABASE: AtomicU64 = AtomicU64::new(1);

    /// A prerequisite report for tests: the required CLIs are available and one action is missing
    /// its plugin, exercising the settings page's actionable, credential-free message path.
    fn test_prerequisite_report() -> PrerequisiteReport {
        PrerequisiteReport {
            gh_cli: Availability::Available,
            az_cli: Availability::NotRequired,
            copilot_cli: Availability::Available,
            review_action: Availability::Unavailable("install the review-lens plugin to enable reviews".to_owned()),
        }
    }

    /// A uniquely-named `SQLite` database file under the current directory, cleaned up on drop.
    /// Mirrors the equivalent private helper in the `storage` crate's test module.
    struct TestDatabase {
        path: PathBuf,
        config_path: PathBuf,
    }

    impl TestDatabase {
        fn new() -> Self {
            let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::current_dir()
                .expect("current directory should be available")
                .join(format!("http-server-test-{}-{sequence}.sqlite3", std::process::id()));
            let config_path = path.with_extension("toml");
            Self::remove_sqlite_files(&path);
            match fs::remove_file(&config_path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("failed to remove {}: {error}", config_path.display()),
            }
            Self { path, config_path }
        }

        fn remove_sqlite_files(path: &Path) {
            for suffix in ["", "-wal", "-shm"] {
                let candidate = PathBuf::from(format!("{}{suffix}", path.display()));
                if let Err(error) = fs::remove_file(&candidate) {
                    assert_eq!(
                        error.kind(),
                        std::io::ErrorKind::NotFound,
                        "failed to remove {}: {error}",
                        candidate.display()
                    );
                }
            }
        }
    }

    impl Drop for TestDatabase {
        fn drop(&mut self) {
            Self::remove_sqlite_files(&self.path);
            match fs::remove_file(&self.config_path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("failed to remove {}: {error}", self.config_path.display()),
            }
        }
    }

    struct Fixture {
        config: AppConfig,
        spawner: anyspawn::Spawner,
        clock: tick::Clock,
        storage: Storage,
        copilot: CopilotService,
        templates: Templates,
        prerequisites: crate::prereqs::PrerequisiteReport,
    }

    impl AsRef<AppConfig> for Fixture {
        fn as_ref(&self) -> &AppConfig {
            &self.config
        }
    }
    impl AsRef<anyspawn::Spawner> for Fixture {
        fn as_ref(&self) -> &anyspawn::Spawner {
            &self.spawner
        }
    }
    impl AsRef<tick::Clock> for Fixture {
        fn as_ref(&self) -> &tick::Clock {
            &self.clock
        }
    }
    impl AsRef<Storage> for Fixture {
        fn as_ref(&self) -> &Storage {
            &self.storage
        }
    }
    impl AsRef<CopilotService> for Fixture {
        fn as_ref(&self) -> &CopilotService {
            &self.copilot
        }
    }
    impl AsRef<Templates> for Fixture {
        fn as_ref(&self) -> &Templates {
            &self.templates
        }
    }
    impl AsRef<crate::prereqs::PrerequisiteReport> for Fixture {
        fn as_ref(&self) -> &crate::prereqs::PrerequisiteReport {
            &self.prerequisites
        }
    }

    /// A running instance of [`HttpServer`] bound to an ephemeral loopback port, plus everything
    /// needed to drive it from a test: the shutdown trigger, the background task, the storage
    /// handle (to seed data and to verify what a handler persisted), and the CSRF token a
    /// mutating request must present.
    struct TestServer {
        port: u16,
        storage: Storage,
        config_path: PathBuf,
        csrf_token: String,
        trigger: Option<shutdown::ShutdownTrigger>,
        handle: Option<tokio::task::JoinHandle<Result<(), std::io::Error>>>,
        _database: TestDatabase,
    }

    impl TestServer {
        async fn start() -> Self {
            let database = TestDatabase::new();
            let storage = Storage::open(database.path.clone()).await.expect("test database should open");

            // Reserve an ephemeral loopback port by briefly binding it ourselves, then hand that
            // same port to the server: `HttpServer::run` owns the actual listener, so this is the
            // simplest way for a test to learn which port an OS-assigned bind landed on.
            let probe = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .expect("should reserve an ephemeral port");
            let port = probe.local_addr().expect("probe listener should have a local address").port();
            drop(probe);

            let config = AppConfig {
                config_path: Some(database.config_path.clone()),
                listen_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
                ..AppConfig::default()
            };
            let clock = tick::Clock::new_tokio();
            let copilot = CopilotService::with_example_backend(config.clone(), clock.clone(), storage.clone());

            let fixture = Fixture {
                config,
                spawner: anyspawn::Spawner::new_tokio(),
                clock,
                storage: storage.clone(),
                copilot,
                templates: Templates::new(),
                prerequisites: test_prerequisite_report(),
            };
            let server = HttpServer::new(&fixture);
            let csrf_token = server.state.csrf.value().to_owned();

            let (trigger, listener) = shutdown::channel();
            let handle = tokio::spawn(server.run(listener));

            let started = Self {
                port,
                storage,
                config_path: database.config_path.clone(),
                csrf_token,
                trigger: Some(trigger),
                handle: Some(handle),
                _database: database,
            };
            started.wait_until_accepting().await;
            started
        }

        /// Polls the loopback port with real TCP connect attempts until one succeeds, so tests
        /// do not race the still-async `TcpListener::bind` inside `HttpServer::run`.
        async fn wait_until_accepting(&self) {
            for _ in 0..200 {
                if TcpStream::connect((Ipv4Addr::LOCALHOST, self.port)).await.is_ok() {
                    return;
                }
                tokio::time::sleep(TokioDuration::from_millis(10)).await;
            }
            panic!("HTTP server did not start accepting connections on port {}", self.port);
        }

        /// Sends a raw HTTP/1.1 request (already including a trailing blank line) and returns the
        /// full response text. Every request line includes `Connection: close` so the server
        /// closes the socket once it has responded, letting a plain `read_to_end` know when the
        /// response is complete.
        async fn send(&self, raw_request: &str) -> String {
            let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, self.port))
                .await
                .expect("should connect to the test server");
            stream.write_all(raw_request.as_bytes()).await.expect("should write the request");
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.expect("should read the response");
            String::from_utf8_lossy(&response).into_owned()
        }

        async fn get(&self, path: &str) -> String {
            self.send(&format!(
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                self.port
            ))
            .await
        }

        async fn post_settings(&self, fields: &str) -> String {
            self.post_settings_from_origin(fields, &format!("http://127.0.0.1:{}", self.port))
                .await
        }

        async fn post_settings_from_origin(&self, fields: &str, origin: &str) -> String {
            let body = format!("csrf_token={}&{fields}", self.csrf_token);
            self.send(&format!(
                "POST /settings HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: {}\r\nAccept: text/html\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                self.port,
                origin,
                body.len(),
                body
            ))
            .await
        }

        async fn seed_repository_with_pull_request(&self) -> (Repository, PullRequestId, i64) {
            let repository = self
                .storage
                .reconcile_repositories(
                    vec![RepositoryConfig {
                        provider: "github".to_owned(),
                        owner: "octo".to_owned(),
                        name: "widgets".to_owned(),
                        remote_id: Some("R_1".to_owned()),
                        action_configuration_fingerprint: "actions-v1".to_owned(),
                    }],
                    1,
                )
                .await
                .expect("repository reconciliation should succeed")
                .into_iter()
                .next()
                .expect("exactly one repository was reconciled");

            self.storage
                .reconcile_pull_requests(
                    repository.id,
                    vec![PullRequestSnapshot {
                        provider_id: "PR_1".to_owned(),
                        number: 7,
                        title: "<script>alert(1)</script>".to_owned(),
                        author: Some("octocat".to_owned()),
                        web_url: "https://github.com/octo/widgets/pull/7".to_owned(),
                        source_branch: "feature".to_owned(),
                        target_branch: "main".to_owned(),
                        state: PullRequestState::Open,
                        revision_fingerprint: "revision-7".to_owned(),
                        provider_updated_at: Some(1_700_000_000_000),
                        details: Some(PullRequestDetails {
                            body: Some("body".to_owned()),
                            is_draft: false,
                            mergeable: Some(true),
                            additions: Some(1),
                            deletions: Some(1),
                            changed_files: Some(1),
                        }),
                    }],
                    3,
                )
                .await
                .expect("pull request reconciliation should succeed");

            let pull_request = self
                .storage
                .list_pull_requests(repository.id)
                .await
                .expect("pull requests should list")
                .into_iter()
                .next()
                .expect("exactly one pull request was recorded");

            (repository, pull_request.id, pull_request.number)
        }

        /// Records a successful stored analysis for the seeded pull request. The overview and
        /// interest rationale deliberately embed HTML so escaping can be asserted, and the
        /// priority is high so priority rendering and filtering can be exercised.
        async fn seed_high_priority_analysis(&self, pull_request_id: PullRequestId) {
            let summary = serde_json::json!({
                "overview": { "summary": "Overview <img src=x onerror=alert(1)>" },
                "interesting": {
                    "interesting": true,
                    "priority": "high",
                    "rationale": "Rationale <script>alert('r')</script>"
                },
                "review": {
                    "verdict": "comment",
                    "summary": "Looks reasonable",
                    "findings": [
                        {
                            "severity": "high",
                            "title": "Finding <b>title</b>",
                            "details": "Detail text",
                            "file": "src/lib.rs",
                            "line": 12
                        }
                    ]
                }
            })
            .to_string();
            self.storage
                .record_analysis(NewAnalysis {
                    pull_request_id,
                    revision_fingerprint: "revision-7".to_owned(),
                    action_configuration_fingerprint: "actions-v1".to_owned(),
                    outcome: AnalysisOutcome::Succeeded,
                    summary: Some(summary),
                    diagnostic: None,
                    completed_at: 1_000,
                })
                .await
                .expect("analysis should record");
        }

        async fn shutdown_and_join(mut self) {
            self.trigger.take().expect("trigger should be present").trigger();
            let handle = self.handle.take().expect("handle should be present");
            tokio::time::timeout(TokioDuration::from_secs(5), handle)
                .await
                .expect("server task should stop within the timeout")
                .expect("server task should not panic")
                .expect("server should shut down cleanly");
        }
    }

    #[tokio::test]
    async fn occupied_port_returns_a_bind_error() {
        let database = TestDatabase::new();
        let storage = Storage::open(database.path.clone()).await.expect("test database should open");
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("should reserve a port");
        let address = listener.local_addr().expect("listener should have an address");
        let config = AppConfig {
            listen_address: address,
            ..AppConfig::default()
        };
        let clock = tick::Clock::new_tokio();
        let copilot = CopilotService::with_example_backend(config.clone(), clock.clone(), storage.clone());
        let fixture = Fixture {
            config,
            spawner: anyspawn::Spawner::new_tokio(),
            clock,
            storage,
            copilot,
            templates: Templates::new(),
            prerequisites: test_prerequisite_report(),
        };
        let server = HttpServer::new(&fixture);
        let (_trigger, shutdown) = shutdown::channel();

        server.run(shutdown).await.expect_err("occupied port should fail startup");
    }

    #[tokio::test]
    async fn health_check_returns_ok_with_restrictive_security_headers() {
        let server = TestServer::start().await;
        let response = server.get("/health").await;

        assert!(response.starts_with("HTTP/1.1 200"), "unexpected response: {response}");
        assert!(
            response
                .to_ascii_lowercase()
                .contains("content-security-policy: default-src 'none'")
        );
        assert!(response.to_ascii_lowercase().contains("x-frame-options: deny"));
        assert!(response.to_ascii_lowercase().contains("x-content-type-options: nosniff"));
        assert!(response.to_ascii_lowercase().contains("cache-control: no-store"));
        assert!(response.contains("ok"));

        server.shutdown_and_join().await;
    }

    #[tokio::test]
    async fn healthz_alias_remains_available() {
        let server = TestServer::start().await;
        let response = server.get("/healthz").await;
        assert!(response.starts_with("HTTP/1.1 200"), "unexpected response: {response}");
        server.shutdown_and_join().await;
    }

    #[tokio::test]
    async fn unknown_path_returns_not_found() {
        let server = TestServer::start().await;
        let response = server.get("/no-such-route").await;
        assert!(response.starts_with("HTTP/1.1 404"), "unexpected response: {response}");
        server.shutdown_and_join().await;
    }

    #[tokio::test]
    async fn non_loopback_host_header_is_rejected() {
        let server = TestServer::start().await;
        let response = server
            .send("GET / HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n")
            .await;
        assert!(response.starts_with("HTTP/1.1 403"), "unexpected response: {response}");
        server.shutdown_and_join().await;
    }

    #[tokio::test]
    async fn oversized_request_target_is_rejected() {
        let server = TestServer::start().await;
        let long_path = format!("/{}", "a".repeat(4096));
        let response = server.get(&long_path).await;
        assert!(response.starts_with("HTTP/1.1 414"), "unexpected response: {response}");
        server.shutdown_and_join().await;
    }

    #[tokio::test]
    async fn enqueue_analysis_without_csrf_token_is_rejected() {
        let server = TestServer::start().await;
        let (repository, _pull_request_id, number) = server.seed_repository_with_pull_request().await;
        let request = format!(
            "POST /api/repositories/{}/pull-requests/{number}/analysis HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            repository.id.0, server.port
        );
        let response = server.send(&request).await;
        assert!(response.starts_with("HTTP/1.1 403"), "unexpected response: {response}");
        server.shutdown_and_join().await;
    }

    #[tokio::test]
    async fn enqueue_analysis_with_valid_csrf_token_queues_transient_analysis() {
        let server = TestServer::start().await;
        let (repository, _pull_request_id, number) = server.seed_repository_with_pull_request().await;
        let request = format!(
            "POST /api/repositories/{}/pull-requests/{number}/analysis HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nX-Csrf-Token: {}\r\nOrigin: http://127.0.0.1:{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            repository.id.0, server.port, server.csrf_token, server.port
        );
        let response = server.send(&request).await;
        assert!(response.starts_with("HTTP/1.1 202"), "unexpected response: {response}");
        assert!(response.contains("\"status\":\"queued\""), "unexpected response: {response}");

        let duplicate = server.send(&request).await;
        assert!(duplicate.starts_with("HTTP/1.1 200"), "unexpected response: {duplicate}");
        assert!(
            duplicate.contains("\"status\":\"already_queued\""),
            "unexpected response: {duplicate}"
        );

        server.shutdown_and_join().await;
    }

    #[tokio::test]
    async fn dashboard_page_renders_escaped_repository_list() {
        let server = TestServer::start().await;
        server.seed_repository_with_pull_request().await;
        let response = server.get("/").await;
        assert!(response.starts_with("HTTP/1.1 200"), "unexpected response: {response}");
        assert!(response.contains("text/html"));
        assert!(
            response.contains("github") && response.contains("octo") && response.contains("widgets"),
            "repo label missing: {response}"
        );
        assert!(
            response.contains("2023-11-14T22:13:20Z"),
            "provider update timestamp missing: {response}"
        );
        assert!(
            response.contains(r#"aria-sort="descending""#),
            "default updated sort state missing: {response}"
        );
        assert!(
            response.contains("sort=updated&amp;direction=asc"),
            "updated sort toggle missing: {response}"
        );
        assert!(
            response.contains("AI analysis pending"),
            "pending analysis indicator missing: {response}"
        );
        server.shutdown_and_join().await;
    }

    #[tokio::test]
    async fn pull_request_detail_page_escapes_untrusted_title() {
        let server = TestServer::start().await;
        let (repository, _pull_request_id, number) = server.seed_repository_with_pull_request().await;
        let response = server
            .get(&format!("/repositories/{}/pull-requests/{number}", repository.id.0))
            .await;
        assert!(response.starts_with("HTTP/1.1 200"), "unexpected response: {response}");
        assert!(!response.contains("<script>"), "unescaped script tag leaked into HTML: {response}");
        assert!(response.contains("&lt;script&gt;"));
        server.shutdown_and_join().await;
    }

    #[tokio::test]
    async fn dashboard_renders_priority_and_escapes_analysis_text() {
        let server = TestServer::start().await;
        let (_repository, pull_request_id, _number) = server.seed_repository_with_pull_request().await;
        server.seed_high_priority_analysis(pull_request_id).await;

        let response = server.get("/").await;
        assert!(response.starts_with("HTTP/1.1 200"), "unexpected response: {response}");
        assert!(response.to_ascii_lowercase().contains("high"), "priority label missing: {response}");
        assert!(
            response.contains("overview-snippet"),
            "compact overview preview missing: {response}"
        );
        assert!(response.contains("class=\"pr-title\""), "compact title styling missing: {response}");
        assert!(
            !response.contains("<img src=x onerror=alert(1)>"),
            "unescaped overview payload leaked: {response}"
        );
        assert!(
            response.contains("onerror=alert(1)&gt;") || response.contains("&lt;img"),
            "overview not escaped: {response}"
        );
        assert!(
            !response.contains("<script>alert('r')</script>"),
            "unescaped rationale leaked: {response}"
        );
        server.shutdown_and_join().await;
    }

    #[tokio::test]
    async fn new_revision_keeps_previous_analysis_and_marks_it_outdated() {
        let server = TestServer::start().await;
        let (repository, pull_request_id, number) = server.seed_repository_with_pull_request().await;
        server.seed_high_priority_analysis(pull_request_id).await;
        server
            .storage
            .reconcile_pull_requests(
                repository.id,
                vec![PullRequestSnapshot {
                    provider_id: "PR_1".to_owned(),
                    number,
                    title: "Updated revision".to_owned(),
                    author: Some("octocat".to_owned()),
                    web_url: "https://github.com/octo/widgets/pull/7".to_owned(),
                    source_branch: "feature".to_owned(),
                    target_branch: "main".to_owned(),
                    state: PullRequestState::Open,
                    revision_fingerprint: "revision-8".to_owned(),
                    provider_updated_at: Some(1_800_000_000_000),
                    details: Some(PullRequestDetails {
                        body: Some("updated body".to_owned()),
                        is_draft: false,
                        mergeable: Some(true),
                        additions: Some(2),
                        deletions: Some(1),
                        changed_files: Some(1),
                    }),
                }],
                2_000,
            )
            .await
            .expect("new revision should persist");

        let response = server
            .get(&format!("/repositories/{}/pull-requests/{number}", repository.id.0))
            .await;

        assert!(response.contains("AI analysis outdated"), "outdated indicator missing: {response}");
        assert!(response.contains("Update AI analysis"), "manual update action missing: {response}");
        assert!(
            response.contains("Looks reasonable"),
            "previous analysis should remain visible: {response}"
        );
        server.shutdown_and_join().await;
    }

    #[tokio::test]
    async fn dashboard_priority_filter_excludes_non_matching_rows() {
        let server = TestServer::start().await;
        let (_repository, pull_request_id, _number) = server.seed_repository_with_pull_request().await;
        server.seed_high_priority_analysis(pull_request_id).await;

        let matching = server.get("/?priority=high").await;
        assert!(
            matching.contains("octo/widgets") || matching.contains("#7"),
            "high row should be present: {matching}"
        );

        let excluded = server.get("/?priority=low").await;
        assert!(
            excluded.contains("No pull requests match") || !excluded.contains("onerror"),
            "low filter should exclude the high-priority row: {excluded}"
        );
        server.shutdown_and_join().await;
    }

    #[tokio::test]
    async fn dashboard_text_search_filters_by_title() {
        let server = TestServer::start().await;
        server.seed_repository_with_pull_request().await;

        let no_match = server.get("/?q=zzz-nonexistent-term").await;
        assert!(no_match.starts_with("HTTP/1.1 200"), "unexpected response: {no_match}");
        assert!(
            no_match.contains("No pull requests match"),
            "empty search result missing: {no_match}"
        );
        server.shutdown_and_join().await;
    }

    #[tokio::test]
    async fn settings_page_lists_actionable_prerequisite_without_secrets() {
        let server = TestServer::start().await;
        let response = server.get("/settings").await;
        assert!(response.starts_with("HTTP/1.1 200"), "unexpected response: {response}");
        assert!(response.contains("text/html"));
        assert!(
            response.contains("install the review-lens plugin to enable reviews"),
            "actionable prerequisite message missing: {response}"
        );
        assert!(
            response.contains(&server.config_path.display().to_string()),
            "configuration source missing: {response}"
        );
        assert!(response.contains("Analysis prompts"), "prompt settings missing: {response}");
        assert!(response.contains("Save prompts"), "prompt editor missing: {response}");
        assert!(response.contains("Add repository"), "repository editor missing: {response}");
        assert!(
            response.contains("Built-in defaults are active"),
            "default source status missing: {response}"
        );
        let lower = response.to_ascii_lowercase();
        assert!(!lower.contains("password"), "settings must not expose credentials: {response}");
        assert!(!lower.contains("secret"), "settings must not expose credentials: {response}");
        assert!(
            !lower.contains("authorization:"),
            "settings must not expose credentials: {response}"
        );
        server.shutdown_and_join().await;
    }

    #[tokio::test]
    async fn settings_prompt_update_is_csrf_protected_and_persisted() {
        let server = TestServer::start().await;
        let rejected = server
            .send(&format!(
                "POST /settings HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                server.port
            ))
            .await;
        assert!(rejected.starts_with("HTTP/1.1 403"), "unexpected response: {rejected}");

        let response = server
            .post_settings_from_origin(
                "operation=update-prompts&overview=Explain+intent+and+scope.&interesting=Prioritize+security%2C+API%2C+and+correctness.",
                "null",
            )
            .await;
        assert!(response.starts_with("HTTP/1.1 303"), "unexpected response: {response}");

        let updated = AppConfig::load(&server.config_path).expect("updated settings file should load");
        assert_eq!(updated.prompts.overview, "Explain intent and scope.");
        assert_eq!(updated.prompts.interesting, "Prioritize security, API, and correctness.");
        let webview_response = server
            .post_settings_from_origin(
                "operation=update-prompts&overview=Summarize+carefully.&interesting=Prioritize+correctness.",
                "vscode-webview://2d547f9b/settings",
            )
            .await;
        assert!(
            webview_response.starts_with("HTTP/1.1 303"),
            "unexpected response: {webview_response}"
        );
        let rendered = server.get("/settings?saved=1").await;
        assert!(rendered.contains("Summarize carefully."));
        assert!(rendered.contains("Restart the dashboard to apply"));
        server.shutdown_and_join().await;
    }

    #[tokio::test]
    async fn settings_can_add_and_remove_registered_repositories() {
        let server = TestServer::start().await;
        let github = server
            .post_settings("operation=add-repository&url=https%3A%2F%2Fgithub.com%2Focto%2Fwidgets&checkout_path=")
            .await;
        assert!(github.starts_with("HTTP/1.1 303"), "unexpected response: {github}");
        let azure = server
            .post_settings("operation=add-repository&url=https%3A%2F%2Fdev.azure.com%2Fcontoso%2Fplatform%2F_git%2Fapi&checkout_path=")
            .await;
        assert!(azure.starts_with("HTTP/1.1 303"), "unexpected response: {azure}");

        let added = AppConfig::load(&server.config_path).expect("repository additions should load");
        assert_eq!(added.github_repositories.len(), 1);
        assert_eq!(added.azure_devops_repositories.len(), 1);

        let removed = server
            .post_settings("operation=remove-repository&url=https%3A%2F%2Fgithub.com%2Focto%2Fwidgets")
            .await;
        assert!(removed.starts_with("HTTP/1.1 303"), "unexpected response: {removed}");
        let updated = AppConfig::load(&server.config_path).expect("repository removal should load");
        assert!(updated.github_repositories.is_empty());
        assert_eq!(updated.azure_devops_repositories.len(), 1);
        let rendered = server.get("/settings?saved=1").await;
        assert!(rendered.starts_with("HTTP/1.1 200"), "unexpected response: {rendered}");
        assert!(
            !rendered.contains("octo&#x2f;widgets"),
            "removed repository still rendered: {rendered}"
        );
        assert!(
            rendered.contains("https:&#x2f;&#x2f;dev.azure.com&#x2f;contoso&#x2f;platform&#x2f;_git&#x2f;api"),
            "Azure DevOps repository missing: {rendered}"
        );
        let removed_azure = server
            .post_settings("operation=remove-repository&url=https%3A%2F%2Fdev.azure.com%2Fcontoso%2Fplatform%2F_git%2Fapi")
            .await;
        assert!(removed_azure.starts_with("HTTP/1.1 303"), "unexpected response: {removed_azure}");
        let empty = AppConfig::load(&server.config_path).expect("Azure DevOps removal should load");
        assert!(empty.github_repositories.is_empty());
        assert!(empty.azure_devops_repositories.is_empty());
        server.shutdown_and_join().await;
    }

    #[tokio::test]
    async fn static_css_is_served_with_stylesheet_content_type() {
        let server = TestServer::start().await;
        let response = server.get("/static/app.css").await;
        assert!(response.starts_with("HTTP/1.1 200"), "unexpected response: {response}");
        assert!(
            response.to_ascii_lowercase().contains("content-type: text/css"),
            "css content-type missing: {response}"
        );
        server.shutdown_and_join().await;
    }

    #[tokio::test]
    async fn static_js_is_served_with_javascript_content_type() {
        let server = TestServer::start().await;
        let response = server.get("/static/app.js").await;
        assert!(response.starts_with("HTTP/1.1 200"), "unexpected response: {response}");
        assert!(
            response.to_ascii_lowercase().contains("content-type: text/javascript")
                || response.to_ascii_lowercase().contains("application/javascript"),
            "js content-type missing: {response}"
        );
        server.shutdown_and_join().await;
    }

    #[tokio::test]
    async fn dashboard_includes_accessibility_landmarks() {
        let server = TestServer::start().await;
        let response = server.get("/").await;
        assert!(response.contains("skip"), "skip link missing: {response}");
        assert!(response.contains("aria-live"), "aria-live status region missing: {response}");
        assert!(response.contains("id=\"main"), "main landmark missing: {response}");
        server.shutdown_and_join().await;
    }

    #[tokio::test]
    async fn enqueue_analysis_form_fallback_redirects_for_html_clients() {
        let server = TestServer::start().await;
        let (repository, _pull_request_id, number) = server.seed_repository_with_pull_request().await;
        let body = format!("csrf_token={}", server.csrf_token);
        let request = format!(
            "POST /api/repositories/{}/pull-requests/{number}/analysis HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: http://127.0.0.1:{}\r\nAccept: text/html\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            repository.id.0,
            server.port,
            server.port,
            body.len(),
            body
        );
        let response = server.send(&request).await;
        assert!(response.starts_with("HTTP/1.1 303"), "expected redirect, got: {response}");
        assert!(
            response.to_ascii_lowercase().contains("location:"),
            "redirect Location header missing: {response}"
        );
        server.shutdown_and_join().await;
    }

    #[tokio::test]
    async fn graceful_shutdown_stops_accepting_connections() {
        let server = TestServer::start().await;
        let port = server.port;
        server.shutdown_and_join().await;

        // The listener is owned by `HttpServer::run` and dropped once it returns, so a fresh
        // connection attempt after shutdown must fail outright rather than hang or succeed.
        let outcome = tokio::time::timeout(TokioDuration::from_secs(2), TcpStream::connect((Ipv4Addr::LOCALHOST, port))).await;
        if let Ok(Ok(_)) = outcome {
            panic!("connection unexpectedly succeeded after graceful shutdown");
        }
    }
}
