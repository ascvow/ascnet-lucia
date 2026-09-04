//! sqlite-vec 本地向量存储。
//!
//! 该模块只在原生 `sqlite-vector-store` feature 下编译。WASM Guest 不直接获得文件写入
//! 能力；可执行搜索示例在宿主进程内打开 SQLite，并把数据库固定放在 Lucia 用户目录。

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use sqlite_vec::sqlite3_vec_init;
use std::mem::transmute;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 当前本地向量数据库结构版本。
const SCHEMA_VERSION: &str = "1";

/// sqlite-vec 虚拟表名称。
const VECTOR_TABLE: &str = "code_context_vectors";

/// 从数据库恢复的一条完整向量记录。
#[derive(Debug)]
pub(crate) struct StoredVector {
    /// 稳定代码块 ID。
    pub(crate) chunk_id: String,
    /// 仓库相对文件路径。
    pub(crate) file_path: String,
    /// 生成向量时的代码块内容哈希。
    pub(crate) content_hash: Option<String>,
    /// 已归一化的 f32 向量。
    pub(crate) vector: Vec<f32>,
}

/// 待批量写入数据库的一条向量记录。
pub(crate) struct VectorWrite<'a> {
    /// 稳定代码块 ID。
    pub(crate) chunk_id: &'a str,
    /// 仓库相对文件路径。
    pub(crate) file_path: &'a str,
    /// 生成向量时的代码块内容哈希。
    pub(crate) content_hash: Option<&'a str>,
    /// 已归一化的 f32 向量。
    pub(crate) vector: &'a [f32],
}

/// sqlite-vec 本地向量数据库。
///
/// 普通字段保存在 SQLite 表中，向量保存在 `vec0` 虚拟表中。两张表共享 `rowid`，批量更新
/// 使用同一事务，进程异常退出时不会留下只有元数据或只有向量的半条记录。
#[derive(Debug)]
pub(crate) struct SqliteVectorStore {
    /// 数据库文件路径，仅用于诊断。
    path: PathBuf,
    /// SQLite 连接；插件搜索流程是同步单线程调用，不跨线程共享该连接。
    connection: Connection,
    /// 当前嵌入维度。
    dimension: usize,
}

