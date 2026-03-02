//! `SessionStore` — 对话历史持久化存储
//!
//! ## 设计原则（对标 Moltis `moltis-sessions`）
//!
//! **Session ≠ Memory**。两者是独立系统：
//!
//! | 系统          | 用途                               | 检索方式        |
//! |-------------|-----------------------------------|----------------|
//! | `SessionStore`| 按顺序存放 `(session_id, role, content)` | 顺序 LIMIT/OFFSET，不走 RRF |
//! | `Memory`     | 用户明确保留的知识 + Agent 归纳的摘要     | 向量/FTS 检索 + RRF |
//!
//! 对话历史写入 `SessionStore` 而非 `Category::Conversation` Memory，
//! 避免大量对话碎片污染语义检索结果。
//!
//! ## SQLite 表结构
//!
//! ```sql
//! CREATE TABLE sessions (
//!     id          INTEGER PRIMARY KEY AUTOINCREMENT,
//!     session_id  TEXT    NOT NULL,
//!     role        TEXT    NOT NULL,  -- "user" | "assistant" | "system" | "tool"
//!     content     TEXT    NOT NULL,
//!     created_at  INTEGER NOT NULL DEFAULT (strftime('%s','now'))
//! );
//! CREATE INDEX idx_sessions_session_id ON sessions(session_id, id);
//! ```

use anyhow::{Result, anyhow};
use rusqlite::{Connection, params};
use std::sync::{Arc, Mutex};

// ── SessionEntry ──────────────────────────────────────────────────────────────

/// 单条对话历史记录。
#[derive(Debug, Clone)]
pub struct SessionEntry {
    /// 自增主键（可用于分页）。
    pub id: i64,
    /// Session 标识符（如 `telegram:12345678`）。
    pub session_id: String,
    /// 消息角色：`"user"` / `"assistant"` / `"system"` / `"tool"`。
    pub role: String,
    /// 消息正文。
    pub content: String,
    /// Unix 时间戳（秒）。
    pub created_at: i64,
}

// ── SessionStore ──────────────────────────────────────────────────────────────

/// 对话历史持久化存储。
///
/// 使用 SQLite 顺序追加写入，通过 `LIMIT` 高效加载最新 N 轮历史。
/// 不走向量/FTS 检索——读取时按 `id ASC` 保证时序。
///
/// # 线程安全
///
/// 内部用 `Arc<Mutex<Connection>>` 保护，允许跨线程（或跨 `tokio::spawn`）共享。
/// 写操作量少（每轮对话 2 条），`Mutex` 争用极低。
pub struct SessionStore {
    conn: Arc<Mutex<Connection>>,
}

impl SessionStore {
    /// 打开（或创建）session 数据库。
    ///
    /// `path` 可以是文件路径或 `":memory:"`（测试用）。
    pub fn new(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;

             CREATE TABLE IF NOT EXISTS sessions (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id  TEXT    NOT NULL,
                 role        TEXT    NOT NULL,
                 content     TEXT    NOT NULL,
                 created_at  INTEGER NOT NULL DEFAULT (strftime('%s','now'))
             );

             -- Fast lookup by session_id ordered by insertion time
             CREATE INDEX IF NOT EXISTS idx_sessions_session_id
                 ON sessions(session_id, id);
            ",
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// 创建内存数据库（适用于测试 / 无需持久化的场景）。
    pub fn new_in_memory() -> Result<Self> {
        Self::new(":memory:")
    }

    // ── Write operations ──────────────────────────────────────────────────────

    /// 追加一条对话记录。
    ///
    /// `role` 建议使用 `"user"` / `"assistant"` / `"system"` / `"tool"`。
    pub async fn append(&self, session_id: &str, role: &str, content: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow!("lock error: {}", e))?;
        conn.execute(
            "INSERT INTO sessions (session_id, role, content) VALUES (?1, ?2, ?3)",
            params![session_id, role, content],
        )?;
        Ok(())
    }

