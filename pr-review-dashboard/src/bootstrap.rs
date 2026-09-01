// Licensed under the MIT License.

use anyspawn::Spawner;
use providers::ProviderRegistry;
use storage::Storage;
use tick::Clock;

use crate::application::Application;
use crate::config::AppConfig;
use crate::copilot::{AnalysisScheduler, CopilotService};
use crate::http_server::HttpServer;
use crate::polling::PollScheduler;
use crate::prereqs::PrerequisiteReport;
use crate::templates::Templates;

#[fundle::bundle]
struct AppDependencies {
    config: AppConfig,
    prerequisites: PrerequisiteReport,
    spawner: Spawner,
    clock: Clock,
    storage: Storage,
    templates: Templates,
    providers: ProviderRegistry,
    copilot: CopilotService,
    analysis_scheduler: AnalysisScheduler,
    poll_scheduler: PollScheduler,
    http_server: HttpServer,
    application: Application,
}

pub(crate) fn build(config: &AppConfig, prerequisites: PrerequisiteReport) -> Application {
    AppDependencies::builder()
        .config(|_| config.clone())
        .prerequisites(move |_| prerequisites.clone())
        .spawner(|_| Spawner::new_tokio())
        .clock(|_| Clock::new_tokio())
        .storage(|dependencies| {
            let config = AsRef::<AppConfig>::as_ref(dependencies);
            Storage::new(config.database_path.clone())
        })
        .templates(|_| Templates::new())
        .providers(|_| ProviderRegistry::new())
        .copilot(CopilotService::new)
        .analysis_scheduler(AnalysisScheduler::new)
        .poll_scheduler(PollScheduler::new)
        .http_server(HttpServer::new)
        .application(Application::new)
        .build()
        .application
}