impl SqliteVectorStore {
    /// 打开或创建一份与指定嵌入空间绑定的本地向量数据库。
    ///
    /// `space_id` 或维度变化时会清空旧向量并重建 `vec0` 表，禁止把不同模型空间的向量
    /// 混在一起。父目录不存在时自动创建。
    ///
    /// # Errors
    ///
    /// 目录创建、SQLite 打开、sqlite-vec 注册、结构初始化或旧记录读取失败时返回错误。
    pub(crate) fn open(
        path: impl AsRef<Path>,
        space_id: &str,
        dimension: u32,
    ) -> Result<(Self, Vec<StoredVector>), String> {
        if dimension == 0 {
            return Err("sqlite-vec 向量维度必须大于零".to_string());
        }
        register_sqlite_vec()?;
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "创建 sqlite-vec 数据目录 `{}` 失败：{error}",
                    parent.display()
                )
            })?;
        }
        let connection = Connection::open(&path).map_err(|error| {
            format!("打开 sqlite-vec 数据库 `{}` 失败：{error}", path.display())
        })?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 PRAGMA foreign_keys = ON;
                 CREATE TABLE IF NOT EXISTS code_context_vector_meta (
                     key TEXT PRIMARY KEY,
                     value TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS code_context_vector_entries (
                     rowid INTEGER PRIMARY KEY,
                     chunk_id TEXT NOT NULL UNIQUE,
                     file_path TEXT NOT NULL,
                     content_hash TEXT
                 );",
            )
            .map_err(|error| format!("初始化 sqlite-vec 基础表失败：{error}"))?;

        let stored_schema = metadata_value(&connection, "schema_version")?;
        let stored_space = metadata_value(&connection, "space_id")?;
        let stored_dimension = metadata_value(&connection, "dimension")?;
        let expected_dimension = dimension.to_string();
        let compatible = stored_schema.as_deref() == Some(SCHEMA_VERSION)
            && stored_space.as_deref() == Some(space_id)
            && stored_dimension.as_deref() == Some(expected_dimension.as_str());
        if !compatible {
            reset_schema(&connection, space_id, dimension)?;
        } else {
            create_vector_table(&connection, dimension)?;
        }

        let store = Self {
            path,
            connection,
            dimension: dimension as usize,
        };
        let records = store.load_all()?;
        Ok((store, records))
    }

    /// 返回数据库文件路径。
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// 批量新增或替换向量。
    ///
    /// # Errors
    ///
    /// 任一向量维度不符或事务写入失败时整批回滚并返回错误。
    pub(crate) fn upsert_batch(&mut self, records: &[VectorWrite<'_>]) -> Result<(), String> {
        if records.is_empty() {
            return Ok(());
        }
        for record in records {
            if record.vector.len() != self.dimension {
                return Err(format!(
                    "sqlite-vec 向量维度不符：期望 {}，实际 {}，代码块 `{}`",
                    self.dimension,
                    record.vector.len(),
                    record.chunk_id
                ));
            }
        }
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| format!("开始 sqlite-vec 写入事务失败：{error}"))?;
        for record in records {
            upsert_record(&transaction, record)?;
        }
        transaction
            .commit()
            .map_err(|error| format!("提交 sqlite-vec 写入事务失败：{error}"))
    }

    /// 删除指定代码块 ID 的全部向量。
    ///
    /// # Errors
    ///
    /// SQLite 查询或事务提交失败时返回错误。
    pub(crate) fn delete_ids(&mut self, chunk_ids: &[String]) -> Result<(), String> {
        if chunk_ids.is_empty() {
            return Ok(());
        }
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| format!("开始 sqlite-vec 删除事务失败：{error}"))?;
        for chunk_id in chunk_ids {
            if let Some(rowid) = rowid_for_chunk(&transaction, chunk_id)? {
                transaction
                    .execute(
                        &format!("DELETE FROM {VECTOR_TABLE} WHERE rowid = ?1"),
                        params![rowid],
                    )
                    .map_err(|error| format!("删除 sqlite-vec 向量失败：{error}"))?;
                transaction
                    .execute(
                        "DELETE FROM code_context_vector_entries WHERE rowid = ?1",
                        params![rowid],
                    )
                    .map_err(|error| format!("删除 sqlite-vec 元数据失败：{error}"))?;
            }
        }
        transaction
            .commit()
            .map_err(|error| format!("提交 sqlite-vec 删除事务失败：{error}"))
    }

    /// 使用 sqlite-vec 执行余弦 KNN 搜索。
    ///
    /// `path_scope` 存在时先扩大 KNN 数量，再按仓库相对路径前缀过滤，保证范围搜索不会被
    /// 全局高分结果提前挤出。返回值按相似度降序排列。
    ///
    /// # Errors
    ///
    /// 查询向量维度不符、SQLite 执行失败或结果引用不存在的元数据时返回错误。
    pub(crate) fn search(
        &self,
        query: &[f32],
        path_scope: Option<&str>,
        limit: usize,
        total_entries: usize,
    ) -> Result<Vec<(String, f32)>, String> {
        if limit == 0 || total_entries == 0 {
            return Ok(Vec::new());
        }
        if query.len() != self.dimension {
            return Err(format!(
                "sqlite-vec 查询维度不符：期望 {}，实际 {}",
                self.dimension,
                query.len()
            ));
        }
        let k = if path_scope.is_some() {
            total_entries
        } else {
            limit.min(total_entries)
        };
        let vector = encode_f32(query);
        let sql = format!(
            "SELECT e.chunk_id, e.file_path, v.distance
             FROM {VECTOR_TABLE} AS v
             JOIN code_context_vector_entries AS e ON e.rowid = v.rowid
             WHERE v.embedding MATCH ?1 AND k = ?2
             ORDER BY v.distance ASC, e.chunk_id ASC"
        );
        let mut statement = self
            .connection
            .prepare(&sql)
            .map_err(|error| format!("准备 sqlite-vec KNN 查询失败：{error}"))?;
        let rows = statement
            .query_map(params![vector, k as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f32>(2)?,
                ))
            })
            .map_err(|error| format!("执行 sqlite-vec KNN 查询失败：{error}"))?;
        let mut matches = Vec::with_capacity(limit);
        for row in rows {
            let (chunk_id, file_path, distance) =
                row.map_err(|error| format!("读取 sqlite-vec KNN 结果失败：{error}"))?;
            if path_scope.is_none_or(|scope| file_path.starts_with(scope)) {
                matches.push((chunk_id, 1.0 - distance));
                if matches.len() == limit {
                    break;
                }
            }
        }
        Ok(matches)
    }

    /// 清空数据库并切换嵌入空间。
    ///
    /// # Errors
    ///
    /// 删除旧虚拟表、更新元数据或创建新表失败时返回错误。
    pub(crate) fn reset(&mut self, space_id: &str, dimension: u32) -> Result<(), String> {
        reset_schema(&self.connection, space_id, dimension)?;
        self.dimension = dimension as usize;
        Ok(())
    }

    /// 从数据库读取全部向量，用于进程启动后的内容哈希复用与内存降级。
    fn load_all(&self) -> Result<Vec<StoredVector>, String> {
        let sql = format!(
            "SELECT e.chunk_id, e.file_path, e.content_hash, v.embedding
             FROM code_context_vector_entries AS e
             JOIN {VECTOR_TABLE} AS v ON v.rowid = e.rowid
             ORDER BY e.chunk_id ASC"
        );
        let mut statement = self
            .connection
            .prepare(&sql)
            .map_err(|error| format!("准备 sqlite-vec 恢复查询失败：{error}"))?;
        let rows = statement
            .query_map([], |row| {
                let bytes = row.get::<_, Vec<u8>>(3)?;
                Ok(StoredVector {
                    chunk_id: row.get(0)?,
                    file_path: row.get(1)?,
                    content_hash: row.get(2)?,
                    vector: decode_f32(&bytes),
                })
            })
            .map_err(|error| format!("执行 sqlite-vec 恢复查询失败：{error}"))?;
        let mut records = Vec::new();
        for row in rows {
            let record = row.map_err(|error| format!("读取 sqlite-vec 向量失败：{error}"))?;
            if record.vector.len() != self.dimension {
                return Err(format!(
                    "sqlite-vec 数据库 `{}` 含有维度异常的向量 `{}`",
                    self.path.display(),
                    record.chunk_id
                ));
            }
            records.push(record);
        }
        Ok(records)
    }
}

