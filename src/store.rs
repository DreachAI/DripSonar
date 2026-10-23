//! SQLite capture index and content-addressed blob files.
//!
//! Captures are appended, never updated. A blob is durable before its row exists, so a crash
//! leaves at most an orphan blob, never a row without content. One writer thread owns every
//! write; blob unlinks run there too, so a row insert always sees its blob or rewrites it.

use std::{
    fs::{self, File},
    io::{self, Write},
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, SystemTime},
};

use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot};

use crate::net::{Fetched, Route};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS captures (
    id INTEGER PRIMARY KEY,
    cache_key TEXT NOT NULL,
    requested_url TEXT NOT NULL,
    final_url TEXT NOT NULL,
    status INTEGER NOT NULL,
    headers TEXT NOT NULL,
    content_type TEXT,
    blob_sha256 TEXT NOT NULL,
    byte_len INTEGER NOT NULL,
    fetched_at_ms INTEGER NOT NULL,
    route TEXT
);
CREATE INDEX IF NOT EXISTS captures_by_key ON captures (cache_key, fetched_at_ms);
CREATE INDEX IF NOT EXISTS captures_by_time ON captures (fetched_at_ms);
CREATE INDEX IF NOT EXISTS captures_by_blob ON captures (blob_sha256);
CREATE INDEX IF NOT EXISTS captures_by_requested ON captures (requested_url, fetched_at_ms, id)
    WHERE status BETWEEN 200 AND 299;
CREATE INDEX IF NOT EXISTS captures_by_final ON captures (final_url, fetched_at_ms, id)
    WHERE status BETWEEN 200 AND 299;
CREATE TABLE IF NOT EXISTS searches (
    id INTEGER PRIMARY KEY,
    query TEXT NOT NULL,
    provider TEXT NOT NULL,
    lang TEXT NOT NULL,
    fetched_at_ms INTEGER NOT NULL,
    hits TEXT,
    capture_id INTEGER NOT NULL UNIQUE REFERENCES captures(id) ON DELETE CASCADE
);
";

/// Expiring capability-addressed files (browser screenshots); blobs live under `blobs/artifacts`.
const ARTIFACTS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS artifacts (
    id TEXT PRIMARY KEY,
    blob_sha256 TEXT NOT NULL,
    media_type TEXT NOT NULL,
    byte_len INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS artifacts_by_expiry ON artifacts (expires_at_ms);
CREATE INDEX IF NOT EXISTS artifacts_by_blob ON artifacts (blob_sha256);
";

const CAPTURE_COLUMNS: &str = "id, requested_url, final_url, status, headers, content_type, \
                               blob_sha256, fetched_at_ms, route, tier";
const CAPTURE_REFERENCED: &str = "SELECT EXISTS(SELECT 1 FROM captures WHERE blob_sha256 = ?1)";
const ARTIFACT_REFERENCED: &str = "SELECT EXISTS(SELECT 1 FROM artifacts WHERE blob_sha256 = ?1)";

/// Captures older than this are deleted by [`Store::sweep`], with their search rows.
pub const CAPTURE_RETENTION: Duration = Duration::from_secs(30 * 24 * 3600);
/// Capture rows deleted per writer transaction, and transactions per sweep.
const SWEEP_BATCH: usize = 1000;
const SWEEP_BATCHES: usize = 50;
/// Blob files examined per root per sweep, resuming where the previous sweep stopped.
const ORPHAN_SCAN: usize = 10_000;
/// Younger unreferenced blobs may belong to a write whose row is still queued.
const ORPHAN_GRACE: Duration = Duration::from_secs(3600);

/// Which engine produced a capture; rows written before the column existed are HTTP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Http,
    Browser,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Browser => "browser",
        }
    }
}

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Idle read-only connections kept for reuse; bursts beyond this open short-lived extras.
const IDLE_READERS: usize = 8;
/// Writes queued for the writer thread before callers wait.
const WRITE_QUEUE: usize = 256;

