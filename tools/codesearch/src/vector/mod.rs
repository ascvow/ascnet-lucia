//! USearch 向量索引：可从 SQLite 完全重建的派生索引。
//!
//! SQLite 与 USearch 没有共同事务，因此一致性依靠三件事：
//!
//! 1. **持久化有序更新日志**（`vector_ops` 表）记录每一次 add / remove；
//! 2. **幂等重放**：`add` 先移除同 key 再插入，`remove` 对缺失 key 无副作用；
//! 3. **检查点**：快照文件 + 原子发布的 manifest，manifest 记录 `durable_applied_seq`。
//!
//! # 崩溃恢复
//!
//! | 崩溃位置 | 恢复行为 |
//! |---|---|
//! | SQLite 已提交、USearch 未更新 | 启动后重放日志补齐 |
//! | USearch 内存已更新、检查点未发布 | manifest 仍指向旧 generation，重放旧 seq 之后的日志 |
//! | 检查点写到一半 | 新快照文件未被 manifest 引用，直接忽略并重放 |
//! | 快照缺失或损坏 | 从 SQLite 已存向量重建，全程不请求模型 |
//!
//! 发布检查点时先完整写入并 `fsync` 新 generation 快照，再原子 rename manifest，
//! 最后才删除旧 generation，因此任何时刻 manifest 都指向一个写入完整的文件。

use std::collections::HashSet;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use usearch::ffi::{IndexOptions, MetricKind, ScalarKind};
use usearch::Index;

use crate::error::{Error, Result};
use crate::store::{chunk_id_from_key, vector_key, Store, VectorOp};

/// manifest 格式版本。
const MANIFEST_VERSION: u32 = 1;
/// 单次重放的最大操作数，避免一次性拉取过多日志。
const REPLAY_BATCH: usize = 4096;
/// HNSW 连接度，使用 USearch 默认量级。
const CONNECTIVITY: usize = 16;
/// 索引容量按此步长增长，减少频繁 reserve。
const CAPACITY_STEP: usize = 4096;

/// 检查点 manifest。原子 rename 发布，内容自校验。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// 格式版本。
    pub version: u32,
    /// 对应的 Embedding 空间。
    pub space_id: String,
    /// 快照 generation。
    pub generation: u64,
    /// 快照文件名（位于同一目录）。
    pub file_name: String,
    /// 该快照已包含的更新日志序号。
    pub durable_applied_seq: i64,
    /// 快照中的向量数量。
    pub vector_count: usize,
    /// 向量维度。
    pub dimensions: usize,
    /// 快照文件字节数，用于快速校验。
    pub file_size: u64,
    /// 快照文件 blake3 哈希，用于检测损坏。
    pub file_hash: String,
    /// 发布时间（UNIX 秒）。
    pub created_at: i64,
}

/// 索引加载来源，供 `status` 与测试观察恢复路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadOrigin {
    /// 目录中没有任何检查点，新建空索引。
    Fresh,
    /// 从有效检查点加载。
    Checkpoint,
    /// 检查点缺失或损坏，从 SQLite 已存向量重建。
    RebuiltFromStore,
}

impl LoadOrigin {
    /// 稳定的字符串标识。
    pub fn as_str(&self) -> &'static str {
        match self {
            LoadOrigin::Fresh => "fresh",
            LoadOrigin::Checkpoint => "checkpoint",
            LoadOrigin::RebuiltFromStore => "rebuilt_from_store",
        }
    }
}

/// USearch 向量索引句柄。
pub struct VectorIndex {
    index: Index,
    dir: PathBuf,
    space_id: String,
    dimensions: usize,
    applied_seq: i64,
    durable_seq: i64,
    generation: u64,
    origin: LoadOrigin,
}

impl std::fmt::Debug for VectorIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VectorIndex")
            .field("space_id", &self.space_id)
            .field("dimensions", &self.dimensions)
            .field("applied_seq", &self.applied_seq)
            .field("durable_seq", &self.durable_seq)
            .field("generation", &self.generation)
            .field("origin", &self.origin)
            .field("len", &self.len())
            .finish()
    }
}

