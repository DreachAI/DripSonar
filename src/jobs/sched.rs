//! Jobs get round-robin turns; each holds only one bounded SQL page of hosts.

use std::{
    collections::{HashMap, VecDeque},
    time::Duration,
};
use tokio::time::Instant;

pub(super) const HOST_PAGE: usize = 64;

struct Host {
    inflight: usize,
    next: Instant,
}

struct Job {
    hosts: VecDeque<String>,
    cursor: String,
    next_at: Instant,
    delay: Duration,
    inflight: usize,
}

pub(super) enum Next {
    Refill { job: String, after: String },
    Lease { job: String, host: String },
}

pub(super) struct Scheduler {
    jobs: HashMap<String, Job>,
    turns: VecDeque<String>,
    hosts: HashMap<String, Host>,
    concurrency: usize,
    spacing: Duration,
}

impl Scheduler {
    pub fn new(concurrency: usize, spacing_ms: u64) -> Self {
        Self {
            jobs: HashMap::new(),
            turns: VecDeque::new(),
            hosts: HashMap::new(),
            concurrency,
            spacing: Duration::from_millis(spacing_ms),
        }
    }

    pub fn wake(&mut self, id: String, delay_ms: u64) {
        self.jobs.entry(id.clone()).or_insert_with(|| Job {
            hosts: VecDeque::new(),
            cursor: String::new(),
            next_at: Instant::now(),
            delay: Duration::from_millis(delay_ms),
            inflight: 0,
        });
        if !self.turns.contains(&id) {
            self.turns.push_back(id);
        }
    }

    pub fn refill(&mut self, id: &str, hosts: Vec<String>) {
        if let Some(job) = self.jobs.get_mut(id) {
            if let Some(last) = hosts.last() {
                job.cursor.clone_from(last);
                job.hosts = hosts.into();
            } else {
                self.turns.retain(|turn| turn != id);
                if job.inflight == 0 {
                    self.jobs.remove(id);
                }
            }
        }
    }

    fn host_due(&self, host: &str, now: Instant) -> Option<Instant> {
        match self.hosts.get(host) {
            Some(entry) if entry.inflight >= self.concurrency => None,
            Some(entry) => Some(entry.next),
            None => Some(now),
        }
    }

    pub fn due(&self) -> Option<Instant> {
        let now = Instant::now();
        self.turns
            .iter()
            .filter_map(|id| {
                let job = self.jobs.get(id)?;
                let host_at = if job.hosts.is_empty() {
                    now
                } else {
                    job.hosts
                        .iter()
                        .filter_map(|host| self.host_due(host, now))
                        .min()?
                };
                Some(job.next_at.max(host_at))
            })
            .min()
    }

    pub fn take(&mut self) -> Option<Next> {
        let now = Instant::now();
        self.hosts
            .retain(|_, host| host.inflight != 0 || host.next > now);
        for _ in 0..self.turns.len() {
            let id = self.turns.pop_front()?;
            self.turns.push_back(id.clone());
            let job = self.jobs.get(&id)?;
            if job.next_at > now {
                continue;
            }
            if job.hosts.is_empty() {
                return Some(Next::Refill {
                    job: id,
                    after: job.cursor.clone(),
                });
            }
            let Some(index) = job
                .hosts
                .iter()
                .position(|host| self.host_due(host, now).is_some_and(|at| at <= now))
            else {
                continue;
            };
            let job = self.jobs.get_mut(&id)?;
            let host = job.hosts.remove(index)?;
            job.next_at = now + job.delay;
            job.inflight += 1;
            let entry = self.hosts.entry(host.clone()).or_insert(Host {
                inflight: 0,
                next: now,
            });
            entry.inflight += 1;
            entry.next = now + self.spacing;
            return Some(Next::Lease { job: id, host });
        }
        None
    }

    pub fn release(&mut self, id: &str, host: &str) {
        if let Some(entry) = self.hosts.get_mut(host) {
            entry.inflight = entry.inflight.saturating_sub(1);
        }
        if let Some(job) = self.jobs.get_mut(id) {
            job.inflight = job.inflight.saturating_sub(1);
        }
    }

    pub fn started(&mut self, id: &str, host: &str) {
        let now = Instant::now();
        if let Some(job) = self.jobs.get_mut(id) {
            job.next_at = now + job.delay;
        }
        if let Some(entry) = self.hosts.get_mut(host) {
            entry.next = now + self.spacing;
        }
    }
}
