//! Durable, single-owner jobs. SQLite owns work; the dispatcher rotates jobs and hosts.

mod db;
mod sched;
mod webhooks;
pub use webhooks::Webhook;

use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use time::OffsetDateTime;
use tokio::{
    sync::{Notify, mpsc},
    task::{JoinHandle, JoinSet},
    time::Instant,
};
use url::Url;

use crate::{
    browser::BrowserError,
    config::Config,
    crawl::{
        self, Scope,
        sitemap::{self, SitemapEntries},
    },
    document::DocumentError,
    net::{FetchError, Fetcher, Target},
    scrape::{Document, ScrapeError, ScrapeTemplate, Scraper},
    store::{Store, StoreError, sha256_hex},
};

/// How often the store's bounded retention and orphan-blob sweep runs.
const STORE_SWEEP_EVERY: Duration = Duration::from_secs(3600);

pub type JobId = uuid::Uuid;
/// The HTTP boundary supplies wire serialization; jobs do not depend on API types.
pub type DocumentEncoder =
    fn(&Document, &serde_json::Value, &str) -> Result<serde_json::Value, StoreError>;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum JobKind {
    Crawl,
    Batch,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Task {
    Page,
    Sitemap,
    Robots,
}
impl Task {
    fn name(self) -> &'static str {
        match self {
            Self::Page => "page",
            Self::Sitemap => "sitemap",
            Self::Robots => "robots",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobSpec {
    pub kind: JobKind,
    pub scrape: ScrapeTemplate,
    pub scope: Option<Scope>,
    pub limit: u32,
    pub delay_ms: Option<u64>,
    #[serde(default)]
    pub seed_origin: Option<String>,
    #[serde(default)]
    pub webhook: Option<Webhook>,
    #[serde(default)]
    pub change_tracking: Option<crate::scrape::ChangeTrackingOptions>,
}

pub struct Seed {
    pub url: Url,
    pub task: Task,
}

pub struct StatusPage {
    pub status: String,
    pub completed: u64,
    pub total: u64,
    pub expires_at: OffsetDateTime,
    pub docs: Vec<(String, Document)>,
    pub next_skip: Option<u64>,
    pub request: serde_json::Value,
}

pub struct PageError {
    pub id: String,
    pub url: String,
    pub code: String,
    pub at: OffsetDateTime,
}

#[derive(Debug, thiserror::Error)]
pub enum JobError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("another sonar process owns the data directory")]
    Locked,
    #[error("maximum active jobs reached")]
    Overloaded,
    #[error("insufficient free disk space")]
    InsufficientStorage,
    #[error("job dispatcher is stopped")]
    Stopped,
    #[error("invalid job: {0}")]
    Invalid(String),
}

struct Inner {
    store: Store,
    scraper: Scraper,
    fetcher: Fetcher,
    cfg: Config,
    document_encoder: DocumentEncoder,
    stopped: AtomicBool,
    stop: Notify,
    _lock: File,
}

#[derive(Clone)]
pub struct Jobs {
    inner: Arc<Inner>,
    wake: mpsc::Sender<Vec<(String, u64)>>,
}

pub(super) struct Work {
    id: i64,
    job_id: String,
    task: Task,
    url: Url,
    depth: u32,
    /// Leases so far, including this one.
    attempts: u32,
    spec: JobSpec,
    formats: serde_json::Value,
}

pub(super) struct Outcome {
    doc: Option<Document>,
    content_sha: Option<String>,
    links: Vec<Seed>,
    capture_id: Option<i64>,
    blob_sha256: Option<String>,
    error: Option<String>,
    /// A transient failure: requeue after a backoff while attempts remain.
    retry: bool,
}

impl Jobs {
    pub async fn start(
        store: Store,
        scraper: Scraper,
        fetcher: Fetcher,
        cfg: &Config,
        document_encoder: DocumentEncoder,
    ) -> Result<(Self, JoinHandle<()>), JobError> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(cfg.data_dir.join("sonar.lock"))
            .map_err(StoreError::from)?;
        lock.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => JobError::Locked,
            std::fs::TryLockError::Error(error) => StoreError::from(error).into(),
        })?;
        db::initialize(&store, cfg.job_retention_hours).await?;
        let active = db::active(&store).await?;
        let (wake, receive) = mpsc::channel(128);
        let this = Self {
            inner: Arc::new(Inner {
                store,
                scraper,
                fetcher,
                cfg: cfg.clone(),
                document_encoder,
                stopped: AtomicBool::new(false),
                stop: Notify::new(),
                _lock: lock,
            }),
            wake,
        };
        let dispatcher = tokio::spawn(this.clone().dispatch(receive, active));
        tokio::spawn(sweep_store(Arc::downgrade(&this.inner)));
        tokio::spawn(webhooks::deliver(Arc::downgrade(&this.inner)));
        Ok((this, dispatcher))
    }

    pub async fn submit(
        &self,
        spec: JobSpec,
        request: serde_json::Value,
        seeds: Vec<Seed>,
    ) -> Result<JobId, JobError> {
        if self.inner.stopped.load(Ordering::Acquire) {
            return Err(JobError::Stopped);
        }
        if spec.limit == 0 || spec.limit > self.inner.cfg.max_crawl_limit || seeds.is_empty() {
            return Err(JobError::Invalid("invalid limit or empty seed list".into()));
        }
        if spec.delay_ms.is_some_and(|delay| delay > 60_000) {
            return Err(JobError::Invalid(
                "delay must be between 0 and 60 seconds".into(),
            ));
        }
        crate::scrape::request_headers(&spec.scrape.headers)
            .map_err(|error| JobError::Invalid(error.to_string()))?;
        if let Some(webhook) = &spec.webhook {
            webhook
                .validate(&self.inner.fetcher)
                .map_err(JobError::Invalid)?;
        }
        if !self
            .inner
            .store
            .free_disk_ok(self.inner.cfg.min_free_disk_mb)
            .await?
        {
            return Err(JobError::InsufficientStorage);
        }
        let id = JobId::new_v4();
        // Cancellation of an HTTP request must not strand a committed job without a wakeup.
        let this = self.clone();
        tokio::spawn(async move {
            let active =
                db::submit(&this.inner.store, id, spec, request, seeds, &this.inner.cfg).await?;
            this.wake
                .send(active)
                .await
                .map_err(|_| JobError::Stopped)?;
            Ok(id)
        })
        .await
        .map_err(|_| StoreError::Task)?
    }

    pub async fn status(
        &self,
        id: JobId,
        kind: JobKind,
        skip: u64,
        max_docs: usize,
        max_bytes: usize,
    ) -> Result<Option<StatusPage>, JobError> {
        Ok(db::status(&self.inner.store, id, kind, skip, max_docs, max_bytes).await?)
    }
    pub async fn active_crawls(&self) -> Result<Vec<serde_json::Value>, JobError> {
        Ok(db::active_crawls(&self.inner.store).await?)
    }
    pub async fn errors(
        &self,
        id: JobId,
        kind: JobKind,
    ) -> Result<Option<Vec<PageError>>, JobError> {
        Ok(db::errors(&self.inner.store, id, kind).await?)
    }
    pub async fn cancel(&self, id: JobId, kind: JobKind) -> Result<Option<bool>, JobError> {
        Ok(db::cancel(
            &self.inner.store,
            id,
            kind,
            self.inner.cfg.job_retention_hours,
        )
        .await?)
    }
    pub fn stop(&self) {
        self.inner.stopped.store(true, Ordering::Release);
        self.inner.stop.notify_one();
    }

    async fn dispatch(
        self,
        mut receive: mpsc::Receiver<Vec<(String, u64)>>,
        active: Vec<(String, u64)>,
    ) {
        let mut scheduler = sched::Scheduler::new(
            self.inner.cfg.per_host_concurrency,
            self.inner.cfg.per_host_spacing_ms,
        );
        for (job, delay) in active {
            scheduler.wake(job, delay);
        }
        let mut workers = JoinSet::new();
        let mut running = HashMap::new();
        let mut recover = Vec::new();
        let mut backoff = Duration::from_secs(1);
        let mut paused_until = Instant::now();
        let mut sweep = tokio::time::interval(Duration::from_secs(600));
        while !self.inner.stopped.load(Ordering::Acquire) {
            // Submissions must get a turn even when leases can be issued without waiting.
            while let Ok(active) = receive.try_recv() {
                for (job, delay) in active {
                    scheduler.wake(job, delay);
                }
            }
            if Instant::now() >= paused_until && !recover.is_empty() {
                match db::requeue(
                    &self.inner.store,
                    Some(recover.clone()),
                    self.inner.cfg.job_retention_hours,
                )
                .await
                {
                    Ok(()) => recover.clear(),
                    Err(error) => {
                        tracing::error!(%error, "job recovery failed; retrying");
                        paused_until = retry_at(&mut backoff);
                    }
                }
            }
            if Instant::now() >= paused_until
                && workers.len() < self.inner.cfg.job_workers
                && let Some(next) = scheduler.take()
            {
                match next {
                    sched::Next::Refill { job, after } => {
                        match db::hosts(&self.inner.store, job.clone(), after).await {
                            Ok((hosts, later)) => {
                                let later = later.map(|at| {
                                    let wait = u64::try_from(at.saturating_sub(now_ms()));
                                    Instant::now() + Duration::from_millis(wait.unwrap_or(0))
                                });
                                scheduler.refill(&job, hosts, later)
                            }
                            Err(error) => {
                                tracing::error!(%error, "job host refill failed; retrying");
                                paused_until = retry_at(&mut backoff);
                            }
                        }
                    }
                    sched::Next::Lease { job, host } => {
                        let disk = self
                            .inner
                            .store
                            .free_disk_ok(self.inner.cfg.min_free_disk_mb)
                            .await;
                        if !matches!(disk, Ok(true)) {
                            scheduler.release(&job, &host);
                            match disk {
                                Err(error) => {
                                    tracing::error!(%error, "free disk check failed; pausing leases")
                                }
                                _ => tracing::warn!("free disk below floor; pausing leases"),
                            }
                            paused_until = retry_at(&mut backoff);
                            continue;
                        }
                        if self.inner.stopped.load(Ordering::Acquire) {
                            break;
                        }
                        match db::lease(&self.inner.store, job.clone(), host.clone()).await {
                            Ok(Some(work)) => {
                                if self.inner.stopped.load(Ordering::Acquire) {
                                    break;
                                }
                                scheduler.started(&job, &host);
                                let (id, delay) = (work.id, work.spec.delay_ms.unwrap_or(0));
                                let this = self.clone();
                                let task = workers.spawn(async move { this.work(work).await });
                                running.insert(task.id(), (job, host, id, delay));
                            }
                            Ok(None) => scheduler.release(&job, &host),
                            Err(error) => {
                                scheduler.release(&job, &host);
                                tracing::error!(%error, "job lease failed; retrying");
                                paused_until = retry_at(&mut backoff);
                            }
                        }
                    }
                }
                continue;
            }
            let due = if workers.len() < self.inner.cfg.job_workers {
                scheduler.due().map(|at| at.max(paused_until))
            } else {
                None
            };
            let due = if recover.is_empty() {
                due
            } else {
                Some(paused_until)
            };
            tokio::select! {
                _ = self.inner.stop.notified() => {},
                Some(active) = receive.recv() => for (job, delay) in active { scheduler.wake(job, delay); },
                Some(result) = workers.join_next_with_id(), if !workers.is_empty() => {
                    let (id, refund_attempt) = match result {
                        Ok((id, Ok(()))) => {
                            backoff = Duration::from_secs(1);
                            (id, None)
                        }
                        Ok((id, Err(error))) => {
                            tracing::error!(%error, "job completion failed; requeuing");
                            (id, Some(true))
                        }
                        Err(error) => {
                            tracing::error!(%error, "job worker crashed; requeuing");
                            (error.id(), Some(false))
                        }
                    };
                    if let Some((job, host, row, delay)) = running.remove(&id) {
                        scheduler.release(&job, &host);
                        scheduler.wake(job, delay);
                        if let Some(refund_attempt) = refund_attempt {
                            recover.push((row, refund_attempt));
                            paused_until = retry_at(&mut backoff);
                        }
                    }
                },
                _ = async { match due { Some(at) => tokio::time::sleep_until(at).await, None => std::future::pending().await } } => {},
                _ = sweep.tick() => if let Err(error) = db::sweep(&self.inner.store).await {
                    tracing::error!(%error, "job retention sweep failed; pausing leases");
                    paused_until = retry_at(&mut backoff);
                },
            }
        }
        let drain = async {
            while let Some(result) = workers.join_next().await {
                if !matches!(result, Ok(Ok(()))) {
                    tracing::error!("job failed while draining");
                }
            }
        };
        if tokio::time::timeout(Duration::from_millis(self.inner.cfg.max_timeout_ms), drain)
            .await
            .is_err()
        {
            workers.abort_all();
            while workers.join_next().await.is_some() {}
        }
        // Graceful interruption must not consume one of the three crash attempts.
        while let Err(error) =
            db::requeue(&self.inner.store, None, self.inner.cfg.job_retention_hours).await
        {
            tracing::error!(%error, "shutdown lease recovery failed; retrying");
            tokio::time::sleep_until(retry_at(&mut backoff)).await;
        }
    }

    async fn work(&self, work: Work) -> Result<(), StoreError> {
        let mut out = Outcome {
            doc: None,
            content_sha: None,
            links: Vec::new(),
            capture_id: None,
            blob_sha256: None,
            error: None,
            retry: false,
        };
        let mut request = work.spec.scrape.request(&work.url);
        if work.spec.kind == JobKind::Crawl
            && work.spec.seed_origin.as_deref()
                != Some(work.url.origin().ascii_serialization().as_str())
        {
            request.headers.clear();
        }
        match work.task {
            Task::Page => {
                request.render.links = true;
                request.render.markdown = true;
                let scraped = async {
                    let mut doc = self.inner.scraper.scrape(&request).await?;
                    if let Some(options) = &work.spec.change_tracking {
                        match self
                            .inner
                            .scraper
                            .track_changes(&doc, &request, options)
                            .await
                        {
                            Ok(tracking) => doc.change_tracking = Some(tracking),
                            Err(error) => {
                                doc.change_tracking = None;
                                crate::extract::append_warning(
                                    &mut doc.warning,
                                    &format!("Comparing failed: {error}"),
                                );
                            }
                        }
                    }
                    Ok::<_, ScrapeError>(doc)
                }
                .await;
                match scraped {
                    Ok(doc) => {
                        if work.spec.scope.is_some() {
                            out.links = doc
                                .rendered
                                .links
                                .iter()
                                .flatten()
                                .take(1000)
                                .filter_map(|href| crawl::normalize_link(&doc.url, href))
                                .map(|url| Seed {
                                    url,
                                    task: Task::Page,
                                })
                                .collect();
                        }
                        out.content_sha = doc
                            .rendered
                            .markdown
                            .as_deref()
                            .filter(|markdown| !markdown.trim().is_empty())
                            .map(|markdown| sha256_hex(markdown.as_bytes()));
                        out.capture_id = doc.capture_id;
                        out.blob_sha256 = doc.blob_sha256.clone();
                        out.retry = transient_status(doc.status);
                        out.doc = Some(doc);
                    }
                    Err(error) => {
                        out.retry = match &error {
                            ScrapeError::Fetch(error)
                            | ScrapeError::Browser(BrowserError::Fetch(error)) => {
                                transient_fetch(error)
                            }
                            ScrapeError::Pdf(DocumentError::Timeout) => true,
                            _ => false,
                        };
                        out.error = Some(error.code().into())
                    }
                }
            }
            Task::Robots | Task::Sitemap => {
                let target = Target {
                    url: work.url.clone(),
                    headers: request.headers,
                    deadline: request.deadline,
                    max_bytes: self.inner.cfg.max_body_bytes,
                };
                match self.inner.fetcher.fetch(&target).await {
                    Ok(fetched) => {
                        match self
                            .inner
                            .store
                            .put(
                                &sha256_hex(format!("discovery\n{}", work.url).as_bytes()),
                                &fetched,
                            )
                            .await
                        {
                            Ok(id) => out.capture_id = Some(id),
                            Err(error) => {
                                tracing::error!(%error, "discovery capture storage failed");
                                out.error = Some("SCRAPE_STORAGE_ERROR".into());
                                return db::complete(
                                    &self.inner.store,
                                    work,
                                    out,
                                    self.inner.cfg.job_retention_hours,
                                    self.inner.document_encoder,
                                )
                                .await;
                            }
                        }
                        out.blob_sha256 = Some(sha256_hex(&fetched.body));
                        if !(200..300).contains(&fetched.status) {
                            out.retry = transient_status(fetched.status);
                            out.error = Some("SCRAPE_SITE_ERROR".into());
                        } else if work.task == Task::Robots {
                            out.links = sitemap::robots_sitemaps(&fetched.text())
                                .into_iter()
                                .take(100)
                                .map(|url| Seed {
                                    url,
                                    task: Task::Sitemap,
                                })
                                .collect();
                        } else {
                            match sitemap::parse(&fetched.body, 50 * 1024 * 1024, 50_000) {
                                Ok(SitemapEntries::Urls(urls)) => {
                                    out.links = urls
                                        .into_iter()
                                        .map(|url| Seed {
                                            url,
                                            task: Task::Page,
                                        })
                                        .collect()
                                }
                                Ok(SitemapEntries::Sitemaps(urls)) if work.depth < 3 => {
                                    out.links = urls
                                        .into_iter()
                                        .take(100)
                                        .map(|url| Seed {
                                            url,
                                            task: Task::Sitemap,
                                        })
                                        .collect()
                                }
                                Ok(_) => {}
                                Err(_) => out.error = Some("SCRAPE_SITEMAP_ERROR".into()),
                            }
                        }
                    }
                    Err(error) => {
                        out.retry = transient_fetch(&error);
                        out.error = Some(ScrapeError::from(error).code().into())
                    }
                }
            }
        }
        db::complete(
            &self.inner.store,
            work,
            out,
            self.inner.cfg.job_retention_hours,
            self.inner.document_encoder,
        )
        .await
    }
}

