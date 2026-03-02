use adaclaw_core::memory::{Category, Memory, MemoryEntry, RecallScope};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
// Phase 14-P2-1: r2d2 SQLite connection pool (multiple readers, WAL-safe writers)
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, Connection};

use crate::embeddings::EmbeddingProvider;
use std::sync::Arc;

#[cfg(feature = "sqlite-vec")]
use crate::embeddings::vec_to_bytes;
#[cfg(feature = "sqlite-vec")]
use crate::rrf::rrf_merge;

// ── Time constants (Phase 14-P3-2: no magic numbers) ─────────────────────────

/// Number of seconds in one calendar day.
///
/// Used for TTL-based memory hygiene calculations.
/// Named constant rather than the magic literal `86_400`.
const SECONDS_PER_DAY: i64 = 24 * 60 * 60; // = 86_400

// ── SqliteMemory ──────────────────────────────────────────────────────────────

/// SQLite-backed memory store with optional vector (KNN) and FTS5 search.
///
/// Phase 14-P2-1: Uses a `r2d2` connection pool instead of a single
/// `Arc<Mutex<Connection>>`.  Benefits:
/// - Multiple concurrent readers (SQLite WAL mode)
/// - Pool size 1 for `:memory:` databases (safe in-memory isolation)
/// - Pool size 4 for file databases (concurrent reads, serialised writes)
pub struct SqliteMemory {
    /// Phase 14-P2-1: r2d2 connection pool.
    /// `Pool<M>: Clone` — the pool handle can be cheaply shared across threads.
    pool: Pool<SqliteConnectionManager>,
    /// Optional embedding provider. When `None` (or dim==0), recall falls back
    /// to pure FTS5 keyword search.
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    /// Vector dimension (0 when embedder is absent / noop).
    embed_dim: usize,
}

impl SqliteMemory {
    /// Create an in-memory database (useful for tests / NoneMemory fallback).
    ///
    /// Uses pool size 1 so all operations share the same in-memory database
    /// (each connection to `:memory:` creates an independent database, but with
    /// pool size 1 the pool always reuses the single pre-created connection).
    pub fn new() -> Self {
        // Opening an in-memory SQLite database is practically infallible — the
        // only failure path requires the SQLite library itself to be absent or
        // broken.  We suppress the `clippy::expect_used` lint here because
        // `Default::default()` has no error channel and callers cannot
        // reasonably handle this condition.
        #[allow(clippy::expect_used)]
        Self::open(":memory:", None).expect("Failed to create in-memory SQLite database")
    }

    /// Open (or create) a file-backed database.
    ///
    /// `embedder` — optional embedding provider; pass `None` for FTS5-only mode.
    pub fn open(path: &str, embedder: Option<Arc<dyn EmbeddingProvider>>) -> Result<Self> {
        // ── Register sqlite-vec extension globally ─────────────────────────
        //
        // sqlite3_auto_extension is a process-global call — registering the same
        // extension multiple times is safe and idempotent.  Must be called before
        // any connections are created so the extension is available in `with_init`.
        //
        // SAFETY: sqlite3_vec_init is a valid SQLite extension init function;
        // the transmute from a typed fn pointer to the generic *const () is the
        // same pattern used by the official sqlite-vec Rust example.
        #[cfg(feature = "sqlite-vec")]
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }

        let embed_dim = embedder.as_ref().map(|e| e.dim()).unwrap_or(0);

        // ── Build connection pool ─────────────────────────────────────────
        //
        // For `:memory:` databases, max_size=1 ensures all callers share the
        // same connection slot (and thus the same in-memory database).
        //
        // For file databases, max_size=4 allows concurrent readers under WAL.
        // Writers should use `BEGIN IMMEDIATE` (which we do for `hygiene` and
        // `session_store::compact`).
        let is_memory = path == ":memory:";
        let embed_dim_for_init = embed_dim;

        let manager = if is_memory {
            SqliteConnectionManager::memory()
        } else {
            SqliteConnectionManager::file(path)
        }
        .with_init(move |conn| setup_schema(conn, embed_dim_for_init));

        let pool = r2d2::Pool::builder()
            .max_size(if is_memory { 1 } else { 4 })
            .build(manager)
            .map_err(|e| anyhow!("Failed to create SQLite connection pool: {}", e))?;

