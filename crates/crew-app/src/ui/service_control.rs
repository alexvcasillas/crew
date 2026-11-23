//! "Start Crew service" and "Restart service". From the app bundle, crewd
//! runs under launchd; a dev build runs the crewd beside it. Either way the
//! app checks until crewd answers, then has the client connect.
//!
//! Anything that would stop a running crewd (a restart, reloading a
//! changed LaunchAgent, stopping a crewd launchd doesn't manage) first
//! counts its agents, and asks before stopping any.

use std::time::{Duration, Instant};

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::*;

use super::workspace::Workspace;
use crate::connection::{self, ConnectionConfig, ConnectionState, StartError};
use crate::launchd::{self, Plan};
use crate::theme::color;

/// How long "Start Crew service" waits for crewd to answer.
const START_WAIT: Duration = Duration::from_secs(5);
/// How long "Restart service" waits for the old crewd to stop. Its runs
/// get up to 8 s to stop.
const STOP_WAIT: Duration = Duration::from_secs(10);

/// What was asked of the service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ServiceAction {
    /// "Start Crew service", or the app opening from its bundle.
    Start,
    /// "Restart service". From the bundle it also asks the login shell
    /// for `PATH` again.
    Restart,
    /// Restarting an out-of-date crewd on its own. Never stops agents.
    AutoRestart,
}

impl ServiceAction {
    fn restart(self) -> bool {
        self != ServiceAction::Start
    }
}

/// What to do, given whether it would stop a running crewd and how many
/// agents that crewd runs.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Go,
    /// Ask before stopping this many agents.
    Ask(usize),
    /// Leave crewd as it is.
    Stay,
}

fn decide(stops_crewd: bool, running: usize, action: ServiceAction) -> Decision {
    if !stops_crewd || running == 0 {
        Decision::Go
    } else if action == ServiceAction::AutoRestart {
        Decision::Stay
    } else {
        Decision::Ask(running)
    }
}

/// Whether the crewd answering on the socket (`peer`) is not the one
/// launchd runs (`job`), such as one a dev build started.
fn unmanaged(peer: Option<u32>, job: Option<u32>) -> bool {
    peer.is_some() && peer != job
}

fn restart_ask(running: usize) -> String {
    if running == 1 {
        "Restarting stops 1 running agent. Its changes are kept.".into()
    } else {
        format!("Restarting stops {running} running agents. Their changes are kept.")
    }
}

/// What a look at crewd found, before anything changes.
struct Look {
    /// The pid answering on the socket.
    peer: Option<u32>,
    /// Its child processes: its running agents.
    children: usize,
    /// From the bundle: what launchd would do.
    plan: Option<Result<Plan, launchd::Error>>,
}

/// The work [`Workspace::run_service`] does.
#[derive(Clone)]
struct Work {
    restart: bool,
    /// From the bundle: the LaunchAgent to apply.
    plan: Option<Plan>,
    /// Stop the crewd on the socket first: launchd doesn't run it.
    stop_unmanaged: bool,
    /// Wait for a crewd that is connected, not one that only answers.
    wait_connected: bool,
}

