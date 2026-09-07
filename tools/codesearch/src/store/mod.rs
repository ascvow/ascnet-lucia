//! SQLite 存储：已采集源码快照、当前文件版本与索引状态的权威记录。
//!
//! 写入统一走本模块，保证以下不变量：
//!
//! - 代码块激活/失效、FTS 行、符号行与向量删除日志在同一个事务内保持一致；
//! - 解析、文件读取与网络请求都在事务之外完成，写事务只做写库；
//! - `chunks.id` 使用 `AUTOINCREMENT`，删除后不会复用，可安全用作 USearch 向量 key。
//!
//! 只读查询在 [`query`] 子模块中定义。

pub mod query;
pub mod schema;
pub mod types;

use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};
use crate::types::Language;

pub use types::{
    CommitOutcome, FtsText, ImportRecord, PreparedChunk, PreparedFile, SymbolRecord,
};

/// 目录修订号在 `meta` 中的键。
const META_CATALOG_EPOCH: &str = "catalog_epoch";
/// workspace 身份在 `meta` 中的键。
const META_WORKSPACE_ID: &str = "workspace_id";
/// 最近一次扫描摘要（JSON）在 `meta` 中的键。
const META_LAST_SCAN: &str = "last_scan";

/// 文件在目录中的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    /// 正常索引中。
    Active,
    /// 已从磁盘删除。
    Deleted,
    /// 因规则或读取问题被跳过。
    Skipped,
}

impl FileState {
    /// 稳定的字符串标识。
    pub fn as_str(&self) -> &'static str {
        match self {
            FileState::Active => "active",
            FileState::Deleted => "deleted",
            FileState::Skipped => "skipped",
        }
    }

    /// 由数据库字符串还原。
    pub fn from_str_opt(value: &str) -> Option<Self> {
        match value {
            "active" => Some(FileState::Active),
            "deleted" => Some(FileState::Deleted),
            "skipped" => Some(FileState::Skipped),
            _ => None,
        }
    }
}

/// 目录中已记录的文件状态，用于扫描时判断是否需要重新解析。
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    /// 文件 ID。
    pub id: i64,
    /// 当前状态。
    pub state: FileState,
    /// 当前版本内容哈希；无当前版本时为 `None`。
    pub file_hash: Option<String>,
    /// 记录的大小。
    pub size: i64,
    /// 记录的修改时间（纳秒）。
    pub mtime_ns: i64,
}

/// 等待生成 Embedding 的代码块。
#[derive(Debug, Clone)]
pub struct PendingEmbedding {
    /// 代码块 ID。
    pub chunk_id: i64,
    /// 发送给模型的完整文档输入。
    pub input: String,
    /// 已尝试次数。
    pub attempts: u32,
}

/// 向量更新日志中的一条操作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorOp {
    /// 顺序号。
    pub seq: i64,
    /// `add` 或 `remove`。
    pub op: String,
    /// 向量 key（等于 `chunks.id`）。
    pub vector_key: i64,
    /// `add` 操作对应的缓存键。
    pub cache_key: Option<String>,
}

/// Embedding 空间身份记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingSpaceRow {
    /// 空间身份哈希。
    pub id: String,
    /// Provider 类型。
    pub provider: String,
    /// 模型名。
    pub model: String,
    /// 缓存命名空间。
    pub cache_namespace: String,
    /// 维度。
    pub dimensions: usize,
    /// 距离度量。
    pub metric: String,
    /// 归一化规则。
    pub normalize: String,
    /// 查询前缀。
    pub query_prefix: String,
    /// 文档前缀。
    pub document_prefix: String,
    /// 输入模板版本。
    pub template_version: u32,
}

/// SQLite 存储句柄。
///
/// 单个连接，写事务短小。同一进程内的并发访问由外层 `Mutex` 串行化。
#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

