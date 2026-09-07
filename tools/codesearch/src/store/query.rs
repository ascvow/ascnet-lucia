//! 检索侧的只读查询。
//!
//! 所有 FTS 表达式都由调用方用受控词条构造，用户原始查询绝不直接拼接进 SQL 或 FTS 语法。

use rusqlite::{params, params_from_iter, OptionalExtension};

use crate::error::{Error, Result};
use crate::store::Store;
use crate::types::{ChunkKind, Language};

/// 代码块的完整元数据行。源码内容不在这里，必须由 [`Store::chunk_content`] 从同一版本切片，
/// 保证代码、行号与 `file_hash` 来自同一个版本。
#[derive(Debug, Clone)]
pub struct ChunkRow {
    /// 代码块 ID（chunk_version_id，同时是向量 key）。
    pub id: i64,
    /// 文件 ID。
    pub file_id: i64,
    /// 版本 ID。
    pub revision_id: i64,
    /// 相对路径。
    pub path: String,
    /// 语言。
    pub language: Language,
    /// 代码块类别。
    pub kind: ChunkKind,
    /// 限定符号名。
    pub symbol: Option<String>,
    /// 父作用域符号。
    pub parent_symbol: Option<String>,
    /// 模块路径。
    pub module_path: Option<String>,
    /// 签名。
    pub signature: Option<String>,
    /// 文档注释。
    pub doc: Option<String>,
    /// 起始字节（含）。
    pub start_byte: i64,
    /// 结束字节（不含）。
    pub end_byte: i64,
    /// 起始行（从 1 开始，含）。
    pub start_line: u32,
    /// 结束行（从 1 开始，含）。
    pub end_line: u32,
    /// 估算 token 数。
    pub token_estimate: usize,
    /// 所属版本的内容哈希。
    pub file_hash: String,
    /// 是否为当前有效版本。
    pub is_current: bool,
    /// Embedding 状态。
    pub embed_state: String,
}

/// 参与作用域过滤的最小信息，用于一次性构造向量检索的允许集合。
#[derive(Debug, Clone)]
pub struct ChunkScopeRow {
    /// 代码块 ID。
    pub id: i64,
    /// 相对路径。
    pub path: String,
    /// 语言。
    pub language: Language,
}

const CHUNK_COLUMNS: &str = "c.id, c.file_id, c.revision_id, f.path, f.language, c.kind, c.symbol,
     c.parent_symbol, c.module_path, c.signature, c.doc, c.start_byte, c.end_byte, c.start_line,
     c.end_line, c.token_estimate, r.file_hash, c.is_current, c.embed_state";

fn map_chunk_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChunkRow> {
    Ok(ChunkRow {
        id: row.get(0)?,
        file_id: row.get(1)?,
        revision_id: row.get(2)?,
        path: row.get(3)?,
        language: Language::from_str_opt(&row.get::<_, String>(4)?).unwrap_or(Language::Text),
        kind: ChunkKind::from_str_opt(&row.get::<_, String>(5)?).unwrap_or(ChunkKind::Text),
        symbol: row.get(6)?,
        parent_symbol: row.get(7)?,
        module_path: row.get(8)?,
        signature: row.get(9)?,
        doc: row.get(10)?,
        start_byte: row.get(11)?,
        end_byte: row.get(12)?,
        start_line: row.get(13)?,
        end_line: row.get(14)?,
        token_estimate: row.get::<_, i64>(15)? as usize,
        file_hash: row.get(16)?,
        is_current: row.get::<_, i64>(17)? == 1,
        embed_state: row.get(18)?,
    })
}

impl Store {
    /// 按 ID 批量读取代码块元数据（不限制是否为当前版本）。
    pub fn chunks_by_ids(&self, ids: &[i64]) -> Result<Vec<ChunkRow>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!(
            "SELECT {CHUNK_COLUMNS} FROM chunks c
             JOIN files f ON f.id = c.file_id
             JOIN file_revisions r ON r.id = c.revision_id
             WHERE c.id IN ({placeholders})"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(ids.iter()), map_chunk_row)?;
        let mut chunks = Vec::new();
        for row in rows {
            chunks.push(row?);
        }
        Ok(chunks)
    }

    /// 读取单个代码块。
    pub fn chunk_by_id(&self, id: i64) -> Result<Option<ChunkRow>> {
        let sql = format!(
            "SELECT {CHUNK_COLUMNS} FROM chunks c
             JOIN files f ON f.id = c.file_id
             JOIN file_revisions r ON r.id = c.revision_id
             WHERE c.id = ?1"
        );
        Ok(self
            .conn()
            .query_row(&sql, params![id], map_chunk_row)
            .optional()?)
    }

