//! SQLite-backed state. Per-profile database (one file per profile).
//!
//! Tracks scanned files, placements, and health flags. Re-runs use a
//! `(size, mtime)` cache to skip rehashing unchanged files.
//!
//! ## Timestamp formats
//!
//! Two ISO-8601 shapes are used:
//!
//! - **System timestamps** (`first_seen`, `last_seen`, `placed_at`,
//!   `detected_at`): UTC, millisecond precision, format
//!   `%Y-%m-%dT%H:%M:%S%.3fZ`.
//! - **Capture timestamps** (`taken_at`): naive-local, second precision,
//!   format `%Y-%m-%dT%H:%M:%S`. No timezone suffix; matches the
//!   `NaiveDateTime` shape carried by [`Metadata::taken_at`].

pub mod runs;
pub mod schema;

pub use runs::{PlacementRow, RunCounts, RunId, RunRow, decode_from_paths, encode_from_paths};

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::config::Profile;
use crate::error::{Error, Result};
use crate::hash::{hex, sha256_file};
use crate::metadata::{DateSource, Metadata};
use crate::scan::ScannedFile;

const SYSTEM_TS_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3fZ";
const CAPTURE_TS_FORMAT: &str = "%Y-%m-%dT%H:%M:%S";

/// Foreign key for `files.id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId(pub i64);

/// Whether [`State::hash_or_lookup`] reused a cached hash or recomputed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashCacheHit {
    Hit,
    Miss,
}

/// Stat trio used as the cache key alongside the source path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatKey {
    pub size: i64,
    pub mtime_secs: i64,
    pub mtime_nanos: i64,
}

/// One row of the bulk-preloaded hash cache returned by
/// [`State::load_hash_cache`].
#[derive(Debug, Clone, Copy)]
pub struct CachedFileEntry {
    pub stat: StatKey,
    pub sha256: [u8; 32],
}

/// Stat `path` and return its `(size, mtime_secs, mtime_nanos)`. Public so the
/// planner's parallel pre-compute path can stat without an `&mut State`.
pub fn stat_for_cache(path: &Path) -> Result<StatKey> {
    stat_size_mtime(path)
}

/// Canonicalise `path` into the form used as the `files.source_path` key.
/// Public so the planner can build cache lookups against the same key shape
/// the DB stores.
#[must_use]
pub fn source_path_key(path: &Path) -> String {
    path_to_string(path)
}

/// One row destined for the `health` table.
#[derive(Debug, Clone)]
pub struct HealthRow<'a> {
    /// On-disk discriminator (`"drift"`, `"missing-destination"`, ...).
    pub kind: &'a str,
    /// Destination path that motivated the row. Used to recover `file_id`
    /// via `placements.dest_path = ?`.
    pub dest_path: &'a str,
    pub detail: Option<&'a str>,
}

/// Connection wrapper. Each profile owns one of these.
pub struct State {
    conn: Connection,
    db_path: PathBuf,
}

/// Build a closure that wraps a `rusqlite::Error` in [`Error::Sqlite`]
/// with the given DB path attached. Used at call sites where `self.conn`
/// is already mutably borrowed by a transaction.
fn sqlite_err_with(path: &Path) -> impl Fn(rusqlite::Error) -> Error + '_ {
    move |source| Error::Sqlite {
        path: path.to_path_buf(),
        source,
    }
}

impl State {
    /// Open the per-profile database, creating parent dirs and running
    /// pending migrations.
    pub fn open(profile_name: &str, profile: &Profile) -> Result<Self> {
        let path = resolve_db_path(profile_name, &profile.state.database)?;
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|source| Error::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let conn = Connection::open(&path).map_err(|source| Error::Sqlite {
            path: path.clone(),
            source,
        })?;
        let mut state = Self {
            conn,
            db_path: path,
        };
        state.configure()?;
        state.migrate()?;
        Ok(state)
    }

