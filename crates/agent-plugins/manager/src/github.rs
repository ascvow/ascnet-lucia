//! GitHub Release 插件 bundle 获取与安全解包。

use crate::{InstallOptions, InstalledPlugin, PluginManager};
use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use reqwest::{header, Client};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::{Cursor, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use zip::ZipArchive;

/// 裸插件名称默认解析到的 GitHub 发布者。
pub const DEFAULT_GITHUB_PUBLISHER: &str = "ascvow";

/// 单个 GitHub Release bundle 的最大下载大小，默认 256 MiB。
const DEFAULT_MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;
/// 单个 bundle 解压后的最大总大小，默认 512 MiB。
const DEFAULT_MAX_UNPACKED_BYTES: u64 = 512 * 1024 * 1024;
/// 单个 bundle 允许包含的最大目录项数量。
const MAX_ARCHIVE_ENTRIES: usize = 4_096;
/// GitHub API 根地址；测试可通过选项覆盖具体 Release API URL。
const GITHUB_API_ROOT: &str = "https://api.github.com";

/// 可定位 GitHub 插件仓库的规范化来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubPluginSource {
    /// GitHub 仓库 owner。
    pub owner: String,
    /// GitHub 仓库名称。
    pub repository: String,
}

impl GithubPluginSource {
    /// 解析裸名称、`owner/repository` 或 GitHub 仓库 URL。
    ///
    /// 裸名称使用 [`DEFAULT_GITHUB_PUBLISHER`]；解析只接受两级 GitHub 仓库路径，
    /// 不允许查询参数、片段或额外路径。
    pub fn parse(value: &str) -> Result<Self> {
        let mut value = value.trim().trim_end_matches('/');
        if value.is_empty() {
            bail!("GitHub 插件名称不能为空");
        }
        if let Some(stripped) = value.strip_prefix("https://github.com/") {
            value = stripped;
        } else if value.contains("://") {
            bail!("仅支持 https://github.com 仓库 URL");
        }
        let value = value.strip_suffix(".git").unwrap_or(value);
        let parts = value.split('/').collect::<Vec<_>>();
        let (owner, repository) = match parts.as_slice() {
            [repository] => (DEFAULT_GITHUB_PUBLISHER, *repository),
            [owner, repository] => (*owner, *repository),
            _ => bail!("GitHub 插件来源必须是名称、owner/repository 或仓库 URL"),
        };
        validate_github_component(owner, "owner")?;
        validate_github_component(repository, "repository")?;
        Ok(Self {
            owner: owner.to_string(),
            repository: repository.to_string(),
        })
    }

    /// 返回可展示和持久化的 GitHub 仓库 URL。
    pub fn repository_url(&self) -> String {
        format!("https://github.com/{}/{}", self.owner, self.repository)
    }
}

/// GitHub Release 安装行为选项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubInstallOptions {
    /// 安装完成后是否立即启用插件。
    pub enabled: bool,
    /// 指定 Release tag；未指定时使用 latest release。
    pub tag: Option<String>,
    /// 指定 ZIP asset 名称；未指定时按稳定命名规则选择。
    pub asset: Option<String>,
    /// Registry 声明的归档 SHA-256；设置后必须与下载内容一致。
    pub expected_sha256: Option<String>,
    /// 允许下载的最大字节数。
    pub max_download_bytes: u64,
}

impl Default for GithubInstallOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            tag: None,
            asset: None,
            expected_sha256: None,
            max_download_bytes: DEFAULT_MAX_DOWNLOAD_BYTES,
        }
    }
}

/// GitHub Release 安装成功结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubInstallResult {
    /// 已写入插件锁文件的安装记录。
    pub plugin: InstalledPlugin,
    /// GitHub Release 的不可变 tag。
    pub release_tag: String,
    /// 实际下载的 ZIP asset 名称。
    pub asset_name: String,
    /// Release 是否同时提供并通过了 SHA-256 文件校验。
    pub checksum_verified: bool,
}

