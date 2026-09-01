// Licensed under the MIT License.

//! Injectable process execution.
//!
//! GitHub credential acquisition shells out to `gh auth token`. Azure DevOps
//! credentials are acquired by `azure_identity` inside Microsoft's SDK and do
//! not use this seam. To keep GitHub tests independent of the host CLI and real
//! accounts, process execution is expressed through [`CommandRunner`].

use std::fmt;
use std::pin::Pin;
use std::process::Stdio;

/// A boxed, `Send` future returned by trait methods so the traits stay
/// object-safe (`dyn`-compatible) and can be stored behind `Arc`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A command to execute: a program and its arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandRequest {
    /// The executable to run (looked up on `PATH`).
    pub(crate) program: String,
    /// Arguments passed to the program.
    pub(crate) args: Vec<String>,
}

impl CommandRequest {
    /// Builds a request from a program name and borrowed argument list.
    pub(crate) fn new(program: impl Into<String>, args: &[&str]) -> Self {
        Self {
            program: program.into(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }
}

/// The captured result of running a command.
#[derive(Clone, Debug)]
pub(crate) struct CommandOutput {
    /// `true` when the process exited with a success status code.
    pub(crate) success: bool,
    /// Captured standard output, lossily decoded as UTF-8.
    pub(crate) stdout: String,
    /// Captured standard error, lossily decoded as UTF-8.
    pub(crate) stderr: String,
}

/// A failure to *launch* a command (e.g. the executable was not found).
///
/// A non-zero exit code is **not** a [`CommandError`]; that is reported through
/// [`CommandOutput::success`] so callers can inspect stderr.
#[derive(ohno::Error)]
#[display("failed to run command `{program}`")]
pub(crate) struct CommandError {
    program: String,
    inner: ohno::OhnoCore,
}

/// Executes external commands.
pub(crate) trait CommandRunner: fmt::Debug + Send + Sync {
    /// Runs `request` to completion and captures its output.
    fn run(&self, request: CommandRequest) -> BoxFuture<'_, Result<CommandOutput, CommandError>>;
}

/// A [`CommandRunner`] backed by [`tokio::process::Command`].
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TokioCommandRunner;

impl CommandRunner for TokioCommandRunner {
    fn run(&self, request: CommandRequest) -> BoxFuture<'_, Result<CommandOutput, CommandError>> {
        Box::pin(async move {
            let output = tokio::process::Command::new(&request.program)
                .args(&request.args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await
                .map_err(|error| CommandError::caused_by(request.program.clone(), error))?;

            Ok(CommandOutput {
                success: output.status.success(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        })
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
pub(crate) mod testing {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::{BoxFuture, CommandError, CommandOutput, CommandRequest, CommandRunner};

    /// A scripted, in-memory [`CommandRunner`] for tests.
    ///
    /// Responses are keyed by program name. Each call pops the next queued
    /// outcome for the requested program, letting tests simulate success,
    /// non-zero exits, and launch failures deterministically.
    #[derive(Debug, Default)]
    pub(crate) struct ScriptedCommandRunner {
        responses: Mutex<HashMap<String, Vec<Result<CommandOutput, String>>>>,
        calls: Mutex<Vec<CommandRequest>>,
    }

    impl ScriptedCommandRunner {
        pub(crate) fn new() -> Self {
            Self::default()
        }

        /// Queues a successful run producing `stdout`.
        pub(crate) fn push_stdout(&self, program: &str, stdout: &str) {
            self.push(
                program,
                Ok(CommandOutput {
                    success: true,
                    stdout: stdout.to_owned(),
                    stderr: String::new(),
                }),
            );
        }

        /// Queues a run that exits non-zero with `stderr`.
        pub(crate) fn push_failure(&self, program: &str, stderr: &str) {
            self.push(
                program,
                Ok(CommandOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: stderr.to_owned(),
                }),
            );
        }

        /// Queues a launch failure (executable not found).
        pub(crate) fn push_launch_error(&self, program: &str, message: &str) {
            self.push(program, Err(message.to_owned()));
        }

        fn push(&self, program: &str, outcome: Result<CommandOutput, String>) {
            self.responses
                .lock()
                .expect("responses lock poisoned")
                .entry(program.to_owned())
                .or_default()
                .push(outcome);
        }

        /// Returns the requests observed so far, in order.
        pub(crate) fn recorded_calls(&self) -> Vec<CommandRequest> {
            self.calls.lock().expect("calls lock poisoned").clone()
        }
    }

    impl CommandRunner for ScriptedCommandRunner {
        fn run(&self, request: CommandRequest) -> BoxFuture<'_, Result<CommandOutput, CommandError>> {
            self.calls.lock().expect("calls lock poisoned").push(request.clone());

            let outcome = self
                .responses
                .lock()
                .expect("responses lock poisoned")
                .get_mut(&request.program)
                .and_then(|queue| if queue.is_empty() { None } else { Some(queue.remove(0)) });

            Box::pin(async move {
                match outcome {
                    Some(Ok(output)) => Ok(output),
                    Some(Err(message)) => Err(CommandError::caused_by(request.program.clone(), message)),
                    None => Err(CommandError::caused_by(request.program.clone(), "no scripted response for program")),
                }
            })
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::testing::ScriptedCommandRunner;
    use super::{CommandRequest, CommandRunner, TokioCommandRunner};

    #[tokio::test]
    async fn scripted_runner_returns_queued_stdout() {
        let runner = ScriptedCommandRunner::new();
        runner.push_stdout("gh", "token-value\n");

        let output = runner
            .run(CommandRequest::new("gh", &["auth", "token"]))
            .await
            .expect("command should launch");

        assert!(output.success);
        assert_eq!(output.stdout, "token-value\n");
        assert_eq!(runner.recorded_calls().len(), 1);
        assert_eq!(runner.recorded_calls()[0].args, vec!["auth", "token"]);
    }

    #[tokio::test]
    async fn scripted_runner_reports_launch_error() {
        let runner = ScriptedCommandRunner::new();
        runner.push_launch_error("az", "program not found");

        let error = runner
            .run(CommandRequest::new("az", &["account"]))
            .await
            .expect_err("launch should fail");
        assert!(error.to_string().contains("az"));
    }

    #[tokio::test]
    async fn tokio_runner_missing_program_is_launch_error() {
        let runner = TokioCommandRunner;
        let error = runner
            .run(CommandRequest::new("definitely-not-a-real-program-xyz", &[]))
            .await
            .expect_err("missing executable should be a launch error");
        assert!(error.to_string().contains("definitely-not-a-real-program-xyz"));
    }
}
