//! 语义向量索引。
//!
//! # 双持久化后端
//!
//! WASM Guest 没有文件写入能力，因此继续把向量压缩为 int4 快照，通过 `host-store` 的
//! Workspace 作用域持久化。原生 `search_repository` 示例不受 WASM 文件能力限制，启用
//! `sqlite-vector-store` feature 后改用 sqlite-vec 保存归一化 f32 向量并执行余弦 KNN。
//!
//! int4 快照使用一个 f32 缩放因子加四位有符号分量，把两个维度装入一个字节；容量不足时
//! 按实际 JSON 字节预算截断并如实报告。sqlite-vec 数据库与完整嵌入空间绑定，模型、端点或
//! 维度变化时清空旧向量；首次切换时会迁移兼容的旧 JSON 快照，避免重新调用付费嵌入接口。
//!
//! 两种后端都在启动时按 chunk 内容哈希丢弃过期记录，只为新增或变化的代码块生成向量。
//! sqlite-vec 写入或查询失败时保留内存副本并降级，WASM 与原生入口共享同一检索接口。
//!
//! # 为什么是线性扫描而不是 ANN
//!
//! HNSW 这类近似索引的收益出现在更大规模。代码仓库的 chunk 数量通常在数千到数万，
//! 当前内存后端使用线性精确扫描，sqlite-vec 后端使用 `vec0` 精确 KNN；两者都保持结果
//! 可回归。真正需要 ANN 时再替换存储层，对外检索接口无需变化。

#[cfg(test)]
mod tests;

use crate::model::EmbeddingSpace;
#[cfg(feature = "sqlite-vector-store")]
use crate::sqlite_vector::{SqliteVectorStore, VectorWrite};
use serde::{Deserialize, Serialize};
#[cfg(feature = "sqlite-vector-store")]
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
#[cfg(feature = "sqlite-vector-store")]
use std::path::Path;

/// 语义快照允许占用的最大 JSON 字节数。
///
/// 宿主存储总上限为 8 MiB；保留约 1 MiB 给 manifest、文件哈希表和 JSON 信封。
/// 快照构建按序列化后的真实字节数判断，而不是用固定条数猜测不同仓库的路径长度。
const MAX_SEMANTIC_SNAPSHOT_BYTES: usize = 7 * 1024 * 1024;

/// 语义快照中的向量量化格式。
///
/// `Int8` 是旧快照的兼容格式；新快照使用 `Int4`，把相邻两个维度打包进一个字节。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SemanticQuantization {
    /// 每个维度使用一个有符号字节。
    #[default]
    Int8,
    /// 每个维度使用四位有符号分量，两个维度共享一个字节。
    Int4,
}

/// 一条可持久化的量化向量记录。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SemanticRecord {
    /// 稳定 chunk ID。
    pub chunk_id: String,
    /// 所属文件路径。
    pub file_path: String,
    /// 生成该向量时对应的代码块内容哈希。
    ///
    /// 旧快照没有该字段；恢复后会被视为待重新嵌入，避免复用正文已经变化的向量。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// 还原量化分量所需的缩放因子。
    pub scale: f32,
    /// base64 编码的量化分量，具体打包方式由快照格式决定。
    pub vector: String,
}

/// 语义索引的持久化载荷。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SemanticSnapshot {
    /// 向量所属的嵌入空间标识；不一致时整份丢弃。
    pub space_id: String,
    /// 向量维度，用于加载时校验。
    pub dimension: u32,
    /// 向量分量的打包格式；旧快照缺失该字段时按 int8 读取。
    #[serde(default)]
    pub quantization: SemanticQuantization,
    /// 量化后的向量记录。
    pub records: Vec<SemanticRecord>,
    /// 是否因容量上限被截断。
    pub truncated: bool,
}

/// 索引中的一条向量记录。
#[derive(Debug, Clone)]
struct SemanticEntry {
    /// 对应的稳定 chunk ID。
    chunk_id: String,
    /// 所属文件路径，用于 `path_scope` 过滤。
    file_path: String,
    /// 生成向量时对应的代码块内容哈希。
    content_hash: Option<String>,
    /// 已归一化的向量。
    vector: Vec<f32>,
}

