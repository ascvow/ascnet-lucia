//! 插件 Bundle 的扫描、打包、解包与路径校验。

use crate::types::InstalledPlugin;
use agent_plugin_host::manifest::PluginManifest;
use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
/// 可归档 bundle 与稳定摘要共享的编码域。
pub(crate) const BUNDLE_ARCHIVE_DOMAIN: &[u8] = b"lucia-plugin-bundle-v1\0";

/// 单个可归档 bundle 的最大编码大小，避免发布控制面无界分配内存。
const MAX_BUNDLE_ARCHIVE_BYTES: usize = 256 * 1024 * 1024;

/// 单个可归档 bundle 的最大条目数。
const MAX_BUNDLE_ARCHIVE_ENTRIES: usize = 16_384;

/// 单个 bundle 相对路径的最大 UTF-8 字节数。
const MAX_BUNDLE_ARCHIVE_PATH_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BundleEntryKind {
    Directory,
    File,
}

#[derive(Debug, Clone)]
struct BundleEntry {
    source: PathBuf,
    normalized: String,
    kind: BundleEntryKind,
}

fn scan_bundle(root: &Path) -> Result<Vec<BundleEntry>> {
    let metadata = fs::symlink_metadata(root)
        .with_context(|| format!("无法读取 bundle：{}", root.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("bundle 必须是非符号链接目录：{}", root.display());
    }
    let mut entries = Vec::new();
    scan_bundle_directory(root, root, &mut entries)?;
    entries.sort_by(|left, right| left.normalized.cmp(&right.normalized));
    Ok(entries)
}

fn scan_bundle_directory(
    root: &Path,
    directory: &Path,
    entries: &mut Vec<BundleEntry>,
) -> Result<()> {
    for entry in fs::read_dir(directory)
        .with_context(|| format!("无法读取 bundle 目录：{}", directory.display()))?
    {
        let entry = entry.context("无法读取 bundle 目录项")?;
        let source = entry.path();
        let metadata = fs::symlink_metadata(&source)
            .with_context(|| format!("无法读取 bundle 条目：{}", source.display()))?;
        if metadata.file_type().is_symlink() {
            bail!("插件 bundle 不允许符号链接：{}", source.display());
        }
        let relative = source
            .strip_prefix(root)
            .context("bundle 条目逃逸出根目录")?
            .to_path_buf();
        let normalized = normalized_relative_path(&relative)?;
        let kind = if metadata.is_dir() {
            BundleEntryKind::Directory
        } else if metadata.is_file() {
            BundleEntryKind::File
        } else {
            bail!("插件 bundle 不允许特殊文件：{}", source.display());
        };
        entries.push(BundleEntry {
            source: source.clone(),
            normalized,
            kind,
        });
        if kind == BundleEntryKind::Directory {
            scan_bundle_directory(root, &source, entries)?;
        }
    }
    Ok(())
}

/// 先复制 manifest 到 staging，再仅按 staged manifest 复制运行时 WASM。
///
/// 本地路径可以指向插件开发 crate，但源码、Cargo 缓存、测试和其他未声明文件都不能成为
/// 已安装 Bundle 的一部分。来源 manifest 只作为待复制文件，身份、能力、依赖和
/// WASM 路径均从 staging 副本解析；每个源文件会在复制前重新检查路径组件。
///
/// 返回已在 staging 中复核 manifest 与 WASM 路径后的不可变授权输入。
pub(crate) fn copy_runtime_bundle(source: &Path, destination: &Path) -> Result<PluginManifest> {
    let manifest_relative = PathBuf::from("plugin.toml");
    copy_runtime_bundle_file(source, destination, &manifest_relative, "plugin.toml")?;
    let staged_manifest_path = destination.join(&manifest_relative);
    let staged_manifest =
        PluginManifest::load(&staged_manifest_path).context("staging 中的插件 manifest 无效")?;
    let wasm_relative =
        validate_relative_path(Path::new(&staged_manifest.plugin.wasm), "plugin.wasm")?;
    copy_runtime_bundle_file(source, destination, &wasm_relative, "plugin.wasm")?;
    load_and_validate_manifest(destination, &staged_manifest_path)
}

/// 把单个已约束的运行时文件复制到 staging。
fn copy_runtime_bundle_file(
    source: &Path,
    destination: &Path,
    relative: &Path,
    label: &str,
) -> Result<()> {
    let source_file = validate_runtime_bundle_file(source, relative, label)?;
    let target = destination.join(relative);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("无法创建运行时 Bundle 文件父目录：{}", parent.display()))?;
    }
    fs::copy(&source_file, &target)
        .with_context(|| format!("无法复制运行时 Bundle 文件：{}", source_file.display()))?;
    Ok(())
}