    /// Open the per-profile database read-only. No parent-dir creation,
    /// no migrations, no write pragmas. Used by `shelf status` so it can
    /// run while another process holds a write connection.
    pub fn open_readonly(profile_name: &str, profile: &Profile) -> Result<Self> {
        let path = resolve_db_path(profile_name, &profile.state.database)?;
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(&path, flags).map_err(|source| Error::Sqlite {
            path: path.clone(),
            source,
        })?;
        Ok(Self {
            conn,
            db_path: path,
        })
    }

    /// In-memory state for tests.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(|source| Error::Sqlite {
            path: PathBuf::from(":memory:"),
            source,
        })?;
        let mut state = Self {
            conn,
            db_path: PathBuf::from(":memory:"),
        };
        state.configure()?;
        state.migrate()?;
        Ok(state)
    }

    #[must_use]
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    #[must_use]
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    fn sql_err(&self, source: rusqlite::Error) -> Error {
        Error::Sqlite {
            path: self.db_path.clone(),
            source,
        }
    }

    fn configure(&self) -> Result<()> {
        // Pragmas live outside migrations: `foreign_keys` is per-connection
        // and `journal_mode = WAL` is a no-op inside the migration
        // transaction.
        self.conn
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| self.sql_err(e))?;
        self.conn
            .pragma_update(None, "temp_store", "MEMORY")
            .map_err(|e| self.sql_err(e))?;
        // Negative cache_size is interpreted as KiB; 64 MiB keeps hot pages
        // resident across the planner's many small probes.
        self.conn
            .pragma_update(None, "cache_size", -65536_i64)
            .map_err(|e| self.sql_err(e))?;
        // WAL + the durability-relaxing `synchronous=NORMAL` are meaningless on
        // `:memory:`; mmap is also pointless there.
        if self.db_path.as_os_str() != ":memory:" {
            self.conn
                .pragma_update(None, "journal_mode", "WAL")
                .map_err(|e| self.sql_err(e))?;
            // Documented-safe on WAL: the last unflushed commit group can be
            // lost on power loss but the DB stays consistent. Next run rebuilds
            // the hash cache from disk anyway, so the loss is bounded.
            self.conn
                .pragma_update(None, "synchronous", "NORMAL")
                .map_err(|e| self.sql_err(e))?;
            self.conn
                .pragma_update(None, "mmap_size", 268_435_456_i64)
                .map_err(|e| self.sql_err(e))?;
        }
        Ok(())
    }

    fn migrate(&mut self) -> Result<()> {
        schema::migrations()
            .to_latest(&mut self.conn)
            .map_err(|source| Error::Migration {
                path: self.db_path.clone(),
                source,
            })
    }

    /// Look up a cached sha256 for `file`, or compute one if absent or stale.
    ///
    /// The `files` row is **not** modified on a miss — call
    /// [`State::upsert_file`] separately once metadata is in hand. On a
    /// hit, `last_seen` is bumped so health checks can find stale rows.
    ///
    /// Cache key: `(source_path, size, mtime_secs, mtime_nanos)`.
    pub fn hash_or_lookup(&mut self, file: &ScannedFile) -> Result<([u8; 32], HashCacheHit)> {
        hash_or_lookup_with(&self.conn, &self.db_path, file)
    }

    /// Insert or update the `files` row for `scanned`. Idempotent.
    ///
    /// **TOCTOU note.** The `sha256` must come from the most recent
    /// [`State::hash_or_lookup`] for this path. If the file changes
    /// between that hash and this stat, the row records a
    /// stale-but-consistent `(size, mtime, sha256)` — all describing
    /// the same earlier version. Next run sees stat drift and self-heals.
    pub fn upsert_file(
        &mut self,
        scanned: &ScannedFile,
        metadata: &Metadata,
        sha256: [u8; 32],
    ) -> Result<FileId> {
        let db_path = self.db_path.clone();
        let sql = sqlite_err_with(&db_path);
        let tx = self.conn.transaction().map_err(&sql)?;
        let id = upsert_file_with(&tx, &db_path, scanned, metadata, sha256)?;
        tx.commit().map_err(&sql)?;
        Ok(id)
    }

    /// Bulk-load `(source_path → cached digest)`. The planner uses this once
    /// before the parallel compute phase so per-file probes happen in memory
    /// instead of through SQLite.
    pub fn load_hash_cache(&self) -> Result<HashMap<String, CachedFileEntry>> {
        let mut stmt = self
            .conn
            .prepare("SELECT source_path, size, mtime_secs, mtime_nanos, sha256 FROM files")
            .map_err(|e| self.sql_err(e))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .map_err(|e| self.sql_err(e))?;
        let mut map = HashMap::new();
        for row in rows {
            let (path, size, secs, nanos, sha_hex) = row.map_err(|e| self.sql_err(e))?;
            if let Some(sha256) = decode_hex32(&sha_hex) {
                map.insert(
                    path,
                    CachedFileEntry {
                        stat: StatKey {
                            size,
                            mtime_secs: secs,
                            mtime_nanos: nanos,
                        },
                        sha256,
                    },
                );
            }
        }
        Ok(map)
    }

    /// Run `f` inside one write transaction so a batch of
    /// `hash_or_lookup` / `upsert_file` calls commits with a single fsync.
    /// On `Err` from `f`, the transaction rolls back; on `Ok`, it commits.
    pub fn with_prepare_tx<F, R>(&mut self, f: F) -> Result<R>
    where
        F: FnOnce(&PrepareBatch<'_>) -> Result<R>,
    {
        let db_path = self.db_path.clone();
        let sql = sqlite_err_with(&db_path);
        let tx = self.conn.transaction().map_err(&sql)?;
        let result = {
            let batch = PrepareBatch {
                conn: &tx,
                db_path: &db_path,
            };
            f(&batch)
        };
        match result {
            Ok(v) => {
                tx.commit().map_err(&sql)?;
                Ok(v)
            }
            Err(e) => {
                // Rollback is implicit on drop, but be explicit for clarity.
                let _ = tx.rollback();
                Err(e)
            }
        }
    }

    /// Insert one row into the `health` table.
    pub fn record_health(&mut self, row: &HealthRow<'_>) -> Result<()> {
        self.record_health_many(std::slice::from_ref(row))
    }

    /// Batch-insert health rows inside a single transaction. `dest_path`
    /// resolves `file_id` via `placements.dest_path = ?`; unmatched rows
    /// store a null `file_id`. No deduplication: repeated `verify --full`
    /// runs append fresh entries on purpose so the table reads as a timeline.
    pub fn record_health_many(&mut self, rows: &[HealthRow<'_>]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let db_path = self.db_path.clone();
        let sql = sqlite_err_with(&db_path);
        let now = now_iso();

        let tx = self.conn.transaction().map_err(&sql)?;
        for row in rows {
            let file_id: Option<i64> = tx
                .query_row(
                    "SELECT file_id FROM placements \
                     WHERE dest_path = ?1 \
                     LIMIT 1",
                    params![row.dest_path],
                    |r| r.get::<_, i64>(0),
                )
                .optional()
                .map_err(&sql)?;

            tx.execute(
                "INSERT INTO health (file_id, kind, detail, detected_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![file_id, row.kind, row.detail, &now],
            )
            .map_err(&sql)?;
        }
        tx.commit().map_err(&sql)?;
        Ok(())
    }
}

/// Borrow handle that funnels both `hash_or_lookup` and `upsert_file` through
/// the same connection (and therefore the same transaction). Created by
/// [`State::with_prepare_tx`].
pub struct PrepareBatch<'a> {
    conn: &'a Connection,
    db_path: &'a Path,
}

