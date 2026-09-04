//! 插件私有向量库：SQLite 存元数据，sqlite-vec 存向量并执行近邻检索。
//!
//! # 为什么不能继续用 store
//!
//! `host-store-*` 是一份重写全量的 JSON 文件，上限 8 MiB。一条 1024 维向量即使压成 int4
//! 再 base64，单条仍要约 900 字节，7 MiB 只装得下八千条左右；超出部分只能静默丢弃，于是
//! "仓库越大，语义检索覆盖得越少"。这不是调参能解决的，是载体选错了。
//!
//! 本模块把向量放进 sqlite-vec，按记录增量写入，**检索在宿主侧完成**：Guest 每次只传一条
//! 查询向量，拿回 top-K 的 ID 与分数。Guest 因此既不需要把整库读进线性内存，也不再受 JSON
//! 信封的体积约束。
//!
//! # 为什么是精确扫描而不是 ANN
//!
//! `vec0` 做的是精确 KNN。ANN 索引的收益出现在百万级向量之上，代价是召回率——而单个代码
//! 仓库通常只有数千到数万条记录，精确扫描既够快又不损失召回。真正需要 ANN 时替换本模块
//! 即可，[`PluginVectorStore`] 的接口不必变化。
//!
//! # 与业务无关
//!
//! 字段只有 `record_id` / `path` / `revision` 三项通用概念：`path` 供前缀过滤，`revision`
//! 供对账判断记录是否过期。任何需要向量检索的插件都能直接使用，这里不出现任何检索引擎或
//! 代码索引的概念。

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use sha2::{Digest, Sha256};
use sqlite_vec::sqlite3_vec_init;
use std::mem::transmute;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 当前向量库结构版本；与结构不兼容的旧库整体重建。
const SCHEMA_VERSION: &str = "1";

/// sqlite-vec 虚拟表名。
const VECTOR_TABLE: &str = "plugin_vector_embeddings";

/// 允许的最大向量维度。
///
/// 常见嵌入模型不超过 4096 维；设上限是为了让 Guest 无法用一个畸形维度撑爆磁盘。
const MAX_DIMENSION: u32 = 4096;

/// 记录 ID 与路径的最大字节数。
const MAX_KEY_BYTES: usize = 1024;

/// 向量库文件所在的子目录，与持久 store 并列。
const VECTOR_SUBDIRECTORY: &str = "vectors";

/// 一条待写入的向量记录。
pub(crate) struct VectorRecord {
    /// 插件自定义的稳定记录 ID。
    pub(crate) record_id: String,
    /// 供前缀过滤的路径；插件不需要过滤时可为空串。
    pub(crate) path: String,
    /// 记录版本标识，用于对账时判断向量是否过期。
    pub(crate) revision: Option<String>,
    /// 已解码的向量分量。
    pub(crate) vector: Vec<f32>,
}

/// 一条检索命中。
pub(crate) struct VectorHit {
    /// 命中的记录 ID。
    pub(crate) record_id: String,
    /// 记录路径。
    pub(crate) path: String,
    /// 余弦相似度，越大越相关。
    pub(crate) score: f32,
}

/// 一次对账中提交的记录标识。
pub(crate) struct SyncEntry {
    /// 记录 ID。
    pub(crate) record_id: String,
    /// 期望的记录版本；`None` 表示只要存在即视为最新。
    pub(crate) revision: Option<String>,
}

/// 插件私有向量库。
///
/// 插件的 Guest 调用是同步串行的，因此内部不做额外并发控制；连接不跨线程共享。
pub(crate) struct PluginVectorStore {
    /// 数据库文件路径，仅用于诊断信息。
    path: PathBuf,
    /// SQLite 连接；元数据表与 `vec0` 虚拟表同库，因此写入可以共用一个事务。
    connection: Connection,
    /// 已绑定向量空间的维度；尚未绑定时为 `None`。
    dimension: Option<usize>,
}

