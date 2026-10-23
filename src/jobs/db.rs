//! Job SQL uses Store's writer transaction and pooled readers; no connections live here.

use super::*;
use rusqlite::{OptionalExtension, Transaction, params};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS jobs (
 id TEXT PRIMARY KEY, kind TEXT NOT NULL, status TEXT NOT NULL, spec TEXT NOT NULL,
 request TEXT NOT NULL, discovered INTEGER NOT NULL DEFAULT 0,
 completed INTEGER NOT NULL DEFAULT 0, failed INTEGER NOT NULL DEFAULT 0, error TEXT,
 created_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL, expires_ms INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS frontier (
 id INTEGER PRIMARY KEY, job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
 task TEXT NOT NULL, url TEXT NOT NULL, key TEXT NOT NULL, host TEXT NOT NULL,
 depth INTEGER NOT NULL, state TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
 seq INTEGER, content_sha TEXT, capture_id INTEGER, blob_sha256 TEXT,
 error_code TEXT, finished_ms INTEGER, not_before_ms INTEGER NOT NULL DEFAULT 0,
 UNIQUE(job_id, task, key));
DROP INDEX IF EXISTS frontier_ready;
DROP INDEX IF EXISTS frontier_job_hosts;
CREATE INDEX IF NOT EXISTS frontier_job ON frontier(job_id, seq);
CREATE INDEX IF NOT EXISTS frontier_dup ON frontier(job_id, content_sha);
CREATE INDEX IF NOT EXISTS frontier_open ON frontier(job_id) WHERE state IN ('queued','leased');
CREATE TABLE IF NOT EXISTS documents (
 frontier_id INTEGER PRIMARY KEY REFERENCES frontier(id) ON DELETE CASCADE, doc TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS webhooks (
 id INTEGER PRIMARY KEY, job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
 event TEXT NOT NULL, payload TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
 next_at INTEGER NOT NULL, delivered INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS webhooks_due ON webhooks(next_at,id) WHERE delivered=0 AND attempts<3;
CREATE INDEX IF NOT EXISTS webhooks_job ON webhooks(job_id);
";
/// Needs `not_before_ms`, which older stores gain by ALTER first. Covering for host refill and
/// lease: both walk hosts or IDs in order and skip rows still backing off.
const QUEUED_INDEX: &str =
    "CREATE INDEX IF NOT EXISTS frontier_queued ON frontier(job_id, host, id, not_before_ms)
     WHERE state='queued'";

/// Pauses before the second and third attempt after a transient failure; then it fails.
const RETRY_BACKOFF_MS: [i64; 2] = [5_000, 30_000];

fn encode<T: Serialize>(value: &T) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(|_| StoreError::Corrupt)
}
fn decode<T: for<'de> Deserialize<'de>>(value: &str) -> Result<T, StoreError> {
    serde_json::from_str(value).map_err(|_| StoreError::Corrupt)
}
fn kind_name(kind: JobKind) -> &'static str {
    match kind {
        JobKind::Crawl => "crawl",
        JobKind::Batch => "batch",
    }
}
fn expiry(now: i64, hours: u64) -> i64 {
    now.saturating_add(hours.saturating_mul(3_600_000).min(i64::MAX as u64) as i64)
}

pub async fn initialize(store: &Store, retention: u64) -> Result<(), StoreError> {
    store
        .write(move |tx| {
            tx.execute_batch(SCHEMA)?;
            let has_not_before: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('frontier') WHERE name='not_before_ms')",
                [],
                |row| row.get(0),
            )?;
            if !has_not_before {
                tx.execute_batch(
                    "ALTER TABLE frontier ADD COLUMN not_before_ms INTEGER NOT NULL DEFAULT 0",
                )?;
            }
            tx.execute_batch(QUEUED_INDEX)?;
            let now = now_ms();
            tx.execute(
                "DELETE FROM jobs WHERE expires_ms <= ?1 AND status!='scraping'
                 AND NOT EXISTS(SELECT 1 FROM webhooks WHERE job_id=jobs.id AND delivered=0 AND attempts<3)",
                [now],
            )?;
            tx.execute(
                "UPDATE frontier SET state='failed', error_code='SCRAPE_CRASH_LOOP', finished_ms=?1
             WHERE state='leased' AND attempts>=3",
                [now],
            )?;
            tx.execute(
                "UPDATE frontier SET state='queued' WHERE state='leased'",
                [],
            )?;
            tx.execute(
                "UPDATE jobs SET failed=(SELECT COUNT(*) FROM frontier
             WHERE job_id=jobs.id AND task='page' AND state='failed') WHERE status='scraping'",
                [],
            )?;
            let ids = tx
                .prepare("SELECT id FROM jobs WHERE status='scraping'")?
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            for id in ids {
                finish(tx, &id, retention)?;
            }
            let terminal = tx.prepare("SELECT id FROM jobs WHERE status!='scraping'")?
                .query_map([], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            for id in terminal { scrub_webhook(tx, &id)?; }
            Ok(())
        })
        .await
}