    /// 从指定版本的源码快照中按字节区间 `[start, end)` 切片。
    ///
    /// 区间不在 UTF-8 字符边界上或越界时返回 [`Error::Internal`]，不会返回被破坏的文本。
    pub fn chunk_content(&self, revision_id: i64, start: i64, end: i64) -> Result<String> {
        let content: String = self.conn().query_row(
            "SELECT content FROM file_revisions WHERE id = ?1",
            params![revision_id],
            |row| row.get(0),
        )?;
        let (start, end) = (start.max(0) as usize, end.max(0) as usize);
        if start > end || end > content.len() {
            return Err(Error::Internal(format!(
                "版本 {revision_id} 的字节区间 [{start}, {end}) 越界"
            )));
        }
        content
            .get(start..end)
            .map(|slice| slice.to_string())
            .ok_or_else(|| {
                Error::Internal(format!("版本 {revision_id} 的字节区间未落在字符边界上"))
            })
    }

    /// 读取整个版本的源码快照与行数。
    pub fn revision_content(&self, revision_id: i64) -> Result<Option<(String, u32)>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT content, line_count FROM file_revisions WHERE id = ?1",
                params![revision_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    /// 当前全部有效代码块的作用域信息，用于构造向量检索允许集合。
    ///
    /// 一次查询取回全部候选，向量过滤回调只查内存集合，不会对每个候选执行 SQL。
    pub fn current_chunk_scopes(&self) -> Result<Vec<ChunkScopeRow>> {
        let mut stmt = self.conn().prepare(
            "SELECT c.id, f.path, f.language FROM chunks c
             JOIN files f ON f.id = c.file_id
             WHERE c.is_current = 1 AND f.state = 'active'",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ChunkScopeRow {
                id: row.get(0)?,
                path: row.get(1)?,
                language: Language::from_str_opt(&row.get::<_, String>(2)?)
                    .unwrap_or(Language::Text),
            })
        })?;
        let mut scopes = Vec::new();
        for row in rows {
            scopes.push(row?);
        }
        Ok(scopes)
    }

    /// 按符号名精确召回：先匹配限定名，再匹配简单名，均忽略大小写差异作为兜底。
    pub fn symbol_candidates(&self, name: &str, limit: usize) -> Result<Vec<(i64, bool)>> {
        let lower = name.to_lowercase();
        // 合并块会为同一代码块登记多个符号，因此按 chunk_id 聚合，避免同一结果重复出现。
        let mut stmt = self.conn().prepare(
            "SELECT s.chunk_id,
                    MAX(CASE WHEN s.qualified = ?1 OR s.name = ?1 THEN 1 ELSE 0 END) AS exact,
                    MIN(LENGTH(s.qualified)) AS shortest
             FROM symbols s
             JOIN chunks c ON c.id = s.chunk_id
             WHERE c.is_current = 1
               AND (s.qualified = ?1 OR s.name = ?1 OR s.qualified_lower = ?2 OR s.name_lower = ?2)
             GROUP BY s.chunk_id
             ORDER BY exact DESC, shortest ASC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![name, lower, limit as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)? == 1))
        })?;
        let mut candidates = Vec::new();
        for row in rows {
            candidates.push(row?);
        }
        Ok(candidates)
    }

    /// 按符号名后缀召回（例如查询 `execute` 命中 `RetryPolicy::execute`）。
    pub fn symbol_suffix_candidates(&self, name: &str, limit: usize) -> Result<Vec<i64>> {
        let pattern = format!("%{}", escape_like(&name.to_lowercase()));
        let mut stmt = self.conn().prepare(
            "SELECT s.chunk_id, MIN(LENGTH(s.qualified)) AS shortest FROM symbols s
             JOIN chunks c ON c.id = s.chunk_id
             WHERE c.is_current = 1 AND s.qualified_lower LIKE ?1 ESCAPE '\\'
             GROUP BY s.chunk_id
             ORDER BY shortest ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![pattern, limit as i64], |row| row.get::<_, i64>(0))?;
        let mut candidates = Vec::new();
        for row in rows {
            candidates.push(row?);
        }
        Ok(candidates)
    }

    /// 按路径片段召回当前文件的代码块。
    pub fn path_candidates(&self, fragment: &str, limit: usize) -> Result<Vec<i64>> {
        let pattern = format!("%{}%", escape_like(&fragment.to_lowercase()));
        let mut stmt = self.conn().prepare(
            "SELECT c.id FROM chunks c
             JOIN files f ON f.id = c.file_id
             WHERE c.is_current = 1 AND f.state = 'active'
               AND LOWER(f.path) LIKE ?1 ESCAPE '\\'
             ORDER BY LENGTH(f.path) ASC, c.ordinal ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![pattern, limit as i64], |row| row.get::<_, i64>(0))?;
        let mut candidates = Vec::new();
        for row in rows {
            candidates.push(row?);
        }
        Ok(candidates)
    }

    /// 字面量召回：在当前版本源码中做子串搜索，再定位到覆盖该偏移的代码块。
    ///
    /// `file_limit` 限制扫描的文件数，保证最坏情况有界。
    pub fn literal_candidates(
        &self,
        literal: &str,
        file_limit: usize,
        chunk_limit: usize,
    ) -> Result<Vec<i64>> {
        if literal.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn().prepare(
            "SELECT r.id, r.content FROM files f
             JOIN file_revisions r ON r.id = f.current_revision_id
             WHERE f.state = 'active' AND INSTR(r.content, ?1) > 0
             LIMIT ?2",
        )?;
        let mut hits: Vec<(i64, usize)> = Vec::new();
        let rows = stmt.query_map(params![literal, file_limit as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (revision_id, content) = row?;
            // 记录该文件中最多前若干个命中位置，避免单文件占满预算。
            for (count, (offset, _)) in content.match_indices(literal).enumerate() {
                if count >= 4 {
                    break;
                }
                hits.push((revision_id, offset));
            }
        }
        drop(stmt);
        if hits.is_empty() {
            return Ok(Vec::new());
        }

        let mut candidates = Vec::new();
        let mut stmt = self.conn().prepare(
            "SELECT id FROM chunks WHERE revision_id = ?1 AND is_current = 1
               AND start_byte <= ?2 AND end_byte > ?2
             ORDER BY (end_byte - start_byte) ASC LIMIT 1",
        )?;
        for (revision_id, offset) in hits {
            if candidates.len() >= chunk_limit {
                break;
            }
            let chunk: Option<i64> = stmt
                .query_row(params![revision_id, offset as i64], |row| row.get(0))
                .optional()?;
            if let Some(chunk_id) = chunk {
                if !candidates.contains(&chunk_id) {
                    candidates.push(chunk_id);
                }
            }
        }
        Ok(candidates)
    }

    /// FTS5 召回。`expr` 必须是调用方构造的合法 FTS 表达式（词条已加引号转义）。
    ///
    /// bm25 权重按 符号 > 路径 > 文档 > 代码 排序，返回结果已按相关度升序（越小越相关）。
    pub fn fts_candidates(&self, expr: &str, limit: usize) -> Result<Vec<i64>> {
        let mut stmt = self.conn().prepare(
            "SELECT t.rowid FROM chunk_fts t
             JOIN chunks c ON c.id = t.rowid
             WHERE chunk_fts MATCH ?1 AND c.is_current = 1
             ORDER BY bm25(chunk_fts, 3.0, 5.0, 1.5, 1.0) ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![expr, limit as i64], |row| row.get::<_, i64>(0));
        let rows = match rows {
            Ok(rows) => rows,
            // 表达式非法不应让整次查询失败，交由调用方降级到其他通路。
            Err(error) => return Err(Error::InvalidQuery(format!("FTS 查询失败: {error}"))),
        };
        let mut candidates = Vec::new();
        for row in rows {
            match row {
                Ok(id) => candidates.push(id),
                Err(error) => return Err(Error::InvalidQuery(format!("FTS 查询失败: {error}"))),
            }
        }
        Ok(candidates)
    }

    /// 查找覆盖指定字节偏移、且属于同一版本的最小定义块，用于上下文扩展。
    pub fn containing_definition(
        &self,
        revision_id: i64,
        start: i64,
        end: i64,
    ) -> Result<Option<ChunkRow>> {
        let sql = format!(
            "SELECT {CHUNK_COLUMNS} FROM chunks c
             JOIN files f ON f.id = c.file_id
             JOIN file_revisions r ON r.id = c.revision_id
             WHERE c.revision_id = ?1 AND c.start_byte <= ?2 AND c.end_byte >= ?3
               AND c.kind NOT IN ('overview','imports','text','section')
             ORDER BY (c.end_byte - c.start_byte) ASC LIMIT 1"
        );
        Ok(self
            .conn()
            .query_row(&sql, params![revision_id, start, end], map_chunk_row)
            .optional()?)
    }

    /// 同一版本内的全部当前代码块，按起始位置排序，用于相邻范围合并。
    pub fn chunks_of_revision(&self, revision_id: i64) -> Result<Vec<ChunkRow>> {
        let sql = format!(
            "SELECT {CHUNK_COLUMNS} FROM chunks c
             JOIN files f ON f.id = c.file_id
             JOIN file_revisions r ON r.id = c.revision_id
             WHERE c.revision_id = ?1 ORDER BY c.start_byte ASC"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(params![revision_id], map_chunk_row)?;
        let mut chunks = Vec::new();
        for row in rows {
            chunks.push(row?);
        }
        Ok(chunks)
    }

    /// 读取某个空间下指定代码块的向量，用于小范围精确距离计算。
    pub fn vectors_for_chunks(&self, space_id: &str, ids: &[i64]) -> Result<Vec<(i64, Vec<f32>)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!(
            "SELECT c.id, e.vector, e.dimensions FROM chunks c
             JOIN embedding_cache e ON e.cache_key = c.cache_key AND e.space_id = c.embedding_space_id
             WHERE c.is_current = 1 AND c.embedding_space_id = ? AND c.id IN ({placeholders})"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(ids.len() + 1);
        binds.push(Box::new(space_id.to_string()));
        for id in ids {
            binds.push(Box::new(*id));
        }
        let rows = stmt.query_map(params_from_iter(binds.iter().map(|b| b.as_ref())), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)? as usize,
            ))
        })?;
        let mut vectors = Vec::new();
        for row in rows {
            let (id, blob, dimensions) = row?;
            vectors.push((id, super::decode_vector(&blob, dimensions)?));
        }
        Ok(vectors)
    }

    /// 统计索引状态，用于 `status` 命令与降级判断。
    pub fn counters(&self) -> Result<StoreCounters> {
        let conn = self.conn();
        let one = |sql: &str| -> Result<i64> { Ok(conn.query_row(sql, [], |row| row.get(0))?) };
        Ok(StoreCounters {
            active_files: one("SELECT COUNT(*) FROM files WHERE state='active'")?,
            skipped_files: one("SELECT COUNT(*) FROM files WHERE state='skipped'")?,
            deleted_files: one("SELECT COUNT(*) FROM files WHERE state='deleted'")?,
            current_chunks: one("SELECT COUNT(*) FROM chunks WHERE is_current=1")?,
            embeddings_ready: one(
                "SELECT COUNT(*) FROM chunks WHERE is_current=1 AND embed_state='ready'",
            )?,
            embeddings_pending: one(
                "SELECT COUNT(*) FROM chunks WHERE is_current=1 AND embed_state='pending'",
            )?,
            embeddings_failed: one(
                "SELECT COUNT(*) FROM chunks WHERE is_current=1 AND embed_state='failed'",
            )?,
            cache_entries: one("SELECT COUNT(*) FROM embedding_cache")?,
        })
    }

    /// 读取 Embedding 空间行。
    pub fn embedding_space(&self, space_id: &str) -> Result<Option<super::EmbeddingSpaceRow>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id, provider, model, cache_namespace, dimensions, metric, normalize,
                        query_prefix, document_prefix, template_version
                 FROM embedding_spaces WHERE id = ?1",
                params![space_id],
                |row| {
                    Ok(super::EmbeddingSpaceRow {
                        id: row.get(0)?,
                        provider: row.get(1)?,
                        model: row.get(2)?,
                        cache_namespace: row.get(3)?,
                        dimensions: row.get::<_, i64>(4)? as usize,
                        metric: row.get(5)?,
                        normalize: row.get(6)?,
                        query_prefix: row.get(7)?,
                        document_prefix: row.get(8)?,
                        template_version: row.get::<_, i64>(9)? as u32,
                    })
                },
            )
            .optional()?)
    }
}

