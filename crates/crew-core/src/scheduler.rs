//! Decides which queued agent runs may start.

use std::collections::HashSet;

use crate::ids::{AgentId, RunId, ThreadId};

/// A queued agent run.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub run: RunId,
    pub thread: ThreadId,
    pub agent: AgentId,
    pub prompt: String,
}

/// Limits parallel runs: a global cap, one active run per thread, and
/// first in, first out order.
pub struct Scheduler {
    cap: usize,
    paused: bool,
    queue: Vec<Job>,
    active: HashSet<ThreadId>,
}

impl Scheduler {
    /// Creates a scheduler that allows at most `cap` active runs.
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            paused: false,
            queue: Vec::new(),
            active: HashSet::new(),
        }
    }

    /// Adds a job to the queue.
    pub fn enqueue(&mut self, job: Job) {
        self.queue.push(job);
    }

    /// Changes the cap. Active runs above a lowered cap keep going; new
    /// ones wait until fewer than `cap` are active.
    pub fn set_cap(&mut self, cap: usize) {
        self.cap = cap;
    }

    /// While paused nothing new starts. Queued jobs keep their order.
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// How many jobs wait in the queue.
    pub fn queued_len(&self) -> usize {
        self.queue.len()
    }

    /// Takes the next job that may start and marks its thread active.
    pub fn next_ready(&mut self) -> Option<Job> {
        if self.paused || self.active.len() >= self.cap {
            return None;
        }
        let pos = self
            .queue
            .iter()
            .position(|j| !self.active.contains(&j.thread))?;
        let job = self.queue.remove(pos);
        self.active.insert(job.thread);
        Some(job)
    }

    /// Marks `thread` active without a job, so nothing starts there until
    /// [`Scheduler::finished`]. Returns false if the thread is already active.
    pub fn claim(&mut self, thread: ThreadId) -> bool {
        self.active.insert(thread)
    }

    /// Frees the slot of `thread`. Does nothing if it has no active run.
    pub fn finished(&mut self, thread: ThreadId) {
        self.active.remove(&thread);
    }

    /// Takes the queued job of `run` out of the queue, if it is there.
    pub fn remove(&mut self, run: RunId) -> Option<Job> {
        let pos = self.queue.iter().position(|j| j.run == run)?;
        Some(self.queue.remove(pos))
    }

    /// Drops all queued jobs and returns them in enqueue order.
    /// Active runs are not touched.
    pub fn clear(&mut self) -> Vec<Job> {
        std::mem::take(&mut self.queue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(thread: ThreadId, prompt: &str) -> Job {
        Job {
            run: RunId::new(),
            thread,
            agent: AgentId::new(),
            prompt: prompt.into(),
        }
    }

    #[test]
    fn one_active_run_per_thread() {
        let mut s = Scheduler::new(4);
        let t = ThreadId::new();
        s.enqueue(job(t, "a"));
        s.enqueue(job(t, "b"));
        assert_eq!(s.next_ready().unwrap().prompt, "a");
        assert!(s.next_ready().is_none());
        s.finished(t);
        assert_eq!(s.next_ready().unwrap().prompt, "b");
    }

    #[test]
    fn claimed_thread_waits() {
        let mut s = Scheduler::new(4);
        let t = ThreadId::new();
        assert!(s.claim(t));
        assert!(!s.claim(t));
        s.enqueue(job(t, "a"));
        assert!(s.next_ready().is_none());
        s.finished(t);
        assert_eq!(s.next_ready().unwrap().prompt, "a");
        assert!(!s.claim(t));
    }

    #[test]
    fn cap_limits_parallel_threads() {
        let mut s = Scheduler::new(2);
        let (t1, t2, t3) = (ThreadId::new(), ThreadId::new(), ThreadId::new());
        s.enqueue(job(t1, "1"));
        s.enqueue(job(t2, "2"));
        s.enqueue(job(t3, "3"));
        assert_eq!(s.next_ready().unwrap().prompt, "1");
        assert_eq!(s.next_ready().unwrap().prompt, "2");
        assert!(s.next_ready().is_none());
        s.finished(t1);
        assert_eq!(s.next_ready().unwrap().prompt, "3");
    }

    #[test]
    fn thread_queue_is_fifo() {
        let mut s = Scheduler::new(1);
        let t = ThreadId::new();
        for p in ["a", "b", "c"] {
            s.enqueue(job(t, p));
        }
        for p in ["a", "b", "c"] {
            assert_eq!(s.next_ready().unwrap().prompt, p);
            s.finished(t);
        }
        assert!(s.next_ready().is_none());
    }

    #[test]
    fn oldest_queued_thread_goes_first() {
        let mut s = Scheduler::new(1);
        let (t1, t2) = (ThreadId::new(), ThreadId::new());
        s.enqueue(job(t1, "t1-a"));
        s.enqueue(job(t2, "t2-a"));
        s.enqueue(job(t1, "t1-b"));
        assert_eq!(s.next_ready().unwrap().prompt, "t1-a");
        s.finished(t1);
        assert_eq!(s.next_ready().unwrap().prompt, "t2-a");
        s.finished(t2);
        assert_eq!(s.next_ready().unwrap().prompt, "t1-b");
    }

    #[test]
    fn blocked_thread_does_not_block_others() {
        let mut s = Scheduler::new(4);
        let (t1, t2) = (ThreadId::new(), ThreadId::new());
        s.enqueue(job(t1, "t1-a"));
        s.enqueue(job(t1, "t1-b"));
        s.enqueue(job(t2, "t2-a"));
        assert_eq!(s.next_ready().unwrap().prompt, "t1-a");
        assert_eq!(s.next_ready().unwrap().prompt, "t2-a");
        assert!(s.next_ready().is_none());
    }

    #[test]
    fn finished_without_active_run_is_noop() {
        let mut s = Scheduler::new(1);
        let t = ThreadId::new();
        s.finished(t);
        s.enqueue(job(t, "a"));
        assert_eq!(s.next_ready().unwrap().prompt, "a");
    }

    #[test]
    fn remove_takes_one_queued_job() {
        let mut s = Scheduler::new(1);
        let (t1, t2) = (ThreadId::new(), ThreadId::new());
        s.enqueue(job(t1, "a"));
        let b = job(t2, "b");
        s.enqueue(b.clone());
        s.enqueue(job(t2, "c"));
        assert_eq!(s.remove(b.run), Some(b.clone()));
        assert_eq!(s.remove(b.run), None);
        assert_eq!(s.next_ready().unwrap().prompt, "a");
        s.finished(t1);
        assert_eq!(s.next_ready().unwrap().prompt, "c");
    }

    #[test]
    fn clear_returns_queued_jobs() {
        let mut s = Scheduler::new(1);
        let (t1, t2) = (ThreadId::new(), ThreadId::new());
        s.enqueue(job(t1, "a"));
        s.enqueue(job(t2, "b"));
        s.enqueue(job(t1, "c"));
        assert_eq!(s.next_ready().unwrap().prompt, "a");
        let dropped: Vec<_> = s.clear().into_iter().map(|j| j.prompt).collect();
        assert_eq!(dropped, vec!["b", "c"]);
        assert!(s.next_ready().is_none());
        s.finished(t1);
        assert!(s.next_ready().is_none());
    }

    #[test]
    fn paused_holds_jobs_and_resumes_in_order() {
        let mut s = Scheduler::new(4);
        let (t1, t2) = (ThreadId::new(), ThreadId::new());
        s.set_paused(true);
        assert!(s.is_paused());
        s.enqueue(job(t1, "a"));
        s.enqueue(job(t2, "b"));
        assert!(s.next_ready().is_none());
        assert_eq!(s.queued_len(), 2);
        s.set_paused(false);
        assert_eq!(s.next_ready().unwrap().prompt, "a");
        assert_eq!(s.next_ready().unwrap().prompt, "b");
        assert_eq!(s.queued_len(), 0);
    }

    #[test]
    fn lowered_cap_waits_for_live_runs_to_drop_below_it() {
        let mut s = Scheduler::new(3);
        let ts: Vec<_> = (0..4).map(|_| ThreadId::new()).collect();
        for (i, t) in ts.iter().take(3).enumerate() {
            s.enqueue(job(*t, &i.to_string()));
            assert!(s.next_ready().is_some());
        }
        s.set_cap(1);
        s.enqueue(job(ts[3], "x"));
        assert!(s.next_ready().is_none());
        s.finished(ts[0]);
        s.finished(ts[1]);
        assert!(s.next_ready().is_none());
        s.finished(ts[2]);
        assert_eq!(s.next_ready().unwrap().prompt, "x");
    }
}