impl Store {
    /// 打开或创建数据库并执行迁移。
    ///
    /// 启用 WAL、外键约束与 busy timeout。`read_only` 为真时不申请写权限也不迁移。
    pub fn open(path: &Path, read_only: bool) -> Result<Self> {
        let conn = if read_only {
            Connection::open_with_flags(
                path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                    | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?
        } else {
            Connection::open(path)?
        };
        Self::configure(&conn, read_only)?;
        if !read_only {
            schema::migrate(&conn)?;
        }
        Ok(Self { conn })
    }

    /// 创建内存数据库（测试与评测使用）。
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::configure(&conn, false)?;
        schema::migrate(&conn)?;
        Ok(Self { conn })
    }

    fn configure(conn: &Connection, read_only: bool) -> Result<()> {
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        if !read_only {
            // WAL 允许读写并发；WAL 下 NORMAL 同步级别仍能保证已提交事务不丢失。
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
        }
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        Ok(())
    }

    /// 底层连接。只读查询模块需要构造自己的语句，故在 crate 内公开。
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// 当前 schema 版本。
    pub fn schema_version(&self) -> Result<i64> {
        Ok(self.conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    /// 读取 `meta` 中的字符串值。
    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    /// 写入 `meta` 中的字符串值。
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// 绑定并校验 workspace 身份，防止把索引目录挪到别的仓库后混用。
    pub fn bind_workspace(&self, workspace_id: &str) -> Result<()> {
        match self.meta(META_WORKSPACE_ID)? {
            Some(existing) if existing != workspace_id => Err(Error::Config(format!(
                "索引数据属于 workspace {existing}，与当前 {workspace_id} 不匹配；请更换数据目录或重建索引"
            ))),
            Some(_) => Ok(()),
            None => self.set_meta(META_WORKSPACE_ID, workspace_id),
        }
    }

    /// 当前目录修订号。每次提交目录状态变更都会递增。
    pub fn catalog_epoch(&self) -> Result<i64> {
        Ok(self
            .meta(META_CATALOG_EPOCH)?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0))
    }

    /// 记录最近一次扫描摘要（JSON 字符串）。
    pub fn set_last_scan(&self, json: &str) -> Result<()> {
        self.set_meta(META_LAST_SCAN, json)
    }

    /// 读取最近一次扫描摘要。
    pub fn last_scan(&self) -> Result<Option<String>> {
        self.meta(META_LAST_SCAN)
    }

    /// 读取整个文件目录，供扫描时做差异比较。
    pub fn catalog(&self) -> Result<HashMap<String, CatalogEntry>> {
        let mut statement = self.conn.prepare(
            "SELECT f.path, f.id, f.state, r.file_hash, f.size, f.mtime_ns
             FROM files f
             LEFT JOIN file_revisions r ON r.id = f.current_revision_id",
        )?;
        let rows = statement.query_map([], |row| {
            let path: String = row.get(0)?;
            let state: String = row.get(2)?;
            Ok((
                path,
                CatalogEntry {
                    id: row.get(1)?,
                    state: FileState::from_str_opt(&state).unwrap_or(FileState::Skipped),
                    file_hash: row.get(3)?,
                    size: row.get(4)?,
                    mtime_ns: row.get(5)?,
                },
            ))
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let (path, entry) = row?;
            map.insert(path, entry);
        }
        Ok(map)
    }

    /// 提交一次文件更新：写入新版本与代码块，失效旧版本，并记录向量删除操作。
    ///
    /// 单个事务内完成：
    ///
    /// 1. 记录旧的当前代码块的向量删除操作；
    /// 2. 清理旧代码块的 FTS 行与符号行，并置 `is_current = 0`；
    /// 3. 写入（或复用）文件版本快照；
    /// 4. 插入新代码块、FTS 行与符号行，状态为 `pending`；
    /// 5. 递增目录修订号。
    ///
    /// 内容回退到某个历史版本时复用已有的 `file_revisions` 行（内容相同），
    /// 但仍然插入一批新的代码块：新块的 `embed_input` 与旧块一致，
    /// 因此 Embedding 缓存必定命中，不会重新请求模型。
    pub fn commit_file(&mut self, prepared: &PreparedFile) -> Result<CommitOutcome> {
        let now = now_secs();
        let tx = self.conn.transaction()?;

        let file_id = upsert_file(&tx, &prepared.rel_path, prepared.language, now)?;
        deactivate_current_chunks(&tx, file_id, now)?;

        let existing_revision: Option<i64> = tx
            .query_row(
                "SELECT id FROM file_revisions WHERE file_id = ?1 AND file_hash = ?2",
                params![file_id, prepared.file_hash],
                |row| row.get(0),
            )
            .optional()?;
        let reused_revision = existing_revision.is_some();
        let revision_id = match existing_revision {
            Some(id) => id,
            None => {
                tx.execute(
                    "INSERT INTO file_revisions(file_id, file_hash, size, line_count, content, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        file_id,
                        prepared.file_hash,
                        prepared.size as i64,
                        prepared.line_count as i64,
                        prepared.content,
                        now
                    ],
                )?;
                tx.last_insert_rowid()
            }
        };

        let mut chunk_ids = Vec::with_capacity(prepared.chunks.len());
        for chunk in &prepared.chunks {
            chunk_ids.push(insert_chunk(&tx, file_id, revision_id, chunk)?);
        }

        tx.execute("DELETE FROM imports WHERE file_id = ?1", params![file_id])?;
        for import in &prepared.imports {
            tx.execute(
                "INSERT INTO imports(file_id, revision_id, raw, module) VALUES (?1, ?2, ?3, ?4)",
                params![file_id, revision_id, import.raw, import.module],
            )?;
        }

        tx.execute(
            "UPDATE files
             SET state = 'active', current_revision_id = ?2, language = ?3,
                 size = ?4, mtime_ns = ?5, skip_reason = NULL, skip_detail = NULL, updated_at = ?6
             WHERE id = ?1",
            params![
                file_id,
                revision_id,
                prepared.language.as_str(),
                prepared.size as i64,
                prepared.mtime_ns,
                now
            ],
        )?;
        bump_epoch(&tx)?;
        tx.commit()?;

        Ok(CommitOutcome {
            file_id,
            revision_id,
            chunk_ids,
            reused_revision,
            cache_hits: 0,
        })
    }

    /// 刷新采集元数据（大小与修改时间），不改变版本与代码块。
    ///
    /// 内容 Hash 未变但时间戳变化时调用，避免下一次扫描重复读取同一个文件。
    /// 这不是目录状态变更，因此不递增目录修订号。
    pub fn touch_file(&self, path: &str, size: u64, mtime_ns: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET size = ?2, mtime_ns = ?3 WHERE path = ?1",
            params![path, size as i64, mtime_ns],
        )?;
        Ok(())
    }