/// 语义向量索引。
///
/// 与结构索引分离：嵌入空间变化时只需重建本索引，结构与词法索引保持有效。
/// 这正是 [`EmbeddingSpace`] 参与兼容性判断的意义所在。
#[derive(Debug)]
pub struct SemanticIndex {
    /// 当前向量所属的嵌入空间；为空表示索引未建立。
    space: Option<EmbeddingSpace>,
    /// 全部向量记录。
    entries: Vec<SemanticEntry>,
    /// 原生搜索入口使用的 sqlite-vec 后端；WASM 插件构建不包含该字段。
    #[cfg(feature = "sqlite-vector-store")]
    sqlite: Option<SqliteVectorStore>,
    /// 最近一次 sqlite-vec 错误；调用方读取后清空并决定是否降级。
    #[cfg(feature = "sqlite-vector-store")]
    backend_error: RefCell<Option<String>>,
    /// sqlite-vec 是否仍可用于本轮运行；失败后固定退回内存检索与 JSON 快照。
    #[cfg(feature = "sqlite-vector-store")]
    sqlite_healthy: Cell<bool>,
}

impl Default for SemanticIndex {
    fn default() -> Self {
        Self {
            space: None,
            entries: Vec::new(),
            #[cfg(feature = "sqlite-vector-store")]
            sqlite: None,
            #[cfg(feature = "sqlite-vector-store")]
            backend_error: RefCell::new(None),
            #[cfg(feature = "sqlite-vector-store")]
            sqlite_healthy: Cell::new(true),
        }
    }
}

impl SemanticIndex {
    /// 创建空索引。
    pub fn new() -> Self {
        Self::default()
    }

    /// 按嵌入空间创建索引。
    pub fn with_space(space: EmbeddingSpace) -> Self {
        Self {
            space: Some(space),
            entries: Vec::new(),
            #[cfg(feature = "sqlite-vector-store")]
            sqlite: None,
            #[cfg(feature = "sqlite-vector-store")]
            backend_error: RefCell::new(None),
            #[cfg(feature = "sqlite-vector-store")]
            sqlite_healthy: Cell::new(true),
        }
    }

    /// 使用本地 sqlite-vec 数据库创建语义索引，并恢复已持久化向量。
    ///
    /// 仅原生 `sqlite-vector-store` feature 可用。数据库与完整嵌入空间绑定；空间变化时
    /// sqlite-vec 会安全清空旧记录。
    ///
    /// # Errors
    ///
    /// 数据库文件无法创建、扩展初始化失败或已有记录损坏时返回错误。
    #[cfg(feature = "sqlite-vector-store")]
    pub fn with_sqlite(space: EmbeddingSpace, path: impl AsRef<Path>) -> Result<Self, String> {
        let (sqlite, records) = SqliteVectorStore::open(path, &space.id(), space.dimension)?;
        let entries = records
            .into_iter()
            .map(|record| SemanticEntry {
                chunk_id: record.chunk_id,
                file_path: record.file_path,
                content_hash: record.content_hash,
                vector: record.vector,
            })
            .collect();
        Ok(Self {
            space: Some(space),
            entries,
            sqlite: Some(sqlite),
            backend_error: RefCell::new(None),
            sqlite_healthy: Cell::new(true),
        })
    }

    /// 返回 sqlite-vec 数据库路径；内存索引返回 `None`。
    #[cfg(feature = "sqlite-vector-store")]
    pub fn sqlite_path(&self) -> Option<&Path> {
        self.sqlite.as_ref().map(SqliteVectorStore::path)
    }

    /// 取出最近一次 sqlite-vec 错误。
    #[cfg(feature = "sqlite-vector-store")]
    pub fn take_backend_error(&self) -> Option<String> {
        self.backend_error.borrow_mut().take()
    }

    /// 标记 sqlite-vec 在本轮运行中不可再用，并保存首个待上报错误。
    #[cfg(feature = "sqlite-vector-store")]
    fn disable_sqlite(&self, error: String) {
        self.sqlite_healthy.set(false);
        *self.backend_error.borrow_mut() = Some(error);
    }