impl PluginVectorStore {
    /// 在插件持久目录下打开或创建向量库。
    ///
    /// 文件名由插件 ID 与 Host 生成的 `scope_key` 摘要而来，与持久 store 使用同一套隔离
    /// 规则：路径分隔符不会进入文件名，不同插件与不同项目作用域互不可见。
    ///
    /// # Errors
    ///
    /// 目录创建、SQLite 打开、sqlite-vec 注册或基础表初始化失败时返回错误。
    pub(crate) fn open(root: &Path, plugin_id: &str, scope_key: &str) -> Result<Self> {
        if root.as_os_str().is_empty() {
            bail!("插件向量库目录不能为空");
        }
        if plugin_id.trim().is_empty() {
            bail!("插件向量库 plugin_id 不能为空");
        }
        if scope_key.trim().is_empty() || scope_key.len() > 256 {
            bail!("插件向量库作用域标识无效");
        }
        register_sqlite_vec()?;

        let directory = root.join(VECTOR_SUBDIRECTORY);
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("创建插件向量库目录 `{}` 失败", directory.display()))?;
        let mut digest = Sha256::new();
        digest.update(b"lucia-plugin-vectors-v1\0");
        digest.update((plugin_id.len() as u64).to_be_bytes());
        digest.update(plugin_id.as_bytes());
        digest.update((scope_key.len() as u64).to_be_bytes());
        digest.update(scope_key.as_bytes());
        let path = directory.join(format!("{:x}.sqlite3", digest.finalize()));

        let connection = Connection::open(&path)
            .with_context(|| format!("打开插件向量库 `{}` 失败", path.display()))?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 CREATE TABLE IF NOT EXISTS plugin_vector_meta (
                     key TEXT PRIMARY KEY,
                     value TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS plugin_vector_records (
                     rowid INTEGER PRIMARY KEY,
                     record_id TEXT NOT NULL UNIQUE,
                     path TEXT NOT NULL,
                     revision TEXT
                 );
                 CREATE INDEX IF NOT EXISTS plugin_vector_records_path
                     ON plugin_vector_records(path);
                 CREATE TABLE IF NOT EXISTS plugin_vector_sync (
                     record_id TEXT PRIMARY KEY
                 );",
            )
            .with_context(|| format!("初始化插件向量库 `{}` 失败", path.display()))?;