/// 向 SQLite 进程全局注册 sqlite-vec 扩展。
///
/// # Errors
///
/// SQLite 拒绝注册自动扩展时返回错误；结果会跨数据库连接复用，避免重复注册。
fn register_sqlite_vec() -> Result<(), String> {
    static REGISTER: OnceLock<Result<(), String>> = OnceLock::new();
    REGISTER
        .get_or_init(|| unsafe {
            let entry = transmute::<*const (), rusqlite::auto_extension::RawAutoExtension>(
                sqlite3_vec_init as *const (),
            );
            rusqlite::auto_extension::register_auto_extension(entry)
                .map_err(|error| format!("注册 sqlite-vec 扩展失败：{error}"))
        })
        .clone()
}

/// 读取一条内部元数据。
fn metadata_value(connection: &Connection, key: &str) -> Result<Option<String>, String> {
    connection
        .query_row(
            "SELECT value FROM code_context_vector_meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| format!("读取 sqlite-vec 元数据 `{key}` 失败：{error}"))
}

/// 重建与嵌入空间绑定的虚拟表和元数据。
fn reset_schema(connection: &Connection, space_id: &str, dimension: u32) -> Result<(), String> {
    connection
        .execute_batch(&format!(
            "DROP TABLE IF EXISTS {VECTOR_TABLE};
             DELETE FROM code_context_vector_entries;
             DELETE FROM code_context_vector_meta;"
        ))
        .map_err(|error| format!("清理旧 sqlite-vec 索引失败：{error}"))?;
    connection
        .execute(
            "INSERT INTO code_context_vector_meta(key, value) VALUES
             ('schema_version', ?1), ('space_id', ?2), ('dimension', ?3)",
            params![SCHEMA_VERSION, space_id, dimension.to_string()],
        )
        .map_err(|error| format!("写入 sqlite-vec 元数据失败：{error}"))?;
    create_vector_table(connection, dimension)
}

/// 创建固定维度、余弦距离的 vec0 虚拟表。
fn create_vector_table(connection: &Connection, dimension: u32) -> Result<(), String> {
    connection
        .execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS {VECTOR_TABLE} USING vec0(
                 embedding float[{dimension}] distance_metric=cosine
             );"
        ))
        .map_err(|error| format!("创建 sqlite-vec 虚拟表失败：{error}"))
}

