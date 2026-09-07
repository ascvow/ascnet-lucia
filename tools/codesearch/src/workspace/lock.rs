//! workspace 进程级写锁。
//!
//! 同一 workspace 同时只允许一个写者进程（`index`、`serve` 或写模式的 `search`）修改
//! SQLite 目录状态与 USearch 检查点。使用操作系统文件锁，进程退出后自动释放。

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// 锁文件名。
pub const LOCK_FILE_NAME: &str = "writer.lock";

/// 已持有的独占写锁；释放于 Drop。
#[derive(Debug)]
pub struct WorkspaceLock {
    file: File,
    path: PathBuf,
}

impl WorkspaceLock {
    /// 尝试获取 `data_dir` 下的独占写锁。
    ///
    /// 被其他进程持有时返回 [`Error::WorkspaceBusy`]，不会阻塞等待。
    pub fn try_exclusive(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join(LOCK_FILE_NAME);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| Error::io(format!("打开锁文件 {}", path.display()), e))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(Error::WorkspaceBusy(path.display().to_string()));
            }
            Err(TryLockError::Error(e)) => {
                return Err(Error::io(format!("锁定 {}", path.display()), e));
            }
        }
        // 写入持有者 PID 便于诊断；失败不影响锁语义。
        let _ = write_owner(&file);
        Ok(Self { file, path })
    }

    /// 锁文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn write_owner(file: &File) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut file = file;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "pid={}", std::process::id())?;
    Ok(())
}

impl Drop for WorkspaceLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_lock_in_same_process_is_busy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = WorkspaceLock::try_exclusive(dir.path()).expect("首个锁应成功");
        let second = WorkspaceLock::try_exclusive(dir.path());
        assert!(matches!(second, Err(Error::WorkspaceBusy(_))));
        drop(first);
        WorkspaceLock::try_exclusive(dir.path()).expect("释放后应可再次获取");
    }
}