/// Response headers never persisted: they carry session credentials.
const UNSTORED_HEADERS: [&str; 1] = ["set-cookie"];

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("blob file error: {0}")]
    Io(#[from] io::Error),
    #[error("stored capture is invalid")]
    Corrupt,
    #[error("store task failed")]
    Task,
    #[error("artifact storage quota exhausted")]
    Quota,
}

type WriteJob = Box<dyn FnOnce(&mut Connection) + Send>;

/// The writer thread's queue plus reusable read-only connections, so cache lookups never queue
/// behind an insert (WAL lets readers run beside the writer).
struct Db {
    path: PathBuf,
    writer: mpsc::Sender<WriteJob>,
    readers: Mutex<Vec<Connection>>,
}

impl Db {
    fn reader(&self) -> Result<Connection, StoreError> {
        let idle = self
            .readers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop();
        match idle {
            Some(conn) => Ok(conn),
            None => {
                let conn = Connection::open_with_flags(
                    &self.path,
                    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )?;
                conn.busy_timeout(BUSY_TIMEOUT)?;
                Ok(conn)
            }
        }
    }

    fn recycle(&self, conn: Connection) {
        let mut idle = self.readers.lock().unwrap_or_else(PoisonError::into_inner);
        if idle.len() < IDLE_READERS {
            idle.push(conn);
        }
    }
}

#[derive(Clone)]
pub struct Store {
    db: Arc<Db>,
    blobs: PathBuf,
    artifacts: PathBuf,
    /// Serializes artifact publishing, so the quota check holds until the insert.
    artifact_lock: Arc<tokio::sync::Mutex<()>>,
    /// Last blob name examined per root (captures, artifacts) by the orphan scan.
    orphan_cursors: Arc<Mutex<[String; 2]>>,
}

pub struct Capture {
    pub id: i64,
    pub blob_sha256: String,
    pub tier: Tier,
    pub fetched: Fetched,
}

/// One recorded search-provider exchange whose response parsed.
pub struct StoredSearch {
    pub id: i64,
    pub query: String,
    pub provider: String,
    pub fetched_at: OffsetDateTime,
    /// JSON array of the parsed hits.
    pub hits: String,
    pub capture_id: i64,
}