    /// 返回当前嵌入空间。
    pub fn space(&self) -> Option<&EmbeddingSpace> {
        self.space.as_ref()
    }

    /// 向量条数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 索引是否为空。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 索引是否可用于检索。
    ///
    /// 声明了嵌入空间但没有任何向量时不可用：此时检索只会返回空结果，
    /// 让调用方误以为「仓库里没有语义相关的代码」。
    pub fn is_ready(&self) -> bool {
        self.space.is_some() && !self.entries.is_empty()
    }

    /// 判断给定嵌入空间是否与当前索引兼容。
    ///
    /// 不兼容意味着已有向量与新查询向量不可比较，必须重建。空索引与任何空间兼容。
    pub fn is_compatible_with(&self, space: &EmbeddingSpace) -> bool {
        match &self.space {
            None => true,
            Some(current) => current.is_compatible_with(space),
        }
    }

    /// 清空索引并切换到新的嵌入空间。
    pub fn reset(&mut self, space: EmbeddingSpace) {
        #[cfg(feature = "sqlite-vector-store")]
        if self.sqlite_healthy.get() {
            let error = self
                .sqlite
                .as_mut()
                .and_then(|sqlite| sqlite.reset(&space.id(), space.dimension).err());
            if let Some(error) = error {
                self.disable_sqlite(error);
            }
        }
        self.space = Some(space);
        self.entries.clear();
    }

    /// 写入一条向量。
    ///
    /// 同一 `chunk_id` 重复写入时替换旧值，使增量重建无需先删除。向量在写入时归一化，
    /// 检索因此可以用点积代替余弦相似度。零向量被丢弃——它无法归一化，且与任何查询的
    /// 相似度都是零，留着只会白白拖慢扫描。
    pub fn upsert(
        &mut self,
        chunk_id: &str,
        file_path: &str,
        content_hash: Option<&str>,
        vector: &[f32],
    ) {
        self.upsert_batch(&[(chunk_id, file_path, content_hash, vector)]);
    }

    /// 批量写入向量，并在 sqlite-vec 模式下用单个事务持久化整批数据。
    ///
    /// 无效零向量或包含非有限数的向量会被跳过；sqlite-vec 写入失败时内存索引仍保留本轮
    /// 可用结果，并通过 [`Self::take_backend_error`] 把错误交给上层报告。
    pub fn upsert_batch(&mut self, records: &[(&str, &str, Option<&str>, &[f32])]) {
        let normalized: Vec<(String, String, Option<String>, Vec<f32>)> = records
            .iter()
            .filter_map(|(chunk_id, file_path, content_hash, vector)| {
                normalize(vector).map(|vector| {
                    (
                        (*chunk_id).to_string(),
                        (*file_path).to_string(),
                        content_hash.map(str::to_string),
                        vector,
                    )
                })
            })
            .collect();
        for (chunk_id, file_path, content_hash, vector) in &normalized {
            match self
                .entries
                .iter_mut()
                .find(|entry| entry.chunk_id == *chunk_id)
            {
                Some(existing) => {
                    existing.file_path.clone_from(file_path);
                    existing.content_hash.clone_from(content_hash);
                    existing.vector.clone_from(vector);
                }
                None => self.entries.push(SemanticEntry {
                    chunk_id: chunk_id.clone(),
                    file_path: file_path.clone(),
                    content_hash: content_hash.clone(),
                    vector: vector.clone(),
                }),
            }
        }
        #[cfg(feature = "sqlite-vector-store")]
        if self.sqlite_healthy.get() {
            let writes: Vec<VectorWrite<'_>> = normalized
                .iter()
                .map(|(chunk_id, file_path, content_hash, vector)| VectorWrite {
                    chunk_id,
                    file_path,
                    content_hash: content_hash.as_deref(),
                    vector,
                })
                .collect();
            let error = self
                .sqlite
                .as_mut()
                .and_then(|sqlite| sqlite.upsert_batch(&writes).err());
            if let Some(error) = error {
                self.disable_sqlite(error);
            }
        }
    }

