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
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const BACKOFF_TTL: Duration = Duration::from_secs(600);

pub(super) struct Gate {
    global: Arc<Semaphore>,
    per_host: usize,
    spacing: Duration,
    hosts: Mutex<HashMap<String, Arc<HostSlot>>>,
}

struct HostSlot {
    permits: Arc<Semaphore>,
    timing: Mutex<Timing>,
}

struct Timing {
    next_start: Instant,
    spacing: Duration,
    penalized_until: Instant,
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
        }
    }

    /// Takes the host permit first, so waiters on one busy host never hold global capacity, then
    /// global capacity before booking host spacing, so global contention cannot bunch starts.
    pub(super) async fn enter(&self, host: &str, deadline: Instant) -> Result<Pass, FetchError> {
        let slot = self.slot(host);
        let host_permit = timeout_at(deadline, Arc::clone(&slot.permits).acquire_owned())
            .await
            .map_err(|_| FetchError::Timeout)?
            .map_err(|_| FetchError::Protocol)?;
        let global = timeout_at(deadline, Arc::clone(&self.global).acquire_owned())
            .await
            .map_err(|_| FetchError::Overloaded)?
            .map_err(|_| FetchError::Protocol)?;
        loop {
            let start = {
                let mut timing = slot.timing.lock().unwrap_or_else(PoisonError::into_inner);
                let now = Instant::now();
                if now >= deadline {
                    return Err(FetchError::Timeout);
                }
                if timing.penalized_until <= now {
                    timing.spacing = self.spacing;
                }
                if timing.next_start <= now {
                    timing.next_start = now + timing.spacing;
                    break;
                }
                timing.next_start
            };
            if start >= deadline {
                return Err(FetchError::Timeout);
            }
            sleep_until(start).await;
        }
        Ok(Pass {
            _host: host_permit,
            _slot: slot,
            _global: global,
        })
    }

    /// Response feedback uses the same host entry as all interactive and job fetches.
    pub(super) fn penalize(&self, host: &str, retry_after: Option<Duration>) {
        let slot = self.slot(host);
        let mut timing = slot.timing.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        if timing.penalized_until <= now {
            timing.spacing = self.spacing;
        }
        timing.spacing = timing
            .spacing
            .max(Duration::from_millis(1))
            .saturating_mul(2)
            .max(retry_after.unwrap_or_default())
            .min(MAX_BACKOFF)
            .max(self.spacing);
        timing.penalized_until = now + BACKOFF_TTL;
        timing.next_start = timing.next_start.max(now + timing.spacing);
    }

    pub(super) fn reward(&self, host: &str) {
        let slot = self.slot(host);
        let mut timing = slot.timing.lock().unwrap_or_else(PoisonError::into_inner);
        timing.spacing = (timing.spacing / 2).max(self.spacing);
    }

    fn slot(&self, host: &str) -> Arc<HostSlot> {
        let mut hosts = self.hosts.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(slot) = hosts.get(host) {
            return Arc::clone(slot);
        }
        if hosts.len() >= MAX_TRACKED_HOSTS {
            let now = Instant::now();
            hosts.retain(|_, slot| {
                let timing = slot.timing.lock().unwrap_or_else(PoisonError::into_inner);
                Arc::strong_count(slot) > 1
                    || timing.next_start > now
                    || timing.penalized_until > now
            });
        }
        let slot = hosts.entry(host.to_owned()).or_insert_with(|| {
            Arc::new(HostSlot {
                permits: Arc::new(Semaphore::new(self.per_host)),
                timing: Mutex::new(Timing {
                    next_start: Instant::now(),
                    spacing: self.spacing,
                    penalized_until: Instant::now(),
                }),
            })
        });
        Arc::clone(slot)
    }
}
