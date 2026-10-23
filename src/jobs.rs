//! Durable, single-owner jobs. SQLite owns work; the dispatcher only schedules hosts.

mod db;
mod sched;

use serde::{Deserialize, Serialize};
use std::{
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
};
use url::Url;

use crate::{
    config::Config,
    crawl::{
        self, Scope,
        sitemap::{self, SitemapEntries},
    },
    net::{Fetcher, Target},
    scrape::{Document, ScrapeTemplate, Scraper},
    store::{Store, StoreError, sha256_hex},
};

pub type JobId = uuid::Uuid;

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
    pub seed_host: Option<String>,
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
    pub id: i64,
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
    spec: JobSpec,
}

pub(super) struct Outcome {
    doc: Option<Document>,
    content_sha: Option<String>,
    links: Vec<Seed>,
    capture_id: Option<i64>,
    blob_sha256: Option<String>,
    error: Option<String>,
}

impl Jobs {
    pub async fn start(
        store: Store,
        scraper: Scraper,
        fetcher: Fetcher,
        cfg: &Config,
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
        let hosts = db::hosts(&store).await?;
        let (wake, receive) = mpsc::channel(128);
        let this = Self {
            inner: Arc::new(Inner {
                store,
                scraper,
                fetcher,
                cfg: cfg.clone(),
                stopped: AtomicBool::new(false),
                stop: Notify::new(),
                _lock: lock,
            }),
            wake,
        };
        let dispatcher = tokio::spawn(this.clone().dispatch(receive, hosts));
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
        crate::scrape::request_headers(&spec.scrape.headers)
            .map_err(|error| JobError::Invalid(error.to_string()))?;
        let dir = self.inner.cfg.data_dir.clone();
        let free = tokio::task::spawn_blocking(move || fs2::available_space(dir))
            .await
            .map_err(|_| StoreError::Task)?
            .map_err(StoreError::from)?;
        if free < self.inner.cfg.min_free_disk_mb.saturating_mul(1024 * 1024) {
            return Err(JobError::InsufficientStorage);
        }
        let id = JobId::new_v4();
        // Cancellation of an HTTP request must not strand a committed job without a wakeup.
        let this = self.clone();
        tokio::spawn(async move {
            let hosts =
                db::submit(&this.inner.store, id, spec, request, seeds, &this.inner.cfg).await?;
            this.wake.send(hosts).await.map_err(|_| JobError::Stopped)?;
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
    pub async fn errors(&self, id: JobId) -> Result<Option<Vec<PageError>>, JobError> {
        Ok(db::errors(&self.inner.store, id).await?)
    }
    pub async fn cancel(&self, id: JobId, kind: JobKind) -> Result<bool, JobError> {
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
        hosts: Vec<(String, u64)>,
    ) {
        let mut scheduler = sched::Scheduler::new(
            self.inner.cfg.per_host_concurrency,
            self.inner.cfg.per_host_spacing_ms,
        );
        for (host, delay) in hosts {
            scheduler.wake(host, delay);
        }
        let mut workers = JoinSet::new();
        let mut sweep = tokio::time::interval(Duration::from_secs(600));
        while !self.inner.stopped.load(Ordering::Acquire) {
            if workers.len() < self.inner.cfg.job_workers
                && let Some(host) = scheduler.take()
            {
                match db::lease(&self.inner.store, host.clone()).await {
                    Ok(Some(work)) => {
                        let this = self.clone();
                        workers.spawn(async move { (host, this.work(work).await) });
                    }
                    Ok(None) => scheduler.release(&host, true),
                    Err(error) => {
                        tracing::error!(%error, "job lease failed; stopping dispatcher");
                        self.stop();
                    }
                }
                continue;
            }
            let due = if workers.len() < self.inner.cfg.job_workers {
                scheduler.due()
            } else {
                None
            };
            tokio::select! {
                _ = self.inner.stop.notified() => {},
                Some(hosts) = receive.recv() => for (host, delay) in hosts { scheduler.wake(host, delay); },
                Some(result) = workers.join_next(), if !workers.is_empty() => match result {
                    Ok((host, Ok(hosts))) => { scheduler.release(&host, false); for (host, delay) in hosts { scheduler.wake(host, delay); } },
                    Ok((host, Err(error))) => { scheduler.release(&host, false); tracing::error!(%error, "job completion failed; stopping dispatcher"); self.stop(); },
                    Err(error) => { tracing::error!(%error, "job worker crashed; stopping dispatcher"); self.stop(); },
                },
                _ = async { match due { Some(at) => tokio::time::sleep_until(at).await, None => std::future::pending().await } } => {},
                _ = sweep.tick() => if let Err(error) = db::sweep(&self.inner.store).await { tracing::error!(%error, "job retention sweep failed"); },
            }
        }
        // Any unfinished lease is recovered at startup, with a bounded crash-attempt count.
        let drain = async {
            while let Some(result) = workers.join_next().await {
                if !matches!(result, Ok((_, Ok(_)))) {
                    tracing::error!("job failed while draining");
                }
            }
        };
        if tokio::time::timeout(Duration::from_millis(self.inner.cfg.max_timeout_ms), drain)
            .await
            .is_err()
        {
            workers.abort_all();
        }
    }

    async fn work(&self, work: Work) -> Result<Vec<(String, u64)>, StoreError> {
        let mut out = Outcome {
            doc: None,
            content_sha: None,
            links: Vec::new(),
            capture_id: None,
            blob_sha256: None,
            error: None,
        };
        let mut request = work.spec.scrape.request(&work.url);
        if work.spec.kind == JobKind::Crawl && work.url.host_str() != work.spec.seed_host.as_deref()
        {
            request.headers.clear();
        }
        match work.task {
            Task::Page => {
                request.render.links = true;
                request.render.markdown = true;
                match self.inner.scraper.scrape(&request).await {
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
                        out.content_sha = Some(sha256_hex(
                            doc.rendered
                                .markdown
                                .as_deref()
                                .unwrap_or_default()
                                .as_bytes(),
                        ));
                        out.capture_id = doc.capture_id;
                        out.blob_sha256 = doc.blob_sha256.clone();
                        out.doc = Some(doc);
                    }
                    Err(error) => out.error = Some(error.code().into()),
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
                        out.capture_id = Some(
                            self.inner
                                .store
                                .put(
                                    &sha256_hex(format!("discovery\n{}", work.url).as_bytes()),
                                    &fetched,
                                )
                                .await?,
                        );
                        out.blob_sha256 = Some(sha256_hex(&fetched.body));
                        if !(200..300).contains(&fetched.status) {
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
                        out.error = Some(crate::scrape::ScrapeError::from(error).code().into())
                    }
                }
            }
        }
        db::complete(
            &self.inner.store,
            work,
            out,
            self.inner.cfg.job_retention_hours,
        )
        .await
    }
}

fn now_ms() -> i64 {
    (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}
fn timestamp(ms: i64) -> Result<OffsetDateTime, StoreError> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .map_err(|_| StoreError::Corrupt)
}