impl VectorIndex {
    /// 打开某个 Embedding 空间的向量索引。
    ///
    /// 依次尝试：加载匹配的有效检查点 → 从 SQLite 重建 → 新建空索引。
    /// 恢复过程不访问网络，也不请求 Embedding 服务。
    pub fn open(dir: &Path, store: &Store, space_id: &str, dimensions: usize) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::io(format!("创建向量目录 {}", dir.display()), e))?;
        let mut instance = Self {
            index: new_index(dimensions, CAPACITY_STEP)?,
            dir: dir.to_path_buf(),
            space_id: space_id.to_string(),
            dimensions,
            applied_seq: 0,
            durable_seq: 0,
            generation: 0,
            origin: LoadOrigin::Fresh,
        };

        match instance.load_checkpoint() {
            Ok(Some(manifest)) => {
                instance.applied_seq = manifest.durable_applied_seq;
                instance.durable_seq = manifest.durable_applied_seq;
                instance.generation = manifest.generation;
                instance.origin = LoadOrigin::Checkpoint;
            }
            Ok(None) => {
                // 目录里没有检查点：若 SQLite 已有向量则重建，否则保持空索引。
                instance.rebuild_from_store(store)?;
            }
            Err(error) => {
                tracing::warn!(
                    space = %space_id,
                    error = %error,
                    "向量检查点不可用，改为从 SQLite 已存向量重建"
                );
                instance.index = new_index(dimensions, CAPACITY_STEP)?;
                instance.rebuild_from_store(store)?;
            }
        }

        instance.replay(store)?;
        Ok(instance)
    }

    /// 仅在内存中创建索引（测试使用），不读写磁盘检查点。
    pub fn in_memory(dimensions: usize) -> Result<Self> {
        Ok(Self {
            index: new_index(dimensions, CAPACITY_STEP)?,
            dir: PathBuf::new(),
            space_id: String::new(),
            dimensions,
            applied_seq: 0,
            durable_seq: 0,
            generation: 0,
            origin: LoadOrigin::Fresh,
        })
    }

    /// 向量维度。
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// 索引中的向量数量。
    pub fn len(&self) -> usize {
        self.index.size()
    }

    /// 索引是否为空。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 内存中已应用到的更新日志序号。
    pub fn applied_seq(&self) -> i64 {
        self.applied_seq
    }

    /// 已持久化检查点对应的序号。
    ///
    /// 与 [`Self::applied_seq`] 的差值就是崩溃后需要重放的部分。
    /// "已写入内存" 绝不等于 "已持久化"。
    pub fn durable_seq(&self) -> i64 {
        self.durable_seq
    }

    /// 本次加载的来源。
    pub fn origin(&self) -> LoadOrigin {
        self.origin
    }

    /// 该索引对应的 Embedding 空间。
    pub fn space_id(&self) -> &str {
        &self.space_id
    }

    /// 重放 SQLite 中尚未应用的向量操作。
    ///
    /// 按 `seq` 升序处理，因此不会出现旧操作覆盖新操作。
    pub fn replay(&mut self, store: &Store) -> Result<usize> {
        if self.space_id.is_empty() {
            return Ok(0);
        }
        let mut total = 0usize;
        loop {
            let ops = store.vector_ops_since(&self.space_id, self.applied_seq, REPLAY_BATCH)?;
            if ops.is_empty() {
                break;
            }
            let count = ops.len();
            for op in ops {
                self.apply(store, &op)?;
            }
            total += count;
            if count < REPLAY_BATCH {
                break;
            }
        }
        Ok(total)
    }

    /// 应用一条更新日志。重复应用同一条操作结果相同（幂等）。
    fn apply(&mut self, store: &Store, op: &VectorOp) -> Result<()> {
        let key = vector_key(op.vector_key)?;
        match op.op.as_str() {
            "add" => {
                let Some(cache_key) = op.cache_key.as_deref() else {
                    return Err(Error::Vector(format!(
                        "add 操作 seq={} 缺少缓存键",
                        op.seq
                    )));
                };
                match store.cached_vector(&self.space_id, cache_key)? {
                    Some(vector) => self.upsert(key, &vector)?,
                    None => {
                        // 缓存被清理时无法补齐该向量，跳过但记录，避免整体重放失败。
                        tracing::warn!(
                            seq = op.seq,
                            chunk = op.vector_key,
                            "缓存中缺少向量，跳过该 add 操作"
                        );
                    }
                }
            }
            "remove" => {
                self.index
                    .remove(key)
                    .map_err(|e| Error::Vector(format!("移除向量 {key} 失败: {e}")))?;
            }
            other => {
                return Err(Error::Vector(format!("未知向量操作 `{other}`")));
            }
        }
        self.applied_seq = self.applied_seq.max(op.seq);
        Ok(())
    }

    /// 插入或替换一个向量。先移除同 key 再插入，保证幂等。
    pub fn upsert(&mut self, key: u64, vector: &[f32]) -> Result<()> {
        if vector.len() != self.dimensions {
            return Err(Error::Vector(format!(
                "向量维度 {} 与索引维度 {} 不一致",
                vector.len(),
                self.dimensions
            )));
        }
        if self.index.contains(key) {
            self.index
                .remove(key)
                .map_err(|e| Error::Vector(format!("替换前移除向量 {key} 失败: {e}")))?;
        }
        self.ensure_capacity(self.index.size() + 1)?;
        self.index
            .add(key, vector)
            .map_err(|e| Error::Vector(format!("写入向量 {key} 失败: {e}")))?;
        Ok(())
    }

    /// 确保索引至少能容纳 `needed` 个向量。
    ///
    /// 容量以 USearch 自身报告的值为准：`load` 会把容量重置为快照中的向量数，
    /// 在宿主侧缓存容量会与真实状态脱节，导致后续插入报 "Reserve capacity ahead of insertions!"。
    fn ensure_capacity(&mut self, needed: usize) -> Result<()> {
        if needed <= self.index.capacity() {
            return Ok(());
        }
        let target = needed.next_multiple_of(CAPACITY_STEP);
        self.index
            .reserve(target)
            .map_err(|e| Error::Vector(format!("扩容到 {target} 失败: {e}")))?;
        Ok(())
    }

    /// 从 SQLite 中已保存的向量重建索引，不请求模型。
    pub fn rebuild_from_store(&mut self, store: &Store) -> Result<usize> {
        if self.space_id.is_empty() {
            return Ok(0);
        }
        let vectors = store.ready_vectors(&self.space_id)?;
        if vectors.is_empty() {
            self.applied_seq = 0;
            self.origin = LoadOrigin::Fresh;
            return Ok(0);
        }
        self.index = new_index(self.dimensions, vectors.len().max(CAPACITY_STEP))?;
        for (chunk_id, vector) in &vectors {
            self.upsert(vector_key(*chunk_id)?, vector)?;
        }
        // 重建结果等价于应用了全部日志，因此把已应用序号推进到当前最大值。
        self.applied_seq = store.max_vector_seq(&self.space_id)?;
        self.durable_seq = 0;
        self.origin = LoadOrigin::RebuiltFromStore;
        Ok(vectors.len())
    }

    /// 近邻检索。`allowed` 为空集合时直接返回空结果。
    ///
    /// 过滤回调只查内存中的允许集合，不会对每个候选执行 SQL。
    /// 返回 `(chunk_id, distance)`，按距离升序。
    pub fn search(
        &self,
        query: &[f32],
        count: usize,
        allowed: &HashSet<i64>,
    ) -> Result<Vec<(i64, f32)>> {
        if count == 0 || self.is_empty() || allowed.is_empty() {
            return Ok(Vec::new());
        }
        if query.len() != self.dimensions {
            return Err(Error::Vector(format!(
                "查询向量维度 {} 与索引维度 {} 不一致",
                query.len(),
                self.dimensions
            )));
        }
        let matches = self
            .index
            .filtered_search(query, count, |key| {
                i64::try_from(key).is_ok_and(|id| allowed.contains(&id))
            })
            .map_err(|e| Error::Vector(format!("向量检索失败: {e}")))?;
        let mut out = Vec::with_capacity(matches.keys.len());
        for (key, distance) in matches.keys.iter().zip(matches.distances.iter()) {
            let chunk_id = chunk_id_from_key(*key)?;
            if allowed.contains(&chunk_id) {
                out.push((chunk_id, *distance));
            }
        }
        Ok(out)
    }

    /// 发布检查点。
    ///
    /// 先把快照写入新 generation 文件并 `fsync`，再原子 rename manifest，
    /// 最后删除旧 generation。旧检查点在新检查点成功发布前始终有效。
    pub fn checkpoint(&mut self) -> Result<Option<Manifest>> {
        if self.space_id.is_empty() || self.dir.as_os_str().is_empty() {
            return Ok(None);
        }
        if self.applied_seq == self.durable_seq && self.generation > 0 {
            return Ok(None);
        }
        let generation = self.generation + 1;
        let file_name = snapshot_name(&self.space_id, generation);
        let snapshot_path = self.dir.join(&file_name);

        self.index
            .save(snapshot_path.to_str().ok_or_else(|| {
                Error::Vector(format!("快照路径不是合法 UTF-8: {}", snapshot_path.display()))
            })?)
            .map_err(|e| Error::Vector(format!("保存向量快照失败: {e}")))?;
        // USearch 自行写文件，这里再打开一次做 fsync，确保内容落盘。
        sync_file(&snapshot_path)?;

        let bytes = std::fs::read(&snapshot_path)
            .map_err(|e| Error::io(format!("读取快照 {}", snapshot_path.display()), e))?;
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            space_id: self.space_id.clone(),
            generation,
            file_name,
            durable_applied_seq: self.applied_seq,
            vector_count: self.index.size(),
            dimensions: self.dimensions,
            file_size: bytes.len() as u64,
            file_hash: blake3::hash(&bytes).to_hex().to_string(),
            created_at: crate::store::now_secs(),
        };

        let previous_generation = self.generation;
        self.publish_manifest(&manifest)?;
        self.durable_seq = manifest.durable_applied_seq;
        self.generation = generation;

        // 新 manifest 已生效，旧快照可以安全删除。
        if previous_generation > 0 {
            let old = self.dir.join(snapshot_name(&self.space_id, previous_generation));
            if let Err(error) = std::fs::remove_file(&old) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::debug!(path = %old.display(), error = %error, "删除旧向量快照失败");
                }
            }
        }
        Ok(Some(manifest))
    }

    /// 原子发布 manifest：写临时文件 → fsync → rename → fsync 目录。
    fn publish_manifest(&self, manifest: &Manifest) -> Result<()> {
        let target = self.manifest_path();
        let temporary = self.dir.join(format!(
            "{}.manifest.tmp",
            sanitize(&self.space_id)
        ));
        let json = serde_json::to_vec_pretty(manifest)?;
        {
            let mut file = File::create(&temporary)
                .map_err(|e| Error::io(format!("创建 {}", temporary.display()), e))?;
            file.write_all(&json)
                .map_err(|e| Error::io("写入 manifest", e))?;
            file.sync_all()
                .map_err(|e| Error::io("同步 manifest", e))?;
        }
        std::fs::rename(&temporary, &target)
            .map_err(|e| Error::io(format!("发布 manifest 到 {}", target.display()), e))?;
        // 目录项本身也需要落盘，否则崩溃后 rename 可能丢失。
        if let Ok(dir) = File::open(&self.dir) {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    fn manifest_path(&self) -> PathBuf {
        self.dir
            .join(format!("{}.manifest.json", sanitize(&self.space_id)))
    }

    /// 读取并校验 manifest，然后加载对应快照。
    ///
    /// 返回 `Ok(None)` 表示目录中没有检查点；校验失败返回 `Err`，由调用方走重建路径。
    fn load_checkpoint(&mut self) -> Result<Option<Manifest>> {
        let path = self.manifest_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(Error::io(format!("读取 {}", path.display()), error));
            }
        };
        let manifest: Manifest = serde_json::from_str(&text)
            .map_err(|e| Error::Vector(format!("manifest 解析失败: {e}")))?;
        if manifest.version != MANIFEST_VERSION {
            return Err(Error::Vector(format!(
                "manifest 版本 {} 不受支持",
                manifest.version
            )));
        }
        if manifest.space_id != self.space_id {
            return Err(Error::Vector(format!(
                "manifest 属于空间 {}，与当前 {} 不匹配",
                manifest.space_id, self.space_id
            )));
        }
        if manifest.dimensions != self.dimensions {
            return Err(Error::Vector(format!(
                "manifest 维度 {} 与当前 {} 不匹配",
                manifest.dimensions, self.dimensions
            )));
        }
        let snapshot = self.dir.join(&manifest.file_name);
        let bytes = std::fs::read(&snapshot).map_err(|e| {
            Error::io(format!("读取快照 {}", snapshot.display()), e)
        })?;
        if bytes.len() as u64 != manifest.file_size {
            return Err(Error::Vector("快照大小与 manifest 不一致".to_string()));
        }
        if blake3::hash(&bytes).to_hex().to_string() != manifest.file_hash {
            return Err(Error::Vector("快照哈希与 manifest 不一致，文件已损坏".to_string()));
        }
        let capacity = manifest.vector_count.max(CAPACITY_STEP);
        let index = new_index(self.dimensions, capacity)?;
        index
            .load(snapshot.to_str().ok_or_else(|| {
                Error::Vector(format!("快照路径不是合法 UTF-8: {}", snapshot.display()))
            })?)
            .map_err(|e| Error::Vector(format!("加载向量快照失败: {e}")))?;
        // `load` 会把容量收缩到快照实际大小，这里重新预留出后续写入空间。
        index
            .reserve(capacity)
            .map_err(|e| Error::Vector(format!("加载后预留容量失败: {e}")))?;
        self.index = index;
        Ok(Some(manifest))
    }
}

