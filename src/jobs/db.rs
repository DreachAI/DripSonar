//! Job SQL uses Store's writer transaction and pooled readers; no connections live here.

use super::*;
use rusqlite::{OptionalExtension, Transaction, params};
use std::collections::HashMap;

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
 error_code TEXT, finished_ms INTEGER, UNIQUE(job_id, task, key));
CREATE INDEX IF NOT EXISTS frontier_ready ON frontier(state, host, id);
CREATE INDEX IF NOT EXISTS frontier_job ON frontier(job_id, seq);
CREATE INDEX IF NOT EXISTS frontier_dup ON frontier(job_id, content_sha);
CREATE TABLE IF NOT EXISTS documents (
 frontier_id INTEGER PRIMARY KEY REFERENCES frontier(id) ON DELETE CASCADE, doc TEXT NOT NULL);
";

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
            let now = now_ms();
            tx.execute(
                "DELETE FROM jobs WHERE expires_ms <= ?1 AND status!='scraping'",
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
            Ok(())
        })
        .await
}

pub async fn hosts(store: &Store) -> Result<Vec<(String, u64)>, StoreError> {
    store
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT f.host, MAX(COALESCE(json_extract(j.spec,'$.delay_ms'),0))
             FROM frontier f JOIN jobs j ON j.id=f.job_id
             WHERE f.state='queued' AND j.status='scraping' GROUP BY f.host",
            )?;
            Ok(stmt
                .query_map([], |row| {
                    Ok((row.get(0)?, u64::from(row.get::<_, u32>(1)?)))
                })?
                .collect::<Result<Vec<_>, _>>()?)
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
    let hosts = store
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
            let mut hosts = HashMap::new();
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
                    hosts.insert(
                        seed.url.host_str().unwrap_or_default().to_owned(),
                        spec.delay_ms.unwrap_or(0),
                    );
                }
            }
            tx.execute(
                "UPDATE jobs SET discovered=?2 WHERE id=?1",
                params![id, pages],
            )?;
            finish(tx, &id, retention)?;
            Ok(Some(hosts.into_iter().collect()))
        })
        .await?;
    hosts.ok_or(JobError::Overloaded)
}

pub async fn lease(store: &Store, host: String) -> Result<Option<Work>, StoreError> {
    store
        .write(move |tx| {
            let row = tx
                .query_row(
                    "UPDATE frontier SET state='leased',attempts=attempts+1 WHERE id=(
             SELECT f.id FROM frontier f JOIN jobs j ON j.id=f.job_id
             WHERE f.state='queued' AND f.host=?1 AND j.status='scraping' ORDER BY f.id LIMIT 1)
             RETURNING id,job_id,task,url,depth",
                    [host],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, u32>(4)?,
                        ))
                    },
                )
                .optional()?;
            let Some((id, job_id, task, url, depth)) = row else {
                return Ok(None);
            };
            let spec: String =
                tx.query_row("SELECT spec FROM jobs WHERE id=?1", [&job_id], |row| {
                    row.get(0)
                })?;
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
                task,
                url: url.parse().map_err(|_| StoreError::Corrupt)?,
                spec: decode(&spec)?,
            }))
        })
        .await
}

fn finish(tx: &Transaction<'_>, id: &str, retention: u64) -> Result<(), StoreError> {
    let now = now_ms();
    tx.execute(
        "UPDATE jobs SET status=CASE WHEN completed=0 AND failed>0 THEN 'failed' ELSE 'completed' END,
         updated_ms=?2, expires_ms=?3, spec=json_set(spec,'$.scrape.headers',json('[]'))
         WHERE id=?1 AND status='scraping' AND NOT EXISTS(
         SELECT 1 FROM frontier WHERE job_id=?1 AND state IN ('queued','leased'))",
        params![id, now, expiry(now, retention)])?;
    Ok(())
}

pub async fn complete(
    store: &Store,
    work: Work,
    out: Outcome,
    retention: u64,
) -> Result<Vec<(String, u64)>, StoreError> {
    store.write(move |tx| {
        let now = now_ms();
        // Both checks are essential: a cancelled job must not accept an in-flight document.
        let changed = tx.execute(
            "UPDATE frontier SET state=?2,content_sha=?3,capture_id=?4,blob_sha256=?5,
             error_code=?6,finished_ms=?7 WHERE id=?1 AND state='leased' AND EXISTS(
             SELECT 1 FROM jobs WHERE id=frontier.job_id AND status='scraping')",
            params![work.id, if out.error.is_some() { "failed" } else { "done" },
                out.content_sha, out.capture_id, out.blob_sha256, out.error, now])?;
        if changed == 0 { return Ok(Vec::new()); }
        if work.task == Task::Page {
            if let Some(doc) = out.doc {
                tx.execute("UPDATE jobs SET completed=completed+1,updated_ms=?2 WHERE id=?1",
                    params![work.job_id, now])?;
                tx.execute("UPDATE frontier SET seq=(SELECT completed FROM jobs WHERE id=?2) WHERE id=?1",
                    params![work.id, work.job_id])?;
                tx.execute("INSERT INTO documents(frontier_id,doc) VALUES(?1,?2)", params![work.id, encode(&doc)?])?;
            } else {
                tx.execute("UPDATE jobs SET failed=failed+1,updated_ms=?2 WHERE id=?1", params![work.job_id, now])?;
            }
        }
        let duplicate: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM frontier WHERE job_id=?1 AND id!=?2 AND content_sha=?3)",
            params![work.job_id, work.id, out.content_sha], |row| row.get(0))?;
        let mut hosts = HashMap::new();
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
                    hosts.insert(seed.url.host_str().unwrap_or_default().to_owned(), work.spec.delay_ms.unwrap_or(0));
                }
            }
            tx.execute("UPDATE jobs SET discovered=?2 WHERE id=?1", params![work.job_id, discovered])?;
        }
        finish(tx, &work.job_id, retention)?;
        Ok(hosts.into_iter().collect())
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
            "SELECT status,completed,discovered,expires_ms,request FROM jobs WHERE id=?1 AND kind=?2",
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

pub async fn errors(store: &Store, id: JobId) -> Result<Option<Vec<PageError>>, StoreError> {
    store
        .read(move |conn| {
            let tx = conn.unchecked_transaction()?;
            let id = id.to_string();
            if !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM jobs WHERE id=?1 AND kind='crawl')",
                [&id],
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
                        id,
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
) -> Result<bool, StoreError> {
    store
        .write(move |tx| {
            let id = id.to_string();
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM jobs WHERE id=?1 AND kind=?2)",
                params![id, kind_name(kind)],
                |row| row.get(0),
            )?;
            if !exists {
                return Ok(false);
            }
            let now = now_ms();
            tx.execute(
                "UPDATE jobs SET status='cancelled',updated_ms=?2,expires_ms=?3,
             spec=json_set(spec,'$.scrape.headers',json('[]')) WHERE id=?1",
                params![id, now, expiry(now, retention)],
            )?;
            tx.execute(
                "UPDATE frontier SET state='cancelled',finished_ms=?2
             WHERE job_id=?1 AND state IN ('queued','leased')",
                params![id, now],
            )?;
            Ok(true)
        })
        .await
}

pub async fn sweep(store: &Store) -> Result<(), StoreError> {
    store
        .write(|tx| {
            tx.execute(
                "DELETE FROM jobs WHERE expires_ms<=?1 AND status!='scraping'",
                [now_ms()],
            )?;
            Ok(())
        })
        .await
}
