//! 可选能力审计文件出口：有界队列、单写入者和有限轮转；不是同 UID 防篡改证据链。

use super::{HostCapabilityCallObservation, HostServiceCallObservation, HostServiceCallObserver};
use anyhow::{bail, Context, Result};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const QUEUE_LIMIT: usize = 1024;
const FILE_LIMIT: u64 = 4 * 1024 * 1024;
const RETAINED_FILES: usize = 3;
enum Message {
    Record(HostCapabilityCallObservation),
    Flush(mpsc::SyncSender<bool>),
}

/// 有界、异步落盘的能力观察器；只接收可信能力记录，不保存旧服务事件的 Guest method。
///
/// 默认 fail-open：队列满或写盘失败会增加 dropped_count，不改变真实能力结果。
/// 最多保留当前文件及三份 4 MiB 轮转文件。应用应在退出前显式 flush；进程被杀时不保证尾部记录。
pub struct RotatingCapabilityAudit {
    tx: mpsc::SyncSender<Message>,
    dropped: Arc<AtomicUsize>,
}

impl RotatingCapabilityAudit {
    /// 在可信应用指定的专用目录启动写入线程。Unix 新目录 0700、新日志 0600。
    ///
    /// 已有目录必须私密且非符号链接；新目录不修改已有祖先权限。非法路径、权限或线程创建失败返回错误。
    pub fn open(directory: impl AsRef<Path>) -> Result<Self> {
        let directory = directory.as_ref().to_path_buf();
        ensure_directory(&directory)?;
        let lease_path = directory.join(".writer.lock");
        let lease = open_log(&lease_path)?;
        lease.try_lock().context("审计目录已有写入者")?;
        let file = open_log(&directory.join("capabilities.jsonl"))?;
        let (tx, rx) = mpsc::sync_channel(QUEUE_LIMIT);
        let dropped = Arc::new(AtomicUsize::new(0));
        let lost = Arc::clone(&dropped);
        std::thread::Builder::new().name("lucia-capability-audit".into()).spawn(move || {
            let _lease = lease;
            let mut file = Some(file);
            let mut sequence = 0u64;
            let epoch = uuid::Uuid::new_v4().to_string();
            while let Ok(message) = rx.recv() {
                match message {
                    Message::Record(record) => {
                        sequence = sequence.saturating_add(1);
                        let line = serde_json::to_vec(&serde_json::json!({
                            "schema_version": 1, "epoch": epoch, "sequence": sequence,
                            "timestamp_ms": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis(),
                            "record": record,
                        }));
                        if line.as_ref().is_err() || line.as_ref().is_ok_and(|line| line.len() > 4096) {
                            lost.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        let result = line.map_err(anyhow::Error::from).and_then(|mut line| {
                            line.push(b'\n');
                            if file.as_ref().context("审计已停用")?.metadata()?.len() + line.len() as u64 > FILE_LIMIT {
                                file.take();
                                rotate(&directory)?;
                                file = Some(open_log(&directory.join("capabilities.jsonl"))?);
                            }
                            file.as_mut().context("审计已停用")?.write_all(&line)?;
                            Ok(())
                        });
                        if result.is_err() { lost.fetch_add(1, Ordering::Relaxed); file.take(); }
                    }
                    Message::Flush(reply) => {
                        let ok = file.as_ref().is_some_and(|file| file.sync_all().is_ok());
                        let _ = reply.send(ok);
                    }
                }
            }
            if let Some(file) = file { let _ = file.sync_all(); }
        }).context("启动审计线程失败")?;
        Ok(Self { tx, dropped })
    }

    /// 返回未落盘记录数；非零意味着审计不完整，调用方应向用户报告。
    pub fn dropped_count(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }

    /// 等待先前队列记录并同步文件，供退出或测试使用；不可在 UI 热路径调用。
    /// 队列已满、线程结束、写盘失败或超时返回错误，不阻塞真实能力操作。
    pub fn flush(&self, timeout: Duration) -> Result<()> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.tx
            .try_send(Message::Flush(tx))
            .map_err(|_| anyhow::anyhow!("审计刷新队列不可用"))?;
        if !rx.recv_timeout(timeout).context("等待审计刷新失败")? {
            bail!("审计文件同步失败");
        }
        Ok(())
    }
}

