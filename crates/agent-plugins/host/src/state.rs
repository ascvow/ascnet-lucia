//! Plugin Host 可选的按插件隔离持久状态。

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// 单个插件状态文件的最大编码大小，防止 Guest 借助多次小写入形成无界文件。
const MAX_PLUGIN_STATE_BYTES: usize = 1024 * 1024;
/// 单个插件允许持有的最大状态键数量。
const MAX_PLUGIN_STATE_ENTRIES: usize = 1024;
/// 当前磁盘状态信封版本；该版本独立于插件 ABI 与 manifest 版本。
const PLUGIN_STATE_SCHEMA_VERSION: u32 = 2;
/// 临时文件名使用的进程内序号，避免同一进程连续写入发生冲突。
static STATE_WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 单个插件的可选持久状态文件；未注入该对象时 Host 保持原有内存语义。
#[derive(Clone)]
pub(crate) struct PluginStateStore {
    root: PathBuf,
    path: PathBuf,
    plugin_id: String,
    state_identity: String,
}

/// 磁盘中的版本化插件状态信封。
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPluginState {
    schema_version: u32,
    plugin_id: String,
    state_identity: String,
    values: HashMap<String, Value>,
}

impl PluginStateStore {
    /// 按稳定插件 ID 和 Host 绑定的 Bundle 身份创建隔离存储地址。
    ///
    /// `state_identity` 必须由 Host 根据真实 Component 与 capability profile 生成，不能接受
    /// manifest metadata 或 Guest 输入。文件名使用摘要编码，禁止路径分隔符进入目标路径。
    pub(crate) fn new(root: PathBuf, plugin_id: &str, state_identity: &str) -> Result<Self> {
        if root.as_os_str().is_empty() {
            bail!("插件状态目录不能为空");
        }
        if plugin_id.trim().is_empty() {
            bail!("插件状态 plugin_id 不能为空");
        }
        if state_identity.trim().is_empty() || state_identity.len() > 256 {
            bail!("插件状态可信身份无效");
        }
        let mut digest = Sha256::new();
        digest.update(b"lucia-plugin-state-v2\0");
        update_identity_field(&mut digest, plugin_id.as_bytes());
        update_identity_field(&mut digest, state_identity.as_bytes());
        let encoded_id = format!("{:x}", digest.finalize());
        let path = root.join(format!("{encoded_id}.json"));
        Ok(Self {
            root,
            path,
            plugin_id: plugin_id.to_string(),
            state_identity: state_identity.to_string(),
        })
    }

    /// 加载并校验当前插件的状态；文件不存在时返回空状态，不会创建目录或文件。
    pub(crate) fn load(&self) -> Result<HashMap<String, Value>> {
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("读取插件状态元数据失败：{}", self.path.display()))
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("插件状态必须是非符号链接普通文件：{}", self.path.display());
        }
        if metadata.len() > MAX_PLUGIN_STATE_BYTES as u64 {
            bail!("插件状态文件超过 1 MiB 限制：{}", self.path.display());
        }
        let bytes = fs::read(&self.path)
            .with_context(|| format!("读取插件状态失败：{}", self.path.display()))?;
        let stored: StoredPluginState = serde_json::from_slice(&bytes)
            .with_context(|| format!("解析插件状态失败：{}", self.path.display()))?;
        if stored.schema_version != PLUGIN_STATE_SCHEMA_VERSION {
            bail!(
                "不支持插件 `{}` 的状态版本 `{}`",
                self.plugin_id,
                stored.schema_version
            );
        }
        if stored.plugin_id != self.plugin_id {
            bail!("插件状态身份不匹配：期望 `{}`", self.plugin_id);
        }
        if stored.state_identity != self.state_identity {
            bail!("插件状态 Bundle 身份不匹配：期望 `{}`", self.plugin_id);
        }
        validate_values(&stored.values)?;
        Ok(stored.values)
    }

    /// 原子保存完整状态映射；任一步失败时不会提交调用方的新内存状态。
    pub(crate) fn save(&self, values: &HashMap<String, Value>) -> Result<()> {
        validate_values(values)?;
        ensure_real_state_directory(&self.root)?;
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                bail!("插件状态必须是非符号链接普通文件：{}", self.path.display());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("读取插件状态目标失败：{}", self.path.display()));
            }
        }
        let contents = serde_json::to_vec_pretty(&StoredPluginState {
            schema_version: PLUGIN_STATE_SCHEMA_VERSION,
            plugin_id: self.plugin_id.clone(),
            state_identity: self.state_identity.clone(),
            values: values.clone(),
        })
        .context("序列化插件状态失败")?;
        if contents.len() > MAX_PLUGIN_STATE_BYTES {
            bail!("插件状态编码超过 1 MiB 限制");
        }

        let sequence = STATE_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("插件状态文件名不是有效 UTF-8"))?;
        let temporary = self.root.join(format!(
            ".{file_name}.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("创建插件状态临时文件失败：{}", temporary.display()))?;
        let written = (|| -> Result<()> {
            file.write_all(&contents)
                .with_context(|| format!("写入插件状态失败：{}", temporary.display()))?;
            file.sync_all()
                .with_context(|| format!("同步插件状态失败：{}", temporary.display()))?;
            fs::rename(&temporary, &self.path)
                .with_context(|| format!("原子更新插件状态失败：{}", self.path.display()))?;
            fs::File::open(&self.root)
                .and_then(|directory| directory.sync_all())
                .with_context(|| format!("同步插件状态目录失败：{}", self.root.display()))?;
            Ok(())
        })();
        if written.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        written
    }
}

