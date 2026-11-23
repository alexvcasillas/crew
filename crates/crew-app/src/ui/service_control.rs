//! "Start Crew service" and "Restart service". From the app bundle, crewd
//! runs under launchd; a dev build runs the crewd beside it. Either way the
//! app checks until crewd answers, then has the client connect.

use std::time::{Duration, Instant};

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::*;

use super::workspace::Workspace;
use crate::connection::{self, ConnectionConfig, ConnectionState, StartError};
use crate::launchd;
use crate::theme::color;

/// How long "Start Crew service" waits for crewd to answer.
const START_WAIT: Duration = Duration::from_secs(5);
/// How long "Restart service" waits for the old crewd to stop. Its runs
/// get up to 8 s to stop.
const STOP_WAIT: Duration = Duration::from_secs(10);

/// What "Restart service" does, given how many agents are running.
#[derive(Debug, PartialEq, Eq)]
enum RestartPlan {
    Go,
    /// Ask before stopping this many agents.
    Ask(usize),
    /// Leave it: the restart was automatic and agents are running.
    Stay,
}

fn restart_plan(running: usize, auto: bool) -> RestartPlan {
    match (running, auto) {
        (0, _) => RestartPlan::Go,
        (_, true) => RestartPlan::Stay,
        (n, false) => RestartPlan::Ask(n),
    }
}

fn restart_ask(running: usize) -> String {
    let agents = if running == 1 { "agent" } else { "agents" };
    format!("Restarting stops {running} running {agents}. Their changes are kept.")
}

impl Workspace {
    /// Restarts crewd, asking first if agents are running. An `auto`
    /// restart (of an out-of-date crewd) doesn't ask: it leaves a busy
    /// crewd alone.
    pub(super) fn restart_service(
        &mut self,
        auto: bool,
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
        self.service_task = Some(cx.spawn_in(window, async move |this, cx| {
            // crewd's own processes are its agents. What the app last saw
            // also counts queued runs.
            let children = executor
                .spawn(async move { connection::crewd_children(&conn) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.starting = false;
                let running = this.state().running().len().max(children);
                match restart_plan(running, auto) {
                    RestartPlan::Go => this.start_service(true, window, cx),
                    RestartPlan::Ask(n) => this.ask_to_restart(n, window, cx),
                    RestartPlan::Stay => {}
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn ask_to_restart(&mut self, running: usize, window: &mut Window, cx: &mut Context<Self>) {
        let ask = restart_ask(running);
        let this = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm = this.clone();
            dialog
                .w(px(420.))
                .title("Restart service")
                .child(
                    div()
                        .id("restart-ask")
                        .text_color(color("text-secondary"))
                        .aria_label(ask.clone())
                        .child(ask.clone())
                        .test_support(),
                )
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("restart-cancel")
                                .ghost()
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("restart-confirm")
                                .danger()
                                .label("Restart")
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    confirm
                                        .update(cx, |this, cx| this.start_service(true, window, cx))
                                        .ok();
                                }),
                        ),
                )
        });
    }

    /// Starts crewd (restarting the running one if `restart`), then
    /// checks until it answers or the wait passes. Whatever happens, the
    /// shown state ends up as a fresh check says.
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
        let bundled = self.bundled_crewd.clone();
        let executor = cx.background_executor().clone();
        self.service_task = Some(cx.spawn_in(window, async move |this, cx| {
            let start_conn = conn.clone();
            let started = executor
                .spawn(async move {
                    match bundled {
                        Some(crewd) if restart => launchd::restart(&crewd, &start_conn.crew_home)
                            .map_err(StartFailure::Launchd),
                        Some(crewd) => launchd::ensure(&crewd, &start_conn.crew_home)
                            .map_err(StartFailure::Launchd),
                        None => {
                            if restart && !connection::stop_crewd(&start_conn, STOP_WAIT) {
                                return Err(StartFailure::StillRunning);
                            }
                            connection::start_crewd(&start_conn).map_err(StartFailure::Start)
                        }
                    }
                })
                .await;
            // A restarted crewd must be the new one; a started one only
            // has to answer.
            let state = match &started {
                Ok(()) if restart => {
                    Some(wait_for_crewd(&executor, &conn, STOP_WAIT + START_WAIT, true).await)
                }
                Ok(()) => Some(wait_for_crewd(&executor, &conn, START_WAIT, false).await),
                Err(_) => None,
            };
            this.update_in(cx, |this, window, cx| {
                this.starting = false;
                let error = match started {
                    Err(e) => Some(e.message()),
                    Ok(()) if state == Some(ConnectionState::NotRunning) => Some(format!(
                        "Crew's service didn't start. Its log is at {}.",
                        conn.log().display()
                    )),
                    Ok(()) => None,
                };
                if let Some(error) = error {
                    this.show_error(error, window, cx);
                }
                // The client reports what it finds, without waiting out
                // its backoff.
                this.client.reconnect_now();
                cx.notify();
            })
            .ok();
        }));
    }
}

/// Why "Start" or "Restart" failed before crewd could be checked.
enum StartFailure {
    StillRunning,
    Start(StartError),
    Launchd(launchd::Error),
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
            StartFailure::Launchd(e) => {
                format!("Crew's service couldn't start ({e}). Reinstall Crew.")
            }
        }
    }
}

/// Probes crewd with growing pauses until it answers (and is connected,
/// if `connected`) or `wait` of real time passes.
async fn wait_for_crewd(
    executor: &BackgroundExecutor,
    conn: &ConnectionConfig,
    wait: Duration,
    connected: bool,
) -> ConnectionState {
    let started = Instant::now();
    let mut pause = Duration::from_millis(100);
    loop {
        let probe_conn = conn.clone();
        let state = executor
            .spawn(async move { connection::probe(&probe_conn) })
            .await;
        let done = match state {
            ConnectionState::Connected => true,
            ConnectionState::NotRunning => false,
            _ => !connected,
        };
        if done || started.elapsed() >= wait {
            return state;
        }
        executor.timer(pause).await;
        pause = (pause * 2).min(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::{RestartPlan, restart_ask, restart_plan};

    #[test]
    fn restart_asks_first_when_agents_run() {
        assert_eq!(restart_plan(0, false), RestartPlan::Go);
        assert_eq!(restart_plan(0, true), RestartPlan::Go);
        assert_eq!(restart_plan(2, false), RestartPlan::Ask(2));
        assert_eq!(restart_plan(2, true), RestartPlan::Stay);
    }

    #[test]
    fn restart_ask_counts_the_agents() {
        assert_eq!(
            restart_ask(1),
            "Restarting stops 1 running agent. Their changes are kept."
        );
        assert_eq!(
            restart_ask(3),
            "Restarting stops 3 running agents. Their changes are kept."
        );
    }
}