impl PluginManager {
    /// 从 GitHub Release 下载预构建 ZIP bundle 并安装。
    ///
    /// 安装器不会克隆仓库、构建源码或执行插件代码。下载和解包使用临时目录，所有路径、
    /// 文件类型和大小通过检查后才进入现有原子安装流程；失败时不会修改插件锁文件。
    pub async fn install_github(
        &self,
        source: &GithubPluginSource,
        options: GithubInstallOptions,
    ) -> Result<GithubInstallResult> {
        self.install_github_with_mode(source, options, false).await
    }

    /// 下载并原子替换同 ID 的已安装插件，供 Registry 更新流程调用。
    pub(crate) async fn update_github(
        &self,
        source: &GithubPluginSource,
        options: GithubInstallOptions,
    ) -> Result<GithubInstallResult> {
        self.install_github_with_mode(source, options, true).await
    }

    /// 共享 GitHub 获取和验证流程，并在最终事务阶段选择新增或替换。
    async fn install_github_with_mode(
        &self,
        source: &GithubPluginSource,
        options: GithubInstallOptions,
        replace: bool,
    ) -> Result<GithubInstallResult> {
        if options.max_download_bytes == 0 {
            bail!("GitHub bundle 下载大小上限必须大于零");
        }
        let client = github_client()?;
        let release = fetch_release(&client, source, options.tag.as_deref()).await?;
        let asset = select_bundle_asset(
            &release.assets,
            &source.repository,
            options.asset.as_deref(),
        )?;
        if asset.size > options.max_download_bytes {
            bail!(
                "GitHub Release asset `{}` 大小 {} 超过限制 {}",
                asset.name,
                asset.size,
                options.max_download_bytes
            );
        }
        let archive = download_asset(&client, asset, options.max_download_bytes).await?;
        let checksum_asset = find_checksum_asset(&release.assets, &asset.name);
        let checksum_verified = if let Some(expected_sha256) = options.expected_sha256.as_deref() {
            verify_expected_checksum(&archive, expected_sha256)?;
            true
        } else if let Some(checksum_asset) = checksum_asset {
            let checksum = download_asset(&client, checksum_asset, 64 * 1024).await?;
            verify_checksum(&archive, &checksum, &asset.name)?;
            true
        } else {
            false
        };

        let temporary = temporary_directory("github")?;
        let result = (|| -> Result<GithubInstallResult> {
            extract_bundle(&archive, &temporary)?;
            let bundle = locate_bundle_root(&temporary)?;
            let source_description = format!(
                "github:{}@{}#{}",
                source.repository_url(),
                release.tag_name,
                asset.name
            );
            let install_options = InstallOptions {
                enabled: options.enabled,
            };
            let plugin = if replace {
                self.replace_with_source(bundle, install_options, Some(source_description))?
            } else {
                self.install_with_source(bundle, install_options, Some(source_description))?
            };
            Ok(GithubInstallResult {
                plugin,
                release_tag: release.tag_name,
                asset_name: asset.name.clone(),
                checksum_verified,
            })
        })();
        let _ = fs::remove_dir_all(&temporary);
        result
    }
}

/// 从指定 GitHub Release 下载具名资产，供 Registry 索引读取复用。
///
/// 返回实际解析到的标签与资产内容；网络、鉴权、资产缺失或大小超限时返回错误。
pub(crate) async fn download_named_release_asset(
    source: &GithubPluginSource,
    tag: Option<&str>,
    asset_name: &str,
    max_download_bytes: u64,
) -> Result<(String, Vec<u8>)> {
    let client = github_client()?;
    let release = fetch_release(&client, source, tag).await?;
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == asset_name)
        .ok_or_else(|| {
            anyhow!(
                "GitHub Release `{}` 缺少资产 `{asset_name}`",
                release.tag_name
            )
        })?;
    let bytes = download_asset(&client, asset, max_download_bytes).await?;
    Ok((release.tag_name, bytes))
}

/// 显式联网检查 GitHub API 是否可访问；不下载插件或修改任何本地状态。
pub async fn check_github_connectivity() -> Result<()> {
    github_request(&github_client()?, format!("{GITHUB_API_ROOT}/rate_limit"))
        .send()
        .await
        .context("连接 GitHub API 失败")?
        .error_for_status()
        .context("GitHub API 返回失败状态")?;
    Ok(())
}

/// GitHub Release API 的最小响应结构。
#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