impl Workspace {
    /// Starts or restarts crewd as `action` says, asking first if that
    /// stops running agents.
    pub(super) fn service(
        &mut self,
        action: ServiceAction,
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
            let look = executor
                .spawn(async move {
                    let peer = connection::crewd_pid(&conn);
                    Look {
                        peer,
                        children: peer.map_or(0, connection::children_of),
                        plan: bundled.map(|crewd| {
                            let fresh_path = action == ServiceAction::Restart;
                            launchd::plan(&crewd, &conn.crew_home, fresh_path)
                        }),
                    }
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.starting = false;
                this.decide_service(action, look, window, cx);
                cx.notify();
            })
            .ok();
        }));
    }

    fn decide_service(
        &mut self,
        action: ServiceAction,
        look: Look,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let restart = action.restart();
        let plan = match look.plan {
            Some(Ok(plan)) => Some(plan),
            Some(Err(e)) => {
                let error = StartFailure::Launchd(e).message(&self.conn);
                self.show_error(error, window, cx);
                return;
            }
            None => None,
        };
        let stop_unmanaged = plan
            .as_ref()
            .is_some_and(|p| unmanaged(look.peer, p.job_pid));
        let reloads_live = plan
            .as_ref()
            .is_some_and(|p| p.reloads() && look.peer.is_some());
        let stops_crewd = restart || stop_unmanaged || reloads_live;
        let work = Work {
            restart,
            plan,
            stop_unmanaged,
            wait_connected: stops_crewd,
        };
        // crewd's own processes are its agents. What the app last saw
        // also counts queued runs.
        let running = self.state().running().len().max(look.children);
        match decide(stops_crewd, running, action) {
            Decision::Go => self.run_service(work, window, cx),
            Decision::Ask(n) => self.ask_to_restart(n, work, window, cx),
            Decision::Stay => {}
        }
    }

    fn ask_to_restart(
        &mut self,
        running: usize,
        work: Work,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ask = restart_ask(running);
        let this = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm = this.clone();
            let work = work.clone();
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
                                    let work = work.clone();
                                    confirm
                                        .update(cx, |this, cx| this.run_service(work, window, cx))
                                        .ok();
                                }),
                        ),
                )
        });
    }

    /// Does `work`, then checks until crewd answers or the wait passes.
    /// Whatever happens, the shown state ends up as a fresh check says.
    fn run_service(&mut self, work: Work, window: &mut Window, cx: &mut Context<Self>) {
        if self.starting {
            return;
        }
        self.starting = true;
        cx.notify();
        let conn = self.conn.clone();
        let executor = cx.background_executor().clone();
        let wait_connected = work.wait_connected;
        self.service_task = Some(cx.spawn_in(window, async move |this, cx| {
            let start_conn = conn.clone();
            let started = executor
                .spawn(async move { do_work(&work, &start_conn) })
                .await;
            let state = match &started {
                Ok(()) if wait_connected => {
                    Some(wait_for_crewd(&executor, &conn, STOP_WAIT + START_WAIT, true).await)
                }
                Ok(()) => Some(wait_for_crewd(&executor, &conn, START_WAIT, false).await),
                Err(_) => None,
            };
            this.update_in(cx, |this, window, cx| {
                this.starting = false;
                let error = match started {
                    Err(e) => Some(e.message(&conn)),
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

/// Stops what must stop and starts crewd. Blocks.
fn do_work(work: &Work, conn: &ConnectionConfig) -> Result<(), StartFailure> {
    let stop_first = work.stop_unmanaged || (work.restart && work.plan.is_none());
    if stop_first && !connection::stop_crewd(conn, STOP_WAIT) {
        return Err(StartFailure::StillRunning);
    }
    match &work.plan {
        Some(plan) => launchd::apply(plan, work.restart).map_err(StartFailure::Launchd),
        None => connection::start_crewd(conn).map_err(StartFailure::Start),
    }
}

/// Why "Start" or "Restart" failed before crewd could be checked.
enum StartFailure {
    StillRunning,
    Start(StartError),
    Launchd(launchd::Error),
}

impl StartFailure {
    fn message(&self, conn: &ConnectionConfig) -> String {
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
            StartFailure::Launchd(e) => format!(
                "Crew's service couldn't start ({e}). Check its log at {}.",
                conn.log().display()
            ),
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
    use super::{Decision, ServiceAction, decide, restart_ask, unmanaged};

    #[test]
    fn stopping_a_busy_crewd_asks_first() {
        use ServiceAction::*;
        assert_eq!(decide(false, 3, Start), Decision::Go, "nothing stops");
        assert_eq!(decide(true, 0, Restart), Decision::Go, "nothing runs");
        assert_eq!(decide(true, 0, AutoRestart), Decision::Go);
        assert_eq!(decide(true, 2, Restart), Decision::Ask(2));
        // Opening the app over a changed LaunchAgent and busy crewd.
        assert_eq!(decide(true, 2, Start), Decision::Ask(2));
        assert_eq!(decide(true, 2, AutoRestart), Decision::Stay);
    }

    #[test]
    fn a_crewd_launchd_doesnt_run_is_unmanaged() {
        assert!(!unmanaged(None, None), "nothing answers");
        assert!(!unmanaged(Some(812), Some(812)), "launchd's own");
        assert!(unmanaged(Some(900), Some(812)));
        assert!(unmanaged(Some(900), None));
    }

    #[test]
    fn restart_ask_counts_the_agents() {
        assert_eq!(
            restart_ask(1),
            "Restarting stops 1 running agent. Its changes are kept."
        );
        assert_eq!(
            restart_ask(3),
            "Restarting stops 3 running agents. Their changes are kept."
        );
    }
}
