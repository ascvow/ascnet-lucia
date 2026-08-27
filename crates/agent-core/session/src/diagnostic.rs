//! 会话文件存储的只读诊断。

use crate::{validate_record, SessionId, SessionRecord};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path, path::PathBuf};

/// 单项会话存储诊断问题。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionDiagnosticIssue {
    /// 出现问题的文件或目录。
    pub path: PathBuf,
    /// 可直接展示给用户的中文问题说明。
    pub message: String,
}

/// 会话文件存储的只读诊断报告。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionDiagnosticReport {
    /// 已成功读取并校验的会话记录数量。
    pub checked_records: usize,
    /// 诊断发现的全部问题。
    pub issues: Vec<SessionDiagnosticIssue>,
}

impl SessionDiagnosticReport {
    /// 报告没有发现问题时返回 `true`。
    pub fn is_healthy(&self) -> bool {
        self.issues.is_empty()
    }
}

/// 只读检查一个 [`crate::FileSessionStore`] 根目录。
///
/// 目录不存在表示尚未产生持久化会话，返回空的健康报告。该函数不会创建目录、锁文件、
/// 摘要索引或修改任何记录；目录项在读取期间变化时会作为诊断问题返回。
pub fn diagnose_file_session_store(root: impl AsRef<Path>) -> SessionDiagnosticReport {
    let root = root.as_ref();
    let mut report = SessionDiagnosticReport::default();
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return report,
        Err(error) => {
            push_issue(&mut report, root, format!("无法检查会话目录：{error}"));
            return report;
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        push_issue(
            &mut report,
            root,
            "会话存储根路径必须是非符号链接目录".to_string(),
        );
        return report;
    }

    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            push_issue(&mut report, root, format!("无法读取会话目录：{error}"));
            return report;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                push_issue(&mut report, root, format!("无法读取会话目录项：{error}"));
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        inspect_record(&path, &mut report);
    }
    report
}

/// 校验单个会话文件的路径、文件名、JSON 和 schema，不产生任何写入。
fn inspect_record(path: &Path, report: &mut SessionDiagnosticReport) {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            push_issue(report, path, format!("无法检查会话文件：{error}"));
            return;
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        push_issue(report, path, "会话路径必须是非符号链接普通文件".to_string());
        return;
    }
    let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
        push_issue(report, path, "会话文件名必须是 UTF-8".to_string());
        return;
    };
    let file_id = match SessionId::new(stem) {
        Ok(id) => id,
        Err(error) => {
            push_issue(report, path, error.to_string());
            return;
        }
    };
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            push_issue(report, path, format!("无法读取会话文件：{error}"));
            return;
        }
    };
    let record: SessionRecord = match serde_json::from_slice(&bytes) {
        Ok(record) => record,
        Err(error) => {
            push_issue(report, path, format!("会话 JSON 无效：{error}"));
            return;
        }
    };
    if let Err(error) = validate_record(&record) {
        push_issue(report, path, error.to_string());
        return;
    }
    if record.id != file_id {
        push_issue(
            report,
            path,
            format!(
                "会话文件名 ID `{}` 与记录 ID `{}` 不一致",
                file_id, record.id
            ),
        );
        return;
    }
    report.checked_records += 1;
}

/// 向报告追加一个与路径关联的问题。
fn push_issue(report: &mut SessionDiagnosticReport, path: &Path, message: String) {
    report.issues.push(SessionDiagnosticIssue {
        path: path.to_path_buf(),
        message,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionStore;
    use agent_core::Session;

    /// 诊断不存在的目录不得创建任何文件或返回错误。
    #[test]
    fn missing_store_is_healthy_and_untouched() {
        let root = std::env::temp_dir().join(format!(
            "lucia-session-doctor-missing-{}",
            SessionId::generate()
        ));
        let report = diagnose_file_session_store(&root);
        assert!(report.is_healthy());
        assert!(!root.exists());
    }

    /// 诊断应识别有效记录和损坏 JSON，且不创建锁文件或摘要索引。
    #[tokio::test]
    async fn diagnoses_records_without_store_side_effects() {
        let root = std::env::temp_dir().join(format!(
            "lucia-session-doctor-records-{}",
            SessionId::generate()
        ));
        let store = crate::FileSessionStore::open(&root)
            .await
            .expect("创建测试存储");
        let record = SessionRecord::new(SessionId::new("healthy").unwrap(), Session::new())
            .expect("创建会话记录");
        store.save(record, None).await.expect("保存测试会话");
        fs::write(root.join("broken.json"), b"{").expect("写入损坏记录");
        let before = fs::read_dir(&root).unwrap().count();

        let report = diagnose_file_session_store(&root);

        assert_eq!(report.checked_records, 1);
        assert_eq!(report.issues.len(), 1);
        assert!(report.issues[0].message.contains("JSON 无效"));
        assert_eq!(fs::read_dir(&root).unwrap().count(), before);
        fs::remove_dir_all(root).expect("清理测试目录");
    }
}