        let dimension = metadata_value(&connection, "dimension")?
            .and_then(|value| value.parse::<u32>().ok())
            .map(|value| value as usize);
        Ok(Self {
            path,
            connection,
            dimension,
        })
    }

    /// 绑定向量空间，返回库中现有记录数。
    ///
    /// 空间标识、维度或结构版本与库中不符时**整库清空重建**。混用不同嵌入空间的向量会让
    /// 检索结果长期缓慢地不正确，重新计算向量的代价远小于给出错误结果。
    ///
    /// # Errors
    ///
    /// 维度非法、SQLite 重建或计数失败时返回错误。
    pub(crate) fn bind_space(&mut self, space_id: &str, dimension: u32) -> Result<u64> {
        if dimension == 0 || dimension > MAX_DIMENSION {
            bail!("插件向量维度必须在 1..={MAX_DIMENSION} 之间，实际为 {dimension}");
        }
        if space_id.trim().is_empty() || space_id.len() > MAX_KEY_BYTES {
            bail!("插件向量空间标识无效");
        }

        let compatible = metadata_value(&self.connection, "schema_version")?.as_deref()
            == Some(SCHEMA_VERSION)
            && metadata_value(&self.connection, "space_id")?.as_deref() == Some(space_id)
            && metadata_value(&self.connection, "dimension")?.as_deref()
                == Some(dimension.to_string().as_str());
        if compatible {
            create_vector_table(&self.connection, dimension)?;
        } else {
            reset_schema(&self.connection, space_id, dimension)?;
        }
        self.dimension = Some(dimension as usize);
        self.count()
    }

    /// 返回当前记录数。
    ///
    /// # Errors
    ///
    /// SQLite 计数查询失败时返回错误。
    pub(crate) fn count(&self) -> Result<u64> {
        self.connection
            .query_row("SELECT COUNT(*) FROM plugin_vector_records", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|count| count.max(0) as u64)
            .context("统计插件向量记录数失败")
    }

    /// 对账一批记录，返回其中缺失或版本不符的记录 ID 以及本次删除的记录数。
    ///
    /// `first` 为真时先清空上一次残留的对账集合；`last` 为真时删除本次对账未出现的全部记录。
    /// 分批提交让 Guest 不必一次性把整库清单塞进一个 JSON 信封，也让这一个入口同时承担
    /// "哪些需要重算向量"与"哪些已经失效"两件事。
    ///
    /// # Errors
    ///
    /// 尚未绑定向量空间，或 SQLite 读写失败时返回错误。
    pub(crate) fn sync(
        &mut self,
        entries: &[SyncEntry],
        first: bool,
        last: bool,
    ) -> Result<(Vec<String>, u64)> {
        self.require_dimension()?;
        for entry in entries {
            validate_key("记录 ID", &entry.record_id)?;
        }

        let transaction = self
            .connection
            .transaction()
            .context("开始插件向量对账事务失败")?;
        if first {
            transaction
                .execute("DELETE FROM plugin_vector_sync", [])
                .context("清理上一次插件向量对账集合失败")?;
        }

        let mut stale = Vec::new();
        for entry in entries {
            transaction
                .execute(
                    "INSERT OR IGNORE INTO plugin_vector_sync(record_id) VALUES (?1)",
                    params![entry.record_id],
                )
                .context("登记插件向量对账记录失败")?;
            let stored: Option<Option<String>> = transaction
                .query_row(
                    "SELECT revision FROM plugin_vector_records WHERE record_id = ?1",
                    params![entry.record_id],
                    |row| row.get(0),
                )
                .optional()
                .context("查询插件向量记录版本失败")?;
            // 记录不存在，或版本与调用方声明的当前版本不一致，都需要重新写入向量。
            let needs_write = match stored {
                None => true,
                Some(stored_revision) => match (&entry.revision, &stored_revision) {
                    (Some(expected), Some(actual)) => expected != actual,
                    (Some(_), None) => true,
                    (None, _) => false,
                },
            };
            if needs_write {
                stale.push(entry.record_id.clone());
            }
        }

        let mut removed = 0u64;
        if last {
            // 先删向量再删元数据：两张表同库同事务，顺序只影响可读性，不影响原子性。
            transaction
                .execute(
                    &format!(
                        "DELETE FROM {VECTOR_TABLE} WHERE rowid IN (
                             SELECT rowid FROM plugin_vector_records
                             WHERE record_id NOT IN (SELECT record_id FROM plugin_vector_sync)
                         )"
                    ),
                    [],
                )
                .context("删除插件向量库中的过期向量失败")?;
            removed = transaction
                .execute(
                    "DELETE FROM plugin_vector_records
                     WHERE record_id NOT IN (SELECT record_id FROM plugin_vector_sync)",
                    [],
                )
                .context("删除插件向量库中的过期记录失败")? as u64;
            transaction
                .execute("DELETE FROM plugin_vector_sync", [])
                .context("清理插件向量对账集合失败")?;
        }
        transaction.commit().context("提交插件向量对账事务失败")?;
        Ok((stale, removed))
    }

    /// 批量写入或替换向量记录。
    ///
    /// # Errors
    ///
    /// 尚未绑定向量空间、任一向量维度不符或事务写入失败时整批回滚并返回错误。
    pub(crate) fn upsert(&mut self, records: &[VectorRecord]) -> Result<u64> {
        let dimension = self.require_dimension()?;
        for record in records {
            validate_key("记录 ID", &record.record_id)?;
            validate_key("记录路径", &record.path)?;
            if record.vector.len() != dimension {
                bail!(
                    "插件向量维度不符：期望 {dimension}，实际 {}，记录 `{}`",
                    record.vector.len(),
                    record.record_id
                );
            }
        }
        if records.is_empty() {
            return Ok(0);
        }
        let transaction = self
            .connection
            .transaction()
            .context("开始插件向量写入事务失败")?;
        for record in records {
            upsert_record(&transaction, record)?;
        }
        transaction.commit().context("提交插件向量写入事务失败")?;
        Ok(records.len() as u64)
    }

    /// 执行一次余弦近邻检索。
    ///
    /// `path_prefix` 存在时把 K 值放大到全库再按前缀过滤，避免范围内的结果被范围外的高分
    /// 记录提前挤出候选窗口。返回值按相似度降序排列。
    ///
    /// # Errors
    ///
    /// 尚未绑定向量空间、查询维度不符或 SQLite 执行失败时返回错误。
    pub(crate) fn search(
        &self,
        query: &[f32],
        limit: usize,
        path_prefix: Option<&str>,
    ) -> Result<Vec<VectorHit>> {
        let dimension = self.require_dimension()?;
        if query.len() != dimension {
            bail!(
                "插件向量查询维度不符：期望 {dimension}，实际 {}",
                query.len()
            );
        }
        if let Some(prefix) = path_prefix {
            validate_key("记录路径", prefix)?;
        }
        let total = self.count()? as usize;
        if limit == 0 || total == 0 {
            return Ok(Vec::new());
        }
        let k = if path_prefix.is_some() {
            total
        } else {
            limit.min(total)
        };

        let mut statement = self
            .connection
            .prepare(&format!(
                "SELECT r.record_id, r.path, v.distance
                 FROM {VECTOR_TABLE} AS v
                 JOIN plugin_vector_records AS r ON r.rowid = v.rowid
                 WHERE v.embedding MATCH ?1 AND k = ?2
                 ORDER BY v.distance ASC, r.record_id ASC"
            ))
            .context("准备插件向量检索失败")?;
        let rows = statement
            .query_map(params![encode_f32(query), k as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f32>(2)?,
                ))
            })
            .context("执行插件向量检索失败")?;

        let mut hits = Vec::with_capacity(limit.min(total));
        for row in rows {
            let (record_id, path, distance) = row.context("读取插件向量检索结果失败")?;
            if path_prefix.is_none_or(|prefix| path.starts_with(prefix)) {
                hits.push(VectorHit {
                    record_id,
                    path,
                    // sqlite-vec 的 cosine distance 是 1 - 相似度，这里还原成相似度。
                    score: 1.0 - distance,
                });
                if hits.len() == limit {
                    break;
                }
            }
        }
        Ok(hits)
    }

    /// 按记录 ID 与路径前缀删除记录，返回实际删除条数。
    ///
    /// # Errors
    ///
    /// SQLite 删除或事务提交失败时返回错误。
    pub(crate) fn remove(
        &mut self,
        record_ids: &[String],
        path_prefixes: &[String],
    ) -> Result<u64> {
        if record_ids.is_empty() && path_prefixes.is_empty() {
            return Ok(0);
        }
        for record_id in record_ids {
            validate_key("记录 ID", record_id)?;
        }
        for prefix in path_prefixes {
            validate_key("记录路径", prefix)?;
        }

        let transaction = self
            .connection
            .transaction()
            .context("开始插件向量删除事务失败")?;
        let mut removed = 0u64;
        for record_id in record_ids {
            removed += transaction
                .execute(
                    "DELETE FROM plugin_vector_records WHERE record_id = ?1",
                    params![record_id],
                )
                .context("按记录 ID 删除插件向量失败")? as u64;
        }
        for prefix in path_prefixes {
            // LIKE 的通配符必须转义，否则插件传入的 `%` 会把整库删掉。
            removed += transaction
                .execute(
                    "DELETE FROM plugin_vector_records WHERE path LIKE ?1 ESCAPE '\\'",
                    params![format!("{}%", escape_like(prefix))],
                )
                .context("按路径前缀删除插件向量失败")? as u64;
        }
        transaction
            .execute(
                &format!(
                    "DELETE FROM {VECTOR_TABLE}
                     WHERE rowid NOT IN (SELECT rowid FROM plugin_vector_records)"
                ),
                [],
            )
            .context("删除插件向量库中的孤立向量失败")?;
        transaction.commit().context("提交插件向量删除事务失败")?;
        Ok(removed)
    }

    /// 返回已绑定的维度；尚未绑定时报错而不是静默返回空结果。
    fn require_dimension(&self) -> Result<usize> {
        self.dimension.ok_or_else(|| {
            anyhow!(
                "插件向量库 `{}` 尚未绑定向量空间，请先调用 host-vector-open",
                self.path.display()
            )
        })
    }
}