pub async fn active(store: &Store) -> Result<Vec<(String, u64)>, StoreError> {
    store
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, MIN(COALESCE(json_extract(spec,'$.delay_ms'),0),60000)
                 FROM jobs WHERE status='scraping' ORDER BY created_ms,id",
            )?;
            Ok(stmt
                .query_map([], |row| {
                    Ok((row.get(0)?, u64::from(row.get::<_, u32>(1)?)))
                })?
                .collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

pub async fn active_crawls(store: &Store) -> Result<Vec<serde_json::Value>, StoreError> {
    store.read(|conn| {
        let mut stmt = conn.prepare(
            "SELECT j.id,f.url,j.spec,j.request,j.created_ms FROM jobs j JOIN frontier f ON f.id=(
             SELECT MIN(id) FROM frontier WHERE job_id=j.id AND task='page')
             WHERE j.kind='crawl' AND j.status='scraping' ORDER BY j.created_ms,j.id")?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?,
            row.get::<_, String>(2)?, row.get::<_, String>(3)?, row.get::<_, i64>(4)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(|(id, url, spec, request, created)| {
            let spec: JobSpec = decode(&spec)?;
            let request: serde_json::Value = decode(&request)?;
            let scope = spec.scope.ok_or(StoreError::Corrupt)?;
            let options = scope.options();
            let created = timestamp(created)?.format(&time::format_description::well_known::Rfc3339)
                .map_err(|_| StoreError::Corrupt)?;
            // Deliberately enumerate public fields: neither page nor webhook credentials escape.
            Ok(serde_json::json!({"id": id, "url": url, "created_at": created, "options": {
                "limit": spec.limit, "maxDepth": options.max_depth,
                "maxDiscoveryDepth": options.max_discovery_hops,
                "includePaths": options.include_paths, "excludePaths": options.exclude_paths,
                "allowBackwardLinks": options.allow_backward_links,
                "crawlEntireDomain": options.crawl_entire_domain,
                "allowExternalLinks": options.allow_external_links, "allowSubdomains": options.allow_subdomains,
                "ignoreQueryParameters": options.ignore_query_parameters,
                "delay": spec.delay_ms.map(|ms| ms as f64 / 1000.0),
                "scrapeOptions": {"formats": request["formats"], "timeout": spec.scrape.timeout_ms,
                    "maxAge": spec.scrape.max_age_ms, "onlyMainContent": spec.scrape.render.only_main_content,
                    "includeTags": spec.scrape.render.include_tags, "excludeTags": spec.scrape.render.exclude_tags}
            }}))
        }).collect()
    }).await
}

/// The event and the state transition that caused it always commit together.
fn enqueue_webhook(
    tx: &Transaction<'_>,
    id: &str,
    event: &str,
    success: bool,
    data: serde_json::Value,
    error: Option<&str>,
    capture_id: Option<i64>,
) -> Result<(), StoreError> {
    let (webhook, kind): (Option<String>, String) = tx.query_row(
        "SELECT json_extract(spec,'$.webhook'),kind FROM jobs WHERE id=?1",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let Some(webhook) = webhook else {
        return Ok(());
    };
    let webhook: Webhook = decode(&webhook)?;
    if !webhook.events.iter().any(|allowed| allowed.name() == event) {
        return Ok(());
    }
    let namespace = if kind == "crawl" {
        "crawl"
    } else {
        "batch_scrape"
    };
    let event = format!("{namespace}.{event}");
    let mut payload = serde_json::json!({"success": success, "type": event, "id": id,
        "webhookId": id, "scrapeId": capture_id.map(|id| id.to_string()),
        "data": data, "metadata": webhook.metadata});
    if let Some(error) = error {
        payload["error"] = serde_json::json!(error);
    }
    tx.execute(
        "INSERT INTO webhooks(job_id,event,payload,next_at) VALUES(?1,?2,?3,?4)",
        params![id, event, encode(&payload)?, now_ms()],
    )?;
    Ok(())
}

fn scrub_webhook(tx: &Transaction<'_>, job: &str) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE jobs SET spec=json_set(spec,'$.webhook',json('null'))
        WHERE id=?1 AND status!='scraping' AND NOT EXISTS(
            SELECT 1 FROM webhooks WHERE job_id=?1 AND delivered=0 AND attempts<3)",
        [job],
    )?;
    Ok(())
}

pub async fn claim_webhook(store: &Store) -> Result<Option<webhooks::Delivery>, StoreError> {
    store.write(|tx| {
        let row = tx.query_row(
            "SELECT w.id,w.payload,json_extract(j.spec,'$.webhook'),w.attempts FROM webhooks w JOIN jobs j ON j.id=w.job_id
             WHERE w.delivered=0 AND w.attempts<3 AND w.next_at<=?1
             ORDER BY w.next_at,w.id LIMIT 1", [now_ms()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?,
                row.get::<_, String>(2)?, row.get::<_, u32>(3)?))).optional()?;
        let Some((id, body, spec, attempts)) = row else { return Ok(None) };
        let webhook: Webhook = decode(&spec)?;
        // Reserve the full send timeout as well as backoff. Restart cannot race a live claim.
        let backoff = if attempts == 0 { 5_000 } else { 60_000 };
        tx.execute("UPDATE webhooks SET attempts=attempts+1,next_at=?2 WHERE id=?1",
            params![id, now_ms().saturating_add(10_000 + backoff)])?;
        Ok(Some(webhooks::Delivery { id, body, webhook, attempt: attempts + 1 }))
    }).await
}

