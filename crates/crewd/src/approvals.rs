//! Handoffs between agents and permission asks, and the approvals they
//! wait on.
//!
//! Same lock order as the rest of the service: `state` before `db`.

use crew_core::agent::AgentSpec;
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ApprovalId, RunId, ThreadId};
use crew_core::policy::{self, Decision, PolicyInputs};
use crew_core::scheduler::Job;
use crew_core::store::{
    ApprovalKind, ApprovalStatus, Author, Store, StoreError, StoredApproval, StoredThread,
    handoff_ask_text, paused_text, permission_text,
};
use crew_proto::{PermissionAnswer, ServerMsg};
use tokio::sync::oneshot;

use crate::service::{Db, Service, State};

/// What an agent is told when its run ends before anyone decides.
const RUN_STOPPED: &str = "The run was stopped.";
/// What an agent is told when you decline its permission ask.
const DECLINED: &str = "The user declined this.";

/// What a run asked and how it will hear the answer.
pub(crate) struct PermissionWait {
    /// The approval made for the ask, or `None` if the run was stopping
    /// and the answer is already in `answer`.
    pub(crate) approval: Option<ApprovalId>,
    /// Gets the answer once someone decides. Dropped if crewd goes away
    /// first.
    pub(crate) answer: oneshot::Receiver<PermissionAnswer>,
}

/// Marks permission asks left pending by an earlier crewd as cancelled.
/// Nothing waits for them any more.
pub(crate) fn cancel_stale_permissions(store: &Store) -> Result<(), StoreError> {
    for approval in store.pending_approvals(None)? {
        if matches!(approval.kind, ApprovalKind::Permission { .. }) {
            store.decide_approval(approval.id, ApprovalStatus::Cancelled, None)?;
        }
    }
    Ok(())
}

fn store_err(e: StoreError) -> String {
    e.to_string()
}

fn spec_of(agents: &[(AgentId, AgentSpec)], id: AgentId) -> Result<&AgentSpec, StoreError> {
    agents
        .iter()
        .find(|(a, _)| *a == id)
        .map(|(_, spec)| spec)
        .ok_or(StoreError::NotFound("agent"))
}

/// One handoff request: in `thread`, the agent of run `run` (`from`)
/// brings in `to` with `brief`.
struct HandoffReq {
    thread: ThreadId,
    run: RunId,
    from: AgentId,
    to: AgentId,
    brief: String,
}

/// Whether `run` is live and nobody asked it to stop.
fn running(state: &State, run: RunId) -> bool {
    state
        .live
        .get(&run)
        .is_some_and(|(_, handle)| !handle.is_stopped())
}

impl Service {
    /// Saves a permission ask of a live run and tells the thread. A run
    /// that is stopping is denied at once.
    pub(crate) fn ask_permission(
        &self,
        run: RunId,
        tool: &str,
        input: serde_json::Value,
        tool_use_id: Option<String>,
    ) -> Result<PermissionWait, String> {
        let (tx, rx) = oneshot::channel();
        let mut state = self.state();
        if state.live.contains_key(&run) && !running(&state, run) {
            let _ = tx.send(stopped_answer());
            return Ok(PermissionWait {
                approval: None,
                answer: rx,
            });
        }
        if !running(&state, run) {
            return Err("That run isn't running.".into());
        }
        let mut db = self.db();
        let stored = db.store.run(run).map_err(store_err)?;
        let agents = db.store.agents().map_err(store_err)?;
        let name = spec_of(&agents, stored.agent)
            .map_err(store_err)?
            .name
            .clone();
        let body = permission_text(&name, tool, &input);
        let kind = ApprovalKind::Permission {
            tool: tool.to_string(),
            input,
            tool_use_id,
        };
        let id = db
            .store
            .create_approval(stored.thread, run, &kind)
            .map_err(store_err)?;
        self.publish_approval(&mut db, id).map_err(store_err)?;
        self.add_approval_message(&mut db, stored.thread, &body, run, id)
            .map_err(store_err)?;
        state.waiting.insert(id, tx);
        Ok(PermissionWait {
            approval: Some(id),
            answer: rx,
        })
    }

