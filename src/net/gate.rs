//! Politeness: a global concurrency limit plus per-host concurrency and spacing.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::{Instant, sleep_until, timeout_at},
};

use super::FetchError;
use crate::config::Config;

/// Above this many tracked hosts, idle entries are dropped.
const MAX_TRACKED_HOSTS: usize = 1024;
const MAX_PENALTIES: usize = 4096;
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const BACKOFF_TTL: Duration = Duration::from_secs(600);

pub(super) struct Gate {
    global: Arc<Semaphore>,
    per_host: usize,
    spacing: Duration,
    hosts: Mutex<HashMap<String, Arc<HostSlot>>>,
    penalties: Mutex<HashMap<String, Penalty>>,
}

struct HostSlot {
    permits: Arc<Semaphore>,
    next_start: Mutex<Instant>,
}

struct Penalty {
    spacing: Duration,
    until: Instant,
}

/// Held while one request and its body are in flight. Holding the slot keeps it tracked, so
/// eviction never splits a busy host into two slots with separate limits and spacing.
pub(super) struct Pass {
    _host: OwnedSemaphorePermit,
    _slot: Arc<HostSlot>,
    _global: OwnedSemaphorePermit,
}

impl Gate {
    pub(super) fn new(cfg: &Config) -> Self {
        Self {
            global: Arc::new(Semaphore::new(cfg.global_concurrency)),
            per_host: cfg.per_host_concurrency,
            spacing: Duration::from_millis(cfg.per_host_spacing_ms),
            hosts: Mutex::default(),
            penalties: Mutex::default(),
        }
    }

    /// Host queueing and spacing never hold global capacity.
    pub(super) async fn enter(&self, host: &str, deadline: Instant) -> Result<Pass, FetchError> {
        let slot = self.slot(host);
        let host_permit = timeout_at(deadline, Arc::clone(&slot.permits).acquire_owned())
            .await
            .map_err(|_| FetchError::Timeout)?
            .map_err(|_| FetchError::Protocol)?;
        let start = {
            let mut penalties = self
                .penalties
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let now = Instant::now();
            if penalties
                .get(host)
                .is_some_and(|penalty| penalty.until <= now)
            {
                penalties.remove(host);
            }
            let spacing = penalties
                .get(host)
                .map_or(self.spacing, |penalty| penalty.spacing);
            let mut next = slot
                .next_start
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let start = (*next).max(now);
            if start >= deadline {
                return Err(FetchError::Timeout);
            }
            *next = start + spacing;
            start
        };
        sleep_until(start).await;
        if Instant::now() >= deadline {
            return Err(FetchError::Timeout);
        }
        let global = timeout_at(deadline, Arc::clone(&self.global).acquire_owned())
            .await
            .map_err(|_| FetchError::Overloaded)?
            .map_err(|_| FetchError::Protocol)?;
        Ok(Pass {
            _host: host_permit,
            _slot: slot,
            _global: global,
        })
    }

    /// Response feedback uses the same host entry as all interactive and job fetches.
    pub(super) fn penalize(&self, host: &str, retry_after: Option<Duration>) {
        let slot = self.slot(host);
        let now = Instant::now();
        let mut penalties = self
            .penalties
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        penalties.retain(|_, penalty| penalty.until > now);
        let spacing = penalties
            .get(host)
            .map_or(self.spacing, |penalty| penalty.spacing)
            .max(Duration::from_millis(1))
            .saturating_mul(2)
            .max(retry_after.unwrap_or_default())
            .min(MAX_BACKOFF)
            .max(self.spacing);
        if spacing > self.spacing {
            if penalties.len() >= MAX_PENALTIES
                && !penalties.contains_key(host)
                && let Some(oldest) = penalties
                    .iter()
                    .min_by_key(|(_, penalty)| penalty.until)
                    .map(|(host, _)| host.clone())
            {
                penalties.remove(&oldest);
            }
            penalties.insert(
                host.to_owned(),
                Penalty {
                    spacing,
                    until: now + BACKOFF_TTL,
                },
            );
        } else {
            penalties.remove(host);
        }
        let mut next = slot
            .next_start
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *next = (*next).max(now + spacing);
    }

    pub(super) fn reward(&self, host: &str) {
        let mut penalties = self
            .penalties
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(penalty) = penalties.get_mut(host) {
            penalty.spacing = (penalty.spacing / 2).max(self.spacing);
            if penalty.until <= Instant::now() || penalty.spacing == self.spacing {
                penalties.remove(host);
            }
        }
    }

    /// A cheap retry preflight; admission still checks live contention and the deadline.
    pub(super) fn next_start(&self, host: &str) -> Instant {
        let slot = self.slot(host);
        let next = slot
            .next_start
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        (*next).max(Instant::now())
    }

    fn slot(&self, host: &str) -> Arc<HostSlot> {
        let mut hosts = self.hosts.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(slot) = hosts.get(host) {
            return Arc::clone(slot);
        }
        if hosts.len() >= MAX_TRACKED_HOSTS {
            let now = Instant::now();
            hosts.retain(|_, slot| {
                Arc::strong_count(slot) > 1
                    || *slot
                        .next_start
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        > now
            });
        }
        let slot = hosts.entry(host.to_owned()).or_insert_with(|| {
            Arc::new(HostSlot {
                permits: Arc::new(Semaphore::new(self.per_host)),
                next_start: Mutex::new(Instant::now()),
            })
        });
        Arc::clone(slot)
    }
}