/// 校验运行时文件及其全部父目录均为真实目录或普通文件。
///
/// 返回经过相对路径约束的源文件路径；任一父目录是符号链接、目标缺失或文件类型不符时
/// 返回错误，防止开发目录中的链接把安装读取范围扩展到 Bundle 根目录之外。
fn validate_runtime_bundle_file(root: &Path, relative: &Path, label: &str) -> Result<PathBuf> {
    let relative = validate_relative_path(relative, label)?;
    let component_count = relative.components().count();
    let mut current = root.to_path_buf();
    for (index, component) in relative.components().enumerate() {
        let Component::Normal(part) = component else {
            unreachable!("运行时文件路径已经完成相对路径校验");
        };
        current.push(part);
        let metadata = fs::symlink_metadata(&current)
            .with_context(|| format!("插件运行时文件不存在：{}", current.display()))?;
        if metadata.file_type().is_symlink() {
            bail!("插件运行时文件路径不允许符号链接：{}", current.display());
        }
        let is_last = index + 1 == component_count;
        if is_last && !metadata.is_file() {
            bail!("插件运行时文件必须是普通文件：{}", current.display());
        }
        if !is_last && !metadata.is_dir() {
            bail!("插件运行时文件父路径必须是目录：{}", current.display());
        }
    }
    Ok(current)
}

/// 把插件目录编码为可进入 Artifact CAS 的确定性 bundle 字节。
///
/// 编码覆盖排序后的目录、普通文件、规范相对路径、文件长度和文件正文；其 SHA-256 与源目录
/// 的 [`hash_plugin_bundle`] 完全一致。安装时 Plugin Manager 会从还原目录提取 manifest 与
/// WASM，因此包含其他文件的发布归档摘要不等于清理后的安装摘要。输入包含符号链接、特殊
/// 文件、过多条目或编码超过 256 MiB 时会拒绝，不会执行 bundle 内代码。
///
/// # Errors
///
/// bundle 路径或条目不安全、文件在读取期间改变、大小超过限制或文件系统读取失败时返回
/// 错误。
pub fn pack_plugin_bundle(root: &Path) -> Result<Vec<u8>> {
    let entries = scan_bundle(root)?;
    if entries.len() > MAX_BUNDLE_ARCHIVE_ENTRIES {
        bail!(
            "插件 bundle 条目数超过限制：{} > {}",
            entries.len(),
            MAX_BUNDLE_ARCHIVE_ENTRIES
        );
    }
    let mut archive = Vec::with_capacity(BUNDLE_ARCHIVE_DOMAIN.len());
    archive.extend_from_slice(BUNDLE_ARCHIVE_DOMAIN);
    for entry in entries {
        let path_bytes = entry.normalized.as_bytes();
        if path_bytes.len() > MAX_BUNDLE_ARCHIVE_PATH_BYTES {
            bail!("插件 bundle 路径过长：{}", entry.normalized);
        }
        append_archive_bytes(
            &mut archive,
            match entry.kind {
                BundleEntryKind::Directory => b"d",
                BundleEntryKind::File => b"f",
            },
        )?;
        append_archive_bytes(&mut archive, &(path_bytes.len() as u64).to_be_bytes())?;
        append_archive_bytes(&mut archive, path_bytes)?;
        if entry.kind == BundleEntryKind::File {
            let metadata = fs::symlink_metadata(&entry.source)
                .with_context(|| format!("无法重新检查 bundle 文件：{}", entry.source.display()))?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!(
                    "归档期间 bundle 文件类型发生变化：{}",
                    entry.source.display()
                );
            }
            let size = usize::try_from(metadata.len()).context("bundle 文件长度超出平台上限")?;
            append_archive_bytes(&mut archive, &metadata.len().to_be_bytes())?;
            let expected_len = archive
                .len()
                .checked_add(size)
                .context("bundle 归档长度溢出")?;
            if expected_len > MAX_BUNDLE_ARCHIVE_BYTES {
                bail!("插件 bundle 归档超过 256 MiB 限制");
            }
            archive
                .try_reserve(size)
                .context("无法为插件 bundle 归档分配内存")?;
            let before = archive.len();
            let file = File::open(&entry.source)
                .with_context(|| format!("无法读取 bundle 文件：{}", entry.source.display()))?;
            file.take(metadata.len().saturating_add(1))
                .read_to_end(&mut archive)
                .context("无法编码插件 bundle")?;
            if archive.len() - before != size {
                bail!(
                    "归档期间 bundle 文件长度发生变化：{}",
                    entry.source.display()
                );
            }
        }
    }
    let encoded_digest = format!("{:x}", Sha256::digest(&archive));
    let observed_digest = hash_plugin_bundle(root)?;
    if encoded_digest != observed_digest {
        bail!("归档期间插件 bundle 内容发生变化");
    }
    Ok(archive)
}