impl Store {
    pub fn open(dir: &Path) -> Result<Self, StoreError> {
        let blobs = dir.join("blobs").join("sha256");
        fs::create_dir_all(&blobs)?;
        let artifacts = dir.join("blobs").join("artifacts");
        fs::create_dir_all(&artifacts)?;
        let path = dir.join("sonar.db");
        let mut writer = Connection::open(&path)?;
        writer.busy_timeout(BUSY_TIMEOUT)?;
        writer
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))?;
        // NORMAL is crash-safe in WAL: a power loss can drop the newest rows, never corrupt.
        writer.pragma_update(None, "synchronous", "NORMAL")?;
        writer.pragma_update(None, "foreign_keys", true)?;
        writer.pragma_update(None, "secure_delete", true)?;
        writer.execute_batch(SCHEMA)?;
        writer.execute_batch(ARTIFACTS_SCHEMA)?;
        // Serialize schema inspection and ALTER across simultaneous opens of an old store.
        let tx = writer.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for column in ["route", "tier"] {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('captures') WHERE name = ?1)",
                [column],
                |row| row.get(0),
            )?;
            if !exists {
                tx.execute_batch(&format!("ALTER TABLE captures ADD COLUMN {column} TEXT"))?;
            }
        }
        tx.commit()?;
        let (sender, mut queue) = mpsc::channel::<WriteJob>(WRITE_QUEUE);
        // Exits once every Store clone, and so every sender, is gone.
        std::thread::Builder::new()
            .name("sonar-store-writer".into())
            .spawn(move || {
                while let Some(job) = queue.blocking_recv() {
                    // A panicking write fails only its caller (the reply is dropped).
                    let _ = catch_unwind(AssertUnwindSafe(|| job(&mut writer)));
                }
            })?;
        Ok(Self {
            db: Arc::new(Db {
                path,
                writer: sender,
                readers: Mutex::new(Vec::new()),
            }),
            blobs,
            artifacts,
            artifact_lock: Arc::default(),
            orphan_cursors: Arc::default(),
        })
    }

    /// Runs job SQL on the writer thread; a failed closure rolls back the whole transaction.
    /// Once queued, the write completes even if the caller is cancelled.
    pub async fn write<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, StoreError> {
        self.on_writer(move |conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let result = f(&tx)?;
            tx.commit()?;
            Ok(result)
        })
        .await
    }

    async fn on_writer<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, StoreError> {
        let (reply, result) = oneshot::channel();
        let job: WriteJob = Box::new(move |conn| {
            let _ = reply.send(f(conn));
        });
        self.db
            .writer
            .send(job)
            .await
            .map_err(|_| StoreError::Task)?;
        result.await.map_err(|_| StoreError::Task)?
    }

    pub async fn read<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, StoreError> {
        let db = Arc::clone(&self.db);
        blocking(move || {
            let conn = db.reader()?;
            let result = f(&conn);
            // Only a connection that just answered is worth reusing.
            if result.is_ok() {
                db.recycle(conn);
            }
            result
        })
        .await
    }

    /// The newest 2xx capture for `key` no older than `max_age`, or `None` when absent or its
    /// blob is gone.
    pub async fn cached_capture(
        &self,
        key: &str,
        max_age: Duration,
    ) -> Result<Option<Capture>, StoreError> {
        let max_age_ms = i64::try_from(max_age.as_millis()).unwrap_or(i64::MAX);
        let since = unix_ms(OffsetDateTime::now_utc())?.saturating_sub(max_age_ms);
        let (blobs, key) = (self.blobs.clone(), key.to_owned());
        self.read(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {CAPTURE_COLUMNS} FROM captures
                 WHERE cache_key = ?1 AND fetched_at_ms >= ?2 AND status BETWEEN 200 AND 299
                 ORDER BY fetched_at_ms DESC, id DESC LIMIT 1"
            ))?;
            let mut rows = stmt.query(params![key, since])?;
            match rows.next()? {
                Some(row) => read_capture(&blobs, row),
                None => Ok(None),
            }
        })
        .await
    }

    /// Newest successful capture under this capture's own HTTP or browser key, strictly before
    /// it. Action keys are unique per visit, so action captures never become a baseline.
    pub async fn previous_capture(
        &self,
        current: i64,
        before: Option<i64>,
    ) -> Result<Option<Capture>, StoreError> {
        let blobs = self.blobs.clone();
        self.read(move |conn| {
            let present: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM captures WHERE id=?1)
                 AND EXISTS(SELECT 1 FROM captures WHERE id=COALESCE(?2,?1))",
                params![current, before], |row| row.get(0))?;
            if !present { return Err(StoreError::Corrupt); }
            let mut stmt = conn.prepare(&format!(
                "SELECT {CAPTURE_COLUMNS} FROM captures WHERE status BETWEEN 200 AND 299
                 AND (fetched_at_ms,id) < (SELECT fetched_at_ms,id FROM captures WHERE id=COALESCE(?2,?1))
                 AND cache_key = (SELECT cache_key FROM captures WHERE id=?1)
                 AND NOT EXISTS(SELECT 1 FROM searches WHERE capture_id=captures.id)
                 ORDER BY fetched_at_ms DESC,id DESC LIMIT 64"
            ))?;
            let mut rows = stmt.query(params![current, before])?;
            let mut missing = 0;
            while let Some(row) = rows.next()? {
                if let Some(capture) = read_capture(&blobs, row)? { return Ok(Some(capture)); }
                missing += 1;
            }
            // Do not call unavailable history a new page. Bound work on damaged stores.
            if missing > 0 { Err(StoreError::Corrupt) } else { Ok(None) }
        }).await
    }

    /// Page captures (not search exchanges) after `after_id` in ID order, and the cursor for the
    /// next call. Rows whose blob vanished meanwhile are skipped.
    pub async fn captures_after(
        &self,
        after_id: i64,
        n: usize,
    ) -> Result<(Vec<Capture>, i64), StoreError> {
        let blobs = self.blobs.clone();
        self.read(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {CAPTURE_COLUMNS} FROM captures WHERE id > ?1
                 AND NOT EXISTS(SELECT 1 FROM searches WHERE capture_id = captures.id)
                 ORDER BY id LIMIT ?2"
            ))?;
            let mut rows = stmt.query(params![after_id, n.clamp(1, 1000) as i64])?;
            let (mut captures, mut next) = (Vec::new(), after_id);
            while let Some(row) = rows.next()? {
                next = row.get(0)?;
                captures.extend(read_capture(&blobs, row)?);
            }
            Ok((captures, next))
        })
        .await
    }

    /// Recorded search exchanges after `after_id` in ID order.
    pub async fn searches_after(
        &self,
        after_id: i64,
        n: usize,
    ) -> Result<Vec<StoredSearch>, StoreError> {
        self.read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, query, provider, fetched_at_ms, hits, capture_id FROM searches
                 WHERE id > ?1 AND hits IS NOT NULL ORDER BY id LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![after_id, n.clamp(1, 1000) as i64], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get::<_, i64>(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter()
                .map(|(id, query, provider, at, hits, capture_id)| {
                    Ok(StoredSearch {
                        id,
                        query,
                        provider,
                        fetched_at: from_unix_ms(at)?,
                        hits,
                        capture_id,
                    })
                })
                .collect()
        })
        .await
    }

    /// Appends a capture: blob file first (temp, fsync, rename), then its row.
    pub async fn put(&self, key: &str, fetched: &Fetched) -> Result<i64, StoreError> {
        self.put_with(key, fetched, Tier::Http, |_, _| Ok(())).await
    }

    /// Appends a browser capture; its body is the rendered DOM as UTF-8 HTML.
    pub async fn put_rendered(&self, key: &str, fetched: &Fetched) -> Result<i64, StoreError> {
        self.put_with(key, fetched, Tier::Browser, |_, _| Ok(()))
            .await
    }

    /// Appends one search-provider exchange as a capture plus its `searches` row. `hits` is
    /// `None` when the response did not parse: the capture stays evidence, never a result.
    pub async fn put_search<H: serde::Serialize + ?Sized>(
        &self,
        key: &str,
        fetched: &Fetched,
        query: &str,
        provider: &str,
        lang: &str,
        hits: Option<&H>,
    ) -> Result<i64, StoreError> {
        let hits = hits
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| StoreError::Corrupt)?;
        let (query, provider, lang) = (query.to_owned(), provider.to_owned(), lang.to_owned());
        let at = unix_ms(fetched.fetched_at)?;
        self.put_with(key, fetched, Tier::Http, move |tx, capture| {
            tx.execute(
                "INSERT INTO searches (query, provider, lang, fetched_at_ms, hits, capture_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![query, provider, lang, at, hits, capture],
            )?;
            Ok(())
        })
        .await
    }

    /// Writes the blob off the writer thread, then inserts the capture row and any `extra` rows
    /// in one writer transaction, rewriting the blob first if a sweep removed it meanwhile.
    async fn put_with(
        &self,
        key: &str,
        fetched: &Fetched,
        tier: Tier,
        extra: impl FnOnce(&Transaction<'_>, i64) -> Result<(), StoreError> + Send + 'static,
    ) -> Result<i64, StoreError> {
        let hash = sha256_hex(&fetched.body);
        let (root, name, body) = (self.blobs.clone(), hash.clone(), fetched.body.clone());
        blocking(move || Ok(write_blob(&root, &name, &body)?)).await?;
        let (root, key, fetched) = (self.blobs.clone(), key.to_owned(), fetched.clone());
        self.write(move |tx| {
            write_blob(&root, &hash, &fetched.body)?;
            let id = insert_capture(tx, &key, &fetched, &hash, tier)?;
            extra(tx, id)?;
            Ok(id)
        })
        .await
    }

    /// Stores `body` under a new random capability ID that expires after `ttl`. The total size
    /// of all unswept artifacts stays within `quota` bytes.
    pub async fn put_artifact(
        &self,
        body: Vec<u8>,
        media_type: &'static str,
        ttl: Duration,
        quota: u64,
    ) -> Result<String, StoreError> {
        let len = i64::try_from(body.len()).map_err(|_| StoreError::Quota)?;
        let quota = i64::try_from(quota).unwrap_or(i64::MAX);
        // Held until the insert runs on the writer thread, even if this caller is cancelled.
        let guard = Arc::clone(&self.artifact_lock).lock_owned().await;
        let used: i64 = self
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT COALESCE(SUM(byte_len), 0) FROM artifacts",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await?;
        if used.saturating_add(len) > quota {
            return Err(StoreError::Quota);
        }
        let hash = sha256_hex(&body);
        let body = Arc::new(body);
        let (root, name, bytes) = (self.artifacts.clone(), hash.clone(), Arc::clone(&body));
        blocking(move || Ok(write_blob(&root, &name, &bytes)?)).await?;
        let id = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let now = unix_ms(OffsetDateTime::now_utc())?;
        let expires = now.saturating_add(i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX));
        let root = self.artifacts.clone();
        self.on_writer(move |conn| {
            let _guard = guard;
            write_blob(&root, &hash, &body)?;
            let inserted = conn.execute(
                "INSERT INTO artifacts (id, blob_sha256, media_type, byte_len, created_at_ms, expires_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, hash, media_type, len, now, expires],
            );
            if let Err(error) = inserted {
                // A failed cleanup only leaves an orphan for the sweep; report the insert.
                let _ = remove_unreferenced(conn, ARTIFACT_REFERENCED, &root, vec![hash]);
                return Err(error.into());
            }
            Ok(id)
        })
        .await
    }

    /// An unexpired artifact's media type and bytes.
    pub async fn artifact(&self, id: &str) -> Result<Option<(String, Vec<u8>)>, StoreError> {
        let now = unix_ms(OffsetDateTime::now_utc())?;
        let (root, id) = (self.artifacts.clone(), id.to_owned());
        self.read(move |conn| {
            let row = conn
                .query_row(
                    "SELECT media_type, blob_sha256 FROM artifacts WHERE id = ?1 AND expires_at_ms > ?2",
                    params![id, now],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            let Some((media_type, hash)) = row else {
                return Ok(None);
            };
            match fs::read(blob_path(&root, &hash)) {
                Ok(body) => Ok(Some((media_type, body))),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    /// Deletes expired artifact rows, then blob files no remaining row references.
    pub async fn sweep_artifacts(&self) -> Result<usize, StoreError> {
        let now = unix_ms(OffsetDateTime::now_utc())?;
        let root = self.artifacts.clone();
        self.on_writer(move |conn| {
            let tx = conn.transaction()?;
            let hashes = tx
                .prepare("DELETE FROM artifacts WHERE expires_at_ms <= ?1 RETURNING blob_sha256")?
                .query_map(params![now], |row| row.get(0))?
                .collect::<Result<Vec<String>, _>>()?;
            tx.commit()?;
            remove_unreferenced(conn, ARTIFACT_REFERENCED, &root, hashes)
        })
        .await
    }

    /// Whether the data directory's file system has at least `min_free_mb` MiB free.
    pub async fn free_disk_ok(&self, min_free_mb: u64) -> Result<bool, StoreError> {
        let dir = self.blobs.clone();
        let free = blocking(move || Ok(fs2::available_space(dir)?)).await?;
        Ok(free >= min_free_mb.saturating_mul(1024 * 1024))
    }

    /// One bounded retention pass: deletes captures older than [`CAPTURE_RETENTION`] (their
    /// search rows cascade) with blobs no row still needs, then examines the next slice of blob
    /// files for orphans. Returns the blob files removed.
    pub async fn sweep(&self) -> Result<usize, StoreError> {
        let retention_ms = i64::try_from(CAPTURE_RETENTION.as_millis()).unwrap_or(i64::MAX);
        let cutoff = unix_ms(OffsetDateTime::now_utc())?.saturating_sub(retention_ms);
        let mut removed = 0;
        for _ in 0..SWEEP_BATCHES {
            let root = self.blobs.clone();
            let (deleted, freed) = self
                .on_writer(move |conn| {
                    let tx = conn.transaction()?;
                    let hashes = tx
                        .prepare(
                            "DELETE FROM captures WHERE id IN (SELECT id FROM captures
                             WHERE fetched_at_ms < ?1 LIMIT ?2) RETURNING blob_sha256",
                        )?
                        .query_map(params![cutoff, SWEEP_BATCH as i64], |row| row.get(0))?
                        .collect::<Result<Vec<String>, _>>()?;
                    tx.commit()?;
                    let deleted = hashes.len();
                    Ok((
                        deleted,
                        remove_unreferenced(conn, CAPTURE_REFERENCED, &root, hashes)?,
                    ))
                })
                .await?;
            removed += freed;
            if deleted < SWEEP_BATCH {
                break;
            }
        }
        for (index, root, referenced) in [
            (0, &self.blobs, CAPTURE_REFERENCED),
            (1, &self.artifacts, ARTIFACT_REFERENCED),
        ] {
            let cursor = self
                .orphan_cursors
                .lock()
                .unwrap_or_else(PoisonError::into_inner)[index]
                .clone();
            let dir = root.clone();
            let (candidates, next) =
                blocking(move || Ok(orphan_candidates(&dir, &cursor)?)).await?;
            self.orphan_cursors
                .lock()
                .unwrap_or_else(PoisonError::into_inner)[index] = next;
            let root = root.clone();
            removed += self
                .on_writer(move |conn| remove_unreferenced(conn, referenced, &root, candidates))
                .await?;
        }
        Ok(removed)
    }
}

/// Runs blocking file or database work off the async runtime.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, StoreError> + Send + 'static,
) -> Result<T, StoreError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| StoreError::Task)?
}