        Ok(Self {
            pool,
            embedder,
            embed_dim,
        })
    }

    /// Whether this instance has an active vector index.
    pub fn has_vector_index(&self) -> bool {
        cfg!(feature = "sqlite-vec") && self.embed_dim > 0 && self.embedder.is_some()
    }

    // ── Internal: upsert embedding ──────────────────────────────────────────

    #[cfg(feature = "sqlite-vec")]
    fn upsert_embedding(&self, conn: &Connection, key: &str, embedding: &[f32]) -> Result<()> {
        if self.embed_dim == 0 || embedding.is_empty() {
            return Ok(());
        }
        let blob = vec_to_bytes(embedding);
        conn.execute(
            "INSERT INTO memory_vss (key, embedding) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET embedding = excluded.embedding",
            params![key, blob],
        )?;
        Ok(())
    }

    #[cfg(not(feature = "sqlite-vec"))]
    fn upsert_embedding(&self, _conn: &Connection, _key: &str, _embedding: &[f32]) -> Result<()> {
        Ok(())
    }

    #[cfg(feature = "sqlite-vec")]
    fn delete_embedding(&self, conn: &Connection, key: &str) -> Result<()> {
        if self.embed_dim == 0 {
            return Ok(());
        }
        conn.execute("DELETE FROM memory_vss WHERE key = ?1", params![key])?;
        Ok(())
    }

    #[cfg(not(feature = "sqlite-vec"))]
    fn delete_embedding(&self, _conn: &Connection, _key: &str) -> Result<()> {
        Ok(())
    }

    // ── Internal: vector KNN query ──────────────────────────────────────────

    #[cfg(feature = "sqlite-vec")]
    fn vector_search(
        conn: &Connection,
        query_bytes: &[u8],
        k: usize,
        session: Option<&str>,
    ) -> Result<Vec<String>> {
        let sql = if session.is_some() {
            "SELECT v.key FROM memory_vss v
             JOIN memory m ON v.key = m.key
             WHERE v.embedding MATCH ?1 AND k = ?2
               AND m.session = ?3
             ORDER BY distance"
        } else {
            "SELECT v.key FROM memory_vss v
             WHERE v.embedding MATCH ?1 AND k = ?2
             ORDER BY distance"
        };

        let mut stmt = conn.prepare(sql)?;
        let keys: Vec<String> = if let Some(sess) = session {
            stmt.query_map(params![query_bytes, k as i64, sess], |row| {
                row.get(0)
            })?
            .collect::<std::result::Result<_, _>>()?
        } else {
            stmt.query_map(params![query_bytes, k as i64], |row| row.get(0))?
                .collect::<std::result::Result<_, _>>()?
        };
        Ok(keys)
    }

    // ── Internal: FTS5 keyword search ───────────────────────────────────────

    fn fts_search(
        conn: &Connection,
        query: &str,
        k: usize,
        session: Option<&str>,
    ) -> Result<Vec<String>> {
        let escaped = query.replace('"', "\"\"");
        let fts_query = format!("\"{}\"", escaped);

        let sql = if session.is_some() {
            "SELECT m.key
             FROM memory m
             JOIN memory_fts f ON m.rowid = f.rowid
             WHERE memory_fts MATCH ?1
               AND m.session = ?3
             ORDER BY rank
             LIMIT ?2"
        } else {
            "SELECT m.key
             FROM memory m
             JOIN memory_fts f ON m.rowid = f.rowid
             WHERE memory_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2"
        };

        let mut stmt = conn.prepare(sql)?;
        let keys: Vec<String> = if let Some(sess) = session {
            stmt.query_map(params![fts_query, k as i64, sess], |row| row.get(0))?
                .collect::<std::result::Result<_, _>>()?
        } else {
            stmt.query_map(params![fts_query, k as i64], |row| row.get(0))?
                .collect::<std::result::Result<_, _>>()?
        };
        Ok(keys)
    }

    // ── Internal: fetch entries by keys (preserving order) ─────────────────

    fn fetch_by_keys(conn: &Connection, keys: &[String]) -> Result<Vec<MemoryEntry>> {
        if keys.is_empty() {
            return Ok(vec![]);
        }
        let placeholders: String = (1..=keys.len())
            .map(|i| format!("?{}", i))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT key, content, category, session, topic FROM memory WHERE key IN ({})",
            placeholders
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(keys.iter()),
            |row| {
                Ok(row_to_entry(
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        let mut map: std::collections::HashMap<String, MemoryEntry> =
            rows.collect::<std::result::Result<Vec<_>, _>>()?
                .into_iter()
                .map(|e| (e.key.clone(), e))
                .collect();

        let ordered: Vec<MemoryEntry> = keys.iter().filter_map(|k| map.remove(k)).collect();
        Ok(ordered)
    }
}

impl Default for SqliteMemory {
    fn default() -> Self {
        Self::new()
    }
}

// ── Schema setup helper ────────────────────────────────────────────────────────

/// Initialise the database schema for a newly-created connection.
///
/// Called by the r2d2 pool's `with_init` callback for every new connection.
/// All DDL uses `IF NOT EXISTS`, making this function fully idempotent.
fn setup_schema(conn: &mut Connection, embed_dim: usize) -> rusqlite::Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA foreign_keys=ON;

         -- Main key-value memory table
         CREATE TABLE IF NOT EXISTS memory (
             key         TEXT    PRIMARY KEY,
             content     TEXT    NOT NULL,
             category    TEXT    NOT NULL DEFAULT 'Daily',
             session     TEXT,
             topic       TEXT,
             created_at  INTEGER NOT NULL DEFAULT (strftime('%s','now')),
             updated_at  INTEGER NOT NULL DEFAULT (strftime('%s','now'))
         );

         -- Topic registry — one row per known topic.
         CREATE TABLE IF NOT EXISTS topics (
             topic_id        TEXT    PRIMARY KEY,
             label           TEXT,
             created_at      INTEGER NOT NULL DEFAULT (strftime('%s','now')),
             last_used_at    INTEGER NOT NULL DEFAULT (strftime('%s','now'))
         );

         -- FTS5 shadow table for full-text search (BM25 ranking)
         CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts
             USING fts5(key UNINDEXED, content,
                        content='memory', content_rowid='rowid',
                        tokenize='unicode61');

         -- Keep FTS in sync via triggers
         CREATE TRIGGER IF NOT EXISTS memory_ai
             AFTER INSERT ON memory BEGIN
                 INSERT INTO memory_fts(rowid, key, content)
                 VALUES (new.rowid, new.key, new.content);
             END;

         CREATE TRIGGER IF NOT EXISTS memory_au
             AFTER UPDATE ON memory BEGIN
                 INSERT INTO memory_fts(memory_fts, rowid, key, content)
                 VALUES ('delete', old.rowid, old.key, old.content);
                 INSERT INTO memory_fts(rowid, key, content)
                 VALUES (new.rowid, new.key, new.content);
             END;

         CREATE TRIGGER IF NOT EXISTS memory_ad
             AFTER DELETE ON memory BEGIN
                 INSERT INTO memory_fts(memory_fts, rowid, key, content)
                 VALUES ('delete', old.rowid, old.key, old.content);
             END;
        ",
    )?;

    // Migration: add topic column to existing databases.
    // Safe to run on new DBs (column already exists) and old DBs (adds it).
    // Ignoring the error because it just means the column already exists.
    let _ = conn.execute_batch("ALTER TABLE memory ADD COLUMN topic TEXT;");

    // Create vector table dynamically based on embedding dimension.
    // Only needed when sqlite-vec feature is active and embeddings are configured.
    #[cfg(feature = "sqlite-vec")]
    if embed_dim > 0 {
        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS memory_vss
                 USING vec0(key TEXT PRIMARY KEY, embedding float[{dim}]);",
            dim = embed_dim
        ))?;
    }

    // Suppress unused warning when sqlite-vec feature is not enabled
    #[cfg(not(feature = "sqlite-vec"))]
    let _ = embed_dim;

    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn category_to_str(cat: &Category) -> String {
    match cat {
        Category::Core => "Core".to_string(),
        Category::Daily => "Daily".to_string(),
        Category::Conversation => "Conversation".to_string(),
        Category::Global => "Global".to_string(),
        Category::Custom(s) => s.clone(),
    }
}