/// 把受信 CAS 中的确定性 bundle 字节安全还原到一个尚不存在的绝对目录。
///
/// 目标父目录必须已存在且不是符号链接，目标只能是该父目录下的单级新目录。函数先完整解析
/// 并验证排序、路径、重复项、父子类型和总大小，再创建文件；失败时尽力移除本次创建的目标。
/// 成功后会验证 `plugin.toml`、WASM 路径和重算摘要，并返回与输入字节 SHA-256 相同的十六
/// 进制摘要。函数不会加载或执行 WASM。
///
/// # Errors
///
/// 编码版本未知、路径逃逸、条目冲突、目标不安全、manifest/WASM 无效、摘要不一致或文件
/// 系统写入失败时返回错误。
pub fn unpack_plugin_bundle(archive: &[u8], destination: &Path) -> Result<String> {
    if archive.len() > MAX_BUNDLE_ARCHIVE_BYTES {
        bail!("插件 bundle 归档超过 256 MiB 限制");
    }
    let entries = parse_plugin_bundle_archive(archive)?;
    let destination = prepare_bundle_destination(destination)?;
    fs::create_dir(&destination)
        .with_context(|| format!("无法创建 bundle 目标目录：{}", destination.display()))?;
    let extracted = (|| -> Result<String> {
        for entry in &entries {
            let target = destination.join(&entry.relative);
            match entry.kind {
                BundleEntryKind::Directory => fs::create_dir_all(&target)
                    .with_context(|| format!("无法创建 bundle 目录：{}", target.display()))?,
                BundleEntryKind::File => {
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent).with_context(|| {
                            format!("无法创建 bundle 文件父目录：{}", parent.display())
                        })?;
                    }
                    let mut file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&target)
                        .with_context(|| format!("无法创建 bundle 文件：{}", target.display()))?;
                    file.write_all(entry.contents)
                        .with_context(|| format!("无法写入 bundle 文件：{}", target.display()))?;
                    file.sync_all()
                        .with_context(|| format!("无法同步 bundle 文件：{}", target.display()))?;
                }
            }
        }
        load_and_validate_manifest(&destination, &destination.join("plugin.toml"))?;
        let actual = hash_plugin_bundle(&destination)?;
        let expected = format!("{:x}", Sha256::digest(archive));
        if actual != expected {
            bail!("还原后的插件 bundle 摘要与归档不一致");
        }
        Ok(actual)
    })();
    if extracted.is_err() {
        let _ = fs::remove_dir_all(&destination);
    }
    extracted
}