/// 在事务中新增或替换一条向量。
fn upsert_record(transaction: &Transaction<'_>, record: &VectorWrite<'_>) -> Result<(), String> {
    let existing = rowid_for_chunk(transaction, record.chunk_id)?;
    let rowid = match existing {
        Some(rowid) => {
            transaction
                .execute(
                    "UPDATE code_context_vector_entries
                     SET file_path = ?2, content_hash = ?3
                     WHERE rowid = ?1",
                    params![rowid, record.file_path, record.content_hash],
                )
                .map_err(|error| format!("更新 sqlite-vec 元数据失败：{error}"))?;
            transaction
                .execute(
                    &format!("DELETE FROM {VECTOR_TABLE} WHERE rowid = ?1"),
                    params![rowid],
                )
                .map_err(|error| format!("替换 sqlite-vec 旧向量失败：{error}"))?;
            rowid
        }
        None => {
            transaction
                .execute(
                    "INSERT INTO code_context_vector_entries(chunk_id, file_path, content_hash)
                     VALUES (?1, ?2, ?3)",
                    params![record.chunk_id, record.file_path, record.content_hash],
                )
                .map_err(|error| format!("写入 sqlite-vec 元数据失败：{error}"))?;
            transaction.last_insert_rowid()
        }
    };
    transaction
        .execute(
            &format!("INSERT INTO {VECTOR_TABLE}(rowid, embedding) VALUES (?1, ?2)"),
            params![rowid, encode_f32(record.vector)],
        )
        .map_err(|error| format!("写入 sqlite-vec 向量失败：{error}"))?;
    Ok(())
}

/// 按代码块 ID 查找稳定 rowid。
fn rowid_for_chunk(connection: &Connection, chunk_id: &str) -> Result<Option<i64>, String> {
    connection
        .query_row(
            "SELECT rowid FROM code_context_vector_entries WHERE chunk_id = ?1",
            params![chunk_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| format!("查询 sqlite-vec 代码块 `{chunk_id}` 失败：{error}"))
}

/// 按 SQLite 与 sqlite-vec 约定的小端序编码 f32 向量。
fn encode_f32(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(std::mem::size_of_val(vector));
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// 从 sqlite-vec 返回的 BLOB 恢复 f32 向量。
fn decode_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(std::mem::size_of::<f32>())
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// sqlite-vec 必须持久化向量，并在重新打开后继续执行 KNN 搜索。
    #[test]
    fn database_survives_reopen_and_searches_vectors() {
        let path = std::env::temp_dir().join(format!(
            "lucia-code-context-sqlite-vec-{}-{}.sqlite3",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
        {
            let (mut store, restored) =
                SqliteVectorStore::open(&path, "space-a", 3).expect("创建数据库");
            assert!(restored.is_empty());
            store
                .upsert_batch(&[
                    VectorWrite {
                        chunk_id: "chunk-a",
                        file_path: "src/a.rs",
                        content_hash: Some("hash-a"),
                        vector: &[1.0, 0.0, 0.0],
                    },
                    VectorWrite {
                        chunk_id: "chunk-b",
                        file_path: "tests/b.rs",
                        content_hash: Some("hash-b"),
                        vector: &[0.0, 1.0, 0.0],
                    },
                ])
                .expect("写入向量");
            assert_eq!(
                store
                    .search(&[1.0, 0.0, 0.0], None, 1, 2)
                    .expect("搜索向量")[0]
                    .0,
                "chunk-a"
            );
        }
        let (store, restored) =
            SqliteVectorStore::open(&path, "space-a", 3).expect("重新打开数据库");
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0].chunk_id, "chunk-a");
        assert_eq!(
            store
                .search(&[0.0, 1.0, 0.0], Some("tests/"), 1, 2)
                .expect("按路径搜索")[0]
                .0,
            "chunk-b"
        );
        drop(store);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    /// 嵌入空间变化时必须删除旧向量，禁止跨模型复用。
    #[test]
    fn changed_space_resets_database() {
        let path = std::env::temp_dir().join(format!(
            "lucia-code-context-sqlite-vec-space-{}.sqlite3",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let (mut store, _) = SqliteVectorStore::open(&path, "space-a", 2).expect("创建数据库");
            store
                .upsert_batch(&[VectorWrite {
                    chunk_id: "old",
                    file_path: "src/lib.rs",
                    content_hash: Some("hash"),
                    vector: &[1.0, 0.0],
                }])
                .expect("写入向量");
        }
        let (_, restored) = SqliteVectorStore::open(&path, "space-b", 2).expect("切换嵌入空间");
        assert!(restored.is_empty());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }
}