    /// 将文件标记为已删除：失效其全部当前代码块并记录向量删除操作。
    ///
    /// 历史版本与代码块保留，旧的 `code_ref` 仍可读取到当时的源码。
    pub fn mark_deleted(&mut self, path: &str) -> Result<bool> {
        self.mark_inactive(path, FileState::Deleted, None, None)
    }

    /// 将文件标记为跳过并记录可观察的原因。
    pub fn mark_skipped(
        &mut self,
        path: &str,
        language: Language,
        reason: &str,
        detail: &str,
    ) -> Result<()> {
        let existed = self.mark_inactive(path, FileState::Skipped, Some(reason), Some(detail))?;
        if !existed {
            let now = now_secs();
            self.conn.execute(
                "INSERT INTO files(path, language, state, skip_reason, skip_detail, updated_at)
                 VALUES (?1, ?2, 'skipped', ?3, ?4, ?5)
                 ON CONFLICT(path) DO UPDATE SET
                     state = 'skipped', skip_reason = excluded.skip_reason,
                     skip_detail = excluded.skip_detail, updated_at = excluded.updated_at",
                params![path, language.as_str(), reason, detail, now],
            )?;
        }
        Ok(())
    }

    fn mark_inactive(
        &mut self,
        path: &str,
        state: FileState,
        reason: Option<&str>,
        detail: Option<&str>,
    ) -> Result<bool> {
        let now = now_secs();
        let tx = self.conn.transaction()?;
        let file_id: Option<i64> = tx
            .query_row("SELECT id FROM files WHERE path = ?1", params![path], |row| {
                row.get(0)
            })
            .optional()?;
        let Some(file_id) = file_id else {
            tx.commit()?;
            return Ok(false);
        };
        deactivate_current_chunks(&tx, file_id, now)?;
        tx.execute(
            "UPDATE files
             SET state = ?2, current_revision_id = NULL, skip_reason = ?3,
                 skip_detail = ?4, updated_at = ?5
             WHERE id = ?1",
            params![file_id, state.as_str(), reason, detail, now],
        )?;
        bump_epoch(&tx)?;
        tx.commit()?;
        Ok(true)
    }