pub async fn webhook_result(
    store: &Store,
    id: i64,
    attempt: u32,
    success: bool,
) -> Result<(), StoreError> {
    store
        .write(move |tx| {
            let row: Option<(String, u32)> = tx
                .query_row(
                    "SELECT job_id,attempts FROM webhooks WHERE id=?1 AND attempts=?2",
                    params![id, attempt],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((job, attempts)) = row else {
                return Ok(());
            };
            let backoff = if attempts == 1 { 5_000 } else { 60_000 };
            tx.execute(
                "UPDATE webhooks SET delivered=?2,next_at=?3,
            payload=CASE WHEN ?2 OR attempts>=3 THEN '{}' ELSE payload END WHERE id=?1",
                params![id, success, now_ms().saturating_add(backoff)],
            )?;
            if !success && attempts >= 3 {
                tracing::warn!(webhook_id = id, "webhook attempts exhausted");
            }
            scrub_webhook(tx, &job)?;
            Ok(())
        })
        .await
}

/// The next page of hosts with a ready row; when none is ready, the earliest time one will be.
pub async fn hosts(
    store: &Store,
    job: String,
    after: String,
) -> Result<(Vec<String>, Option<i64>), StoreError> {
    store
        .read(move |conn| {
            let now = now_ms();
            let mut stmt = conn.prepare(
            "SELECT DISTINCT host FROM frontier INDEXED BY frontier_queued WHERE job_id=?1 AND state='queued'
             AND host>?2 AND not_before_ms<=?4 AND EXISTS(SELECT 1 FROM jobs WHERE id=?1 AND status='scraping')
             ORDER BY host LIMIT ?3",
            )?;
            let mut page = |after: &str| -> Result<Vec<String>, StoreError> {
                Ok(stmt
                    .query_map(params![job, after, sched::HOST_PAGE as i64, now], |r| r.get(0))?
                    .collect::<Result<Vec<_>, _>>()?)
            };
            let mut hosts = page(&after)?;
            if hosts.is_empty() && !after.is_empty() {
                hosts = page("")?;
            }
            if !hosts.is_empty() {
                return Ok((hosts, None));
            }
            // Only rows backing off remain queued, so this scan stays small.
            let later = conn.query_row(
                "SELECT MIN(not_before_ms) FROM frontier INDEXED BY frontier_queued
                 WHERE job_id=?1 AND state='queued'
                 AND EXISTS(SELECT 1 FROM jobs WHERE id=?1 AND status='scraping')",
                [&job],
                |r| r.get(0),
            )?;
            Ok((hosts, later))
        })
        .await
}

fn insert(
    tx: &Transaction<'_>,
    id: &str,
    seed: &Seed,
    depth: u32,
    ignore_query: bool,
) -> Result<bool, StoreError> {
    let Some(host) = seed.url.host_str() else {
        return Err(StoreError::Corrupt);
    };
    let key = crawl::dedup_key(&seed.url, ignore_query && seed.task == Task::Page);
    Ok(tx.execute(
        "INSERT OR IGNORE INTO frontier(job_id,task,url,key,host,depth,state)
         VALUES(?1,?2,?3,?4,?5,?6,'queued')",
        params![id, seed.task.name(), seed.url.as_str(), key, host, depth],
    )? != 0)
}

pub async fn submit(
    store: &Store,
    id: JobId,
    spec: JobSpec,
    request: serde_json::Value,
    seeds: Vec<Seed>,
    cfg: &Config,
) -> Result<Vec<(String, u64)>, JobError> {
    let (max_active, retention) = (cfg.max_active_jobs, cfg.job_retention_hours);
    let delay = spec.delay_ms.unwrap_or(0);
    let inserted = store
        .write(move |tx| {
            let active: i64 = tx.query_row(
                "SELECT COUNT(*) FROM jobs WHERE status='scraping'",
                [],
                |row| row.get(0),
            )?;
            if active as u64 >= max_active as u64 {
                return Ok(None);
            }
            let id = id.to_string();
            let now = now_ms();
            // Retain only output preferences, never the original body or its credentials.
            let request = serde_json::json!({"formats": request.get("formats").cloned()
            .unwrap_or_else(|| serde_json::json!(["markdown"]))});
            tx.execute(
                "INSERT INTO jobs(id,kind,status,spec,request,created_ms,updated_ms,expires_ms)
             VALUES(?1,?2,'scraping',?3,?4,?5,?5,?6)",
                params![
                    id,
                    kind_name(spec.kind),
                    encode(&spec)?,
                    encode(&request)?,
                    now,
                    expiry(now, retention)
                ],
            )?;
            let ignore_query = spec
                .scope
                .as_ref()
                .is_some_and(|s| s.options().ignore_query_parameters);
            let mut pages = 0u32;
            let mut sitemaps = 0;
            for seed in seeds {
                if seed.task == Task::Page && pages >= spec.limit {
                    break;
                }
                if seed.task == Task::Sitemap && sitemaps >= 100 {
                    continue;
                }
                if insert(tx, &id, &seed, 0, ignore_query)? {
                    pages += u32::from(seed.task == Task::Page);
                    sitemaps += usize::from(seed.task == Task::Sitemap);
                }
            }
            tx.execute(
                "UPDATE jobs SET discovered=?2 WHERE id=?1",
                params![id, pages],
            )?;
            enqueue_webhook(tx, &id, "started", true, serde_json::json!([]), None, None)?;
            finish(tx, &id, retention)?;
            Ok(Some(()))
        })
        .await?;
    inserted.ok_or(JobError::Overloaded)?;
    Ok(vec![(id.to_string(), delay)])
}

pub async fn lease(store: &Store, job: String, host: String) -> Result<Option<Work>, StoreError> {
    store
        .write(move |tx| {
            let row = tx
                .query_row(
                    "UPDATE frontier SET state='leased',attempts=attempts+1 WHERE id=(
             SELECT f.id FROM frontier f JOIN jobs j ON j.id=f.job_id
              WHERE f.state='queued' AND f.host=?1 AND f.job_id=?2 AND f.not_before_ms<=?3
              AND j.status='scraping' ORDER BY f.id LIMIT 1)
             RETURNING id,job_id,task,url,depth,attempts",
                    params![host, job, now_ms()],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, u32>(4)?,
                            row.get::<_, u32>(5)?,
                        ))
                    },
                )
                .optional()?;
            let Some((id, job_id, task, url, depth, attempts)) = row else {
                return Ok(None);
            };
            let (spec, request): (String, String) = tx.query_row(
                "SELECT spec,request FROM jobs WHERE id=?1",
                [&job_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let task = match task.as_str() {
                "page" => Task::Page,
                "sitemap" => Task::Sitemap,
                "robots" => Task::Robots,
                _ => return Err(StoreError::Corrupt),
            };
            Ok(Some(Work {
                id,
                job_id,
                depth,
                attempts,
                task,
                url: url.parse().map_err(|_| StoreError::Corrupt)?,
                spec: decode(&spec)?,
                formats: decode::<serde_json::Value>(&request)?["formats"].clone(),
            }))
        })
        .await
}

