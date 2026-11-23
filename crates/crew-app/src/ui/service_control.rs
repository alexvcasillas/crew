//! "Start Crew service" and "Restart service": run crewd, then check
//! until it answers. Task 9 replaces the spawning with launchd.

use std::time::{Duration, Instant};

use gpui_kit::*;

use super::workspace::Workspace;
use crate::connection::{self, ConnectionConfig, ConnectionState, StartError};

/// How long "Start Crew service" waits for crewd to answer.
const START_WAIT: Duration = Duration::from_secs(5);
/// How long "Restart service" waits for the old crewd to stop. Its runs
/// get up to 8 s to stop.
const STOP_WAIT: Duration = Duration::from_secs(10);

impl Workspace {
    /// Starts crewd (stopping the running one first if `restart`), then
    /// checks until it answers or [`START_WAIT`] passes. Whatever happens,
    /// the shown state ends up as a fresh check says.
    pub(super) fn start_service(
        &mut self,
        restart: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.starting {
            return;
        }
        self.starting = true;
        cx.notify();
        let conn = self.conn.clone();
        let executor = cx.background_executor().clone();
        self.probe = Some(cx.spawn_in(window, async move |this, cx| {
            let start_conn = conn.clone();
            let started = executor
                .spawn(async move {
                    if restart && !connection::stop_crewd(&start_conn, STOP_WAIT) {
                        return Err(StartFailure::StillRunning);
                    }
                    connection::start_crewd(&start_conn).map_err(StartFailure::Start)
                })
                .await;
            let state = match &started {
                Ok(()) => wait_for_crewd(&executor, &conn).await,
                Err(_) => {
                    let probe_conn = conn.clone();
                    executor
                        .spawn(async move { connection::probe(&probe_conn) })
                        .await
                }
            };
            this.update_in(cx, |this, window, cx| {
                this.starting = false;
                let error = match started {
                    Err(e) => Some(e.message()),
                    Ok(()) if state == ConnectionState::NotRunning => Some(format!(
                        "Crew's service didn't start. Its log is at {}.",
                        conn.log().display()
                    )),
                    Ok(()) => None,
                };
                if let Some(error) = error {
                    this.show_error(error, window, cx);
                }
                this.set_connection(state, cx);
            })
            .ok();
        }));
    }
}

/// Why "Start" or "Restart" failed before crewd could be checked.
enum StartFailure {
    StillRunning,
    Start(StartError),
}

impl StartFailure {
    fn message(&self) -> String {
        match self {
            StartFailure::StillRunning => {
                "Crew's old service didn't stop. Quit it, then try again.".into()
            }
            StartFailure::Start(StartError::NotFound) => {
                "crewd wasn't found next to Crew. Reinstall Crew.".into()
            }
            StartFailure::Start(StartError::Io(e)) => {
                format!("Crew's service couldn't start ({e}). Reinstall Crew.")
            }
        }
    }
}

/// Probes crewd with growing pauses until it answers or [`START_WAIT`]
/// of real time passes.
async fn wait_for_crewd(executor: &BackgroundExecutor, conn: &ConnectionConfig) -> ConnectionState {
    let started = Instant::now();
    let mut pause = Duration::from_millis(100);
    loop {
        let probe_conn = conn.clone();
        let state = executor
            .spawn(async move { connection::probe(&probe_conn) })
            .await;
        if state != ConnectionState::NotRunning || started.elapsed() >= START_WAIT {
            return state;
        }
        executor.timer(pause).await;
        pause = (pause * 2).min(Duration::from_secs(1));
    }
}