/// Decodes one `CAPTURE_COLUMNS` row; `None` when its blob is gone.
fn read_capture(blobs: &Path, row: &rusqlite::Row<'_>) -> Result<Option<Capture>, StoreError> {
    let hash: String = row.get(6)?;
    let body = match fs::read(blob_path(blobs, &hash)) {
        Ok(body) => body,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let route: Option<String> = row.get(8)?;
    let tier: Option<String> = row.get(9)?;
    let url = |index| -> Result<url::Url, StoreError> {
        row.get::<_, String>(index)?
            .parse()
            .map_err(|_| StoreError::Corrupt)
    };
    Ok(Some(Capture {
        id: row.get(0)?,
        blob_sha256: hash,
        tier: stored_tier(tier.as_deref())?,
        fetched: Fetched {
            // Rows written before the column existed were direct.
            route: route
                .as_deref()
                .map_or(Some(Route::Direct), Route::parse)
                .ok_or(StoreError::Corrupt)?,
            requested_url: url(1)?,
            final_url: url(2)?,
            status: row.get(3)?,
            headers: serde_json::from_str(&row.get::<_, String>(4)?)
                .map_err(|_| StoreError::Corrupt)?,
            content_type: row.get(5)?,
            body: body.into(),
            fetched_at: from_unix_ms(row.get(7)?)?,
        },
    }))
}

fn insert_capture(
    tx: &Transaction<'_>,
    key: &str,
    fetched: &Fetched,
    hash: &str,
    tier: Tier,
) -> Result<i64, StoreError> {
    let headers: Vec<_> = fetched
        .headers
        .iter()
        .filter(|(name, _)| {
            !UNSTORED_HEADERS
                .iter()
                .any(|h| name.eq_ignore_ascii_case(h))
        })
        .collect();
    let headers = serde_json::to_string(&headers).map_err(|_| StoreError::Corrupt)?;
    tx.execute(
        "INSERT INTO captures (cache_key, requested_url, final_url, status, headers,
                               content_type, blob_sha256, byte_len, fetched_at_ms, route, tier)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            key,
            fetched.requested_url.as_str(),
            fetched.final_url.as_str(),
            fetched.status,
            headers,
            fetched.content_type,
            hash,
            i64::try_from(fetched.body.len()).map_err(|_| StoreError::Corrupt)?,
            unix_ms(fetched.fetched_at)?,
            fetched.route.as_str(),
            tier.as_str(),
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

/// Unlinks each blob in `root` that `referenced` (SQL taking the hash) finds no row for. Runs on
/// the writer thread, so no insert of a referencing row interleaves.
fn remove_unreferenced(
    conn: &Connection,
    referenced: &str,
    root: &Path,
    mut hashes: Vec<String>,
) -> Result<usize, StoreError> {
    hashes.sort();
    hashes.dedup();
    let mut stmt = conn.prepare_cached(referenced)?;
    let mut removed = 0;
    for hash in hashes {
        if stmt.query_row([&hash], |row| row.get::<_, bool>(0))? {
            continue;
        }
        match fs::remove_file(blob_path(root, &hash)) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(removed)
}

/// Blob names after `cursor` in hash order, up to [`ORPHAN_SCAN`] examined, that are older than
/// [`ORPHAN_GRACE`]; plus the next cursor, empty once the scan wraps.
fn orphan_candidates(root: &Path, cursor: &str) -> io::Result<(Vec<String>, String)> {
    let names = |dir: &Path, keep: &dyn Fn(&str) -> bool| -> io::Result<Vec<String>> {
        let mut names: Vec<String> = fs::read_dir(dir)?
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| keep(name))
            .collect();
        names.sort();
        Ok(names)
    };
    let shard = cursor.get(..2).unwrap_or_default();
    let is_hash = |name: &str| name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit());
    let old = SystemTime::now()
        .checked_sub(ORPHAN_GRACE)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let (mut candidates, mut examined, mut last) = (Vec::new(), 0, String::new());
    for prefix in names(root, &|name| name.len() == 2 && name >= shard)? {
        for name in names(&root.join(&prefix), &|name| is_hash(name) && name > cursor)? {
            if examined == ORPHAN_SCAN {
                return Ok((candidates, last));
            }
            examined += 1;
            let modified = fs::metadata(blob_path(root, &name)).and_then(|meta| meta.modified());
            if modified.is_ok_and(|at| at < old) {
                candidates.push(name.clone());
            }
            last = name;
        }
    }
    Ok((candidates, String::new()))
}

/// Rows written before the `tier` column existed are HTTP captures.
fn stored_tier(tier: Option<&str>) -> Result<Tier, StoreError> {
    match tier {
        None | Some("http") => Ok(Tier::Http),
        Some("browser") => Ok(Tier::Browser),
        Some(_) => Err(StoreError::Corrupt),
    }
}

fn blob_path(root: &Path, hash: &str) -> PathBuf {
    root.join(&hash[..2]).join(hash)
}

fn write_blob(root: &Path, hash: &str, body: &[u8]) -> io::Result<()> {
    let path = blob_path(root, hash);
    if path.exists() {
        return Ok(());
    }
    let dir = root.join(&hash[..2]);
    fs::create_dir_all(&dir)?;
    let temp = dir.join(format!(".{hash}.{}.tmp", uuid::Uuid::new_v4()));
    let written = File::create(&temp)
        .and_then(|mut file| file.write_all(body).and_then(|()| file.sync_all()))
        .and_then(|()| fs::rename(&temp, &path));
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written?;
    // Persist the rename itself before the row that points at it.
    File::open(&dir)?.sync_all()
}

/// Lowercase hex SHA-256: blob names and cache keys.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn unix_ms(at: OffsetDateTime) -> Result<i64, StoreError> {
    i64::try_from(at.unix_timestamp_nanos() / 1_000_000).map_err(|_| StoreError::Corrupt)
}

fn from_unix_ms(ms: i64) -> Result<OffsetDateTime, StoreError> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .map_err(|_| StoreError::Corrupt)
}