/// 已完成边界校验、引用原始归档正文的单个条目。
struct ParsedBundleArchiveEntry<'a> {
    relative: PathBuf,
    kind: BundleEntryKind,
    contents: &'a [u8],
}

/// 解析并验证确定性 bundle 编码，不产生任何文件系统副作用。
fn parse_plugin_bundle_archive(archive: &[u8]) -> Result<Vec<ParsedBundleArchiveEntry<'_>>> {
    if !archive.starts_with(BUNDLE_ARCHIVE_DOMAIN) {
        bail!("插件 bundle 归档编码域无效");
    }
    let mut cursor = BUNDLE_ARCHIVE_DOMAIN.len();
    let mut entries = Vec::new();
    let mut known = BTreeMap::<String, BundleEntryKind>::new();
    let mut previous: Option<String> = None;
    while cursor < archive.len() {
        if entries.len() >= MAX_BUNDLE_ARCHIVE_ENTRIES {
            bail!("插件 bundle 归档条目数超过限制");
        }
        let kind = match take_archive_bytes(archive, &mut cursor, 1)?[0] {
            b'd' => BundleEntryKind::Directory,
            b'f' => BundleEntryKind::File,
            _ => bail!("插件 bundle 归档包含未知条目类型"),
        };
        let path_len = read_archive_u64(archive, &mut cursor)?;
        let path_len = usize::try_from(path_len).context("bundle 路径长度超出平台上限")?;
        if path_len == 0 || path_len > MAX_BUNDLE_ARCHIVE_PATH_BYTES {
            bail!("插件 bundle 归档路径长度无效");
        }
        let normalized = std::str::from_utf8(take_archive_bytes(archive, &mut cursor, path_len)?)
            .context("插件 bundle 归档路径不是 UTF-8")?
            .to_string();
        let relative = validate_relative_path(Path::new(&normalized), "bundle 归档路径")?;
        if normalized_relative_path(&relative)? != normalized {
            bail!("插件 bundle 归档路径不是规范形式：{normalized}");
        }
        if previous.as_ref().is_some_and(|value| value >= &normalized) {
            bail!("插件 bundle 归档条目未严格排序或存在重复路径");
        }
        for ancestor in relative.ancestors().skip(1) {
            if ancestor.as_os_str().is_empty() {
                break;
            }
            let ancestor = normalized_relative_path(ancestor)?;
            if known.get(&ancestor) == Some(&BundleEntryKind::File) {
                bail!("插件 bundle 归档把文件作为父目录：{ancestor}");
            }
        }
        let contents = if kind == BundleEntryKind::File {
            let size = read_archive_u64(archive, &mut cursor)?;
            let size = usize::try_from(size).context("bundle 文件长度超出平台上限")?;
            take_archive_bytes(archive, &mut cursor, size)?
        } else {
            &[]
        };
        previous = Some(normalized.clone());
        known.insert(normalized.clone(), kind);
        entries.push(ParsedBundleArchiveEntry {
            relative,
            kind,
            contents,
        });
    }
    if entries.is_empty() {
        bail!("插件 bundle 归档不能为空");
    }
    if !known.contains_key("plugin.toml") {
        bail!("插件 bundle 归档缺少 plugin.toml");
    }
    Ok(entries)
}

