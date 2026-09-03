//! 语义向量索引。
//!
//! # 持久化与 int8 量化
//!
//! `adr-storage.md` 把嵌入向量列为「V1 不存在」，理由是 f32 原样存放远超 `host-store`
//! 的 8 MiB 上限——本仓库约 5000 个 chunk × 1024 维 × 4 字节就是 20 MB 以上。
//!
//! 但那条结论的前提是**不量化**。检索用的向量对精度并不敏感：向量已归一化，
//! 每个分量都落在 `[-1, 1]`，用一个 f32 缩放因子加 int8 分量即可还原到千分之一的误差，
//! 而体积降到四分之一。再算上 base64 的 4/3 膨胀，单条约 1.4 KB，8 MiB 能装下约 5000 条
//! ——正好是本仓库的量级。
//!
//! 因此本模块**持久化量化后的向量**：重启后直接加载，不必重新嵌入。这对 API provider
//! 是实打实的省钱与省时间。容量不足时按 [`MAX_PERSISTED_VECTORS`] 截断并如实报告，
//! 而不是写一份残缺数据——被截断的部分下次会重新嵌入，结果仍然正确。
//!
//! 真正解除容量上限仍要靠 blob 存储（见 `adr-storage.md` 的 V2），届时换掉序列化层即可，
//! 检索逻辑不受影响。
//!
//! # 为什么是线性扫描而不是 ANN
//!
//! HNSW 这类近似索引的收益出现在十万级向量以上。代码仓库的 chunk 数量通常在数千到数万，
//! 此时线性扫描的常数因子更小，且**结果精确、行为确定**——确定性对检索质量的可回归性
//! 很重要，近似索引会让同一查询在索引重建后给出不同结果。真正需要 ANN 时再换，
//! 届时本模块的对外接口不变。

#[cfg(test)]
mod tests;

use crate::model::EmbeddingSpace;
use serde::{Deserialize, Serialize};

/// 允许持久化的最大向量条数。
///
/// 宿主存储 8 MiB、每次写入重写全量。单条量化记录约 1.4 KB（1024 维 int8 经 base64
/// 膨胀后约 1366 字节，加 chunk ID 与路径约 100 字节），5000 条约 7 MiB，
/// 给 manifest 与哈希表留出余量。超出部分不写入，下次启动重新嵌入。
const MAX_PERSISTED_VECTORS: usize = 5_000;

/// 一条可持久化的量化向量记录。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SemanticRecord {
    /// 稳定 chunk ID。
    pub chunk_id: String,
    /// 所属文件路径。
    pub file_path: String,
    /// 还原量化分量所需的缩放因子。
    pub scale: f32,
    /// base64 编码的 int8 分量。
    pub vector: String,
}

/// 语义索引的持久化载荷。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SemanticSnapshot {
    /// 向量所属的嵌入空间标识；不一致时整份丢弃。
    pub space_id: String,
    /// 向量维度，用于加载时校验。
    pub dimension: u32,
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
    /// 已归一化的向量。
    vector: Vec<f32>,
}

/// 语义向量索引。
///
/// 与结构索引分离：嵌入空间变化时只需重建本索引，结构与词法索引保持有效。
/// 这正是 [`EmbeddingSpace`] 参与兼容性判断的意义所在。
#[derive(Debug, Default)]
pub struct SemanticIndex {
    /// 当前向量所属的嵌入空间；为空表示索引未建立。
    space: Option<EmbeddingSpace>,
    /// 全部向量记录。
    entries: Vec<SemanticEntry>,
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
        }
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
        self.space = Some(space);
        self.entries.clear();
    }

    /// 写入一条向量。
    ///
    /// 同一 `chunk_id` 重复写入时替换旧值，使增量重建无需先删除。向量在写入时归一化，
    /// 检索因此可以用点积代替余弦相似度。零向量被丢弃——它无法归一化，且与任何查询的
    /// 相似度都是零，留着只会白白拖慢扫描。
    pub fn upsert(&mut self, chunk_id: &str, file_path: &str, vector: &[f32]) {
        let Some(normalized) = normalize(vector) else {
            return;
        };
        match self
            .entries
            .iter_mut()
            .find(|entry| entry.chunk_id == chunk_id)
        {
            Some(existing) => {
                existing.file_path = file_path.to_string();
                existing.vector = normalized;
            }
            None => self.entries.push(SemanticEntry {
                chunk_id: chunk_id.to_string(),
                file_path: file_path.to_string(),
                vector: normalized,
            }),
        }
    }

    /// 判断某个 chunk 是否已有向量。
    ///
    /// 增量嵌入依赖它跳过已算过的 chunk：在 API 模式下重复嵌入等于重新付费。
    pub fn contains(&self, chunk_id: &str) -> bool {
        self.entries.iter().any(|entry| entry.chunk_id == chunk_id)
    }

    /// 移除一个文件的全部向量，供增量重建使用。
    pub fn remove_file(&mut self, file_path: &str) {
        self.entries.retain(|entry| entry.file_path != file_path);
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
    ) -> Vec<(&str, f32)> {
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

        let mut scored: Vec<(&str, f32)> = self
            .entries
            .iter()
            .filter(|entry| {
                path_scope.is_none_or(|scope| entry.file_path.starts_with(scope))
            })
            .map(|entry| (entry.chunk_id.as_str(), dot(&query, &entry.vector)))
            .collect();
        scored.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.0.cmp(right.0))
        });
        scored.truncate(limit);
        scored
    }
}