    /// 注册（或确认）一个 Embedding 空间。
    ///
    /// 同一 `id` 的记录若与传入身份不一致则报错，避免同维度不同模型的向量混用。
    pub fn ensure_embedding_space(&self, space: &EmbeddingSpaceRow) -> Result<()> {
        match self.embedding_space(&space.id)? {
            Some(found) if &found != space => Err(Error::Internal(format!(
                "Embedding 空间 {} 的身份与已存记录冲突：模型或维度不同却复用了同一 id",
                space.id
            ))),
            Some(_) => Ok(()),
            None => {
                self.conn.execute(
                    "INSERT INTO embedding_spaces(id, provider, model, cache_namespace, dimensions,
                                                  metric, normalize, query_prefix, document_prefix,
                                                  template_version, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    params![
                        space.id,
                        space.provider,
                        space.model,
                        space.cache_namespace,
                        space.dimensions as i64,
                        space.metric,
                        space.normalize,
                        space.query_prefix,
                        space.document_prefix,
                        space.template_version as i64,
                        now_secs()
                    ],
                )?;
                Ok(())
            }
        }
    }

    /// 取出一批等待 Embedding 的当前代码块。
    pub fn pending_embeddings(
        &self,
        limit: usize,
        max_attempts: u32,
    ) -> Result<Vec<PendingEmbedding>> {
        let mut statement = self.conn.prepare(
            "SELECT id, embed_input, embed_attempts
             FROM chunks
             WHERE is_current = 1 AND embed_state = 'pending' AND embed_attempts < ?2
             ORDER BY id
             LIMIT ?1",
        )?;
        let rows = statement.query_map(params![limit as i64, max_attempts as i64], |row| {
            Ok(PendingEmbedding {
                chunk_id: row.get(0)?,
                input: row.get(1)?,
                attempts: row.get::<_, i64>(2)? as u32,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 读取单个缓存向量。
    pub fn cached_vector(&self, space_id: &str, cache_key: &str) -> Result<Option<Vec<f32>>> {
        let row: Option<(Vec<u8>, i64)> = self
            .conn
            .query_row(
                "SELECT vector, dimensions FROM embedding_cache
                 WHERE cache_key = ?1 AND space_id = ?2",
                params![cache_key, space_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match row {
            Some((blob, dimensions)) => Ok(Some(decode_vector(&blob, dimensions as usize)?)),
            None => Ok(None),
        }
    }

    /// 写入 Embedding 缓存。已存在时保持原值。
    pub fn put_cached_vector(&self, space_id: &str, cache_key: &str, vector: &[f32]) -> Result<()> {
        self.conn.execute(
            "INSERT INTO embedding_cache(cache_key, space_id, dimensions, vector, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(cache_key) DO NOTHING",
            params![
                cache_key,
                space_id,
                vector.len() as i64,
                encode_vector(vector),
                now_secs()
            ],
        )?;
        Ok(())
    }

    /// 把已生成的向量关联到代码块，并登记一次向量添加操作。
    ///
    /// 关联前重新检查代码块是否仍是当前有效版本：已失效时返回 `false`，
    /// 既不重新激活旧代码，也不产生向量添加操作。
    /// 因此模型请求乱序返回时，晚到的旧结果无法覆盖新代码。
    pub fn attach_embedding(
        &mut self,
        chunk_id: i64,
        space_id: &str,
        cache_key: &str,
    ) -> Result<bool> {
        let tx = self.conn.transaction()?;
        let is_current: Option<i64> = tx
            .query_row(
                "SELECT is_current FROM chunks WHERE id = ?1",
                params![chunk_id],
                |row| row.get(0),
            )
            .optional()?;
        if !matches!(is_current, Some(1)) {
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            "UPDATE chunks
             SET embed_state = 'ready', embedding_space_id = ?2, cache_key = ?3, embed_error = NULL
             WHERE id = ?1",
            params![chunk_id, space_id, cache_key],
        )?;
        append_vector_op(&tx, space_id, "add", chunk_id, Some(cache_key), now_secs())?;
        tx.commit()?;
        Ok(true)
    }

    /// 记录一次 Embedding 失败；达到尝试上限后标记为 `failed` 不再重试。
    ///
    /// `message` 必须已由 Provider 层清洗，不含凭证。
    pub fn record_embedding_failure(
        &self,
        chunk_id: i64,
        message: &str,
        max_attempts: u32,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE chunks
             SET embed_attempts = embed_attempts + 1,
                 embed_error = ?2,
                 embed_state = CASE WHEN embed_attempts + 1 >= ?3 THEN 'failed' ELSE 'pending' END
             WHERE id = ?1",
            params![chunk_id, message, max_attempts as i64],
        )?;
        Ok(())
    }

    /// 把当前所有 `failed` 的代码块重置为 `pending`。
    pub fn reset_failed_embeddings(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE chunks SET embed_state = 'pending', embed_attempts = 0, embed_error = NULL
             WHERE is_current = 1 AND embed_state = 'failed'",
            [],
        )?)
    }

    /// 把不属于目标空间的当前代码块重新标记为待嵌入，并删除其旧向量。
    ///
    /// 模型空间变化时调用，保证不会复用别的模型产生的向量。
    pub fn migrate_to_space(&mut self, space_id: &str) -> Result<usize> {
        let now = now_secs();
        let tx = self.conn.transaction()?;
        let stale: Vec<(i64, String)> = {
            let mut statement = tx.prepare(
                "SELECT id, embedding_space_id FROM chunks
                 WHERE is_current = 1 AND embed_state = 'ready'
                   AND embedding_space_id IS NOT NULL AND embedding_space_id <> ?1",
            )?;
            let rows = statement
                .query_map(params![space_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        for (chunk_id, old_space) in &stale {
            append_vector_op(&tx, old_space, "remove", *chunk_id, None, now)?;
        }
        let changed = tx.execute(
            "UPDATE chunks
             SET embed_state = 'pending', embed_attempts = 0, embed_error = NULL,
                 embedding_space_id = NULL, cache_key = NULL
             WHERE is_current = 1 AND embed_state <> 'pending'
               AND (embedding_space_id IS NULL OR embedding_space_id <> ?1)",
            params![space_id],
        )?;
        tx.commit()?;
        Ok(changed)
    }

    /// 读取更新日志中序号大于 `after_seq` 的操作，按序号升序。
    pub fn vector_ops_since(
        &self,
        space_id: &str,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<VectorOp>> {
        let mut statement = self.conn.prepare(
            "SELECT seq, op, vector_key, cache_key FROM vector_ops
             WHERE space_id = ?1 AND seq > ?2
             ORDER BY seq
             LIMIT ?3",
        )?;
        let rows = statement.query_map(params![space_id, after_seq, limit as i64], |row| {
            Ok(VectorOp {
                seq: row.get(0)?,
                op: row.get(1)?,
                vector_key: row.get(2)?,
                cache_key: row.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 更新日志中该空间的最大序号。
    pub fn max_vector_seq(&self, space_id: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM vector_ops WHERE space_id = ?1",
            params![space_id],
            |row| row.get(0),
        )?)
    }

    /// 该空间下尚未应用的操作数量。
    pub fn vector_ops_backlog(&self, space_id: &str, applied_seq: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM vector_ops WHERE space_id = ?1 AND seq > ?2",
            params![space_id, applied_seq],
            |row| row.get(0),
        )?)
    }

    /// 列出当前所有已就绪、可用于重建向量索引的代码块及其向量。
    ///
    /// 用于索引文件缺失或损坏后从 SQLite 恢复，全程不请求模型。
    pub fn ready_vectors(&self, space_id: &str) -> Result<Vec<(i64, Vec<f32>)>> {
        let mut statement = self.conn.prepare(
            "SELECT c.id, e.vector, e.dimensions FROM chunks c
             JOIN embedding_cache e ON e.cache_key = c.cache_key AND e.space_id = c.embedding_space_id
             WHERE c.is_current = 1 AND c.embed_state = 'ready' AND c.embedding_space_id = ?1
             ORDER BY c.id",
        )?;
        let rows = statement.query_map(params![space_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)? as usize,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, blob, dimensions) = row?;
            out.push((id, decode_vector(&blob, dimensions)?));
        }
        Ok(out)
    }
}

fn upsert_file(
    tx: &rusqlite::Transaction<'_>,
    path: &str,
    language: Language,
    now: i64,
) -> Result<i64> {
    tx.execute(
        "INSERT INTO files(path, language, state, updated_at) VALUES (?1, ?2, 'active', ?3)
         ON CONFLICT(path) DO UPDATE SET language = excluded.language, updated_at = excluded.updated_at",
        params![path, language.as_str(), now],
    )?;
    Ok(tx.query_row("SELECT id FROM files WHERE path = ?1", params![path], |row| {
        row.get(0)
    })?)
}

/// 失效文件的全部当前代码块，同步清理 FTS 与符号，并记录向量删除操作。
///
/// 删除 FTS 行是必须的：否则旧版本的拆词文本会继续作为当前代码被检索到。
fn deactivate_current_chunks(
    tx: &rusqlite::Transaction<'_>,
    file_id: i64,
    now: i64,
) -> Result<()> {
    let ready: Vec<(i64, String)> = {
        let mut statement = tx.prepare(
            "SELECT id, embedding_space_id FROM chunks
             WHERE file_id = ?1 AND is_current = 1 AND embed_state = 'ready'
               AND embedding_space_id IS NOT NULL",
        )?;
        let rows = statement.query_map(params![file_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    // 无论是否为当前空间都要登记删除，避免切换模型后旧向量残留在索引里。
    for (chunk_id, chunk_space) in ready {
        append_vector_op(tx, &chunk_space, "remove", chunk_id, None, now)?;
    }
    tx.execute(
        "DELETE FROM chunk_fts WHERE rowid IN
            (SELECT id FROM chunks WHERE file_id = ?1 AND is_current = 1)",
        params![file_id],
    )?;
    tx.execute(
        "DELETE FROM symbols WHERE chunk_id IN
            (SELECT id FROM chunks WHERE file_id = ?1 AND is_current = 1)",
        params![file_id],
    )?;
    tx.execute(
        "UPDATE chunks SET is_current = 0 WHERE file_id = ?1 AND is_current = 1",
        params![file_id],
    )?;
    Ok(())
}

/// 插入一个代码块及其 FTS 行与符号行，返回新的 `chunk_version_id`。
fn insert_chunk(
    tx: &rusqlite::Transaction<'_>,
    file_id: i64,
    revision_id: i64,
    chunk: &PreparedChunk,
) -> Result<i64> {
    tx.execute(
        "INSERT INTO chunks(revision_id, file_id, is_current, ordinal, kind, symbol, symbol_key,
                            parent_symbol, module_path, signature, doc, start_byte, end_byte,
                            start_line, end_line, token_estimate, content_hash, embed_input,
                            embed_input_truncated, embed_state)
         VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, 'pending')",
        params![
            revision_id,
            file_id,
            chunk.ordinal as i64,
            chunk.kind.as_str(),
            chunk.symbol,
            chunk.symbol_key,
            chunk.parent_symbol,
            chunk.module_path,
            chunk.signature,
            chunk.doc,
            chunk.start_byte as i64,
            chunk.end_byte as i64,
            chunk.start_line as i64,
            chunk.end_line as i64,
            chunk.token_estimate as i64,
            chunk.content_hash,
            chunk.embed_input,
            i64::from(chunk.embed_input_truncated),
        ],
    )?;
    let chunk_id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO chunk_fts(rowid, path_terms, symbol_terms, doc_terms, code_terms)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            chunk_id,
            chunk.fts.path_terms,
            chunk.fts.symbol_terms,
            chunk.fts.doc_terms,
            chunk.fts.code_terms
        ],
    )?;
    // 每个符号只登记一行：`name` 支持简单名查询，`qualified` 支持 `Type::method` 精确查询，
    // 两者都建了索引，因此无需额外插入冗余行（否则限定名查询会重复命中同一代码块）。
    for symbol in &chunk.symbols {
        tx.execute(
            "INSERT INTO symbols(chunk_id, file_id, name, name_lower, qualified, qualified_lower, kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                chunk_id,
                file_id,
                symbol.name,
                symbol.name.to_lowercase(),
                symbol.qualified,
                symbol.qualified.to_lowercase(),
                symbol.kind.as_str()
            ],
        )?;
    }
    Ok(chunk_id)
}

/// 追加一条向量更新日志。`space_id` 为空表示该块从未产生向量，无需登记。
fn append_vector_op(
    tx: &rusqlite::Transaction<'_>,
    space_id: &str,
    op: &str,
    vector_key: i64,
    cache_key: Option<&str>,
    now: i64,
) -> Result<()> {
    if space_id.is_empty() {
        return Ok(());
    }
    tx.execute(
        "INSERT INTO vector_ops(space_id, op, vector_key, cache_key, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![space_id, op, vector_key, cache_key, now],
    )?;
    Ok(())
}

fn bump_epoch(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute(
        "INSERT INTO meta(key, value) VALUES (?1, '1')
         ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(meta.value AS INTEGER) + 1 AS TEXT)",
        params![META_CATALOG_EPOCH],
    )?;
    Ok(())
}

/// 统计文本行数：以 `\n` 分隔，末尾无换行时最后一段也算一行。
pub fn count_lines(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    let newlines = text.matches('\n').count();
    let extra = usize::from(!text.ends_with('\n'));
    u32::try_from(newlines + extra).unwrap_or(u32::MAX)
}

/// 当前 UNIX 秒。
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// 把 f32 向量编码为小端字节串。
pub fn encode_vector(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// 从小端字节串解码 f32 向量，并校验长度与声明的维度一致。
///
/// 维度不符说明缓存记录已损坏，此时返回错误而不是交出长度错误的向量。
pub fn decode_vector(bytes: &[u8], dimensions: usize) -> Result<Vec<f32>> {
    if bytes.len() != dimensions * 4 {
        return Err(Error::Internal(format!(
            "缓存向量字节数 {} 与维度 {dimensions} 不匹配",
            bytes.len()
        )));
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect())
}

/// 把 `chunks.id` 转换为 USearch 向量 key。
///
/// SQLite 的 `INTEGER PRIMARY KEY AUTOINCREMENT` 是有符号 64 位且从 1 递增，
/// 而 USearch key 是 `u64`。这里显式校验，不允许截断或溢出。
pub fn vector_key(chunk_id: i64) -> Result<u64> {
    u64::try_from(chunk_id)
        .map_err(|_| Error::Vector(format!("代码块 ID {chunk_id} 无法作为向量 key")))
}

/// 把 USearch 向量 key 还原为 `chunks.id`。
pub fn chunk_id_from_key(key: u64) -> Result<i64> {
    i64::try_from(key).map_err(|_| Error::Vector(format!("向量 key {key} 超出代码块 ID 范围")))
}

#[cfg(test)]
mod tests;
