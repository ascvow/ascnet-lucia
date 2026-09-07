//! workspace 身份、路径作用域校验与数据目录。
//!
//! 所有文件访问都必须经过 [`Workspace::resolve`]，防止 `..`、绝对路径或符号链接绕过作用域。

pub mod lock;
pub mod scan;

use std::path::{Component, Path, PathBuf};

use crate::config::DEFAULT_DATA_DIR;
use crate::error::{Error, Result};

/// 已规范化的 workspace。
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
    data_dir: PathBuf,
    id: String,
}

impl Workspace {
    /// 打开工作目录：规范化路径并推导 workspace 身份。
    ///
    /// `data_dir` 为空时使用 `<root>/.codesearch`。
    pub fn open(root: &Path, data_dir: Option<&Path>) -> Result<Self> {
        let root = std::fs::canonicalize(root).map_err(|source| Error::Workspace {
            path: root.to_path_buf(),
            source,
        })?;
        if !root.is_dir() {
            return Err(Error::Workspace {
                path: root.clone(),
                source: std::io::Error::new(std::io::ErrorKind::NotADirectory, "不是目录"),
            });
        }
        let id = workspace_id(&root);
        let data_dir = match data_dir {
            Some(dir) if dir.is_absolute() => dir.to_path_buf(),
            Some(dir) => root.join(dir),
            None => root.join(DEFAULT_DATA_DIR),
        };
        Ok(Self { root, data_dir, id })
    }

    /// 规范化后的根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 索引数据目录（数据库、向量检查点、锁文件）。
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// 与规范化根目录一一对应的 workspace 身份。
    pub fn id(&self) -> &str {
        &self.id
    }

    /// 创建数据目录，并写入 `.gitignore` 让 Git 忽略其内容。
    pub fn ensure_data_dir(&self) -> Result<()> {
        std::fs::create_dir_all(&self.data_dir)
            .map_err(|e| Error::io(format!("创建数据目录 {}", self.data_dir.display()), e))?;
        let gitignore = self.data_dir.join(".gitignore");
        if !gitignore.exists() {
            std::fs::write(&gitignore, "# codesearch 索引数据，自动生成。\n*\n")
                .map_err(|e| Error::io("写入数据目录 .gitignore", e))?;
        }
        Ok(())
    }

    /// 校验并解析一个相对路径，返回 workspace 内的绝对路径。
    ///
    /// 拒绝绝对路径、`..` 组件以及解析后逃出根目录的符号链接。
    /// 路径不存在时仅做词法校验（用于删除事件等场景）。
    pub fn resolve(&self, relative: &str) -> Result<PathBuf> {
        let candidate = Path::new(relative);
        if candidate.is_absolute() {
            return Err(Error::PathOutOfScope(relative.to_string()));
        }
        let mut joined = self.root.clone();
        for component in candidate.components() {
            match component {
                Component::Normal(part) => joined.push(part),
                Component::CurDir => {}
                _ => return Err(Error::PathOutOfScope(relative.to_string())),
            }
        }
        if joined == self.root {
            return Err(Error::PathOutOfScope(relative.to_string()));
        }
        // 对存在的路径做物理校验：符号链接解析后必须仍在根目录内。
        if joined.symlink_metadata().is_ok() {
            let real = std::fs::canonicalize(&joined)
                .map_err(|e| Error::io(format!("解析路径 {}", joined.display()), e))?;
            if !real.starts_with(&self.root) {
                return Err(Error::PathOutOfScope(relative.to_string()));
            }
        }
        Ok(joined)
    }

    /// 把 workspace 内的绝对路径转换为 `/` 分隔的相对路径。
    pub fn relativize(&self, absolute: &Path) -> Option<String> {
        let relative = absolute.strip_prefix(&self.root).ok()?;
        let mut parts = Vec::new();
        for component in relative.components() {
            match component {
                Component::Normal(part) => parts.push(part.to_str()?.to_string()),
                Component::CurDir => {}
                _ => return None,
            }
        }
        if parts.is_empty() {
            return None;
        }
        Some(parts.join("/"))
    }
}

/// 由规范化根目录派生稳定的 workspace 身份（blake3 前 16 个十六进制字符）。
pub fn workspace_id(root: &Path) -> String {
    let hash = blake3::hash(root.to_string_lossy().as_bytes());
    hash.to_hex()[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_and_absolute_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = Workspace::open(dir.path(), None).expect("open");
        assert!(matches!(ws.resolve("../x"), Err(Error::PathOutOfScope(_))));
        assert!(matches!(ws.resolve("a/../../x"), Err(Error::PathOutOfScope(_))));
        assert!(matches!(ws.resolve("/etc/passwd"), Err(Error::PathOutOfScope(_))));
        assert!(matches!(ws.resolve(""), Err(Error::PathOutOfScope(_))));
        assert!(ws.resolve("src/lib.rs").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escaping_workspace() {
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret.txt"), "secret").expect("write");
        let dir = tempfile::tempdir().expect("tempdir");
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), dir.path().join("link.txt"))
            .expect("symlink");
        let ws = Workspace::open(dir.path(), None).expect("open");
        assert!(matches!(ws.resolve("link.txt"), Err(Error::PathOutOfScope(_))));
    }

    #[test]
    fn relativize_uses_forward_slashes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = Workspace::open(dir.path(), None).expect("open");
        let abs = ws.root().join("a").join("b.rs");
        assert_eq!(ws.relativize(&abs).as_deref(), Some("a/b.rs"));
        assert_eq!(ws.relativize(ws.root()), None);
    }
}