/// GitHub Release asset 的下载元数据。
#[derive(Debug, Clone, Deserialize)]
struct GithubAsset {
    name: String,
    size: u64,
    url: String,
}

/// 创建带固定客户端身份和安全默认值的 GitHub HTTP 客户端。
fn github_client() -> Result<Client> {
    Client::builder()
        .user_agent(concat!("lucia/", env!("CARGO_PKG_VERSION")))
        .https_only(true)
        .build()
        .context("创建 GitHub HTTP 客户端失败")
}

/// 请求指定 tag 或 latest release，并保留 GitHub API 的状态错误。
async fn fetch_release(
    client: &Client,
    source: &GithubPluginSource,
    tag: Option<&str>,
) -> Result<GithubRelease> {
    if let Some(tag) = tag {
        validate_release_tag(tag)?;
    }
    let endpoint = match tag {
        Some(tag) => format!(
            "{GITHUB_API_ROOT}/repos/{}/{}/releases/tags/{tag}",
            source.owner, source.repository
        ),
        None => format!(
            "{GITHUB_API_ROOT}/repos/{}/{}/releases/latest",
            source.owner, source.repository
        ),
    };
    github_request(client, endpoint)
        .send()
        .await
        .context("请求 GitHub Release 失败")?
        .error_for_status()
        .context("GitHub Release 不存在或无权访问")?
        .json()
        .await
        .context("解析 GitHub Release 响应失败")
}

/// 构造带可选 `GITHUB_TOKEN` 的 GitHub API 请求，且不会读取或输出 token 内容。
fn github_request(client: &Client, url: String) -> reqwest::RequestBuilder {
    let request = client
        .get(url)
        .header(header::ACCEPT, "application/vnd.github+json");
    match env::var("GITHUB_TOKEN") {
        Ok(token) if !token.trim().is_empty() => request.bearer_auth(token),
        _ => request,
    }
}

/// 按显式名称或稳定候选名选择唯一 ZIP bundle。
fn select_bundle_asset<'a>(
    assets: &'a [GithubAsset],
    repository: &str,
    explicit: Option<&str>,
) -> Result<&'a GithubAsset> {
    if let Some(name) = explicit {
        return assets
            .iter()
            .find(|asset| asset.name == name && asset.name.ends_with(".zip"))
            .ok_or_else(|| anyhow!("GitHub Release 中不存在 ZIP asset `{name}`"));
    }
    let candidates = [
        format!("lucia-plugin-{repository}.zip"),
        format!("{repository}.zip"),
        "lucia-plugin.zip".to_string(),
    ];
    for candidate in candidates {
        if let Some(asset) = assets.iter().find(|asset| asset.name == candidate) {
            return Ok(asset);
        }
    }
    let zip_assets = assets
        .iter()
        .filter(|asset| asset.name.ends_with(".zip"))
        .collect::<Vec<_>>();
    match zip_assets.as_slice() {
        [only] => Ok(*only),
        [] => bail!("GitHub Release 没有可安装的 ZIP bundle"),
        _ => bail!("GitHub Release 包含多个 ZIP asset，请使用 --asset 明确选择"),
    }
}

/// 查找与 bundle asset 配套的 SHA-256 文件。
fn find_checksum_asset<'a>(
    assets: &'a [GithubAsset],
    bundle_name: &str,
) -> Option<&'a GithubAsset> {
    let exact = format!("{bundle_name}.sha256");
    let stem = bundle_name.strip_suffix(".zip").unwrap_or(bundle_name);
    let short = format!("{stem}.sha256");
    assets
        .iter()
        .find(|asset| asset.name == exact)
        .or_else(|| assets.iter().find(|asset| asset.name == short))
}

