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

pub(super) struct Gate {
    global: Arc<Semaphore>,
    per_host: usize,
    spacing: Duration,
    hosts: Mutex<HashMap<String, Arc<HostSlot>>>,
}

struct HostSlot {
    permits: Arc<Semaphore>,
    next_start: Mutex<Instant>,
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

    /// Takes global capacity before booking host spacing, so global contention cannot bunch starts.
    pub(super) async fn enter(&self, host: &str, deadline: Instant) -> Result<Pass, FetchError> {
        let global = timeout_at(deadline, Arc::clone(&self.global).acquire_owned())
            .await
            .map_err(|_| FetchError::Overloaded)?
            .map_err(|_| FetchError::Protocol)?;
        let slot = self.slot(host);
        let host_permit = timeout_at(deadline, Arc::clone(&slot.permits).acquire_owned())
            .await
            .map_err(|_| FetchError::Timeout)?
            .map_err(|_| FetchError::Protocol)?;
        let start = {
            let mut next = slot
                .next_start
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let start = (*next).max(Instant::now());
            if start > deadline {
                return Err(FetchError::Timeout);
            }
            *next = start + self.spacing;
            start
        };
        sleep_until(start).await;
        Ok(Pass {
            _host: host_permit,
            _slot: slot,
            _global: global,
        })
    }

    fn slot(&self, host: &str) -> Arc<HostSlot> {
        let mut hosts = self.hosts.lock().unwrap_or_else(PoisonError::into_inner);
        if hosts.len() >= MAX_TRACKED_HOSTS {
            let now = Instant::now();
            hosts.retain(|_, slot| {
                let next = *slot
                    .next_start
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                Arc::strong_count(slot) > 1 || next > now
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
