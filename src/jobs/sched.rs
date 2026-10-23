//! Host scheduling is advisory; the network gate remains the authority for politeness.

use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap},
    time::Duration,
};
use tokio::time::Instant;

struct Host {
    inflight: usize,
    next: Instant,
    last_started: Option<Instant>,
    spacing: Duration,
    queued: bool,
    in_heap: bool,
}

pub(super) struct Scheduler {
    hosts: HashMap<String, Host>,
    ready: BinaryHeap<Reverse<(Instant, String)>>,
    concurrency: usize,
    spacing: Duration,
}

impl Scheduler {
    pub fn new(concurrency: usize, spacing_ms: u64) -> Self {
        Self {
            hosts: HashMap::new(),
            ready: BinaryHeap::new(),
            concurrency,
            spacing: Duration::from_millis(spacing_ms),
        }
    }

    pub fn wake(&mut self, host: String, delay_ms: u64) {
        let entry = self.hosts.entry(host.clone()).or_insert(Host {
            inflight: 0,
            next: Instant::now(),
            last_started: None,
            spacing: self.spacing,
            queued: true,
            in_heap: false,
        });
        entry.queued = true;
        entry.spacing = entry.spacing.max(Duration::from_millis(delay_ms));
        if let Some(last) = entry.last_started {
            entry.next = entry.next.max(last + entry.spacing);
        }
        self.push(&host);
    }

    fn push(&mut self, host: &str) {
        if let Some(entry) = self.hosts.get_mut(host)
            && entry.queued
            && !entry.in_heap
            && entry.inflight < self.concurrency
        {
            self.ready.push(Reverse((entry.next, host.to_owned())));
            entry.in_heap = true;
        }
    }

    pub fn due(&self) -> Option<Instant> {
        self.ready.peek().map(|Reverse((at, _))| *at)
    }

    pub fn take(&mut self) -> Option<String> {
        if self.due()? > Instant::now() {
            return None;
        }
        let Reverse((_, host)) = self.ready.pop()?;
        let entry = self.hosts.get_mut(&host)?;
        entry.in_heap = false;
        // A newly submitted crawl may have raised this host's spacing since it was enqueued.
        if entry.next > Instant::now() {
            self.push(&host);
            return None;
        }
        entry.inflight += 1;
        debug_assert!(entry.inflight <= self.concurrency);
        let now = Instant::now();
        entry.last_started = Some(now);
        entry.next = now + entry.spacing;
        self.push(&host);
        Some(host)
    }

    pub fn release(&mut self, host: &str, empty: bool) {
        if let Some(entry) = self.hosts.get_mut(host) {
            entry.inflight = entry.inflight.saturating_sub(1);
            if empty {
                entry.queued = false;
            }
        }
        self.push(host);
        // A queued heap entry may need one final empty lease before it can be forgotten.
        if self
            .hosts
            .get(host)
            .is_some_and(|entry| !entry.queued && entry.inflight == 0 && !entry.in_heap)
        {
            self.hosts.remove(host);
        }
    }
}