/// 下载 GitHub asset，并同时执行声明大小和实际流量限制。
async fn download_asset(client: &Client, asset: &GithubAsset, limit: u64) -> Result<Vec<u8>> {
    if asset.size > limit {
        bail!("GitHub asset `{}` 超过下载大小限制", asset.name);
    }
    let response = github_request(client, asset.url.clone())
        .header(header::ACCEPT, "application/octet-stream")
        .send()
        .await
        .with_context(|| format!("下载 GitHub asset `{}` 失败", asset.name))?
        .error_for_status()
        .with_context(|| format!("GitHub asset `{}` 返回失败状态", asset.name))?;
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        bail!("GitHub asset `{}` 超过下载大小限制", asset.name);
    }
    let mut bytes = Vec::with_capacity((asset.size.min(limit)) as usize);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("读取 GitHub asset `{}` 失败", asset.name))?;
        if bytes.len() as u64 + chunk.len() as u64 > limit {
            bail!("GitHub asset `{}` 实际内容超过下载大小限制", asset.name);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// 校验可选的 Release SHA-256 文件。
fn verify_checksum(archive: &[u8], checksum: &[u8], asset_name: &str) -> Result<()> {
    let text = std::str::from_utf8(checksum).context("SHA-256 文件不是 UTF-8")?;
    let expected = text
        .split_whitespace()
        .next()
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| anyhow!("SHA-256 文件格式无效"))?;
    let actual = format!("{:x}", Sha256::digest(archive));
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("GitHub asset `{asset_name}` 的 SHA-256 校验失败");
    }
    Ok(())
}

/// 校验 Registry 内联声明的 SHA-256，避免索引与资产被拆分替换。
fn verify_expected_checksum(archive: &[u8], expected: &str) -> Result<()> {
    let expected = expected.trim();
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("Registry 中的 SHA-256 格式无效");
    }
    let actual = format!("{:x}", Sha256::digest(archive));
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("插件资产 SHA-256 校验失败");
    }
    Ok(())
}

/// 安全解包 ZIP，拒绝路径穿越、符号链接、特殊文件和资源超限。
fn extract_bundle(archive: &[u8], destination: &Path) -> Result<()> {
    let mut zip = ZipArchive::new(Cursor::new(archive)).context("插件 bundle 不是有效 ZIP")?;
    if zip.len() > MAX_ARCHIVE_ENTRIES {
        bail!("插件 ZIP 条目数量超过限制 {MAX_ARCHIVE_ENTRIES}");
    }
    let mut unpacked = 0_u64;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).context("读取插件 ZIP 条目失败")?;
        let Some(relative) = entry.enclosed_name() else {
            bail!("插件 ZIP 包含越界路径：{}", entry.name());
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            if kind != 0 && kind != 0o100000 && kind != 0o040000 {
                bail!("插件 ZIP 不允许符号链接或特殊文件：{}", entry.name());
            }
        }
        unpacked = unpacked
            .checked_add(entry.size())
            .filter(|size| *size <= DEFAULT_MAX_UNPACKED_BYTES)
            .ok_or_else(|| anyhow!("插件 ZIP 解压后大小超过限制"))?;
        let target = destination.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&target)
                .with_context(|| format!("创建插件解压目录失败：{}", target.display()))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("创建插件文件目录失败：{}", parent.display()))?;
        }
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .with_context(|| format!("创建插件解压文件失败：{}", target.display()))?;
        std::io::copy(&mut entry, &mut output)
            .with_context(|| format!("解压插件文件失败：{}", target.display()))?;
        output.flush().context("刷新插件解压文件失败")?;
    }
    Ok(())
}

/// 定位 ZIP 中唯一包含 `plugin.toml` 的 bundle 根目录。
fn locate_bundle_root(root: &Path) -> Result<PathBuf> {
    let mut manifests = Vec::new();
    collect_manifests(root, &mut manifests)?;
    match manifests.as_slice() {
        [manifest] => manifest
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow!("plugin.toml 缺少父目录")),
        [] => bail!("GitHub bundle 缺少 plugin.toml"),
        _ => bail!("GitHub bundle 包含多个 plugin.toml，无法确定安装根目录"),
    }
}

/// 递归收集普通 `plugin.toml`，并拒绝解包后出现的符号链接。
fn collect_manifests(directory: &Path, manifests: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(directory)
        .with_context(|| format!("读取插件解压目录失败：{}", directory.display()))?
    {
        let entry = entry.context("读取插件解压目录项失败")?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("检查插件解压条目失败：{}", path.display()))?;
        if metadata.file_type().is_symlink() {
            bail!("插件解压目录不允许符号链接：{}", path.display());
        }
        if metadata.is_dir() {
            collect_manifests(&path, manifests)?;
        } else if metadata.is_file()
            && path.file_name().and_then(|name| name.to_str()) == Some("plugin.toml")
        {
            manifests.push(path);
        }
    }
    Ok(())
}