    /// 判断某个 chunk 是否已有与当前正文一致的向量。
    ///
    /// 增量嵌入依赖它跳过已算过的 chunk：在 API 模式下重复嵌入等于重新付费；只比较
    /// chunk ID 会错误复用正文变化后的旧向量，因此内容哈希必须同时一致。
    pub fn contains_current(&self, chunk_id: &str, content_hash: &str) -> bool {
        self.entries.iter().any(|entry| {
            entry.chunk_id == chunk_id && entry.content_hash.as_deref() == Some(content_hash)
        })
    }

    /// 移除当前结构索引中已不存在或内容哈希已变化的向量。
    ///
    /// 返回删除条数。调用方应在计算待嵌入集合前执行，使删除、重命名和正文变化不会在
    /// 持久快照中累积孤儿记录。
    pub fn retain_current(&mut self, chunks: &[(&str, &str)]) -> usize {
        let current: HashMap<&str, &str> = chunks.iter().copied().collect();
        let before = self.entries.len();
        #[cfg(feature = "sqlite-vector-store")]
        let removed_ids: Vec<String> = self
            .entries
            .iter()
            .filter(|entry| {
                current
                    .get(entry.chunk_id.as_str())
                    .is_none_or(|hash| entry.content_hash.as_deref() != Some(*hash))
            })
            .map(|entry| entry.chunk_id.clone())
            .collect();
        self.entries.retain(|entry| {
            current
                .get(entry.chunk_id.as_str())
                .is_some_and(|hash| entry.content_hash.as_deref() == Some(*hash))
        });
        #[cfg(feature = "sqlite-vector-store")]
        if self.sqlite_healthy.get() {
            let error = self
                .sqlite
                .as_mut()
                .and_then(|sqlite| sqlite.delete_ids(&removed_ids).err());
            if let Some(error) = error {
                self.disable_sqlite(error);
            }
        }
        before - self.entries.len()
    }

    /// 移除一个文件的全部向量，供增量重建使用。
    pub fn remove_file(&mut self, file_path: &str) {
        #[cfg(feature = "sqlite-vector-store")]
        let removed_ids: Vec<String> = self
            .entries
            .iter()
            .filter(|entry| entry.file_path == file_path)
            .map(|entry| entry.chunk_id.clone())
            .collect();
        self.entries.retain(|entry| entry.file_path != file_path);
        #[cfg(feature = "sqlite-vector-store")]
        if self.sqlite_healthy.get() {
            let error = self
                .sqlite
                .as_mut()
                .and_then(|sqlite| sqlite.delete_ids(&removed_ids).err());
            if let Some(error) = error {
                self.disable_sqlite(error);
            }
        }
    }

    /// 执行一次语义检索，返回按相似度降序的 `(chunk_id, 相似度)`。
    ///
    /// `query_vector` 会被归一化后与库内向量做点积。`path_scope` 非空时只保留路径以其
    /// 为前缀的向量。相似度相同时按 chunk ID 排序，保证结果确定。
    pub fn search(
        &self,
        query_vector: &[f32],
        path_scope: Option<&str>,
        limit: usize,
    ) -> Vec<(String, f32)> {
        if limit == 0 || self.entries.is_empty() {
            return Vec::new();
        }
        let Some(query) = normalize(query_vector) else {
            return Vec::new();
        };
        // 维度不符说明查询向量与库内向量来自不同嵌入空间，点积没有意义。
        let Some(dimension) = self.entries.first().map(|entry| entry.vector.len()) else {
            return Vec::new();
        };
        if query.len() != dimension {
            return Vec::new();
        }

        #[cfg(feature = "sqlite-vector-store")]
        if self.sqlite_healthy.get() {
            if let Some(sqlite) = self.sqlite.as_ref() {
                match sqlite.search(&query, path_scope, limit, self.entries.len()) {
                    Ok(matches) => return matches,
                    Err(error) => self.disable_sqlite(error),
                }
            }
        }

        let mut scored: Vec<(String, f32)> = self
            .entries
            .iter()
            .filter(|entry| path_scope.is_none_or(|scope| entry.file_path.starts_with(scope)))
            .map(|entry| (entry.chunk_id.clone(), dot(&query, &entry.vector)))
            .collect();
        scored.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.0.cmp(&right.0))
        });
        scored.truncate(limit);
        scored
    }
}

