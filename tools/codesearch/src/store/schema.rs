//! 数据库 schema 与迁移。
//!
//! 版本号写入 `PRAGMA user_version`。每个迁移是一段幂等 SQL，按序执行；
//! 已经应用过的版本不会重复执行。新增字段必须追加新的迁移而不是修改旧迁移。

use rusqlite::Connection;

use crate::error::{Error, Result};

/// 当前 schema 版本。
pub const SCHEMA_VERSION: i64 = 1;

/// 各版本的迁移 SQL，下标 `i` 对应版本 `i + 1`。
const MIGRATIONS: &[&str] = &[MIGRATION_V1];

const MIGRATION_V1: &str = r#"
-- workspace 与索引状态的键值元数据
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- 文件目录：每条记录对应一个相对路径的当前状态
CREATE TABLE IF NOT EXISTS files (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    path                TEXT    NOT NULL UNIQUE,
    language            TEXT    NOT NULL,
    state               TEXT    NOT NULL,        -- active | deleted | skipped
    current_revision_id INTEGER REFERENCES file_revisions(id),
    skip_reason         TEXT,
    skip_detail         TEXT,
    size                INTEGER NOT NULL DEFAULT 0,
    mtime_ns            INTEGER NOT NULL DEFAULT 0,
    updated_at          INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_files_state ON files(state);

-- 文件版本：保存完整源码快照，是结果代码、行号与 file_hash 的唯一来源
CREATE TABLE IF NOT EXISTS file_revisions (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id    INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    file_hash  TEXT    NOT NULL,
    size       INTEGER NOT NULL,
    line_count INTEGER NOT NULL,
    content    TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE(file_id, file_hash)
);

-- 代码块：id 即 chunk_version_id，同时作为 USearch 向量 key；AUTOINCREMENT 保证永不复用
CREATE TABLE IF NOT EXISTS chunks (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    revision_id         INTEGER NOT NULL REFERENCES file_revisions(id) ON DELETE CASCADE,
    file_id             INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    is_current          INTEGER NOT NULL DEFAULT 1,
    ordinal             INTEGER NOT NULL,
    kind                TEXT    NOT NULL,
    symbol              TEXT,
    symbol_key          TEXT,
    parent_symbol       TEXT,
    module_path         TEXT,
    signature           TEXT,
    doc                 TEXT,
    start_byte          INTEGER NOT NULL,
    end_byte            INTEGER NOT NULL,
    start_line          INTEGER NOT NULL,
    end_line            INTEGER NOT NULL,
    token_estimate      INTEGER NOT NULL,
    content_hash        TEXT    NOT NULL,
    embed_input         TEXT    NOT NULL,
    embed_input_truncated INTEGER NOT NULL DEFAULT 0,
    embed_state         TEXT    NOT NULL DEFAULT 'pending',  -- pending | ready | failed
    embed_attempts      INTEGER NOT NULL DEFAULT 0,
    embed_error         TEXT,
    embedding_space_id  TEXT,
    cache_key           TEXT
);
CREATE INDEX IF NOT EXISTS idx_chunks_file_current ON chunks(file_id, is_current);
CREATE INDEX IF NOT EXISTS idx_chunks_current_embed ON chunks(is_current, embed_state);
CREATE INDEX IF NOT EXISTS idx_chunks_revision ON chunks(revision_id);
CREATE INDEX IF NOT EXISTS idx_chunks_symbol_key ON chunks(symbol_key) WHERE symbol_key IS NOT NULL;

-- 符号表：仅索引当前代码块，删除时随代码块一起清理
CREATE TABLE IF NOT EXISTS symbols (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    chunk_id  INTEGER NOT NULL REFERENCES chunks(id) ON DELETE CASCADE,
    file_id   INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name      TEXT    NOT NULL,
    name_lower TEXT   NOT NULL,
    qualified TEXT    NOT NULL,
    qualified_lower TEXT NOT NULL,
    kind      TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_symbols_name_lower ON symbols(name_lower);
CREATE INDEX IF NOT EXISTS idx_symbols_qualified_lower ON symbols(qualified_lower);
CREATE INDEX IF NOT EXISTS idx_symbols_chunk ON symbols(chunk_id);

-- 导入声明：当前版本的导入元数据（启发式，用于上下文扩展）
CREATE TABLE IF NOT EXISTS imports (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id     INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    revision_id INTEGER NOT NULL REFERENCES file_revisions(id) ON DELETE CASCADE,
    raw         TEXT    NOT NULL,
    module      TEXT
);
CREATE INDEX IF NOT EXISTS idx_imports_file ON imports(file_id);

-- FTS5 全文索引：只保存拆词文本，rowid 对应 chunks.id；原始代码不放这里
CREATE VIRTUAL TABLE IF NOT EXISTS chunk_fts USING fts5(
    path_terms,
    symbol_terms,
    doc_terms,
    code_terms,
    tokenize = "unicode61 remove_diacritics 2 tokenchars '_'"
);

-- Embedding 空间：模型、修订、维度、度量、归一化、前缀与模板版本共同决定身份
CREATE TABLE IF NOT EXISTS embedding_spaces (
    id               TEXT PRIMARY KEY,
    provider         TEXT    NOT NULL,
    model            TEXT    NOT NULL,
    cache_namespace  TEXT    NOT NULL,
    dimensions       INTEGER NOT NULL,
    metric           TEXT    NOT NULL,
    normalize        TEXT    NOT NULL,
    query_prefix     TEXT    NOT NULL,
    document_prefix  TEXT    NOT NULL,
    template_version INTEGER NOT NULL,
    created_at       INTEGER NOT NULL
);

-- Embedding 缓存：key = hash(space_id + 实际发送给模型的完整输入)，向量以小端 f32 保存
CREATE TABLE IF NOT EXISTS embedding_cache (
    cache_key  TEXT PRIMARY KEY,
    space_id   TEXT    NOT NULL REFERENCES embedding_spaces(id),
    dimensions INTEGER NOT NULL,
    vector     BLOB    NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_embedding_cache_space ON embedding_cache(space_id);

-- 向量更新日志：SQLite 与 USearch 之间的持久化有序操作流，Vector Writer 顺序消费
CREATE TABLE IF NOT EXISTS vector_ops (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    space_id   TEXT    NOT NULL,
    op         TEXT    NOT NULL,   -- add | remove
    vector_key INTEGER NOT NULL,
    cache_key  TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_vector_ops_space ON vector_ops(space_id, seq);
"#;

/// 在连接上执行所有尚未应用的迁移。
///
/// 全新数据库直接跳到最新版本；已有数据库逐个应用。
/// 数据库版本高于本程序支持的版本时返回错误，避免旧程序破坏新格式。
pub fn migrate(conn: &Connection) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if current > SCHEMA_VERSION {
        return Err(Error::Config(format!(
            "数据库 schema 版本 {current} 高于本程序支持的 {SCHEMA_VERSION}，请升级 codesearch 或删除数据目录重建"
        )));
    }
    for (index, sql) in MIGRATIONS.iter().enumerate() {
        let version = index as i64 + 1;
        if version <= current {
            continue;
        }
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let applied = conn
            .execute_batch(sql)
            .and_then(|_| conn.pragma_update(None, "user_version", version));
        match applied {
            Ok(()) => conn.execute_batch("COMMIT")?,
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(Error::Sqlite(error));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_is_idempotent_and_sets_version() {
        let conn = Connection::open_in_memory().expect("open");
        migrate(&conn).expect("first migrate");
        migrate(&conn).expect("second migrate");
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("version");
        assert_eq!(version, SCHEMA_VERSION);
        // FTS5 表必须可用。
        conn.execute(
            "INSERT INTO chunk_fts(rowid, path_terms, symbol_terms, doc_terms, code_terms) VALUES (1, 'a', 'b', 'c', 'd')",
            [],
        )
        .expect("fts insert");
    }

    #[test]
    fn rejects_newer_schema() {
        let conn = Connection::open_in_memory().expect("open");
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .expect("set version");
        assert!(migrate(&conn).is_err());
    }
}