fn finish(tx: &Transaction<'_>, id: &str, retention: u64) -> Result<(), StoreError> {
    let now = now_ms();
    let changed = tx.execute(
        "UPDATE jobs SET status=CASE WHEN completed=0 AND failed>0 THEN 'failed' ELSE 'completed' END,
         updated_ms=?2, expires_ms=?3, spec=json_set(spec,'$.scrape.headers',json('[]'))
         WHERE id=?1 AND status='scraping' AND NOT EXISTS(
          SELECT 1 FROM frontier INDEXED BY frontier_open WHERE job_id=?1 AND state IN ('queued','leased'))",
        params![id, now, expiry(now, retention)])?;
    if changed != 0 {
        let status: String = tx.query_row("SELECT status FROM jobs WHERE id=?1", [id], |row| {
            row.get(0)
        })?;
        enqueue_webhook(
            tx,
            id,
            &status,
            status == "completed",
            serde_json::json!([]),
            None,
            None,
        )?;
        scrub_webhook(tx, id)?;
    }
    Ok(())
}

pub async fn complete(
    store: &Store,
    work: Work,
    out: Outcome,
    retention: u64,
    document_encoder: DocumentEncoder,
) -> Result<(), StoreError> {
    let page_data = if work.task == Task::Page
        && work.spec.webhook.as_ref().is_some_and(|webhook| {
            webhook
                .events
                .iter()
                .any(|event| matches!(event, webhooks::Event::Page))
        }) {
        out.doc
            .as_ref()
            .map(|doc| document_encoder(doc, &work.formats, work.url.as_str()))
            .transpose()?
    } else {
        None
    };
    let doc = out.doc.map(|doc| encode(&doc)).transpose()?;
    let backoff = (work.attempts as usize)
        .checked_sub(1)
        .and_then(|index| RETRY_BACKOFF_MS.get(index).copied())
        .filter(|_| out.retry);
    store.write(move |tx| {
        let now = now_ms();
        if let Some(backoff) = backoff {
            tx.execute(
                "UPDATE frontier SET state='queued',not_before_ms=?2 WHERE id=?1 AND state='leased'
                 AND EXISTS(SELECT 1 FROM jobs WHERE id=frontier.job_id AND status='scraping')",
                params![work.id, now.saturating_add(backoff)],
            )?;
            return Ok(());
        }
        // Both checks are essential: a cancelled job must not accept an in-flight document.
        let changed = tx.execute(
            "UPDATE frontier SET state=?2,content_sha=?3,capture_id=?4,blob_sha256=?5,
             error_code=?6,finished_ms=?7 WHERE id=?1 AND state='leased' AND EXISTS(
             SELECT 1 FROM jobs WHERE id=frontier.job_id AND status='scraping')",
            params![work.id, if out.error.is_some() { "failed" } else { "done" },
                out.content_sha, out.capture_id, out.blob_sha256, out.error, now])?;
        if changed == 0 { return Ok(()); }
        if work.task == Task::Page {
            enqueue_webhook(tx, &work.job_id, "page", out.error.is_none(),
                serde_json::Value::Array(page_data.into_iter().collect()), out.error.as_deref(), out.capture_id)?;
            if let Some(doc) = doc {
                tx.execute("UPDATE jobs SET completed=completed+1,updated_ms=?2 WHERE id=?1",
                    params![work.job_id, now])?;
                tx.execute("UPDATE frontier SET seq=(SELECT completed FROM jobs WHERE id=?2) WHERE id=?1",
                    params![work.id, work.job_id])?;
                tx.execute("INSERT INTO documents(frontier_id,doc) VALUES(?1,?2)", params![work.id, doc])?;
            } else {
                tx.execute("UPDATE jobs SET failed=failed+1,updated_ms=?2 WHERE id=?1", params![work.job_id, now])?;
            }
        }
        let duplicate: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM frontier WHERE job_id=?1 AND id!=?2 AND content_sha=?3)",
            params![work.job_id, work.id, out.content_sha], |row| row.get(0))?;
        if !duplicate && out.error.is_none() {
            let mut discovered: u32 = tx.query_row("SELECT discovered FROM jobs WHERE id=?1",
                [&work.job_id], |row| row.get(0))?;
            let mut sitemaps: u32 = tx.query_row(
                "SELECT COUNT(*) FROM frontier WHERE job_id=?1 AND task='sitemap'",
                [&work.job_id], |row| row.get(0))?;
            for seed in out.links {
                let depth = match (work.task, seed.task) { (Task::Page, Task::Page) | (Task::Sitemap, Task::Sitemap) => work.depth.saturating_add(1), _ => 0 };
                if seed.task == Task::Page && (discovered >= work.spec.limit
                    || work.spec.scope.as_ref().is_none_or(|scope| !scope.admits(&seed.url, depth))) { continue; }
                if seed.task == Task::Sitemap && (sitemaps >= 100 || depth > 3) { continue; }
                let ignore_query = work.spec.scope.as_ref().is_some_and(|s| s.options().ignore_query_parameters);
                if insert(tx, &work.job_id, &seed, depth, ignore_query)? {
                    discovered += u32::from(seed.task == Task::Page);
                    sitemaps += u32::from(seed.task == Task::Sitemap);
                }
            }
            tx.execute("UPDATE jobs SET discovered=?2 WHERE id=?1", params![work.job_id, discovered])?;
        }
        finish(tx, &work.job_id, retention)?;
        Ok(())
    }).await
}