impl<'a> PrepareBatch<'a> {
    pub fn hash_or_lookup(&self, file: &ScannedFile) -> Result<([u8; 32], HashCacheHit)> {
        hash_or_lookup_with(self.conn, self.db_path, file)
    }

    pub fn upsert_file(
        &self,
        scanned: &ScannedFile,
        metadata: &Metadata,
        sha256: [u8; 32],
    ) -> Result<FileId> {
        upsert_file_with(self.conn, self.db_path, scanned, metadata, sha256)
    }
}

fn hash_or_lookup_with(
    conn: &Connection,
    db_path: &Path,
    file: &ScannedFile,
) -> Result<([u8; 32], HashCacheHit)> {
    let path = file.absolute_path.as_path();
    let stat = stat_size_mtime(path)?;
    let source_key = path_to_string(path);
    let sql = sqlite_err_with(db_path);

    let cached: Option<(i64, String, i64, i64, i64)> = conn
        .query_row(
            "SELECT id, sha256, size, mtime_secs, mtime_nanos \
             FROM files WHERE source_path = ?1",
            params![&source_key],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()
        .map_err(&sql)?;

    if let Some((id, sha_hex, cached_size, cached_secs, cached_nanos)) = cached
        && cached_size == stat.size
        && cached_secs == stat.mtime_secs
        && cached_nanos == stat.mtime_nanos
        && let Some(bytes) = decode_hex32(&sha_hex)
    {
        conn.execute(
            "UPDATE files SET last_seen = ?1 WHERE id = ?2",
            params![&now_iso(), id],
        )
        .map_err(&sql)?;
        tracing::trace!(path = %path.display(), "hash cache hit");
        return Ok((bytes, HashCacheHit::Hit));
    }

    let bytes = sha256_file(path)?;
    tracing::trace!(path = %path.display(), "hash cache miss");
    Ok((bytes, HashCacheHit::Miss))
}

fn upsert_file_with(
    conn: &Connection,
    db_path: &Path,
    scanned: &ScannedFile,
    metadata: &Metadata,
    sha256: [u8; 32],
) -> Result<FileId> {
    let path = scanned.absolute_path.as_path();
    let stat = stat_size_mtime(path)?;
    let source_key = path_to_string(path);
    let sha_hex = hex(&sha256);
    let taken_at = metadata.taken_at.format(CAPTURE_TS_FORMAT).to_string();
    let source_str = date_source_str(metadata.taken_at_source);
    let now = now_iso();
    let sql = sqlite_err_with(db_path);

    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM files WHERE source_path = ?1",
            params![&source_key],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(&sql)?;

    let id = if let Some(id) = existing {
        conn.execute(
            "UPDATE files SET \
                size = ?1, mtime_secs = ?2, mtime_nanos = ?3, \
                sha256 = ?4, taken_at = ?5, taken_at_source = ?6, \
                kind = ?7, camera = ?8, lens = ?9, width = ?10, height = ?11, \
                author = ?12, title = ?13, last_seen = ?14 \
             WHERE id = ?15",
            params![
                stat.size,
                stat.mtime_secs,
                stat.mtime_nanos,
                &sha_hex,
                &taken_at,
                source_str,
                &metadata.kind,
                &metadata.camera,
                &metadata.lens,
                metadata.width,
                metadata.height,
                &metadata.author,
                &metadata.title,
                &now,
                id,
            ],
        )
        .map_err(&sql)?;
        id
    } else {
        conn.execute(
            "INSERT INTO files ( \
                source_path, size, mtime_secs, mtime_nanos, sha256, \
                taken_at, taken_at_source, kind, camera, lens, width, height, \
                author, title, first_seen, last_seen \
             ) VALUES ( \
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16 \
             )",
            params![
                &source_key,
                stat.size,
                stat.mtime_secs,
                stat.mtime_nanos,
                &sha_hex,
                &taken_at,
                source_str,
                &metadata.kind,
                &metadata.camera,
                &metadata.lens,
                metadata.width,
                metadata.height,
                &metadata.author,
                &metadata.title,
                &now,
                &now,
            ],
        )
        .map_err(&sql)?;
        conn.last_insert_rowid()
    };

    Ok(FileId(id))
}

/// Decode the placeholder DB path. Three cases:
/// 1. Literal default `~/.local/share/shelf/<profile>.db` →
///    `$XDG_DATA_HOME/shelf/<name>.db` (or `$HOME/.local/share/...`).
/// 2. Leading `~/...` → expand against `$HOME`.
/// 3. Anything else → used verbatim.
fn resolve_db_path(profile_name: &str, raw: &Path) -> Result<PathBuf> {
    let raw_str = raw.to_string_lossy();

    if raw_str == "~/.local/share/shelf/<profile>.db" {
        let base = state_base_dir()?;
        return Ok(base.join("shelf").join(format!("{profile_name}.db")));
    }

    if let Some(rest) = raw_str.strip_prefix("~/") {
        let home = std::env::var("HOME").map_err(|_| Error::NoStateDir)?;
        if home.is_empty() {
            return Err(Error::NoStateDir);
        }
        return Ok(PathBuf::from(home).join(rest));
    }

    Ok(raw.to_path_buf())
}

/// Resolve `$XDG_DATA_HOME` with fallback to `$HOME/.local/share`.
fn state_base_dir() -> Result<PathBuf> {
    if let Ok(val) = std::env::var("XDG_DATA_HOME")
        && !val.is_empty()
    {
        return Ok(PathBuf::from(val));
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return Ok(PathBuf::from(home).join(".local").join("share"));
    }
    Err(Error::NoStateDir)
}

/// Stat helper returning the cache-key trio. Pre-epoch mtimes collapse to zero.
fn stat_size_mtime(path: &Path) -> Result<StatKey> {
    let meta = fs::metadata(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let size = i64::try_from(meta.len()).unwrap_or(i64::MAX);
    let mtime = meta.modified().map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let (mtime_secs, mtime_nanos) = duration_since_epoch(mtime);
    Ok(StatKey {
        size,
        mtime_secs,
        mtime_nanos,
    })
}

fn duration_since_epoch(t: SystemTime) -> (i64, i64) {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => (
            i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
            i64::from(d.subsec_nanos()),
        ),
        Err(_) => (0, 0),
    }
}

/// Current UTC instant formatted as `%Y-%m-%dT%H:%M:%S%.3fZ`.
fn now_iso() -> String {
    let now: DateTime<Utc> = Utc::now();
    now.format(SYSTEM_TS_FORMAT).to_string()
}

/// Path string used as the `source_path` key.
///
/// Non-UTF-8 paths are mangled lossily and may collide on the unique index
/// or produce a false cache hit between two distinct OS-encoded paths.
/// Storing raw bytes is a future migration (BLOB column).
fn path_to_string(p: &Path) -> String {
    if let Some(s) = p.to_str() {
        return s.to_owned();
    }
    let lossy = p.to_string_lossy().into_owned();
    tracing::warn!(
        path = %lossy,
        os_encoding_mangled = true,
        "source path is not valid UTF-8; using lossy form as DB key — \
         distinct OS-encoded paths may collide on this key (schema-level \
         BLOB storage is a future migration)"
    );
    lossy
}

fn date_source_str(source: DateSource) -> &'static str {
    match source {
        DateSource::Exif => "exif",
        DateSource::Quicktime => "quicktime",
        DateSource::Pdf => "pdf",
        DateSource::Filename => "filename",
        DateSource::Mtime => "mtime",
    }
}