impl SemanticIndex {
    /// 导出可持久化的量化快照。
    ///
    /// 未声明嵌入空间时返回 `None`——没有空间标识的向量无法判断加载时是否仍然可比。
    /// 条数超过 [`MAX_PERSISTED_VECTORS`] 时截断并置位 `truncated`，
    /// 而不是放弃整份快照：存下大部分依然能省掉大部分嵌入调用。
    pub fn snapshot(&self) -> Option<SemanticSnapshot> {
        let space = self.space.as_ref()?;
        let dimension = self.entries.first().map(|entry| entry.vector.len())?;
        let truncated = self.entries.len() > MAX_PERSISTED_VECTORS;
        let records = self
            .entries
            .iter()
            .take(MAX_PERSISTED_VECTORS)
            .map(|entry| {
                let (scale, quantized) = quantize(&entry.vector);
                SemanticRecord {
                    chunk_id: entry.chunk_id.clone(),
                    file_path: entry.file_path.clone(),
                    scale,
                    vector: base64_encode(&quantized),
                }
            })
            .collect();
        Some(SemanticSnapshot {
            space_id: space.id(),
            dimension: u32::try_from(dimension).unwrap_or(u32::MAX),
            records,
            truncated,
        })
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
        for record in &snapshot.records {
            let Some(bytes) = base64_decode(&record.vector) else {
                continue;
            };
            if bytes.len() != snapshot.dimension as usize {
                continue;
            }
            let vector = dequantize(&bytes, record.scale);
            self.upsert(&record.chunk_id, &record.file_path, &vector);
        }
        !self.entries.is_empty()
    }
}

/// 把归一化向量量化为 int8 分量与一个缩放因子。
///
/// 缩放因子取最大绝对值而不是固定 `1.0`：归一化向量的分量普遍远小于 1，
/// 用固定刻度会浪费掉大部分量化区间，误差反而更大。
fn quantize(vector: &[f32]) -> (f32, Vec<u8>) {
    let peak = vector
        .iter()
        .map(|value| value.abs())
        .fold(0.0f32, f32::max);
    if peak <= f32::EPSILON {
        return (0.0, vec![0; vector.len()]);
    }
    let scale = peak / 127.0;
    let quantized = vector
        .iter()
        .map(|value| ((value / scale).round().clamp(-127.0, 127.0) as i8) as u8)
        .collect();
    (scale, quantized)
}

/// 把 int8 分量还原为浮点向量。
fn dequantize(bytes: &[u8], scale: f32) -> Vec<f32> {
    bytes
        .iter()
        .map(|byte| f32::from(*byte as i8) * scale)
        .collect()
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
    let norm = vector
        .iter()
        .map(|value| value * value)
        .sum::<f32>()
        .sqrt();
    if norm <= f32::EPSILON {
        return None;
    }
    Some(vector.iter().map(|value| value / norm).collect())
}

/// 计算两个等长向量的点积。
///
/// 两侧都已归一化时，点积即余弦相似度。
fn dot(left: &[f32], right: &[f32]) -> f32 {
    left.iter()
        .zip(right.iter())
        .map(|(a, b)| a * b)
        .sum()
}