pub async fn status(
    store: &Store,
    id: JobId,
    kind: JobKind,
    skip: u64,
    max_docs: usize,
    max_bytes: usize,
) -> Result<Option<StatusPage>, StoreError> {
    store.read(move |conn| {
        let tx = conn.unchecked_transaction()?;
        let row = tx.query_row(
            "SELECT status,completed,completed+(SELECT COUNT(*) FROM frontier INDEXED BY frontier_open
             WHERE job_id=jobs.id AND task='page' AND state IN ('queued','leased')),expires_ms,request
             FROM jobs WHERE id=?1 AND kind=?2",
            params![id.to_string(), kind_name(kind)], |r| {
                Ok((r.get::<_, String>(0)?, u64::from(r.get::<_, u32>(1)?),
                    u64::from(r.get::<_, u32>(2)?), r.get::<_, i64>(3)?, r.get::<_, String>(4)?))
            }).optional()?;
        let Some((status, completed, total, expires, request)) = row else { return Ok(None); };
        let mut stmt = tx.prepare(
            "SELECT f.url,d.doc FROM frontier f JOIN documents d ON d.frontier_id=f.id
             WHERE f.job_id=?1 AND f.seq>?2 ORDER BY f.seq LIMIT ?3")?;
        let mut rows = stmt.query(params![id.to_string(), skip.min(i64::MAX as u64) as i64,
            max_docs.clamp(1,1000) as i64])?;
        let mut bytes = 0;
        let mut docs = Vec::new();
        while let Some(row) = rows.next()? {
            let doc: String = row.get(1)?;
            if !docs.is_empty() && bytes + doc.len() > max_bytes { break; }
            bytes += doc.len();
            docs.push((row.get(0)?, decode(&doc)?));
        }
        let next = skip.saturating_add(docs.len() as u64);
        let next_skip = (status == "scraping" || next < completed).then_some(next);
        Ok(Some(StatusPage { status, completed, total, expires_at: timestamp(expires)?,
            docs, next_skip, request: decode(&request)? }))
    }).await
}