/// Decode a 64-char lowercase hex string into a 32-byte array. `None` on
/// any encoding violation so callers fall back to "miss" rather than panic.
fn decode_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    let bytes = s.as_bytes();
    for i in 0..32 {
        let hi = hex_nibble(bytes[i * 2])?;
        let lo = hex_nibble(bytes[i * 2 + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_hex32_roundtrip() {
        let bytes = [0xab_u8; 32];
        let s = hex(&bytes);
        assert_eq!(decode_hex32(&s), Some(bytes));
    }

    #[test]
    fn decode_hex32_rejects_wrong_length() {
        assert_eq!(decode_hex32(""), None);
        assert_eq!(decode_hex32("ab"), None);
    }

    #[test]
    fn decode_hex32_rejects_non_hex() {
        let bad = "z".repeat(64);
        assert_eq!(decode_hex32(&bad), None);
    }

    #[test]
    fn resolve_db_path_passes_absolute_through() {
        let raw = PathBuf::from("/var/lib/shelf/foo.db");
        let p = resolve_db_path("anyname", &raw).unwrap();
        assert_eq!(p, raw);
    }

    #[test]
    fn record_health_inserts_row() {
        let mut state = State::open_in_memory().unwrap();
        state
            .record_health(&HealthRow {
                kind: "drift",
                dest_path: "/some/path",
                detail: Some("expected=abc got=def"),
            })
            .unwrap();
        let (kind, detail): (String, Option<String>) = state
            .conn()
            .query_row("SELECT kind, detail FROM health LIMIT 1", [], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .unwrap();
        assert_eq!(kind, "drift");
        assert_eq!(detail.as_deref(), Some("expected=abc got=def"));
    }

    #[test]
    fn record_health_many_is_transactional() {
        let mut state = State::open_in_memory().unwrap();
        let rows = vec![
            HealthRow {
                kind: "drift",
                dest_path: "/a",
                detail: None,
            },
            HealthRow {
                kind: "missing-destination",
                dest_path: "/b",
                detail: Some("gone"),
            },
        ];
        state.record_health_many(&rows).unwrap();
        let count: i64 = state
            .conn()
            .query_row("SELECT COUNT(*) FROM health", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }
}

// Cache integration tests live in-crate because `Metadata` is `#[non_exhaustive]`.
#[cfg(test)]
mod cache_tests {
    use super::*;
    use chrono::NaiveDate;

    fn scanned(root: &Path, name: &str) -> ScannedFile {
        ScannedFile {
            source_root: root.to_path_buf(),
            absolute_path: root.join(name),
            relative_path: PathBuf::from(name),
        }
    }

    fn fake_metadata(kind: &str) -> Metadata {
        Metadata {
            taken_at: NaiveDate::from_ymd_opt(2024, 1, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
            taken_at_source: DateSource::Mtime,
            camera: None,
            lens: None,
            kind: kind.to_string(),
            width: None,
            height: None,
            author: None,
            title: None,
            vendor: None,
        }
    }

    fn make_fixture() -> (tempfile::TempDir, Vec<ScannedFile>) {
        let tmp = tempfile::TempDir::new().unwrap();
        let names = ["a.bin", "b.bin", "c.bin"];
        for (i, name) in names.iter().enumerate() {
            // Distinct bytes so identical-hash collisions can't hide a bug.
            let body = vec![u8::try_from(i).unwrap_or(0) + 1; 4096];
            std::fs::write(tmp.path().join(name), &body).unwrap();
        }
        let files: Vec<ScannedFile> = names.iter().map(|n| scanned(tmp.path(), n)).collect();
        (tmp, files)
    }

    fn count_misses(state: &mut State, files: &[ScannedFile]) -> usize {
        files
            .iter()
            .map(|f| state.hash_or_lookup(f).unwrap().1)
            .filter(|h| matches!(h, HashCacheHit::Miss))
            .count()
    }

    fn count_hits(state: &mut State, files: &[ScannedFile]) -> usize {
        files
            .iter()
            .map(|f| state.hash_or_lookup(f).unwrap().1)
            .filter(|h| matches!(h, HashCacheHit::Hit))
            .count()
    }

    #[test]
    fn cold_run_is_all_misses() {
        let (_tmp, files) = make_fixture();
        let mut state = State::open_in_memory().unwrap();
        assert_eq!(count_misses(&mut state, &files), files.len());
    }

    #[test]
    fn warm_run_is_all_hits() {
        let (_tmp, files) = make_fixture();
        let mut state = State::open_in_memory().unwrap();

        for f in &files {
            let (sha, _) = state.hash_or_lookup(f).unwrap();
            state.upsert_file(f, &fake_metadata("bin"), sha).unwrap();
        }

        assert_eq!(count_hits(&mut state, &files), files.len());
    }

    #[test]
    fn mutated_file_misses_others_hit() {
        let (tmp, files) = make_fixture();
        let mut state = State::open_in_memory().unwrap();

        for f in &files {
            let (sha, _) = state.hash_or_lookup(f).unwrap();
            state.upsert_file(f, &fake_metadata("bin"), sha).unwrap();
        }

        // Mutate b.bin: change bytes and bump mtime forward to defeat
        // coarse-grained mtime resolution.
        let mutated = tmp.path().join("b.bin");
        std::fs::write(&mutated, vec![0xff_u8; 8192]).unwrap();
        let later_secs =
            filetime::FileTime::from_last_modification_time(&std::fs::metadata(&mutated).unwrap())
                .unix_seconds()
                + 5;
        filetime::set_file_mtime(&mutated, filetime::FileTime::from_unix_time(later_secs, 0))
            .unwrap();

        let mut hits = 0_usize;
        let mut misses = 0_usize;
        for f in &files {
            match state.hash_or_lookup(f).unwrap().1 {
                HashCacheHit::Hit => hits += 1,
                HashCacheHit::Miss => misses += 1,
            }
        }
        assert_eq!(hits, files.len() - 1);
        assert_eq!(misses, 1);
    }

    #[test]
    fn upsert_file_is_idempotent_on_same_inputs() {
        let (_tmp, files) = make_fixture();
        let mut state = State::open_in_memory().unwrap();

        let (sha, _) = state.hash_or_lookup(&files[0]).unwrap();
        let id_a = state
            .upsert_file(&files[0], &fake_metadata("bin"), sha)
            .unwrap();
        let id_b = state
            .upsert_file(&files[0], &fake_metadata("bin"), sha)
            .unwrap();
        assert_eq!(id_a, id_b);

        let count: i64 = state
            .conn()
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn cached_hash_matches_fresh_hash() {
        let (_tmp, files) = make_fixture();
        let mut state = State::open_in_memory().unwrap();

        let (sha_cold, miss) = state.hash_or_lookup(&files[0]).unwrap();
        assert_eq!(miss, HashCacheHit::Miss);
        state
            .upsert_file(&files[0], &fake_metadata("bin"), sha_cold)
            .unwrap();
        let (sha_warm, hit) = state.hash_or_lookup(&files[0]).unwrap();
        assert_eq!(hit, HashCacheHit::Hit);
        assert_eq!(sha_cold, sha_warm);
    }

    #[test]
    fn upsert_writes_author_and_title_columns() {
        let (_tmp, files) = make_fixture();
        let mut state = State::open_in_memory().unwrap();
        let mut md = fake_metadata("document");
        md.author = Some("Jane Doe".into());
        md.title = Some("Invoice March".into());

        let (sha, _) = state.hash_or_lookup(&files[0]).unwrap();
        state.upsert_file(&files[0], &md, sha).unwrap();

        let (author, title): (Option<String>, Option<String>) = state
            .conn()
            .query_row("SELECT author, title FROM files LIMIT 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(author.as_deref(), Some("Jane Doe"));
        assert_eq!(title.as_deref(), Some("Invoice March"));
    }
}