/// 校验解包目标只能是已存在真实父目录下的一个新直接子目录。
fn prepare_bundle_destination(destination: &Path) -> Result<PathBuf> {
    if !destination.is_absolute() {
        bail!("bundle 解包目标必须是绝对路径：{}", destination.display());
    }
    let name = destination
        .file_name()
        .context("bundle 解包目标缺少目录名")?;
    let name = name.to_str().context("bundle 解包目录名必须是 UTF-8")?;
    validate_storage_component(name, "bundle 解包目录名")?;
    let parent = destination.parent().context("bundle 解包目标缺少父目录")?;
    let metadata = fs::symlink_metadata(parent)
        .with_context(|| format!("无法读取 bundle 解包父目录：{}", parent.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("bundle 解包父目录必须是真实目录：{}", parent.display());
    }
    let canonical_parent = parent
        .canonicalize()
        .with_context(|| format!("无法解析 bundle 解包父目录：{}", parent.display()))?;
    let target = canonical_parent.join(name);
    if fs::symlink_metadata(&target).is_ok() {
        bail!("bundle 解包目标已存在：{}", target.display());
    }
    Ok(target)
}

/// 在有界归档缓冲区尾部追加字节。
fn append_archive_bytes(archive: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    let expected = archive
        .len()
        .checked_add(bytes.len())
        .context("bundle 归档长度溢出")?;
    if expected > MAX_BUNDLE_ARCHIVE_BYTES {
        bail!("插件 bundle 归档超过 256 MiB 限制");
    }
    archive.extend_from_slice(bytes);
    Ok(())
}

/// 从归档游标读取固定长度切片。
fn take_archive_bytes<'a>(archive: &'a [u8], cursor: &mut usize, len: usize) -> Result<&'a [u8]> {
    let end = cursor.checked_add(len).context("bundle 归档长度溢出")?;
    let bytes = archive
        .get(*cursor..end)
        .context("插件 bundle 归档被截断")?;
    *cursor = end;
    Ok(bytes)
}

/// 从归档游标读取大端 `u64`。
fn read_archive_u64(archive: &[u8], cursor: &mut usize) -> Result<u64> {
    let bytes: [u8; 8] = take_archive_bytes(archive, cursor, 8)?
        .try_into()
        .expect("固定八字节切片必须可转换");
    Ok(u64::from_be_bytes(bytes))
}

/// 计算插件 bundle 的稳定 SHA-256 摘要。
///
/// 摘要覆盖 bundle 内所有目录、普通文件、相对路径和文件内容；符号链接、特殊文件与
/// 非法路径会被拒绝。Plugin Manager 锁文件必须复用该算法，避免
/// 安装完整性与运行身份采用不同口径。
///
/// # Errors
///
/// 根目录不是普通目录、存在不安全条目、路径无法规范化或文件读取失败时返回错误。
pub fn hash_plugin_bundle(root: &Path) -> Result<String> {
    hash_plugin_bundle_impl(root, false).map(|(digest, _)| digest)
}

/// 返回参与同次完整摘要的 manifest 原文，防止解析与校验读取不同版本的文件。
pub(crate) fn hash_plugin_bundle_with_manifest(root: &Path) -> Result<(String, String)> {
    let (digest, manifest) = hash_plugin_bundle_impl(root, true)?;
    Ok((
        digest,
        String::from_utf8(manifest.ok_or_else(|| anyhow!("bundle 缺少 plugin.toml"))?)
            .context("manifest 不是 UTF-8")?,
    ))
}