pub async fn errors(
    store: &Store,
    id: JobId,
    kind: JobKind,
) -> Result<Option<Vec<PageError>>, StoreError> {
    store
        .read(move |conn| {
            let tx = conn.unchecked_transaction()?;
            let id = id.to_string();
            if !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM jobs WHERE id=?1 AND kind=?2)",
                params![id, kind_name(kind)],
                |row| row.get::<_, bool>(0),
            )? {
                return Ok(None);
            }
            let mut stmt = tx.prepare(
                "SELECT id,url,error_code,finished_ms FROM frontier
             WHERE job_id=?1 AND state='failed' ORDER BY id",
            )?;
            let rows = stmt
                .query_map([id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter()
                .map(|(id, url, code, at)| {
                    Ok(PageError {
                        id: id.to_string(),
                        url,
                        code,
                        at: timestamp(at)?,
                    })
                })
                .collect::<Result<Vec<_>, StoreError>>()
                .map(Some)
        })
        .await
}

pub async fn cancel(
    store: &Store,
    id: JobId,
    kind: JobKind,
    retention: u64,
) -> Result<Option<bool>, StoreError> {
    store
        .write(move |tx| {
            let id = id.to_string();
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM jobs WHERE id=?1 AND kind=?2)",
                params![id, kind_name(kind)],
                |row| row.get(0),
            )?;
            if !exists {
                return Ok(None);
            }
            let now = now_ms();
            let changed = tx.execute(
                "UPDATE jobs SET status='cancelled',updated_ms=?2,expires_ms=?3,
             spec=json_set(spec,'$.scrape.headers',json('[]')) WHERE id=?1 AND status='scraping'",
                params![id, now, expiry(now, retention)],
            )?;
            if changed == 0 {
                return Ok(Some(false));
            }
            tx.execute(
                "UPDATE frontier SET state='cancelled',finished_ms=?2
             WHERE job_id=?1 AND state IN ('queued','leased')",
                params![id, now],
            )?;
            enqueue_webhook(
                tx,
                &id,
                "failed",
                false,
                serde_json::json!([]),
                Some("Job cancelled"),
                None,
            )?;
            scrub_webhook(tx, &id)?;
            Ok(Some(true))
        })
        .await
}

