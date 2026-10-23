//! SQLite capture index and content-addressed blob files.
//!
//! Captures are appended, never updated. A blob is durable before its row exists, so a crash
//! leaves at most an orphan blob, never a row without content.

use std::{
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

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
";

/// Idle read-only connections kept for reuse; bursts beyond this open short-lived extras.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const IDLE_READERS: usize = 8;

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
}

/// One writer connection plus reusable read-only connections, so cache lookups never queue
/// behind an insert (WAL lets readers run beside the writer).
struct Db {
    path: PathBuf,
    writer: Mutex<Connection>,
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
}

impl Store {
    pub fn open(dir: &Path) -> Result<Self, StoreError> {
        let blobs = dir.join("blobs").join("sha256");
        fs::create_dir_all(&blobs)?;
        let path = dir.join("sonar.db");
        let writer = Connection::open(&path)?;
        writer.busy_timeout(BUSY_TIMEOUT)?;
        writer
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))?;
        // NORMAL is crash-safe in WAL: a power loss can drop the newest rows, never corrupt.
        writer.pragma_update(None, "synchronous", "NORMAL")?;
        writer.pragma_update(None, "foreign_keys", true)?;
        writer.execute_batch(SCHEMA)?;
        // Serialize schema inspection and ALTER across simultaneous opens of an old store.
        let writer = {
            let mut writer = writer;
            let tx = writer.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let has_route: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('captures') WHERE name = 'route')",
                [],
                |row| row.get(0),
            )?;
            if !has_route {
                tx.execute_batch("ALTER TABLE captures ADD COLUMN route TEXT")?;
            }
            tx.commit()?;
            writer
        };
        Ok(Self {
            db: Arc::new(Db {
                path,
                writer: Mutex::new(writer),
                readers: Mutex::new(Vec::new()),
            }),
            blobs,
        })
    }

    /// The newest 2xx capture for `key` no older than `max_age`, or `None` when absent or its
    /// blob is gone.
    pub async fn cached(
        &self,
        key: &str,
        max_age: Duration,
    ) -> Result<Option<Fetched>, StoreError> {
        let (store, key) = (self.clone(), key.to_owned());
        tokio::task::spawn_blocking(move || store.cached_blocking(&key, max_age))
            .await
            .map_err(|_| StoreError::Task)?
    }

    /// Appends a capture: blob file first (temp, fsync, rename), then its row.
    pub async fn put(&self, key: &str, fetched: &Fetched) -> Result<(), StoreError> {
        let (store, key, fetched) = (self.clone(), key.to_owned(), fetched.clone());
        tokio::task::spawn_blocking(move || store.put_blocking(&key, &fetched))
            .await
            .map_err(|_| StoreError::Task)?
    }

    fn cached_blocking(&self, key: &str, max_age: Duration) -> Result<Option<Fetched>, StoreError> {
        let max_age_ms = i64::try_from(max_age.as_millis()).unwrap_or(i64::MAX);
        let since = unix_ms(OffsetDateTime::now_utc())?.saturating_sub(max_age_ms);
        let row = {
            let db = self.db.reader()?;
            let row = db
                .query_row(
                    "SELECT requested_url, final_url, status, headers, content_type, blob_sha256,
                        fetched_at_ms, route
                 FROM captures
                 WHERE cache_key = ?1 AND fetched_at_ms >= ?2 AND status BETWEEN 200 AND 299
                 ORDER BY fetched_at_ms DESC, id DESC LIMIT 1",
                    params![key, since],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, u16>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, i64>(6)?,
                            row.get::<_, Option<String>>(7)?,
                        ))
                    },
                )
                .optional();
            // Only a connection that just answered is worth reusing.
            if row.is_ok() {
                self.db.recycle(db);
            }
            row?
        };
        let Some((requested, final_url, status, headers, content_type, hash, fetched_at, route)) =
            row
        else {
            return Ok(None);
        };
        let body = match fs::read(self.blob_path(&hash)) {
            Ok(body) => body,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(Some(Fetched {
            route: match route.as_deref() {
                None | Some("direct") => Route::Direct,
                Some("proxy") => Route::Proxy,
                _ => return Err(StoreError::Corrupt),
            },
            requested_url: requested.parse().map_err(|_| StoreError::Corrupt)?,
            final_url: final_url.parse().map_err(|_| StoreError::Corrupt)?,
            status,
            headers: serde_json::from_str(&headers).map_err(|_| StoreError::Corrupt)?,
            content_type,
            body: body.into(),
            fetched_at: OffsetDateTime::from_unix_timestamp_nanos(
                i128::from(fetched_at) * 1_000_000,
            )
            .map_err(|_| StoreError::Corrupt)?,
        }))
    }

    fn put_blocking(&self, key: &str, fetched: &Fetched) -> Result<(), StoreError> {
        let hash = sha256_hex(&fetched.body);
        self.write_blob(&hash, &fetched.body)?;
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
        let db = self
            .db
            .writer
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        db.execute(
            "INSERT INTO captures (cache_key, requested_url, final_url, status, headers,
                                   content_type, blob_sha256, byte_len, fetched_at_ms, route)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
                match fetched.route {
                    Route::Direct => "direct",
                    Route::Proxy => "proxy",
                },
            ],
        )?;
        Ok(())
    }

    fn blob_path(&self, hash: &str) -> PathBuf {
        self.blobs.join(&hash[..2]).join(hash)
    }

    fn write_blob(&self, hash: &str, body: &[u8]) -> io::Result<()> {
        let path = self.blob_path(hash);
        if path.exists() {
            return Ok(());
        }
        let dir = self.blobs.join(&hash[..2]);
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
