//! Scheduled jobs: their requests, the timer loop that fires them and the
//! prompt a job run gets.

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Local, TimeZone, Weekday};
use crew_core::agent::Runtime;
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, JobId, RunId, ThreadId};
use crew_core::schedule::{Repeat, WeekdaySet, missed_between, next_after};
use crew_core::scheduler::Job;
use crew_core::store::{Author, JobPatch, Missed, NewJob, RunFilter, StoreError, StoredJob};
use crew_proto::{ServerMsg, SkillRef};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::service::{Db, Pending, Service, State, log, tidy_model_override};

/// The repeat intervals a job may use, in minutes.
const EVERY_MINUTES: [u16; 5] = [15, 30, 60, 120, 360];
/// The longest the timer loop sleeps before it looks again.
const LONGEST_SLEEP: Duration = Duration::from_secs(60);
/// The shortest, so a job that can't move on doesn't spin the loop.
const SHORTEST_SLEEP: Duration = Duration::from_secs(1);
/// Waking this much later than planned means the Mac slept.
const SLEEP_GAP: Duration = Duration::from_secs(90);
/// How many earlier found reports a run is shown.
const REPORTS_SHOWN: u32 = 3;
/// Why a job of a deleted agent was turned off.
const AGENT_DELETED: &str = "Agent deleted";
const AGENT_GONE: &str = "That agent no longer exists. Pick another.";
pub(crate) const ALREADY_RUNNING: &str = "This job is already running.";
/// The line that tells a job run how to report.
const REPORT_LINE: &str = "When you're done, call the `report` tool once: found=true only if there is something new the user hasn't been told about above, with a one-line summary. Otherwise found=false.";

/// The time jobs go by, the real clock unless a test set one, and a nudge
/// for the timer loop when a job's next time changed.
#[derive(Default)]
pub(crate) struct Clock {
    fixed: Mutex<Option<DateTime<Local>>>,
    changed: Notify,
}

impl Clock {
    pub(crate) fn now(&self) -> DateTime<Local> {
        self.fixed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .unwrap_or_else(Local::now)
    }

    /// Stops the clock at `now`, for tests.
    #[cfg(test)]
    pub(crate) fn set(&self, now: DateTime<Local>) {
        *self.fixed.lock().unwrap_or_else(PoisonError::into_inner) = Some(now);
    }
}

/// Unix ms as local time.
fn local(ms: i64) -> DateTime<Local> {
    Local
        .timestamp_millis_opt(ms)
        .earliest()
        .unwrap_or_else(Local::now)
}

/// The message a job run gets: the job's message, what its last runs
/// found (newest first, as Unix ms and text), then how to report.
pub(crate) fn job_prompt(job: &StoredJob, found: &[(i64, String)]) -> String {
    let mut prompt = String::new();
    if !job.body.is_empty() {
        prompt.push_str(&job.body);
        prompt.push_str("\n\n");
    }
    if !found.is_empty() {
        prompt.push_str("Earlier reports from this job, newest first:\n");
        for (at, text) in found {
            let at = local(*at).format("%a %-d %b %H:%M");
            prompt.push_str(&format!("- {at}: {text}\n"));
        }
        prompt.push('\n');
    }
    prompt.push_str(REPORT_LINE);
    prompt
}

fn no_days(days: WeekdaySet) -> bool {
    use Weekday::*;
    ![Mon, Tue, Wed, Thu, Fri, Sat, Sun]
        .into_iter()
        .any(|d| days.contains(d))
}

fn check_repeat(repeat: &Repeat) -> Result<(), String> {
    match *repeat {
        Repeat::Every {
            minutes,
            days,
            window,
        } => {
            if !EVERY_MINUTES.contains(&minutes) {
                return Err("Repeat every 15 or 30 minutes, or every 1, 2 or 6 hours.".into());
            }
            if days.is_some_and(no_days) {
                return Err("Pick at least one day.".into());
            }
            if window.is_some_and(|(start, end)| start == end) {
                return Err("The window has to start and end at different times.".into());
            }
        }
        Repeat::Daily { .. } => {}
        Repeat::Weekly { days, .. } => {
            if no_days(days) {
                return Err("Pick at least one day.".into());
            }
        }
    }
    Ok(())
}