/// Refund storage failures and orderly interruption, but retain the three-crash bound.
pub async fn requeue(
    store: &Store,
    ids: Option<Vec<(i64, bool)>>,
    retention: u64,
) -> Result<(), StoreError> {
    store
        .write(move |tx| {
            if let Some(ids) = ids {
            for (id, refund_attempt) in ids {
                let row = tx.query_row(
                    "UPDATE frontier SET state=CASE WHEN ?2 OR attempts<3 THEN 'queued' ELSE 'failed' END,
                     attempts=MAX(attempts-?2,0),
                     error_code=CASE WHEN NOT ?2 AND attempts>=3 THEN 'SCRAPE_CRASH_LOOP' END,
                     finished_ms=CASE WHEN NOT ?2 AND attempts>=3 THEN ?3 END
                     WHERE id=?1 AND state='leased' RETURNING job_id,task,state",
                    params![id, refund_attempt, now_ms()], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
                    }).optional()?;
                if let Some((job, task, state)) = row && state == "failed" {
                    if task == "page" {
                        tx.execute("UPDATE jobs SET failed=failed+1 WHERE id=?1", [&job])?;
                    }
                    finish(tx, &job, retention)?;
                }
                }
            } else {
                tx.execute(
                    "UPDATE frontier SET state='queued',attempts=MAX(attempts-1,0)
                        WHERE state='leased'",
                    [],
                )?;
            }
            Ok(())
        })
        .await
}

pub async fn sweep(store: &Store) -> Result<(), StoreError> {
    store
        .write(|tx| {
            tx.execute(
                "DELETE FROM jobs WHERE expires_ms<=?1 AND status!='scraping'
                 AND NOT EXISTS(SELECT 1 FROM webhooks WHERE job_id=jobs.id AND delivered=0 AND attempts<3)",
                [now_ms()],
            )?;
            Ok(())
        })
        .await
}