/// 向 SQLite 进程全局注册 sqlite-vec 扩展。
///
/// 注册结果被缓存并跨连接复用；SQLite 拒绝注册时返回错误。
fn register_sqlite_vec() -> Result<()> {
    static REGISTER: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    REGISTER
        .get_or_init(|| unsafe {
            // sqlite-vec 暴露的是 C 入口函数指针，签名与 rusqlite 的自动扩展类型一致。
            let entry = transmute::<*const (), rusqlite::auto_extension::RawAutoExtension>(
                sqlite3_vec_init as *const (),
            );
            rusqlite::auto_extension::register_auto_extension(entry)
                .map_err(|error| format!("注册 sqlite-vec 扩展失败：{error}"))
        })
        .clone()
        .map_err(|error| anyhow!(error))
}

/// 读取一条内部元数据。
fn metadata_value(connection: &Connection, key: &str) -> Result<Option<String>> {
    connection
        .query_row(
            "SELECT value FROM plugin_vector_meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .with_context(|| format!("读取插件向量库元数据 `{key}` 失败"))
}

/// 清空并重建与向量空间绑定的表结构。
fn reset_schema(connection: &Connection, space_id: &str, dimension: u32) -> Result<()> {
    connection
        .execute_batch(&format!(
            "DROP TABLE IF EXISTS {VECTOR_TABLE};
             DELETE FROM plugin_vector_records;
             DELETE FROM plugin_vector_sync;
             DELETE FROM plugin_vector_meta;"
        ))
        .context("清理旧插件向量库失败")?;
    connection
        .execute(
            "INSERT INTO plugin_vector_meta(key, value) VALUES
             ('schema_version', ?1), ('space_id', ?2), ('dimension', ?3)",
            params![SCHEMA_VERSION, space_id, dimension.to_string()],
        )
        .context("写入插件向量库元数据失败")?;
    create_vector_table(connection, dimension)
}

/// 创建固定维度、余弦距离的 vec0 虚拟表。
fn create_vector_table(connection: &Connection, dimension: u32) -> Result<()> {
    connection
        .execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS {VECTOR_TABLE} USING vec0(
                 embedding float[{dimension}] distance_metric=cosine
             );"
        ))
        .context("创建插件向量虚拟表失败")
}