fn str_to_category(s: &str) -> Category {
    match s {
        "Core" => Category::Core,
        "Daily" => Category::Daily,
        "Conversation" => Category::Conversation,
        "Global" => Category::Global,
        other => Category::Custom(other.to_string()),
    }
}

fn row_to_entry(
    key: String,
    content: String,
    category: String,
    session: Option<String>,
    topic: Option<String>,
) -> MemoryEntry {
    MemoryEntry {
        key,
        content,
        category: str_to_category(&category),
        session,
        topic,
    }
}

// ── Memory trait impl ─────────────────────────────────────────────────────────

#[async_trait]
impl Memory for SqliteMemory {
    fn name(&self) -> &str {
        "sqlite"
    }

    // ── store ────────────────────────────────────────────────────────────────

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: Category,
        session: Option<&str>,
        topic: Option<&str>,
    ) -> Result<()> {
        let cat_str = category_to_str(&category);

        // Compute embedding BEFORE acquiring a connection to minimise hold time.
        let embedding: Option<Vec<f32>> = if let Some(ref embedder) = self.embedder {
            if embedder.dim() > 0 {
                match embedder.embed_one(content).await {
                    Ok(v) => Some(v),
                    Err(e) => {
                        tracing::warn!("Embedding failed for key '{}': {}; skipping vector index", key, e);
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        let conn = self.pool.get().map_err(|e| anyhow!("Pool error: {}", e))?;

        conn.execute(
            "INSERT INTO memory (key, content, category, session, topic)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(key) DO UPDATE SET
                 content    = excluded.content,
                 category   = excluded.category,
                 session    = excluded.session,
                 topic      = excluded.topic,
                 updated_at = strftime('%s','now')",
            params![key, content, cat_str, session, topic],
        )?;

        if let Some(ref emb) = embedding {
            self.upsert_embedding(&conn, key, emb)?;
        }

        Ok(())
    }

    // ── recall ───────────────────────────────────────────────────────────────

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        session: Option<&str>,
        scope: RecallScope,
    ) -> Result<Vec<MemoryEntry>> {
        if scope == RecallScope::Clean {
            return Ok(vec![]);
        }

        let fetch_n = (limit * 3).max(20);

        // ── Path A: hybrid vector + FTS5 + RRF ───────────────────────────────
        #[cfg(feature = "sqlite-vec")]
        if self.has_vector_index() {
            if let Some(ref embedder) = self.embedder {
                match embedder.embed_one(query).await {
                    Ok(q_emb) => {
                        let q_bytes = vec_to_bytes(&q_emb);
                        let conn = self.pool.get().map_err(|e| anyhow!("Pool error: {}", e))?;

                        let vec_keys =
                            Self::vector_search(&conn, &q_bytes, fetch_n, session)
                                .unwrap_or_default();
                        let fts_keys =
                            Self::fts_search(&conn, query, fetch_n, session)
                                .unwrap_or_default();

                        if !vec_keys.is_empty() || !fts_keys.is_empty() {
                            let rrf = rrf_merge(&vec_keys, &fts_keys);
                            let mut top_keys: Vec<String> =
                                rrf.into_iter().map(|r| r.key).collect();
                            top_keys = self.filter_keys_by_scope(&conn, top_keys, &scope)?;
                            let entries = Self::fetch_by_keys(&conn, &top_keys[..limit.min(top_keys.len())])?;
                            return Ok(entries);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Query embedding failed: {}; falling back to FTS5", e);
                    }
                }
            }
        }

        // ── Path B: FTS5-only ─────────────────────────────────────────────────
        {
            let conn = self.pool.get().map_err(|e| anyhow!("Pool error: {}", e))?;
            let fts_keys = Self::fts_search(&conn, query, fetch_n, session).unwrap_or_default();

            if !fts_keys.is_empty() {
                let filtered = self.filter_keys_by_scope(&conn, fts_keys, &scope)?;
                let top_keys: Vec<String> = filtered.into_iter().take(limit).collect();
                return Self::fetch_by_keys(&conn, &top_keys);
            }

            // ── Path C: LIKE scan fallback ────────────────────────────────────
            let like_pat = format!("%{}%", query);
            let sql = if session.is_some() {
                "SELECT key, content, category, session, topic FROM memory
                 WHERE content LIKE ?1 AND session = ?2 LIMIT ?3"
            } else {
                "SELECT key, content, category, session, topic FROM memory
                 WHERE content LIKE ?1 LIMIT ?2"
            };
            let mut stmt = conn.prepare(sql)?;
            let entries: Vec<MemoryEntry> = if let Some(sess) = session {
                stmt.query_map(params![like_pat, sess, fetch_n as i64], |row| {
                    Ok(row_to_entry(
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
            } else {
                stmt.query_map(params![like_pat, fetch_n as i64], |row| {
                    Ok(row_to_entry(
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
            };
            let filtered: Vec<MemoryEntry> = entries
                .into_iter()
                .filter(|e| scope_matches_entry(e, &scope))
                .take(limit)
                .collect();
            Ok(filtered)
        }
    }

    // ── get ──────────────────────────────────────────────────────────────────

    async fn get(&self, key: &str) -> Result<Option<MemoryEntry>> {
        let conn = self.pool.get().map_err(|e| anyhow!("Pool error: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT key, content, category, session, topic FROM memory WHERE key = ?1",
        )?;
        let mut rows = stmt.query(params![key])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_entry(
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            )))
        } else {
            Ok(None)
        }
    }

    // ── list ─────────────────────────────────────────────────────────────────

    async fn list(
        &self,
        category: Option<&Category>,
        session: Option<&str>,
    ) -> Result<Vec<MemoryEntry>> {
        let conn = self.pool.get().map_err(|e| anyhow!("Pool error: {}", e))?;

        let (sql, cat_str) = match (category, session) {
            (Some(cat), Some(_)) => (
                "SELECT key, content, category, session, topic FROM memory
                 WHERE category = ?1 AND session = ?2
                 ORDER BY updated_at DESC",
                Some(category_to_str(cat)),
            ),
            (Some(cat), None) => (
                "SELECT key, content, category, session, topic FROM memory
                 WHERE category = ?1 ORDER BY updated_at DESC",
                Some(category_to_str(cat)),
            ),
            (None, Some(_)) => (
                "SELECT key, content, category, session, topic FROM memory
                 WHERE session = ?1 ORDER BY updated_at DESC",
                None,
            ),
            (None, None) => (
                "SELECT key, content, category, session, topic FROM memory
                 ORDER BY updated_at DESC",
                None,
            ),
        };

        let mut stmt = conn.prepare(sql)?;
        let entries = match (cat_str, session) {
            (Some(cat), Some(sess)) => {
                stmt.query_map(params![cat, sess], |row| {
                    Ok(row_to_entry(
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
            }
            (Some(cat), None) => {
                stmt.query_map(params![cat], |row| {
                    Ok(row_to_entry(
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
            }
            // SQL uses `?1` for session (fixed from the previous `?2` / Null-placeholder
            // style that was confusing to read and inconsistent with other branches).
            (None, Some(sess)) => {
                stmt.query_map(params![sess], |row| {
                    Ok(row_to_entry(
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
            }
            (None, None) => {
                stmt.query_map([], |row| {
                    Ok(row_to_entry(
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
            }
        };

        Ok(entries)
    }

    // ── forget ───────────────────────────────────────────────────────────────

    async fn forget(&self, key: &str) -> Result<bool> {
        let conn = self.pool.get().map_err(|e| anyhow!("Pool error: {}", e))?;
        self.delete_embedding(&conn, key)?;
        let affected = conn.execute("DELETE FROM memory WHERE key = ?1", params![key])?;
        Ok(affected > 0)
    }

    // ── count ────────────────────────────────────────────────────────────────

    async fn count(&self) -> Result<usize> {
        let conn = self.pool.get().map_err(|e| anyhow!("Pool error: {}", e))?;
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM memory", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    // ── health_check ──────────────────────────────────────────────────────────

    async fn health_check(&self) -> bool {
        let conn = match self.pool.get() {
            Ok(c) => c,
            Err(_) => return false,
        };
        conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
            .is_ok()
    }
}

// ── RecallScope helpers ───────────────────────────────────────────────────────

fn scope_matches_entry(entry: &MemoryEntry, scope: &RecallScope) -> bool {
    match scope {
        RecallScope::Clean => false,
        RecallScope::Full => true,
        RecallScope::FactsOnly => entry.category != Category::Conversation,
        RecallScope::CurrentTopic { topic_id } => match entry.category {
            Category::Conversation => entry.topic.as_deref() == Some(topic_id.as_str()),
            _ => true,
        },
    }
}

impl SqliteMemory {
    fn filter_keys_by_scope(
        &self,
        conn: &Connection,
        keys: Vec<String>,
        scope: &RecallScope,
    ) -> Result<Vec<String>> {
        if keys.is_empty() {
            return Ok(keys);
        }
        let placeholders: String = (1..=keys.len())
            .map(|i| format!("?{}", i))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT key, category, topic FROM memory WHERE key IN ({})",
            placeholders
        );
        let mut stmt = conn.prepare(&sql)?;
        let meta: std::collections::HashMap<String, (String, Option<String>)> = stmt
            .query_map(rusqlite::params_from_iter(keys.iter()), |row| {
                Ok((row.get::<_, String>(0)?, (row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?)))
            })?
            .collect::<std::result::Result<_, _>>()?;

        let filtered: Vec<String> = keys
            .into_iter()
            .filter(|k| {
                if let Some((cat_str, topic)) = meta.get(k) {
                    let cat = str_to_category(cat_str);
                    let entry_stub = MemoryEntry {
                        key: k.clone(),
                        content: String::new(),
                        category: cat,
                        session: None,
                        topic: topic.clone(),
                    };
                    scope_matches_entry(&entry_stub, scope)
                } else {
                    false
                }
            })
            .collect();
        Ok(filtered)
    }
}

// ── Memory hygiene ─────────────────────────────────────────────────────────────

impl SqliteMemory {
    /// Delete memory entries older than `ttl_days` for the given category.
    ///
    /// `ttl_days == 0` means "never expire" (skip deletion).
    /// Returns the number of rows deleted.
    ///
    /// Phase 14-F1: Uses `SECONDS_PER_DAY` constant instead of magic literal.
    pub async fn hygiene(&self, category: &Category, ttl_days: u32) -> Result<usize> {
        if ttl_days == 0 {
            return Ok(0);
        }
        let cat_str = category_to_str(category);
        // Phase 14-P3-2: Named constant instead of magic number 86_400
        let cutoff_secs = ttl_days as i64 * SECONDS_PER_DAY;
        let conn = self.pool.get().map_err(|e| anyhow!("Pool error: {}", e))?;

        let keys: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT key FROM memory
                 WHERE category = ?1
                   AND created_at < (strftime('%s','now') - ?2)",
            )?;
            let result: Vec<String> = stmt
                .query_map(params![cat_str, cutoff_secs], |row| row.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            result
        };

        for key in &keys {
            self.delete_embedding(&conn, key)?;
        }

        let n = conn.execute(
            "DELETE FROM memory
             WHERE category = ?1
               AND created_at < (strftime('%s','now') - ?2)",
            params![cat_str, cutoff_secs],
        )?;

        tracing::info!(
            category = %cat_str,
            ttl_days,
            deleted = n,
            "memory_hygiene complete"
        );
        Ok(n)
    }
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_mem() -> SqliteMemory {
        SqliteMemory::new()
    }

    #[tokio::test]
    async fn test_store_and_get() {
        let mem = make_mem();
        mem.store("k1", "hello world", Category::Daily, None, None)
            .await
            .unwrap();
        let entry = mem.get("k1").await.unwrap().unwrap();
        assert_eq!(entry.content, "hello world");
        assert_eq!(entry.category, Category::Daily);
        assert_eq!(entry.topic, None);
    }

    #[tokio::test]
    async fn test_store_with_topic() {
        let mem = make_mem();
        mem.store("k-topic", "content", Category::Conversation, Some("s1"), Some("topic-rust"))
            .await
            .unwrap();
        let entry = mem.get("k-topic").await.unwrap().unwrap();
        assert_eq!(entry.topic, Some("topic-rust".to_string()));
    }

    #[tokio::test]
    async fn test_forget() {
        let mem = make_mem();
        mem.store("k2", "to forget", Category::Core, None, None)
            .await
            .unwrap();
        assert!(mem.forget("k2").await.unwrap());
        assert!(mem.get("k2").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_count() {
        let mem = make_mem();
        assert_eq!(mem.count().await.unwrap(), 0);
        mem.store("a", "aa", Category::Daily, None, None).await.unwrap();
        mem.store("b", "bb", Category::Daily, None, None).await.unwrap();
        assert_eq!(mem.count().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn test_upsert() {
        let mem = make_mem();
        mem.store("k", "v1", Category::Daily, None, None).await.unwrap();
        mem.store("k", "v2", Category::Core, None, Some("t1")).await.unwrap();
        let entry = mem.get("k").await.unwrap().unwrap();
        assert_eq!(entry.content, "v2");
        assert_eq!(entry.category, Category::Core);
        assert_eq!(entry.topic, Some("t1".to_string()));
        assert_eq!(mem.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_fts_recall_full_scope() {
        let mem = make_mem();
        mem.store("doc1", "the quick brown fox", Category::Daily, None, None)
            .await
            .unwrap();
        mem.store("doc2", "lazy dog sleeps", Category::Daily, None, None)
            .await
            .unwrap();
        mem.store("doc3", "fox and hound", Category::Daily, None, None)
            .await
            .unwrap();

        let results = mem.recall("fox", 10, None, RecallScope::Full).await.unwrap();
        assert!(!results.is_empty());
        let keys: Vec<&str> = results.iter().map(|e| e.key.as_str()).collect();
        assert!(keys.contains(&"doc1") || keys.contains(&"doc3"));
    }

    #[tokio::test]
    async fn test_recall_clean_scope_returns_empty() {
        let mem = make_mem();
        mem.store("k1", "core content", Category::Core, None, None)
            .await
            .unwrap();
        let results = mem.recall("content", 10, None, RecallScope::Clean).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_recall_facts_only_excludes_conversation() {
        let mem = make_mem();
        mem.store("core1", "core fact about fox", Category::Core, None, None)
            .await
            .unwrap();
        mem.store("conv1", "conversation about fox", Category::Conversation, Some("s1"), Some("t1"))
            .await
            .unwrap();

        let results = mem.recall("fox", 10, None, RecallScope::FactsOnly).await.unwrap();
        let keys: Vec<&str> = results.iter().map(|e| e.key.as_str()).collect();
        assert!(keys.contains(&"core1"));
        assert!(!keys.contains(&"conv1"));
    }

    #[tokio::test]
    async fn test_recall_current_topic_filters_conversation() {
        let mem = make_mem();
        mem.store("conv-rust", "rust topic fox", Category::Conversation, Some("s1"), Some("topic-rust"))
            .await
            .unwrap();
        mem.store("conv-poem", "poem topic fox", Category::Conversation, Some("s1"), Some("topic-poem"))
            .await
            .unwrap();
        // Core entries are stored with the same session so the session-scoped FTS
        // query can find them; scope_matches_entry then passes them unconditionally
        // (Category::Core is not topic-filtered in CurrentTopic scope).
        mem.store("core1", "core fox fact", Category::Core, Some("s1"), None)
            .await
            .unwrap();

        let scope = RecallScope::CurrentTopic { topic_id: "topic-rust".to_string() };
        let results = mem.recall("fox", 10, Some("s1"), scope).await.unwrap();
        let keys: Vec<&str> = results.iter().map(|e| e.key.as_str()).collect();
        assert!(keys.contains(&"conv-rust"), "rust topic conversation should be included");
        assert!(!keys.contains(&"conv-poem"), "poem topic conversation should be excluded");
        assert!(keys.contains(&"core1"), "core entries always included");
    }

    #[tokio::test]
    async fn test_recall_fallback_like() {
        let mem = make_mem();
        mem.store("k1", "abc123def", Category::Daily, None, None)
            .await
            .unwrap();
        let results = mem.recall("abc123", 10, None, RecallScope::Full).await.unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].key, "k1");
    }

    #[tokio::test]
    async fn test_session_isolation() {
        let mem = make_mem();
        mem.store("s1k1", "session one data", Category::Conversation, Some("s1"), None)
            .await
            .unwrap();
        mem.store("s2k1", "session two data", Category::Conversation, Some("s2"), None)
            .await
            .unwrap();

        let s1_results = mem.recall("data", 10, Some("s1"), RecallScope::Full).await.unwrap();
        assert!(s1_results.iter().all(|e| e.session.as_deref() == Some("s1")));
    }

    #[tokio::test]
    async fn test_list_by_category() {
        let mem = make_mem();
        mem.store("c1", "core fact", Category::Core, None, None)
            .await
            .unwrap();
        mem.store("d1", "daily note", Category::Daily, None, None)
            .await
            .unwrap();

        let core = mem.list(Some(&Category::Core), None).await.unwrap();
        assert_eq!(core.len(), 1);
        assert_eq!(core[0].key, "c1");
    }

    #[tokio::test]
    async fn test_hygiene_noop_when_ttl_zero() {
        let mem = make_mem();
        mem.store("h1", "should stay", Category::Daily, None, None)
            .await
            .unwrap();
        let deleted = mem.hygiene(&Category::Daily, 0).await.unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(mem.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_health_check() {
        let mem = make_mem();
        assert!(mem.health_check().await);
    }

    #[test]
    fn test_seconds_per_day_constant() {
        // Verify the constant matches the expected value (no magic numbers in code).
        assert_eq!(SECONDS_PER_DAY, 86_400, "SECONDS_PER_DAY must equal 86400");
    }
}