/// 索引计数。
#[derive(Debug, Clone, Default)]
pub struct StoreCounters {
    /// 活跃文件数。
    pub active_files: i64,
    /// 被跳过的文件数。
    pub skipped_files: i64,
    /// 已删除文件数。
    pub deleted_files: i64,
    /// 当前代码块数。
    pub current_chunks: i64,
    /// 已完成 Embedding 的代码块数。
    pub embeddings_ready: i64,
    /// 等待 Embedding 的代码块数。
    pub embeddings_pending: i64,
    /// 失败的代码块数。
    pub embeddings_failed: i64,
    /// 缓存条目数。
    pub cache_entries: i64,
}

/// 转义 `LIKE` 模式中的通配符，配合 `ESCAPE '\'` 使用。
pub fn escape_like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' | '%' | '_' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            _ => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{FtsText, ImportRecord, PreparedChunk, PreparedFile, SymbolRecord};

    fn store_with_file(content: &str, path: &str, symbol: &str) -> (Store, i64) {
        let mut store = Store::open_in_memory().expect("store");
        let prepared = PreparedFile {
            rel_path: path.to_string(),
            language: Language::Rust,
            file_hash: "hash1".into(),
            size: content.len() as u64,
            mtime_ns: 1,
            line_count: content.lines().count() as u32,
            content: content.to_string(),
            chunks: vec![PreparedChunk {
                ordinal: 0,
                kind: ChunkKind::Function,
                symbol: Some(symbol.to_string()),
                symbol_key: Some(format!("rust:{symbol}")),
                parent_symbol: None,
                module_path: None,
                signature: None,
                doc: None,
                start_byte: 0,
                end_byte: content.len(),
                start_line: 1,
                end_line: content.lines().count().max(1) as u32,
                token_estimate: 5,
                content_hash: "c0".into(),
                embed_input: content.to_string(),
                embed_input_truncated: false,
                fts: FtsText {
                    path_terms: path.replace('/', " "),
                    symbol_terms: symbol.to_string(),
                    doc_terms: String::new(),
                    code_terms: content.to_string(),
                },
                symbols: vec![SymbolRecord {
                    name: symbol.rsplit("::").next().unwrap_or(symbol).to_string(),
                    qualified: symbol.to_string(),
                    kind: ChunkKind::Function,
                }],
            }],
            imports: vec![ImportRecord {
                raw: "use std::io;".into(),
                module: Some("std::io".into()),
                start_byte: 0,
                end_byte: "use std::io;".len(),
            }],
        };
        let outcome = store.commit_file(&prepared).expect("commit");
        let chunk_id = outcome.chunk_ids[0];
        (store, chunk_id)
    }

    #[test]
    fn symbol_lookup_prefers_exact_match() {
        let (store, chunk_id) =
            store_with_file("fn execute() {}", "src/retry.rs", "RetryPolicy::execute");
        let hits = store
            .symbol_candidates("RetryPolicy::execute", 10)
            .expect("symbols");
        assert_eq!(hits, vec![(chunk_id, true)]);
        let by_name = store.symbol_candidates("execute", 10).expect("symbols");
        assert_eq!(by_name, vec![(chunk_id, true)]);
        // 大小写不同仍可命中，但不算精确。
        let loose = store.symbol_candidates("EXECUTE", 10).expect("symbols");
        assert_eq!(loose, vec![(chunk_id, false)]);
    }

    #[test]
    fn path_and_literal_recall_work_on_current_version() {
        let (store, chunk_id) = store_with_file(
            "fn boom() { panic!(\"连接超时: timeout\"); }",
            "src/net/retry.rs",
            "boom",
        );
        assert_eq!(
            store.path_candidates("net/retry", 10).expect("path"),
            vec![chunk_id]
        );
        // 含中文与特殊字符的字面量按原样子串匹配，不经过 FTS 语法。
        assert_eq!(
            store
                .literal_candidates("连接超时: timeout", 100, 10)
                .expect("literal"),
            vec![chunk_id]
        );
        assert!(store
            .literal_candidates("不存在的字符串", 100, 10)
            .expect("literal")
            .is_empty());
    }

    #[test]
    fn chunk_content_comes_from_the_same_revision() {
        let source = "fn 中文() { let x = 1; }";
        let (store, chunk_id) = store_with_file(source, "src/a.rs", "中文");
        let chunk = store.chunk_by_id(chunk_id).expect("row").expect("exists");
        let content = store
            .chunk_content(chunk.revision_id, chunk.start_byte, chunk.end_byte)
            .expect("content");
        assert_eq!(content, source);
        // 非字符边界的区间必须报错而不是返回破碎文本。
        assert!(store.chunk_content(chunk.revision_id, 0, 4).is_err());
    }

    #[test]
    fn like_escaping_prevents_wildcard_injection() {
        assert_eq!(escape_like("a%b_c"), "a\\%b\\_c");
        let (store, _) = store_with_file("fn a() {}", "src/a.rs", "a");
        // `%` 被转义后不再是通配符，因此查不到任何东西。
        assert!(store.path_candidates("%", 10).expect("path").is_empty());
    }
}