impl HostServiceCallObserver for RotatingCapabilityAudit {
    fn observe(&self, _record: HostServiceCallObservation) {}
    fn observe_capability(&self, record: HostCapabilityCallObservation) {
        if self.tx.try_send(Message::Record(record)).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn ensure_directory(path: &Path) -> Result<()> {
    if !path.exists() {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(path)?;
    }
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        bail!("审计目录必须是非符号链接目录");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            bail!("已有审计专用目录必须设置为 0700");
        }
    }
    Ok(())
}

fn open_log(path: &Path) -> Result<fs::File> {
    check_file(path)?;
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        bail!("审计路径不是普通文件");
    }
    check_file(path)?;
    Ok(file)
}
fn check_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if !meta.is_file() || meta.file_type().is_symlink() {
                bail!("审计路径必须是非符号链接普通文件");
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o077 != 0 {
                    bail!("已有审计文件必须设置为 0600");
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}
fn rotate(directory: &Path) -> Result<()> {
    ensure_directory(directory)?;
    let path = |index: usize| -> PathBuf {
        directory.join(if index == 0 {
            "capabilities.jsonl".into()
        } else {
            format!("capabilities.{index}.jsonl")
        })
    };
    for index in (0..RETAINED_FILES).rev() {
        let source = path(index);
        let target = path(index + 1);
        check_file(&source)?;
        check_file(&target)?;
        if source.exists() {
            if target.exists() {
                fs::remove_file(&target)?;
            }
            fs::rename(source, target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::HostCapabilityCallResult;

    /// 超大单条仅丢该记录，后续正常记录及实际写线程轮转仍可工作。
    #[test]
    fn oversized_record_does_not_disable_writer() {
        let root =
            std::env::temp_dir().join(format!("lucia-audit-oversize-{}", uuid::Uuid::new_v4()));
        let sink = RotatingCapabilityAudit::open(&root).unwrap();
        let record = |id: String| HostCapabilityCallObservation {
            schema_version: 1,
            plugin_id: id,
            operation: "host-state-get",
            result: HostCapabilityCallResult::ReturnedOk,
        };
        sink.observe_capability(record("x".repeat(5000)));
        sink.observe_capability(record("healthy".into()));
        sink.flush(Duration::from_secs(2)).unwrap();
        assert_eq!(sink.dropped_count(), 1);
        let path = root.join("capabilities.jsonl");
        assert!(fs::read_to_string(&path).unwrap().contains("healthy"));
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(FILE_LIMIT)
            .unwrap();
        sink.observe_capability(record("after-rotation".into()));
        sink.flush(Duration::from_secs(2)).unwrap();
        assert_eq!(
            fs::metadata(root.join("capabilities.1.jsonl"))
                .unwrap()
                .len(),
            FILE_LIMIT
        );
        assert!(fs::read_to_string(path).unwrap().contains("after-rotation"));
        drop(sink);
        fs::remove_dir_all(root).unwrap();
    }

    /// 文件出口有序且不记录正文，创建权限和轮转保留数受控。
    #[test]
    fn audit_file_is_private_ordered_and_rotated() {
        let root = std::env::temp_dir().join(format!("lucia-audit-{}", uuid::Uuid::new_v4()));
        let sink = RotatingCapabilityAudit::open(&root).unwrap();
        for _ in 0..2 {
            sink.observe_capability(HostCapabilityCallObservation {
                schema_version: 1,
                plugin_id: "trusted".into(),
                operation: "host-fs-read",
                result: HostCapabilityCallResult::Denied,
            });
        }
        sink.flush(Duration::from_secs(2)).unwrap();
        let expected = root.canonicalize().unwrap();
        assert!(
            RotatingCapabilityAudit::open(&expected).is_err(),
            "同一目录不能有两个写入者"
        );
        let log = root.join("capabilities.jsonl");
        let values = fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values[0]["sequence"], 1);
        assert_eq!(values[1]["sequence"], 2);
        assert_eq!(sink.dropped_count(), 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&log).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        for _ in 0..6 {
            rotate(&root).unwrap();
            drop(open_log(&log).unwrap());
        }
        assert!(!root.join("capabilities.4.jsonl").exists());
        drop(sink);
        fs::remove_dir_all(root).unwrap();
    }
}