/// 在事务中新增或替换一条向量记录。
fn upsert_record(transaction: &Transaction<'_>, record: &VectorRecord) -> Result<()> {
    let existing: Option<i64> = transaction
        .query_row(
            "SELECT rowid FROM plugin_vector_records WHERE record_id = ?1",
            params![record.record_id],
            |row| row.get(0),
        )
        .optional()
        .context("查询插件向量记录失败")?;
    let rowid = match existing {
        Some(rowid) => {
            transaction
                .execute(
                    "UPDATE plugin_vector_records SET path = ?2, revision = ?3 WHERE rowid = ?1",
                    params![rowid, record.path, record.revision],
                )
                .context("更新插件向量记录失败")?;
            // vec0 没有原地更新：不先删除，同一 rowid 会插入失败或留下旧向量。
            transaction
                .execute(
                    &format!("DELETE FROM {VECTOR_TABLE} WHERE rowid = ?1"),
                    params![rowid],
                )
                .context("替换插件旧向量失败")?;
            rowid
        }
        None => {
            transaction
                .execute(
                    "INSERT INTO plugin_vector_records(record_id, path, revision)
                     VALUES (?1, ?2, ?3)",
                    params![record.record_id, record.path, record.revision],
                )
                .context("写入插件向量记录失败")?;
            transaction.last_insert_rowid()
        }
    };
    transaction
        .execute(
            &format!("INSERT INTO {VECTOR_TABLE}(rowid, embedding) VALUES (?1, ?2)"),
            params![rowid, encode_f32(&record.vector)],
        )
        .context("写入插件向量失败")?;
    Ok(())
}