/// Timeouts, refused or reset connections, and a full fetch queue may pass on a later attempt.
fn transient_fetch(error: &FetchError) -> bool {
    matches!(
        error,
        FetchError::Timeout | FetchError::Connect | FetchError::Overloaded
    )
}

/// Rate limiting and temporary unavailability.
fn transient_status(status: u16) -> bool {
    matches!(status, 429 | 503)
}

/// Runs the store's retention sweep hourly until the jobs stop.
async fn sweep_store(inner: std::sync::Weak<Inner>) {
    let mut every = tokio::time::interval(STORE_SWEEP_EVERY);
    loop {
        every.tick().await;
        let Some(inner) = inner.upgrade() else { return };
        if inner.stopped.load(Ordering::Acquire) {
            return;
        }
        match inner.store.sweep().await {
            Ok(0) => {}
            Ok(removed) => tracing::info!(removed, "retention removed blob files"),
            Err(error) => tracing::warn!(%error, "store retention sweep failed"),
        }
    }
}

fn retry_at(backoff: &mut Duration) -> Instant {
    let at = Instant::now() + *backoff;
    *backoff = (*backoff * 2).min(Duration::from_secs(30));
    at
}

fn now_ms() -> i64 {
    (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}
fn timestamp(ms: i64) -> Result<OffsetDateTime, StoreError> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .map_err(|_| StoreError::Corrupt)
}