impl SemanticIndex {
    /// 导出可持久化的量化快照。
    ///
    /// 未声明嵌入空间时返回 `None`——没有空间标识的向量无法判断加载时是否仍然可比。
    /// 序列化体积超过 [`MAX_SEMANTIC_SNAPSHOT_BYTES`] 时截断并置位 `truncated`，
    /// 而不是放弃整份快照：存下大部分依然能省掉大部分嵌入调用。
    pub fn snapshot(&self) -> Option<SemanticSnapshot> {
        #[cfg(feature = "sqlite-vector-store")]
        if self.sqlite.is_some() && self.sqlite_healthy.get() {
            return None;
        }
        self.snapshot_with_budget(MAX_SEMANTIC_SNAPSHOT_BYTES)
    }

    /// 按给定 JSON 字节预算导出 int4 快照。
    ///
    /// 该内部入口让容量边界可以用小夹具精确测试；正式调用统一使用宿主预算。
    fn snapshot_with_budget(&self, max_bytes: usize) -> Option<SemanticSnapshot> {
        let space = self.space.as_ref()?;
        let dimension = self.entries.first().map(|entry| entry.vector.len())?;
        let mut snapshot = SemanticSnapshot {
            space_id: space.id(),
            dimension: u32::try_from(dimension).unwrap_or(u32::MAX),
            quantization: SemanticQuantization::Int4,
            records: Vec::new(),
            truncated: false,
        };
        let mut serialized_bytes = serde_json::to_vec(&snapshot).ok()?.len();
        for entry in &self.entries {
            let (scale, quantized) = quantize_int4(&entry.vector);
            let record = SemanticRecord {
                chunk_id: entry.chunk_id.clone(),
                file_path: entry.file_path.clone(),
                content_hash: entry.content_hash.clone(),
                scale,
                vector: base64_encode(&quantized),
            };
            let separator = usize::from(!snapshot.records.is_empty());
            let record_bytes = serde_json::to_vec(&record).ok()?.len() + separator;
            if serialized_bytes.saturating_add(record_bytes) > max_bytes {
                snapshot.truncated = true;
                break;
            }
            serialized_bytes += record_bytes;
            snapshot.records.push(record);
        }
        Some(snapshot)
    }

    /// 从持久化快照恢复向量。
    ///
    /// 嵌入空间标识不一致时**整份丢弃并返回 `false`**：混用不同空间的向量会让检索结果
    /// 长期缓慢地不正确，重新嵌入的代价远小于给出错误结果。维度不符的单条记录被跳过。
    pub fn restore(&mut self, space: &EmbeddingSpace, snapshot: &SemanticSnapshot) -> bool {
        if snapshot.space_id != space.id() {
            return false;
        }
        self.reset(space.clone());
        let mut restored = Vec::new();
        for record in &snapshot.records {
            let Some(bytes) = base64_decode(&record.vector) else {
                continue;
            };
            let dimension = snapshot.dimension as usize;
            let expected_bytes = match snapshot.quantization {
                SemanticQuantization::Int8 => dimension,
                SemanticQuantization::Int4 => dimension.div_ceil(2),
            };
            if bytes.len() != expected_bytes {
                continue;
            }
            let vector = match snapshot.quantization {
                SemanticQuantization::Int8 => dequantize_int8(&bytes, record.scale),
                SemanticQuantization::Int4 => dequantize_int4(&bytes, record.scale, dimension),
            };
            restored.push((
                record.chunk_id.clone(),
                record.file_path.clone(),
                record.content_hash.clone(),
                vector,
            ));
        }
        let records: Vec<(&str, &str, Option<&str>, &[f32])> = restored
            .iter()
            .map(|(chunk_id, file_path, content_hash, vector)| {
                (
                    chunk_id.as_str(),
                    file_path.as_str(),
                    content_hash.as_deref(),
                    vector.as_slice(),
                )
            })
            .collect();
        self.upsert_batch(&records);
        !self.entries.is_empty()
    }
}