/// 校验 Guest 提交的键，防止畸形输入撑爆数据库或污染诊断输出。
fn validate_key(label: &str, value: &str) -> Result<()> {
    if value.len() > MAX_KEY_BYTES {
        bail!("插件向量{label}超过 {MAX_KEY_BYTES} 字节限制");
    }
    if value.chars().any(char::is_control) {
        bail!("插件向量{label}不得包含控制字符");
    }
    Ok(())
}

/// 转义 LIKE 模式中的通配符。
///
/// 不转义时，插件传入的 `%` 会把整库匹配掉——按前缀删除必须只删真正的前缀。
fn escape_like(prefix: &str) -> String {
    let mut escaped = String::with_capacity(prefix.len());
    for character in prefix.chars() {
        if matches!(character, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// 按 sqlite-vec 约定的小端序编码 f32 向量。
fn encode_f32(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(std::mem::size_of_val(vector));
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 创建一个用完即弃的向量库目录。
    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "lucia-plugin-vector-{}-{}-{label}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("系统时间应晚于纪元")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    /// 写入的向量必须能被检索到，并且重新打开后依然存在。
    #[test]
    fn vectors_survive_reopen_and_are_searchable() {
        let root = temp_root("reopen");
        {
            let mut store =
                PluginVectorStore::open(&root, "demo", "workspace").expect("打开向量库");
            assert_eq!(store.bind_space("space-a", 3).expect("绑定空间"), 0);
            store
                .upsert(&[
                    VectorRecord {
                        record_id: "a".into(),
                        path: "src/a.rs".into(),
                        revision: Some("r1".into()),
                        vector: vec![1.0, 0.0, 0.0],
                    },
                    VectorRecord {
                        record_id: "b".into(),
                        path: "tests/b.rs".into(),
                        revision: Some("r1".into()),
                        vector: vec![0.0, 1.0, 0.0],
                    },
                ])
                .expect("写入向量");
        }

        let mut store = PluginVectorStore::open(&root, "demo", "workspace").expect("重新打开");
        assert_eq!(store.bind_space("space-a", 3).expect("绑定空间"), 2);
        let hits = store.search(&[1.0, 0.0, 0.0], 1, None).expect("检索");
        assert_eq!(hits[0].record_id, "a");
        assert!(hits[0].score > 0.99, "实际相似度 {}", hits[0].score);

        let scoped = store
            .search(&[1.0, 0.0, 0.0], 5, Some("tests/"))
            .expect("按前缀检索");
        assert_eq!(scoped.len(), 1, "前缀过滤必须只保留 tests/ 下的记录");
        assert_eq!(scoped[0].record_id, "b");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 重复写入同一记录必须替换旧向量，不能留下历史行。
    #[test]
    fn upsert_replaces_previous_vector() {
        let root = temp_root("replace");
        let mut store = PluginVectorStore::open(&root, "demo", "workspace").expect("打开向量库");
        store.bind_space("space-a", 2).expect("绑定空间");
        for vector in [vec![1.0, 0.0], vec![0.0, 1.0]] {
            store
                .upsert(&[VectorRecord {
                    record_id: "same".into(),
                    path: "src/lib.rs".into(),
                    revision: Some("r1".into()),
                    vector,
                }])
                .expect("写入向量");
        }

        let hits = store.search(&[0.0, 1.0], 10, None).expect("检索");
        assert_eq!(hits.len(), 1, "同一记录不得返回多行");
        assert!(hits[0].score > 0.99, "必须命中最新向量：{}", hits[0].score);
        assert_eq!(store.count().expect("计数"), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 向量空间变化必须整库清空，禁止跨模型复用旧向量。
    #[test]
    fn changed_space_resets_store() {
        let root = temp_root("space");
        let mut store = PluginVectorStore::open(&root, "demo", "workspace").expect("打开向量库");
        store.bind_space("space-a", 2).expect("绑定空间");
        store
            .upsert(&[VectorRecord {
                record_id: "old".into(),
                path: "src/lib.rs".into(),
                revision: None,
                vector: vec![1.0, 0.0],
            }])
            .expect("写入向量");

        assert_eq!(
            store.bind_space("space-b", 2).expect("切换空间"),
            0,
            "切换嵌入空间必须清空旧向量"
        );
        assert!(
            store
                .search(&[1.0, 0.0], 10, None)
                .expect("检索")
                .is_empty(),
            "旧空间的向量不得继续被检索到"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 对账必须同时报告缺失与版本过期，并在收尾时删除清单外的记录。
    #[test]
    fn sync_reports_stale_and_prunes_missing() {
        let root = temp_root("sync");
        let mut store = PluginVectorStore::open(&root, "demo", "workspace").expect("打开向量库");
        store.bind_space("space-a", 2).expect("绑定空间");
        store
            .upsert(&[
                VectorRecord {
                    record_id: "keep".into(),
                    path: "src/keep.rs".into(),
                    revision: Some("r1".into()),
                    vector: vec![1.0, 0.0],
                },
                VectorRecord {
                    record_id: "changed".into(),
                    path: "src/changed.rs".into(),
                    revision: Some("r1".into()),
                    vector: vec![0.0, 1.0],
                },
                VectorRecord {
                    record_id: "gone".into(),
                    path: "src/gone.rs".into(),
                    revision: Some("r1".into()),
                    vector: vec![1.0, 1.0],
                },
            ])
            .expect("写入向量");

        let (stale, removed) = store
            .sync(
                &[
                    SyncEntry {
                        record_id: "keep".into(),
                        revision: Some("r1".into()),
                    },
                    SyncEntry {
                        record_id: "changed".into(),
                        revision: Some("r2".into()),
                    },
                    SyncEntry {
                        record_id: "added".into(),
                        revision: Some("r1".into()),
                    },
                ],
                true,
                true,
            )
            .expect("对账");

        assert_eq!(stale, ["changed", "added"], "版本变化与新增都需要重新写入");
        assert_eq!(removed, 1, "清单外的 gone 必须被删除");
        assert_eq!(store.count().expect("计数"), 2);
        assert!(
            store
                .search(&[1.0, 1.0], 10, None)
                .expect("检索")
                .iter()
                .all(|hit| hit.record_id != "gone"),
            "被删除记录的向量不得残留"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 路径前缀删除不得被通配符放大成整库删除。
    #[test]
    fn prefix_removal_escapes_wildcards() {
        let root = temp_root("wildcard");
        let mut store = PluginVectorStore::open(&root, "demo", "workspace").expect("打开向量库");
        store.bind_space("space-a", 2).expect("绑定空间");
        store
            .upsert(&[
                VectorRecord {
                    record_id: "a".into(),
                    path: "src/a.rs".into(),
                    revision: None,
                    vector: vec![1.0, 0.0],
                },
                VectorRecord {
                    record_id: "b".into(),
                    path: "src/b.rs".into(),
                    revision: None,
                    vector: vec![0.0, 1.0],
                },
            ])
            .expect("写入向量");

        assert_eq!(store.remove(&[], &["%".to_string()]).expect("删除"), 0);
        assert_eq!(store.count().expect("计数"), 2, "通配符不得删除任何记录");
        assert_eq!(
            store.remove(&[], &["src/a".to_string()]).expect("删除"),
            1,
            "真实前缀必须生效"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 未绑定向量空间时必须报错，而不是静默返回空结果。
    #[test]
    fn unbound_store_rejects_operations() {
        let root = temp_root("unbound");
        let mut store = PluginVectorStore::open(&root, "demo", "workspace").expect("打开向量库");

        let error = store.upsert(&[]).expect_err("未绑定空间必须失败");
        assert!(error.to_string().contains("尚未绑定向量空间"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