/// Refuses a job that can't run as it is.
fn check(db: &Db, job: &NewJob) -> Result<(), String> {
    if job.name.trim().is_empty() {
        return Err("Give the job a name.".into());
    }
    if job.body.trim().is_empty() && job.skill.is_none() {
        return Err("Say what the job should do.".into());
    }
    check_repeat(&job.repeat)?;
    let agent = db.store.agent(job.agent).map_err(|_| AGENT_GONE)?;
    if agent.deleted_at.is_some() {
        return Err(AGENT_GONE.into());
    }
    db.store
        .ensure_channel_open(job.channel)
        .map_err(|e| e.to_string())
}

/// A skill name with blanks trimmed, and none for an empty one.
fn tidy_skill(skill: Option<String>) -> Option<String> {
    skill
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn err(e: StoreError) -> String {
    e.to_string()
}

/// Whether a run in the thread is queued or working.
fn thread_busy(db: &Db, thread: ThreadId) -> Result<bool, StoreError> {
    let runs = db.store.runs(RunFilter {
        status: None,
        thread: Some(thread),
    })?;
    Ok(runs
        .iter()
        .any(|r| matches!(r.status, RunStatus::Queued | RunStatus::Working)))
}

/// Whether the job's latest run waits on an answer from you.
fn waiting_on_you(db: &Db, job: &StoredJob) -> Result<bool, StoreError> {
    let Some(last) = db.store.job_runs(job.id, 1)?.into_iter().next() else {
        return Ok(false);
    };
    Ok(db
        .store
        .pending_approvals(Some(job.thread))?
        .iter()
        .any(|a| a.run == last.id))
}

impl Service {
    /// Fires jobs as they come due. It sleeps until the earliest next
    /// time, at most a minute, and wakes early when a job's time changes.
    /// macOS sleep stops the loop, so waking well after the planned time
    /// means the Mac slept; `fire_due` then deals with the missed slots.
    pub fn run_jobs(&self) -> JoinHandle<()> {
        let svc = self.clone();
        tokio::spawn(async move {
            svc.schedule_unscheduled();
            loop {
                let now = svc.clock().now();
                svc.find_skills_due(now).await;
                svc.fire_due(now);
                let wait = svc.until_next_due(now);
                tokio::select! {
                    () = tokio::time::sleep(wait) => {
                        let woke = svc.clock().now();
                        let late = (woke - now).to_std().unwrap_or_default().saturating_sub(wait);
                        if late > SLEEP_GAP {
                            log(format!("jobs: woke {} s late, the Mac likely slept", late.as_secs()));
                        }
                    }
                    () = svc.clock().changed.notified() => {}
                }
            }
        })
    }

    /// Gives enabled jobs without a next time one, as when crewd stopped
    /// before saving it.
    fn schedule_unscheduled(&self) {
        let now = self.clock().now();
        let db = self.db();
        let jobs = match db.store.jobs() {
            Ok(jobs) => jobs,
            Err(e) => return log(format!("couldn't read the jobs: {e}")),
        };
        for job in jobs.iter().filter(|j| j.enabled && j.next_due.is_none()) {
            let next = next_after(&job.repeat, now).timestamp_millis();
            if let Err(e) = db.store.set_job_next_due(job.id, Some(next)) {
                log(format!("couldn't schedule job {}: {e}", job.id));
            }
        }
    }

    /// Looks up the Codex skills of the jobs due at `now`, so their runs
    /// can name the skill's file.
    async fn find_skills_due(&self, now: DateTime<Local>) {
        let due: Vec<StoredJob> = match self.db().store.jobs() {
            Ok(jobs) => jobs
                .into_iter()
                .filter(|j| j.enabled && j.next_due.is_some_and(|d| d <= now.timestamp_millis()))
                .collect(),
            Err(e) => return log(format!("couldn't read the jobs: {e}")),
        };
        for job in &due {
            self.find_skill(job).await;
        }
    }

    /// Looks up the skills of the job's project when its agent uses Codex
    /// and the job's skill hasn't been found yet. Codex needs the skill's
    /// file, which only a listing gives.
    async fn find_skill(&self, job: &StoredJob) {
        let Some(name) = &job.skill else { return };
        let project = {
            let db = self.db();
            let codex = db
                .store
                .agent(job.agent)
                .is_ok_and(|a| a.spec.runtime == Runtime::Codex);
            match db.store.project_of_thread(job.thread) {
                Ok(p) if codex => p.id,
                _ => return,
            }
        };
        if self
            .cached_skill_path(project, &Runtime::Codex, name)
            .is_empty()
            && let Err(e) = self.skills(project, false).await
        {
            log(format!("couldn't list the skills of job {}: {e}", job.id));
        }
    }

    /// How long until the earliest next time of an enabled job, within
    /// the loop's bounds.
    fn until_next_due(&self, now: DateTime<Local>) -> Duration {
        let earliest = match self.db().store.jobs() {
            Ok(jobs) => jobs
                .iter()
                .filter(|j| j.enabled)
                .filter_map(|j| j.next_due)
                .min(),
            Err(e) => {
                log(format!("couldn't read the jobs: {e}"));
                None
            }
        };
        let wait = earliest.map_or(LONGEST_SLEEP, |due| {
            let ms = u64::try_from(due - now.timestamp_millis()).unwrap_or(0);
            Duration::from_millis(ms)
        });
        wait.clamp(SHORTEST_SLEEP, LONGEST_SLEEP)
    }

    /// Fires every enabled job due at `now`. Each one's next time moves
    /// past `now` first, so a slot never fires twice. A job whose slots
    /// came due while the Mac slept runs once, noting how many more it
    /// missed, or with `Skip` just moves on. One still running, or
    /// waiting on you, skips the slot and counts it.
    pub fn fire_due(&self, now: DateTime<Local>) {
        let queued = {
            let mut state = self.state();
            let mut db = self.db();
            let jobs = match db.store.jobs() {
                Ok(jobs) => jobs,
                Err(e) => {
                    log(format!("couldn't read the jobs: {e}"));
                    return;
                }
            };
            let mut queued = false;
            for job in &jobs {
                match self.fire_one(&mut state, &mut db, job, now) {
                    Ok(q) => queued |= q,
                    Err(e) => log(format!("job {} didn't run: {e}", job.id)),
                }
            }
            queued
        };
        if queued {
            self.start_ready();
        }
    }

    /// Fires the job if it is due. Says whether a run was queued.
    fn fire_one(
        &self,
        state: &mut State,
        db: &mut Db,
        job: &StoredJob,
        now: DateTime<Local>,
    ) -> Result<bool, String> {
        let Some(due) = job.next_due.filter(|_| job.enabled) else {
            return Ok(false);
        };
        if due > now.timestamp_millis() {
            return Ok(false);
        }
        // Slots after the one that is due.
        let missed = missed_between(&job.repeat, local(due), now);
        let next = next_after(&job.repeat, now).timestamp_millis();
        db.store.set_job_next_due(job.id, Some(next)).map_err(err)?;
        let queued = if missed > 0 && job.missed == Missed::Skip {
            false
        } else if thread_busy(db, job.thread).map_err(err)?
            || waiting_on_you(db, job).map_err(err)?
        {
            db.store
                .set_job_skipped(job.id, job.skipped.saturating_add(1))
                .map_err(err)?;
            false
        } else {
            self.queue_job_run(state, db, job, missed)?;
            true
        };
        self.job_changed(db, job.id)?;
        Ok(queued)
    }

    /// Queues a run of the job in its thread. No message is added: the
    /// run answers the job's newest message, and starts a session of its
    /// own with the job's prompt. A Codex skill whose file wasn't found
    /// fails the run at once, with a note in the thread.
    fn queue_job_run(
        &self,
        state: &mut State,
        db: &mut Db,
        job: &StoredJob,
        missed: u32,
    ) -> Result<RunId, String> {
        db.store.ensure_thread_open(job.thread).map_err(err)?;
        let agent = db.store.agent(job.agent).map_err(|_| AGENT_GONE)?;
        if agent.deleted_at.is_some() {
            return Err(AGENT_GONE.into());
        }
        let message = db
            .store
            .messages(job.thread)
            .map_err(err)?
            .into_iter()
            .rev()
            .find(|m| m.job_message)
            .map(|m| m.id)
            .ok_or("The job's message is missing.")?;
        let found = db.store.recent_found(job.id, REPORTS_SHOWN).map_err(err)?;
        let prompt = job_prompt(job, &found);
        let skill = match &job.skill {
            Some(name) => {
                let project = db.store.project_of_thread(job.thread).map_err(err)?.id;
                Some(SkillRef {
                    path: self.cached_skill_path(project, &agent.spec.runtime, name),
                    runtime: agent.spec.runtime.clone(),
                    name: name.clone(),
                    agent: Some(job.agent),
                })
            }
            None => None,
        };
        let run = db
            .store
            .create_job_run(job.thread, job.agent, job.id, missed)
            .map_err(err)?;
        if let Some(name) = &job.skill {
            db.store.set_run_skill(run, name).map_err(err)?;
        }
        db.store
            .set_run_request(run, message, job.model_override.as_deref())
            .map_err(err)?;
        db.store.set_job_skipped(job.id, 0).map_err(err)?;
        // Codex can't run a skill without its file: the slot fails, and
        // the schedule goes on.
        if let Some(skill) = skill
            .as_ref()
            .filter(|s| s.runtime == Runtime::Codex && s.path.is_empty())
        {
            db.store
                .set_run_status(run, RunStatus::Failed)
                .map_err(err)?;
            self.publish_status(db, job.thread, run, RunStatus::Failed);
            let note = format!(
                "Couldn't find the skill /{}. Edit the job to pick another.",
                skill.name
            );
            self.add_message(db, job.thread, Author::System, &note, Some(run))
                .map_err(err)?;
            return Ok(run);
        }
        self.publish_status(db, job.thread, run, RunStatus::Queued);
        state.scheduler.enqueue(Job {
            run,
            thread: job.thread,
            agent: job.agent,
            prompt: prompt.clone(),
        });
        state.pending.insert(
            run,
            Pending {
                skill,
                model_override: job.model_override.clone(),
                message,
                fresh: prompt,
            },
        );
        Ok(run)
    }

    /// Tells `SubscribeAll` clients the job as it is now, and the timer
    /// loop that its time may have changed.
    fn job_changed(&self, db: &mut Db, job: JobId) -> Result<(), String> {
        let job = db.store.job(job).map_err(err)?;
        self.publish_global(db, ServerMsg::JobChanged { job: Box::new(job) });
        self.clock().changed.notify_one();
        Ok(())
    }

    /// Creates the job and its thread. An enabled one is due at its next
    /// slot.
    pub(crate) fn create_job(&self, mut new: NewJob) -> Result<JobId, String> {
        new.model_override = tidy_model_override(new.model_override)?;
        new.skill = tidy_skill(new.skill);
        let now = self.clock().now();
        let mut db = self.db();
        check(&db, &new)?;
        let id = db.store.create_job(&new).map_err(err)?;
        let thread = db.store.job(id).map_err(err)?.thread;
        let thread = db.store.thread(thread).map_err(err)?;
        self.publish_global(
            &mut db,
            ServerMsg::ThreadCreated {
                channel: new.channel,
                thread,
            },
        );
        if new.enabled {
            let next = next_after(&new.repeat, now).timestamp_millis();
            db.store.set_job_next_due(id, Some(next)).map_err(err)?;
        }
        self.job_changed(&mut db, id)?;
        Ok(id)
    }

    /// Changes the job. A new message or skill is posted to its thread
    /// for later runs to answer. A new repeat moves the next time.
    pub(crate) fn update_job(&self, id: JobId, mut patch: JobPatch) -> Result<(), String> {
        if let Some(model) = patch.model_override.take() {
            patch.model_override = Some(tidy_model_override(model)?);
        }
        if let Some(skill) = patch.skill.take() {
            patch.skill = Some(tidy_skill(skill));
        }
        let now = self.clock().now();
        let mut db = self.db();
        let old = db.store.job(id).map_err(err)?;
        let new = NewJob {
            name: patch.name.clone().unwrap_or_else(|| old.name.clone()),
            body: patch.body.clone().unwrap_or_else(|| old.body.clone()),
            skill: patch.skill.clone().unwrap_or_else(|| old.skill.clone()),
            model_override: patch
                .model_override
                .clone()
                .unwrap_or_else(|| old.model_override.clone()),
            agent: patch.agent.unwrap_or(old.agent),
            channel: old.channel,
            repeat: patch.repeat.unwrap_or(old.repeat),
            tell: patch.tell.unwrap_or(old.tell),
            on_ask: patch.on_ask.unwrap_or(old.on_ask),
            missed: patch.missed.unwrap_or(old.missed),
            enabled: old.enabled,
        };
        check(&db, &new)?;
        db.store.update_job(id, &patch).map_err(err)?;
        if new.body != old.body || new.skill != old.skill {
            let message = db
                .store
                .add_job_message(old.thread, &new.body)
                .map_err(err)?;
            self.publish_message(&mut db, old.thread, message)
                .map_err(err)?;
        }
        if new.repeat != old.repeat && old.enabled {
            let next = next_after(&new.repeat, now).timestamp_millis();
            db.store.set_job_next_due(id, Some(next)).map_err(err)?;
        }
        self.job_changed(&mut db, id)
    }

    /// Turns the job on, due at its next slot, or off with no next time.
    pub(crate) fn set_job_enabled(&self, id: JobId, enabled: bool) -> Result<(), String> {
        let now = self.clock().now();
        let mut db = self.db();
        let job = db.store.job(id).map_err(err)?;
        if enabled {
            let agent = db.store.agent(job.agent).map_err(|_| AGENT_GONE)?;
            if agent.deleted_at.is_some() {
                return Err(AGENT_GONE.into());
            }
            let next = next_after(&job.repeat, now).timestamp_millis();
            db.store.set_job_paused(id, None).map_err(err)?;
            db.store.set_job_next_due(id, Some(next)).map_err(err)?;
        } else {
            db.store.set_job_next_due(id, None).map_err(err)?;
            db.store.set_job_skipped(id, 0).map_err(err)?;
        }
        db.store.set_job_enabled(id, enabled).map_err(err)?;
        self.job_changed(&mut db, id)
    }

    /// Deletes the job. Its thread stays as a normal thread.
    pub(crate) fn delete_job(&self, id: JobId) -> Result<(), String> {
        let mut db = self.db();
        db.store.delete_job(id).map_err(err)?;
        self.publish_global(&mut db, ServerMsg::JobRemoved { job: id });
        Ok(())
    }

    /// Runs the job now, outside its schedule, which stays as it was.
    pub(crate) async fn run_job_now(&self, id: JobId) -> Result<RunId, String> {
        let job = self.db().store.job(id).map_err(err)?;
        self.find_skill(&job).await;
        let run = {
            let mut state = self.state();
            let mut db = self.db();
            let job = db.store.job(id).map_err(err)?;
            if thread_busy(&db, job.thread).map_err(err)? {
                return Err(ALREADY_RUNNING.into());
            }
            let run = self.queue_job_run(&mut state, &mut db, &job, 0)?;
            self.job_changed(&mut db, id)?;
            run
        };
        self.start_ready();
        Ok(run)
    }

    /// Creates the job turned off and runs it once.
    pub(crate) async fn test_job(&self, mut new: NewJob) -> Result<JobId, String> {
        new.enabled = false;
        let id = self.create_job(new)?;
        self.run_job_now(id).await?;
        Ok(id)
    }

    /// Turns off the jobs of a deleted agent, saying why.
    pub(crate) fn pause_agent_jobs(&self, db: &mut Db, agent: AgentId) -> Result<(), String> {
        for job in db.store.jobs_for_agent(agent).map_err(err)? {
            db.store
                .set_job_paused(job.id, Some(AGENT_DELETED))
                .map_err(err)?;
            db.store.set_job_enabled(job.id, false).map_err(err)?;
            db.store.set_job_next_due(job.id, None).map_err(err)?;
            self.job_changed(db, job.id)?;
        }
        Ok(())
    }

    /// Deletes the job whose thread this is, if any, as when the thread
    /// is archived.
    pub(crate) fn delete_thread_job(&self, db: &mut Db, thread: ThreadId) -> Result<(), String> {
        if let Some(job) = db.store.job_for_thread(thread).map_err(err)? {
            db.store.delete_job(job.id).map_err(err)?;
            self.publish_global(db, ServerMsg::JobRemoved { job: job.id });
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "jobs_tests.rs"]
mod tests;
