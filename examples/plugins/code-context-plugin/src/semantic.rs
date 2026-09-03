//! 语义向量索引。
//!
//! # 为什么是内存驻留
//!
//! `adr-storage.md` 把嵌入向量列为「V1 不存在」，理由是数十 MB 远超 `host-store` 的
//! 8 MiB 上限。但那条结论说的是**持久化**，不是**存在**：结构索引、符号表与倒排索引
//! 同样装不下，它们的处理方式是内存驻留、进程启动后按需重建。向量走同一条路，语义检索
//! 就能在 V1 成立，而落盘仍留给 blob 存储落地后的 V2。
//!
//! 代价要说清楚：进程重启后向量需要重新嵌入，对 API provider 意味着重新付费与等待。
//! 这不是本模块能解决的问题——它的解法就是 blob 存储。
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
