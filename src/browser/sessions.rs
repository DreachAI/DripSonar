//! Short multi-step sessions: one tab each, steps serialized, bounded count and lifetime.
//! Temporary state only; a Chrome restart ends them.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tokio::{
    sync::{Notify, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore},
    time::{Instant, timeout_at},
};

use super::{BrowserError, pool::Tab};
use crate::net::FetchError;

pub(super) const REAP_EVERY: Duration = Duration::from_secs(10);
const IDLE_LIMIT: Duration = Duration::from_secs(120);
const LIFETIME: Duration = Duration::from_secs(15 * 60);

/// Held by a running step, so the session's place stays taken until the step ends.
pub(super) struct Session {
    tab: Arc<tokio::sync::Mutex<Tab>>,
    created: Instant,
    last_used: Mutex<Instant>,
    /// Set before the session leaves the map; queued steps are woken and refused.
    closed: AtomicBool,
    closing: Notify,
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

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.closing.notify_waiters();
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
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
            tab: Arc::new(tokio::sync::Mutex::new(tab)),
            created: now,
            last_used: Mutex::new(now),
            closed: AtomicBool::new(false),
            closing: Notify::new(),
            _permit: permit,
        });
        self.lock().insert(id.clone(), session);
        id
    }

    /// The session's tab once earlier steps finish. A session that closes while this waits,
    /// or is found expired or dead once its turn comes, is removed and reported not found.
    pub(super) async fn acquire(
        &self,
        id: &str,
        deadline: Instant,
    ) -> Result<(OwnedMutexGuard<Tab>, Arc<Session>), BrowserError> {
        let session = self.get(id)?;
        let tab = {
            let closing = session.closing.notified();
            tokio::pin!(closing);
            closing.as_mut().enable();
            if session.is_closed() {
                return Err(BrowserError::SessionNotFound);
            }
            tokio::select! {
                tab = timeout_at(deadline, Arc::clone(&session.tab).lock_owned()) => {
                    tab.map_err(|_| FetchError::Timeout)?
                }
                () = closing => return Err(BrowserError::SessionNotFound),
            }
        };
        if session.is_closed() || session.expired(Instant::now()) || tab.is_dead() {
            self.remove(id);
            return Err(BrowserError::SessionNotFound);
        }
        Ok((tab, session))
    }

    /// A live session, marked used; expired ones are removed instead.
    fn get(&self, id: &str) -> Result<Arc<Session>, BrowserError> {
        let now = Instant::now();
        let mut live = self.lock();
        let session = live.get(id).cloned().ok_or(BrowserError::SessionNotFound)?;
        if session.expired(now) {
            live.remove(id);
            session.close();
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
        let removed = self.lock().remove(id);
        removed.map(|session| session.close()).is_some()
    }

    /// Ends expired sessions and those whose Chrome is gone (checked only between steps).
    pub(super) fn reap(&self) {
        let now = Instant::now();
        self.lock().retain(|_, session| {
            let dead = session.tab.try_lock().is_ok_and(|tab| tab.is_dead());
            let keep = !dead && !session.expired(now);
            if !keep {
                session.close();
            }
            keep
        });
    }

    pub(super) fn clear(&self) {
        for (_, session) in self.lock().drain() {
            session.close();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Session>>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