/// 把归一化向量量化为 int4 分量与一个缩放因子。
///
/// 缩放因子取最大绝对值而不是固定 `1.0`：归一化向量的分量普遍远小于 1，
/// 用固定刻度会浪费掉大部分量化区间，误差反而更大。
fn quantize_int4(vector: &[f32]) -> (f32, Vec<u8>) {
    let peak = vector
        .iter()
        .map(|value| value.abs())
        .fold(0.0f32, f32::max);
    if peak <= f32::EPSILON {
        return (0.0, vec![0x88; vector.len().div_ceil(2)]);
    }
    let scale = peak / 7.0;
    let mut packed = Vec::with_capacity(vector.len().div_ceil(2));
    for pair in vector.chunks(2) {
        let high = quantize_nibble(pair[0], scale);
        let low = pair
            .get(1)
            .map_or(8, |value| quantize_nibble(*value, scale));
        packed.push((high << 4) | low);
    }
    (scale, packed)
}

/// 把一个浮点分量压缩为偏移后的四位有符号值。
fn quantize_nibble(value: f32, scale: f32) -> u8 {
    let quantized = (value / scale).round().clamp(-7.0, 7.0) as i8;
    (quantized + 8) as u8
}

/// 把旧快照的 int8 分量还原为浮点向量。
fn dequantize_int8(bytes: &[u8], scale: f32) -> Vec<f32> {
    bytes
        .iter()
        .map(|byte| f32::from(*byte as i8) * scale)
        .collect()
}

/// 把打包的 int4 分量还原为指定维度的浮点向量。
fn dequantize_int4(bytes: &[u8], scale: f32, dimension: usize) -> Vec<f32> {
    let mut vector = Vec::with_capacity(dimension);
    for byte in bytes {
        for nibble in [byte >> 4, byte & 0x0F] {
            if vector.len() == dimension {
                return vector;
            }
            vector.push(f32::from(nibble as i8 - 8) * scale);
        }
    }
    vector
}

/// base64 字母表。
const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 把字节编码为 base64。
///
/// 自实现而不是引入依赖：插件产物已有 3.4 MB，为二十行代码再加一个 crate 不划算，
/// 且这里只需要标准字母表与填充。
fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let block = (u32::from(group[0]) << 16)
            | (group.get(1).map_or(0, |byte| u32::from(*byte)) << 8)
            | group.get(2).map_or(0, |byte| u32::from(*byte));
        for offset in 0..4 {
            if offset <= group.len() {
                let index = (block >> (18 - offset * 6)) & 0x3F;
                out.push(char::from(BASE64_ALPHABET[index as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// 解码 base64；含非法字符时返回 `None`。
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    /// 把 base64 字符映射回 6 位值。
    fn value_of(character: u8) -> Option<u32> {
        match character {
            b'A'..=b'Z' => Some(u32::from(character - b'A')),
            b'a'..=b'z' => Some(u32::from(character - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(character - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let raw: Vec<u8> = text.bytes().filter(|byte| *byte != b'=').collect();
    let mut out = Vec::with_capacity(raw.len() * 3 / 4);
    for group in raw.chunks(4) {
        let mut block = 0u32;
        for (offset, character) in group.iter().enumerate() {
            block |= value_of(*character)? << (18 - offset * 6);
        }
        // 4 个字符还原 3 字节，末组按实际字符数决定还原几个。
        for offset in 0..group.len().saturating_sub(1) {
            out.push(((block >> (16 - offset * 8)) & 0xFF) as u8);
        }
    }
    Some(out)
}

/// 归一化向量；零向量或含非有限分量时返回 `None`。
///
/// 非有限分量必须挡住：一个 `NaN` 会让它参与的全部比较返回 `false`，
/// 排序结果随之变成未定义，且不会有任何报错。
fn normalize(vector: &[f32]) -> Option<Vec<f32>> {
    if vector.is_empty() || vector.iter().any(|value| !value.is_finite()) {
        return None;
    }
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm <= f32::EPSILON {
        return None;
    }
    Some(vector.iter().map(|value| value / norm).collect())
}

/// 计算两个等长向量的点积。
///
/// 两侧都已归一化时，点积即余弦相似度。
fn dot(left: &[f32], right: &[f32]) -> f32 {
    left.iter().zip(right.iter()).map(|(a, b)| a * b).sum()
}