    /// Cancels a permission ask nobody waits for any more: crew-mcp went
    /// away, or Claude gave up on the call. Does nothing once it is decided.
    pub(crate) fn abandon_permission(&self, id: ApprovalId) {
        let mut state = self.state();
        let mut db = self.db();
        state.waiting.remove(&id);
        let pending = db
            .store
            .approval(id)
            .is_ok_and(|a| a.status == ApprovalStatus::Pending);
        if !pending {
            return;
        }
        let result = db
            .store
            .decide_approval(id, ApprovalStatus::Cancelled, None)
            .and_then(|cancelled| {
                let (thread, run) = (cancelled.thread, cancelled.run);
                self.publish(
                    &mut db,
                    thread,
                    ServerMsg::Approval {
                        thread,
                        approval: cancelled,
                    },
                );
                let agents = db.store.agents_with_deleted()?;
                let agent = db.store.run(run)?.agent;
                let name = spec_of(&agents, agent)?.name.clone();
                let body = format!("{name} stopped waiting for permission.");
                self.add_approval_message(&mut db, thread, &body, run, id)
            });
        if let Err(e) = result {
            crate::service::log(format!("couldn't cancel an abandoned ask: {e}"));
        }
    }

    /// The handoff tool: the agent of live run `run` brings in `agent`.
    /// Returns the text for the agent.
    pub(crate) fn handoff(&self, run: RunId, agent: &str, brief: String) -> Result<String, String> {
        let text = {
            let mut state = self.state();
            if !running(&state, run) {
                return Err("That run isn't running.".into());
            }
            let mut db = self.db();
            let stored = db.store.run(run).map_err(store_err)?;
            let agents = db.store.agents().map_err(store_err)?;
            let wanted = agent.trim().trim_start_matches('@');
            let to = agents
                .iter()
                .find(|(_, spec)| spec.name.eq_ignore_ascii_case(wanted))
                .map(|(id, _)| *id)
                .ok_or_else(|| format!("There's no agent named {wanted}."))?;
            if to == stored.agent {
                return Err("You can't hand off to yourself.".into());
            }
            state.handed.entry(run).or_default().push(to);
            let req = HandoffReq {
                thread: stored.thread,
                run,
                from: stored.agent,
                to,
                brief,
            };
            self.request_handoff(&mut state, &mut db, &agents, req)
                .map_err(store_err)?
        };
        self.start_ready();
        Ok(text)
    }

    /// Hands off from the reply of a run that ended `Done`: each agent it
    /// mentions, minus those the run already handed off to with the tool,
    /// gets the reply as its brief.
    pub(crate) fn reply_handoffs(
        &self,
        state: &mut State,
        db: &mut Db,
        run: RunId,
        agent: AgentId,
        reply: &str,
        handed: &[AgentId],
    ) -> Result<(), StoreError> {
        let agents = db.store.agents()?;
        let names: Vec<&str> = agents.iter().map(|(_, a)| a.name.as_str()).collect();
        let me = spec_of(&agents, agent)?.name.clone();
        let thread = db.store.run(run)?.thread;
        for name in policy::mentions_in_reply(reply, &names, &me) {
            let Some((to, _)) = agents.iter().find(|(_, a)| a.name == name) else {
                continue;
            };
            if handed.contains(to) {
                continue;
            }
            let req = HandoffReq {
                thread,
                run,
                from: agent,
                to: *to,
                brief: reply.to_string(),
            };
            self.request_handoff(state, db, &agents, req)?;
        }
        Ok(())
    }