/// 按维度与容量创建 USearch 索引。
///
/// 固定使用余弦度量与 f32 量化：文档与查询向量都已 L2 归一化，
/// 因此余弦距离稳定且与内积等价。
fn new_index(dimensions: usize, capacity: usize) -> Result<Index> {
    if dimensions == 0 {
        return Err(Error::Vector("向量维度必须大于 0".to_string()));
    }
    let options = IndexOptions {
        dimensions,
        metric: MetricKind::Cos,
        quantization: ScalarKind::F32,
        connectivity: CONNECTIVITY,
        expansion_add: 0,
        expansion_search: 0,
        multi: false,
    };
    let index =
        Index::new(&options).map_err(|e| Error::Vector(format!("创建向量索引失败: {e}")))?;
    index
        .reserve(capacity.max(1))
        .map_err(|e| Error::Vector(format!("预留向量容量失败: {e}")))?;
    Ok(index)
}

fn snapshot_name(space_id: &str, generation: u64) -> String {
    format!("{}-{generation:08}.usearch", sanitize(space_id))
}

/// 把空间 ID 变成安全的文件名片段。
fn sanitize(space_id: &str) -> String {
    let cleaned: String = space_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if cleaned.is_empty() {
        "space".to_string()
    } else {
        format!("vectors-{cleaned}")
    }
}

fn sync_file(path: &Path) -> Result<()> {
    let file = File::open(path)
        .map_err(|e| Error::io(format!("打开 {} 以同步", path.display()), e))?;
    file.sync_all()
        .map_err(|e| Error::io(format!("同步 {}", path.display()), e))?;
    Ok(())
}

#[cfg(test)]
mod tests;