#[cfg(test)]
std::thread_local! { pub(crate) static HASH_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

fn hash_plugin_bundle_impl(root: &Path, capture: bool) -> Result<(String, Option<Vec<u8>>)> {
    #[cfg(test)]
    HASH_CALLS.with(|count| count.set(count.get() + 1));
    let mut manifest = None;
    let entries = scan_bundle(root)?;
    let mut digest = Sha256::new();
    digest.update(BUNDLE_ARCHIVE_DOMAIN);
    let mut buffer = [0_u8; 64 * 1024];
    for entry in entries {
        digest.update(match entry.kind {
            BundleEntryKind::Directory => *b"d",
            BundleEntryKind::File => *b"f",
        });
        let path_bytes = entry.normalized.as_bytes();
        digest.update((path_bytes.len() as u64).to_be_bytes());
        digest.update(path_bytes);
        if entry.kind == BundleEntryKind::File {
            let metadata = fs::metadata(&entry.source)
                .with_context(|| format!("无法读取文件长度：{}", entry.source.display()))?;
            digest.update(metadata.len().to_be_bytes());
            let mut file = File::open(&entry.source)
                .with_context(|| format!("无法读取 bundle 文件：{}", entry.source.display()))?;
            let mut captured = (capture && entry.normalized == "plugin.toml").then(Vec::new);
            loop {
                let read = file.read(&mut buffer).context("无法计算 bundle SHA-256")?;
                if read == 0 {
                    break;
                }
                digest.update(&buffer[..read]);
                if let Some(bytes) = captured.as_mut() {
                    bytes.extend_from_slice(&buffer[..read]);
                }
            }
            if captured.is_some() {
                manifest = captured;
            }
        }
    }
    Ok((format!("{:x}", digest.finalize()), manifest))
}

pub(crate) fn load_and_validate_manifest(
    bundle: &Path,
    manifest_path: &Path,
) -> Result<PluginManifest> {
    validate_runtime_bundle_file(bundle, Path::new("plugin.toml"), "plugin.toml")?;
    let manifest = PluginManifest::load(manifest_path).context("插件 manifest 无效")?;
    let wasm_relative = validate_relative_path(Path::new(&manifest.plugin.wasm), "plugin.wasm")?;
    validate_runtime_bundle_file(bundle, &wasm_relative, "plugin.wasm")?;
    Ok(manifest)
}

pub(crate) fn validate_storage_component(value: &str, label: &str) -> Result<()> {
    let mut components = Path::new(value).components();
    if value.is_empty()
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        bail!("{label} 不能包含目录分隔或路径跳转：`{value}`");
    }
    Ok(())
}

pub(crate) fn validate_relative_path(path: &Path, label: &str) -> Result<PathBuf> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        bail!("{label} 必须是非空相对路径：{}", path.display());
    }
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => result.push(part),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                bail!("{label} 不允许目录跳转：{}", path.display());
            }
        }
    }
    Ok(result)
}

fn normalized_relative_path(path: &Path) -> Result<String> {
    let validated = validate_relative_path(path, "bundle 相对路径")?;
    validated
        .components()
        .map(|component| match component {
            Component::Normal(part) => part
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("bundle 路径必须是 UTF-8：{}", path.display())),
            _ => unreachable!("路径已经完成相对路径校验"),
        })
        .collect::<Result<Vec<_>>>()
        .map(|parts| parts.join("/"))
}

pub(crate) fn path_to_lock_string(path: &Path) -> Result<String> {
    normalized_relative_path(path)
}

pub(crate) fn ensure_real_directory(path: &Path, create: bool) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("受管理目录不能是符号链接：{}", path.display());
        }
        Ok(metadata) if !metadata.is_dir() => {
            bail!("受管理路径不是目录：{}", path.display());
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
            fs::create_dir_all(path)
                .with_context(|| format!("无法创建受管理目录：{}", path.display()))
        }
        Err(error) => Err(error).with_context(|| format!("无法检查目录：{}", path.display())),
    }
}

pub(crate) fn remove_directory_if_empty(path: &Path) -> Result<()> {
    let mut entries =
        fs::read_dir(path).with_context(|| format!("无法检查目录是否为空：{}", path.display()))?;
    if entries.next().is_none() {
        fs::remove_dir(path).with_context(|| format!("无法删除空目录：{}", path.display()))?;
    }
    Ok(())
}

pub(crate) fn sort_lock_plugins(plugins: &mut [InstalledPlugin]) {
    plugins.sort_by(|left, right| left.id.cmp(&right.id));
}

/// 按稳定 ID 和版本规范化人工版本历史顺序，保证锁文件可复现。
pub(crate) fn sort_version_history(history: &mut [InstalledPlugin]) {
    history.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then_with(|| left.version.cmp(&right.version))
    });
}

/// 以插件 ID 和版本为键更新人工版本历史，避免重复归档同一固定 Bundle。
pub(crate) fn upsert_version_history(history: &mut Vec<InstalledPlugin>, plugin: InstalledPlugin) {
    if let Some(index) = history
        .iter()
        .position(|item| item.id == plugin.id && item.version == plugin.version)
    {
        history[index] = plugin;
    } else {
        history.push(plugin);
    }
}