    /// Applies the thread's handoff policy to one request and returns the
    /// text for the requesting agent.
    fn request_handoff(
        &self,
        state: &mut State,
        db: &mut Db,
        agents: &[(AgentId, AgentSpec)],
        req: HandoffReq,
    ) -> Result<String, StoreError> {
        let from_spec = spec_of(agents, req.from)?;
        let from_name = from_spec.name.clone();
        let to_name = spec_of(agents, req.to)?.name.clone();
        let (thread, run) = (req.thread, req.run);
        let row = db.store.thread(thread)?;
        let inputs = policy_inputs(db, &row, Some(from_spec))?;
        match policy::decide(&inputs) {
            Decision::Ask => {
                let id = self.ask_handoff(db, req, false)?;
                let body = handoff_ask_text(&from_name, &to_name);
                self.add_approval_message(db, thread, &body, run, id)?;
                Ok(format!("Asked the user to approve bringing in {to_name}."))
            }
            Decision::Run => {
                db.store.bump_handoffs_used(thread)?;
                let queued = db.store.create_run(thread, req.to)?;
                let body = format!("{from_name} brought in {to_name}.");
                self.add_message(db, thread, Author::System, &body, Some(run))?;
                self.queue_handoff(state, db, thread, queued, req.to, req.brief);
                Ok(format!("{to_name} will pick this up after you finish."))
            }
            Decision::Pause { budget } => {
                let id = self.ask_handoff(db, req, true)?;
                self.add_approval_message(db, thread, &paused_text(budget), run, id)?;
                Ok("This thread used its handoffs. Asked the user whether to continue.".into())
            }
        }
    }

    fn ask_handoff(
        &self,
        db: &mut Db,
        req: HandoffReq,
        paused: bool,
    ) -> Result<ApprovalId, StoreError> {
        let kind = ApprovalKind::Handoff {
            from: req.from,
            to: req.to,
            brief: req.brief,
            paused,
        };
        let id = db.store.create_approval(req.thread, req.run, &kind)?;
        self.publish_approval(db, id)?;
        Ok(id)
    }

    /// Queues the saved run `run` of `to` with `brief` as its prompt. It
    /// waits behind whatever runs in the thread now.
    fn queue_handoff(
        &self,
        state: &mut State,
        db: &mut Db,
        thread: ThreadId,
        run: RunId,
        to: AgentId,
        brief: String,
    ) {
        self.publish_status(db, thread, run, RunStatus::Queued);
        state.scheduler.enqueue(Job {
            run,
            thread,
            agent: to,
            prompt: brief,
        });
    }

    /// Approves or declines a pending approval.
    pub(crate) fn decide(
        &self,
        id: ApprovalId,
        approve: bool,
        brief: Option<String>,
    ) -> Result<(), String> {
        {
            let mut state = self.state();
            let mut db = self.db();
            let current = db.store.approval(id).map_err(store_err)?;
            if current.status != ApprovalStatus::Pending {
                return Err("That request was already answered.".into());
            }
            if let ApprovalKind::Handoff { to, .. } = &current.kind
                && approve
            {
                let to = db.store.agent(*to).map_err(store_err)?;
                if to.deleted_at.is_some() {
                    return Err(format!(
                        "{} was deleted. Decline this instead.",
                        to.spec.name
                    ));
                }
            }
            // The handoff's own changes come first, so if one fails the
            // approval stays pending.
            let queued = match &current.kind {
                ApprovalKind::Handoff {
                    from, to, paused, ..
                } if approve => Some(
                    self.approve_handoff(&mut db, &current, *from, *to, *paused)
                        .map_err(store_err)?,
                ),
                _ => None,
            };
            let status = if approve {
                ApprovalStatus::Approved
            } else {
                ApprovalStatus::Declined
            };
            let decided = db
                .store
                .decide_approval(id, status, brief)
                .map_err(store_err)?;
            self.publish(
                &mut db,
                decided.thread,
                ServerMsg::Approval {
                    thread: decided.thread,
                    approval: decided.clone(),
                },
            );
            match decided.kind {
                ApprovalKind::Permission { .. } => {
                    if let Some(tx) = state.waiting.remove(&id) {
                        let message = (!approve).then(|| DECLINED.to_string());
                        // The ask may have gone away with its connection.
                        let _ = tx.send(PermissionAnswer {
                            allow: approve,
                            message,
                        });
                    }
                }
                ApprovalKind::Handoff {
                    from, to, brief, ..
                } => {
                    let agents = db.store.agents_with_deleted().map_err(store_err)?;
                    let to_name = spec_of(&agents, to).map_err(store_err)?.name.clone();
                    let body = match queued {
                        Some(run) => {
                            let from_name = spec_of(&agents, from).map_err(store_err)?.name.clone();
                            self.queue_handoff(&mut state, &mut db, decided.thread, run, to, brief);
                            format!("{from_name} brought in {to_name}.")
                        }
                        None => format!("Declined {to_name}."),
                    };
                    self.add_approval_message(&mut db, decided.thread, &body, decided.run, id)
                        .map_err(store_err)?;
                }
            }
        }
        self.start_ready();
        Ok(())
    }

