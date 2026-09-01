// Licensed under the MIT License.

use std::fmt;

use anyspawn::Spawner;
use storage::Storage;

use crate::copilot::{AnalysisScheduler, CopilotService};
use crate::error::AppError;
use crate::http_server::HttpServer;
use crate::polling::PollScheduler;
use crate::shutdown;

enum StopReason {
    Server(Result<(), std::io::Error>),
    Signal(Result<(), AppError>),
}

pub struct Application {
    spawner: Spawner,
    storage: Storage,
    http_server: HttpServer,
    poll_scheduler: PollScheduler,
    analysis_scheduler: AnalysisScheduler,
    copilot: CopilotService,
}

impl Application {
    pub(crate) fn new<D>(dependencies: &D) -> Self
    where
        D: AsRef<Spawner> + AsRef<Storage> + AsRef<HttpServer> + AsRef<PollScheduler> + AsRef<AnalysisScheduler> + AsRef<CopilotService>,
    {
        Self {
            spawner: AsRef::<Spawner>::as_ref(dependencies).clone(),
            storage: AsRef::<Storage>::as_ref(dependencies).clone(),
            http_server: AsRef::<HttpServer>::as_ref(dependencies).clone(),
            poll_scheduler: AsRef::<PollScheduler>::as_ref(dependencies).clone(),
            analysis_scheduler: AsRef::<AnalysisScheduler>::as_ref(dependencies).clone(),
            copilot: AsRef::<CopilotService>::as_ref(dependencies).clone(),
        }
    }

    pub(crate) async fn run(self) -> Result<(), AppError> {
        self.storage.ensure_ready().map_err(AppError::caused_by)?;

        let (shutdown_trigger, shutdown_listener) = shutdown::channel();
        let mut server = Box::pin(self.spawner.spawn(self.http_server.run(shutdown_listener.clone())));
        let poller = self.spawner.spawn(self.poll_scheduler.run(shutdown_listener.clone()));
        let analysis_scheduler = self.spawner.spawn(self.analysis_scheduler.run(shutdown_listener.clone()));
        let copilot = self.spawner.spawn(self.copilot.run(shutdown_listener));

        let stop_reason = tokio::select! {
            result = &mut server => StopReason::Server(result),
            result = shutdown::wait_for_signal() => StopReason::Signal(result),
        };
        shutdown_trigger.trigger();
        tracing::info!("shutdown requested; waiting for background work to stop");

        match stop_reason {
            StopReason::Server(result) => {
                let ((), (), ()) = tokio::join!(poller, analysis_scheduler, copilot);
                result.map_err(AppError::caused_by)?;
            }
            StopReason::Signal(result) => {
                let (server_result, (), (), ()) = tokio::join!(server, poller, analysis_scheduler, copilot);
                server_result.map_err(AppError::caused_by)?;
                result?;
            }
        }
        tracing::info!("PR review dashboard stopped");
        Ok(())
    }
}

impl fmt::Debug for Application {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Application").finish_non_exhaustive()
    }
}