    /// 将 session 历史替换为一条摘要（用于滚动压缩）。
    ///
    /// 删除该 session 的所有记录，然后插入一条 `role = "system"` 的摘要条目。
    /// 调用方应确保摘要已经生成完毕再调用此方法。
    pub async fn compact(&self, session_id: &str, summary: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow!("lock error: {}", e))?;
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        conn.execute(
            "DELETE FROM sessions WHERE session_id = ?1",
            params![session_id],
        )?;
        conn.execute(
            "INSERT INTO sessions (session_id, role, content) VALUES (?1, 'system', ?2)",
            params![session_id, summary],
        )?;
        conn.execute_batch("COMMIT;")?;
        Ok(())
    }

    /// 清除指定 session 的全部历史记录。
    pub async fn clear(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow!("lock error: {}", e))?;
        conn.execute(
            "DELETE FROM sessions WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    // ── Read operations ───────────────────────────────────────────────────────

    /// 加载 session 的最新 `limit` 条记录（按时序升序）。
    ///
    /// 使用子查询取末尾 N 条再正序返回，保证时序正确。
    pub async fn load(&self, session_id: &str, limit: usize) -> Result<Vec<SessionEntry>> {
        let conn = self.conn.lock().map_err(|e| anyhow!("lock error: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, role, content, created_at
             FROM (
                 SELECT id, session_id, role, content, created_at
                 FROM sessions
                 WHERE session_id = ?1
                 ORDER BY id DESC
                 LIMIT ?2
             ) sub
             ORDER BY id ASC",
        )?;
        let entries = stmt
            .query_map(params![session_id, limit as i64], |row| {
                Ok(SessionEntry {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    role: row.get(2)?,
                    content: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(entries)
    }

    /// 返回 session 的消息总条数。
    pub async fn count(&self, session_id: &str) -> Result<usize> {
        let conn = self.conn.lock().map_err(|e| anyhow!("lock error: {}", e))?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(n as usize)
    }

    /// 列出所有活跃的 session ID（有记录的 session，去重）。
    pub async fn list_sessions(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().map_err(|e| anyhow!("lock error: {}", e))?;
        let mut stmt =
            conn.prepare("SELECT DISTINCT session_id FROM sessions ORDER BY session_id")?;
        let ids = stmt
            .query_map([], |row| row.get(0))?
            .collect::<std::result::Result<Vec<String>, _>>()?;
        Ok(ids)
    }
}

// ── 单元测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    async fn make_store() -> SessionStore {
        SessionStore::new_in_memory().expect("in-memory store must succeed")
    }

    #[tokio::test]
    async fn test_append_and_load_ordered() {
        let store = make_store().await;
        store.append("sess1", "user", "hello").await.unwrap();
        store.append("sess1", "assistant", "hi").await.unwrap();
        store.append("sess1", "user", "how are you?").await.unwrap();

        let entries = store.load("sess1", 10).await.unwrap();
        assert_eq!(entries.len(), 3, "should have 3 entries");
        assert_eq!(entries[0].role, "user");
        assert_eq!(entries[0].content, "hello");
        assert_eq!(entries[1].role, "assistant");
        assert_eq!(entries[2].content, "how are you?");
    }

    #[tokio::test]
    async fn test_load_limit_returns_latest() {
        let store = make_store().await;
        for i in 0..10 {
            store
                .append("sess2", "user", &format!("msg {}", i))
                .await
                .unwrap();
        }

        let entries = store.load("sess2", 3).await.unwrap();
        assert_eq!(entries.len(), 3, "limit=3 should return last 3");
        // Should be the LAST 3 in ascending order: msg 7, 8, 9
        assert_eq!(entries[0].content, "msg 7");
        assert_eq!(entries[1].content, "msg 8");
        assert_eq!(entries[2].content, "msg 9");
    }

    #[tokio::test]
    async fn test_session_isolation() {
        let store = make_store().await;
        store
            .append("alice", "user", "alice message")
            .await
            .unwrap();
        store.append("bob", "user", "bob message").await.unwrap();

        let alice = store.load("alice", 10).await.unwrap();
        let bob = store.load("bob", 10).await.unwrap();
        assert_eq!(alice.len(), 1);
        assert_eq!(bob.len(), 1);
        assert_eq!(alice[0].content, "alice message");
        assert_eq!(bob[0].content, "bob message");
    }

    #[tokio::test]
    async fn test_count() {
        let store = make_store().await;
        assert_eq!(store.count("s").await.unwrap(), 0);
        store.append("s", "user", "a").await.unwrap();
        store.append("s", "assistant", "b").await.unwrap();
        assert_eq!(store.count("s").await.unwrap(), 2);
    }

    #[tokio::test]
    async fn test_clear() {
        let store = make_store().await;
        store.append("s", "user", "msg").await.unwrap();
        store.clear("s").await.unwrap();
        assert_eq!(store.count("s").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_compact_replaces_history_with_summary() {
        let store = make_store().await;
        store.append("s", "user", "msg 1").await.unwrap();
        store.append("s", "assistant", "reply 1").await.unwrap();
        store.append("s", "user", "msg 2").await.unwrap();

        store
            .compact("s", "Summary of conversation so far.")
            .await
            .unwrap();

        let entries = store.load("s", 10).await.unwrap();
        assert_eq!(
            entries.len(),
            1,
            "compact should leave exactly one summary entry"
        );
        assert_eq!(entries[0].role, "system");
        assert_eq!(entries[0].content, "Summary of conversation so far.");
    }

    #[tokio::test]
    async fn test_list_sessions() {
        let store = make_store().await;
        store.append("alice", "user", "hi").await.unwrap();
        store.append("bob", "user", "hello").await.unwrap();
        store.append("alice", "assistant", "hey").await.unwrap();

        let sessions = store.list_sessions().await.unwrap();
        assert!(sessions.contains(&"alice".to_string()));
        assert!(sessions.contains(&"bob".to_string()));
        assert_eq!(sessions.len(), 2, "only 2 unique sessions");
    }

    #[tokio::test]
    async fn test_load_empty_session() {
        let store = make_store().await;
        let entries = store.load("nonexistent", 10).await.unwrap();
        assert!(entries.is_empty());
    }
}