    /// Saves what an approved handoff changes: one more in the budget if
    /// it was paused, one more used, and a new run of `to`. Returns that run.
    fn approve_handoff(
        &self,
        db: &mut Db,
        approval: &StoredApproval,
        from: AgentId,
        to: AgentId,
        paused: bool,
    ) -> Result<RunId, StoreError> {
        let thread = approval.thread;
        if paused {
            // One more, so exactly this handoff runs.
            let agents = db.store.agents_with_deleted()?;
            let row = db.store.thread(thread)?;
            let inputs = policy_inputs(db, &row, Some(spec_of(&agents, from)?))?;
            let raised = policy::budget(&inputs).saturating_add(1);
            db.store
                .set_thread_handoff(thread, row.handoff_policy, Some(raised))?;
        }
        db.store.bump_handoffs_used(thread)?;
        db.store.create_run(thread, to)
    }

    /// Lets `n` more handoffs run in the thread on their own.
    pub(crate) fn allow_more_handoffs(&self, thread: ThreadId, n: u32) -> Result<(), StoreError> {
        let db = self.db();
        let row = db.store.thread(thread)?;
        let budget = policy::budget(&policy_inputs(&db, &row, None)?);
        db.store
            .set_thread_handoff(thread, row.handoff_policy, Some(budget.saturating_add(n)))
    }

    /// Cancels the run's pending permission asks and tells each waiting
    /// agent the run was stopped. Handoff asks stay: they are about work
    /// after the run.
    pub(crate) fn cancel_permissions(&self, state: &mut State, db: &mut Db, run: RunId) {
        let pending = match db.store.pending_approvals(None) {
            Ok(pending) => pending,
            Err(e) => {
                crate::service::log(format!("couldn't read the asks of run {run}: {e}"));
                return;
            }
        };
        for approval in pending {
            if approval.run != run || !matches!(approval.kind, ApprovalKind::Permission { .. }) {
                continue;
            }
            match db
                .store
                .decide_approval(approval.id, ApprovalStatus::Cancelled, None)
            {
                Ok(cancelled) => self.publish(
                    db,
                    cancelled.thread,
                    ServerMsg::Approval {
                        thread: cancelled.thread,
                        approval: cancelled,
                    },
                ),
                Err(e) => crate::service::log(format!("couldn't cancel an ask: {e}")),
            }
            if let Some(tx) = state.waiting.remove(&approval.id) {
                let _ = tx.send(stopped_answer());
            }
        }
    }

    fn publish_approval(&self, db: &mut Db, id: ApprovalId) -> Result<(), StoreError> {
        let approval = db.store.approval(id)?;
        let thread = approval.thread;
        self.publish(db, thread, ServerMsg::Approval { thread, approval });
        Ok(())
    }
}

/// The answer to a permission ask whose run ended first.
pub(crate) fn stopped_answer() -> PermissionAnswer {
    PermissionAnswer {
        allow: false,
        message: Some(RUN_STOPPED.into()),
    }
}

/// The policy layers for a handoff in `thread` asked by `agent`, if any.
fn policy_inputs(
    db: &Db,
    thread: &StoredThread,
    agent: Option<&AgentSpec>,
) -> Result<PolicyInputs, StoreError> {
    let project = db.store.project_of_thread(thread.id)?;
    let channel = db
        .store
        .channels(project.id)?
        .into_iter()
        .find(|c| c.id == thread.channel)
        .ok_or(StoreError::NotFound("channel"))?;
    Ok(PolicyInputs {
        thread: (thread.handoff_policy, thread.handoff_budget),
        agent: agent.map_or((None, None), |a| (a.handoff_policy, a.handoff_budget)),
        channel: (channel.handoff_policy, channel.handoff_budget),
        used: thread.handoffs_used,
    })
}