/// 使用长度前缀写入身份字段，避免相邻可变长字段产生拼接歧义。
fn update_identity_field(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

/// 校验持久映射的数量和键边界，防止绕过 Guest 请求校验注入异常磁盘状态。
fn validate_values(values: &HashMap<String, Value>) -> Result<()> {
    if values.len() > MAX_PLUGIN_STATE_ENTRIES {
        bail!("插件状态键数量超过 {MAX_PLUGIN_STATE_ENTRIES} 项限制");
    }
    for key in values.keys() {
        if key.trim().is_empty() || key.len() > 256 {
            bail!("插件状态文件包含无效状态键");
        }
    }
    Ok(())
}

/// 创建并校验应用注入的状态根目录，最终目录不得是符号链接。
fn ensure_real_state_directory(root: &Path) -> Result<()> {
    fs::create_dir_all(root)
        .with_context(|| format!("创建插件状态目录失败：{}", root.display()))?;
    let metadata = fs::symlink_metadata(root)
        .with_context(|| format!("读取插件状态目录失败：{}", root.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("插件状态目录必须是非符号链接目录：{}", root.display());
    }
    #[cfg(unix)]
    fs::set_permissions(root, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("收紧插件状态目录权限失败：{}", root.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 为单元测试创建进程内唯一的状态根目录。
    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "lucia-plugin-state-{name}-{}-{}",
            std::process::id(),
            STATE_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// 持久状态必须按插件 ID 隔离，并在重新创建存储对象后恢复。
    #[test]
    fn plugin_state_round_trip_is_scoped_by_plugin_id() {
        let root = test_root("round-trip");
        let first = PluginStateStore::new(root.clone(), "permission", "bundle-a")
            .expect("创建权限状态存储");
        first
            .save(&HashMap::from([(
                "mode".into(),
                Value::String("model_review".into()),
            )]))
            .expect("保存权限状态");

        let reopened =
            PluginStateStore::new(root.clone(), "permission", "bundle-a").expect("重新打开状态");
        assert_eq!(
            reopened.load().expect("恢复权限状态").get("mode"),
            Some(&Value::String("model_review".into()))
        );
        let other = PluginStateStore::new(root.clone(), "other", "bundle-a").expect("创建隔离状态");
        assert!(other.load().expect("读取隔离状态").is_empty());
        let replacement = PluginStateStore::new(root.clone(), "permission", "bundle-b")
            .expect("创建不同 Bundle 状态");
        assert!(replacement.load().expect("读取不同 Bundle 状态").is_empty());
        fs::remove_dir_all(root).expect("清理插件状态测试目录");
    }

    /// 持久状态目录和文件必须使用仅当前用户可访问的权限。
    #[cfg(unix)]
    #[test]
    fn plugin_state_uses_private_unix_permissions() {
        let root = test_root("permissions");
        let store = PluginStateStore::new(root.clone(), "permission", "bundle-a")
            .expect("创建权限状态存储");
        store
            .save(&HashMap::from([(
                "mode".into(),
                Value::String("manual".into()),
            )]))
            .expect("保存权限状态");

        assert_eq!(
            fs::metadata(&root)
                .expect("读取状态目录权限")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&store.path)
                .expect("读取状态文件权限")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_dir_all(root).expect("清理插件状态测试目录");
    }

    /// Guest 不得通过大量状态键绕过单插件存储边界。
    #[test]
    fn plugin_state_rejects_entry_limit_before_writing() {
        let root = test_root("entry-limit");
        let store = PluginStateStore::new(root.clone(), "permission", "bundle-a")
            .expect("创建权限状态存储");
        let values = (0..=MAX_PLUGIN_STATE_ENTRIES)
            .map(|index| (format!("key-{index}"), Value::Null))
            .collect::<HashMap<_, _>>();

        let error = store.save(&values).expect_err("超量状态键必须被拒绝");
        assert!(error.to_string().contains("状态键数量"));
        assert!(!root.exists());
    }

    /// 已存在的符号链接状态目标必须被拒绝，不能跟随到应用目录之外。
    #[cfg(unix)]
    #[test]
    fn plugin_state_rejects_symlink_target() {
        use std::os::unix::fs::symlink;

        let root = test_root("symlink");
        fs::create_dir_all(&root).expect("创建状态测试目录");
        let external = root.with_extension("external.json");
        fs::write(&external, "外部文件").expect("创建符号链接外部目标");
        let store = PluginStateStore::new(root.clone(), "permission", "bundle-a")
            .expect("创建权限状态存储");
        symlink(&external, &store.path).expect("创建状态符号链接");

        let error = store
            .save(&HashMap::new())
            .expect_err("符号链接状态目标必须被拒绝");
        assert!(error.to_string().contains("非符号链接普通文件"));
        assert_eq!(
            fs::read_to_string(&external).expect("读取外部目标"),
            "外部文件"
        );
        fs::remove_dir_all(root).expect("清理插件状态测试目录");
        fs::remove_file(external).expect("清理符号链接外部目标");
    }
}
