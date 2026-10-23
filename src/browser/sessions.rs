//! Short multi-step sessions: one tab each, steps serialized, bounded count and lifetime.
//! Temporary state only; a Chrome restart ends them.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::Instant,
};

use super::{BrowserError, pool::Tab};

pub(super) const REAP_EVERY: Duration = Duration::from_secs(10);
const IDLE_LIMIT: Duration = Duration::from_secs(120);
const LIFETIME: Duration = Duration::from_secs(15 * 60);

pub(super) struct Session {
    pub tab: tokio::sync::Mutex<Tab>,
    created: Instant,
    last_used: Mutex<Instant>,
    _permit: OwnedSemaphorePermit,
}

impl Session {
    fn expired(&self, now: Instant) -> bool {
        let last_used = *self
            .last_used
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        now.duration_since(self.created) > LIFETIME || now.duration_since(last_used) > IDLE_LIMIT
    }
}

pub(super) struct Sessions {
    live: Mutex<HashMap<String, Arc<Session>>>,
    permits: Arc<Semaphore>,
}

impl Sessions {
    pub(super) fn new(max: usize) -> Self {
        Self {
            live: Mutex::default(),
            permits: Arc::new(Semaphore::new(max)),
        }
    }

    /// Claims a session place before any tab is opened.
    pub(super) fn reserve(&self) -> Result<OwnedSemaphorePermit, BrowserError> {
        Arc::clone(&self.permits)
            .try_acquire_owned()
            .map_err(|_| BrowserError::SessionLimit)
    }

    pub(super) fn insert(&self, permit: OwnedSemaphorePermit, tab: Tab) -> String {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let now = Instant::now();
        let session = Arc::new(Session {
            tab: tokio::sync::Mutex::new(tab),
            created: now,
            last_used: Mutex::new(now),
            _permit: permit,
        });
        self.lock().insert(id.clone(), session);
        id
    }

    /// A live session, marked used; expired ones are removed instead.
    pub(super) fn get(&self, id: &str) -> Result<Arc<Session>, BrowserError> {
        let now = Instant::now();
        let mut live = self.lock();
        let session = live.get(id).cloned().ok_or(BrowserError::SessionNotFound)?;
        if session.expired(now) {
            live.remove(id);
            return Err(BrowserError::SessionNotFound);
        }
        *session
            .last_used
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = now;
        Ok(session)
    }

    /// Removes a session; its tab closes once any running step finishes.
    pub(super) fn remove(&self, id: &str) -> bool {
        self.lock().remove(id).is_some()
    }

    /// Ends expired sessions and those whose Chrome is gone (checked only between steps).
    pub(super) fn reap(&self) {
        let now = Instant::now();
        self.lock().retain(|_, session| {
            let dead = session.tab.try_lock().is_ok_and(|tab| tab.is_dead());
            !dead && !session.expired(now)
        });
    }

    pub(super) fn clear(&self) {
        self.lock().clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Session>>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