/// 创建不与并发安装冲突的临时目录。
fn temporary_directory(purpose: &str) -> Result<PathBuf> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("系统时间早于 UNIX epoch")?
        .as_nanos();
    let directory = env::temp_dir().join(format!(
        "lucia-plugin-{purpose}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&directory)
        .with_context(|| format!("创建插件临时目录失败：{}", directory.display()))?;
    Ok(directory)
}

/// 校验 GitHub owner 和 repository 路径分量。
fn validate_github_component(value: &str, field: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("GitHub {field} 无效：`{value}`");
    }
    Ok(())
}

/// 校验 Release tag 不会改变 GitHub API 路径层级。
fn validate_release_tag(tag: &str) -> Result<()> {
    if tag.is_empty()
        || !tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("GitHub Release tag 无效：`{tag}`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::{write::SimpleFileOptions, ZipWriter};

    /// 裸名称、简写和 URL 必须解析为相同的稳定仓库身份。
    #[test]
    fn parses_supported_github_sources() {
        assert_eq!(
            GithubPluginSource::parse("demo").unwrap(),
            GithubPluginSource {
                owner: DEFAULT_GITHUB_PUBLISHER.into(),
                repository: "demo".into(),
            }
        );
        assert_eq!(
            GithubPluginSource::parse("owner/demo").unwrap(),
            GithubPluginSource::parse("https://github.com/owner/demo.git").unwrap()
        );
        assert!(GithubPluginSource::parse("https://example.com/owner/demo").is_err());
        assert!(GithubPluginSource::parse("owner/repo/extra").is_err());
    }

    /// 多个 ZIP asset 必须显式选择，稳定命名则可自动确定。
    #[test]
    fn selects_release_asset_without_ambiguity() {
        let assets = vec![
            asset("other.zip"),
            asset("lucia-plugin-demo.zip"),
            asset("notes.txt"),
        ];
        assert_eq!(
            select_bundle_asset(&assets, "demo", None).unwrap().name,
            "lucia-plugin-demo.zip"
        );
        assert_eq!(
            select_bundle_asset(&assets, "demo", Some("other.zip"))
                .unwrap()
                .name,
            "other.zip"
        );
    }

    /// ZIP 解包必须拒绝路径穿越，并能定位单一 bundle 根目录。
    #[test]
    fn extracts_only_safe_single_bundle() {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("release/plugin.toml", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"manifest").unwrap();
        writer
            .start_file("release/plugin.wasm", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"wasm").unwrap();
        let archive = writer.finish().unwrap().into_inner();
        let root = temporary_directory("extract-test").unwrap();

        extract_bundle(&archive, &root).unwrap();
        assert_eq!(locate_bundle_root(&root).unwrap(), root.join("release"));
        fs::remove_dir_all(root).unwrap();
    }

    /// ZIP 中的父目录穿越路径必须在创建目标文件前被拒绝。
    #[test]
    fn rejects_archive_path_traversal() {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("../escape", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"escape").unwrap();
        let archive = writer.finish().unwrap().into_inner();
        let root = temporary_directory("traversal-test").unwrap();

        let error = extract_bundle(&archive, &root).expect_err("路径穿越必须失败");

        assert!(error.to_string().contains("越界路径"));
        assert!(!root.join("escape").exists());
        fs::remove_dir_all(root).unwrap();
    }

    /// 配套 SHA-256 文件必须匹配下载的原始 ZIP 字节。
    #[test]
    fn verifies_release_checksum() {
        let archive = b"bundle";
        let checksum = format!("{:x}  plugin.zip\n", Sha256::digest(archive));
        verify_checksum(archive, checksum.as_bytes(), "plugin.zip").unwrap();
        assert!(verify_checksum(archive, b"0000", "plugin.zip").is_err());
    }

    /// 创建 Release asset 测试值。
    fn asset(name: &str) -> GithubAsset {
        GithubAsset {
            name: name.into(),
            size: 1,
            url: "https://api.github.com/assets/1".into(),
        }
    }
}
