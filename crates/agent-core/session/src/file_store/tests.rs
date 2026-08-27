use super::*;
use crate::{MemorySessionStore, CURRENT_SESSION_SCHEMA_VERSION};
use agent_core::Session;

const LOCK_HELPER_ROOT_ENV: &str = "LUCIA_SESSION_LOCK_HELPER_ROOT";
const LOCK_HELPER_READY_ENV: &str = "LUCIA_SESSION_LOCK_HELPER_READY";
const LOCK_HELPER_RELEASE_ENV: &str = "LUCIA_SESSION_LOCK_HELPER_RELEASE";

fn id(value: &str) -> SessionId {
    SessionId::new(value).expect("测试会话标识应该有效")
}

fn record(value: &str) -> SessionRecord {
    let mut session = Session::new();
    session.set_system("测试系统提示词");
    session.push_user("测试消息");
    SessionRecord::new(id(value), session).expect("应该可以创建测试记录")
}

fn test_directory() -> PathBuf {
    std::env::temp_dir().join(format!("lucia-session-test-{}", Uuid::new_v4()))
}

async fn remove_test_directory(path: &Path) {
    let _ = fs::remove_dir_all(path).await;
}

#[test]
fn file_store_cross_process_lock_holder_helper() {
    let Some(root) = std::env::var_os(LOCK_HELPER_ROOT_ENV) else {
        return;
    };
    let ready =
        PathBuf::from(std::env::var_os(LOCK_HELPER_READY_ENV).expect("锁测试必须提供就绪文件路径"));
    let release = PathBuf::from(
        std::env::var_os(LOCK_HELPER_RELEASE_ENV).expect("锁测试必须提供释放文件路径"),
    );
    let lock_path = PathBuf::from(root).join(STORE_LOCK_FILE_NAME);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("子进程应该可以打开会话存储锁文件");
    file.lock().expect("子进程应该可以获取跨进程锁");
    std::fs::write(&ready, b"ready").expect("子进程应该可以发送就绪信号");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !release.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "等待父进程释放跨进程锁超时"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    file.unlock().expect("子进程应该可以释放跨进程锁");
}

#[test]
fn generated_session_ids_are_valid_and_unique() {
    let first = SessionId::generate();
    let second = SessionId::generate();

    assert_ne!(first, second);
    assert!(Uuid::parse_str(first.as_str()).is_ok());
    assert_eq!(SessionId::new(first.to_string()).unwrap(), first);
}

#[test]
fn session_id_rejects_path_escape_and_invalid_json() {
    for invalid in ["", "../escape", "nested/name", ".", "会话"] {
        assert!(SessionId::new(invalid).is_err(), "应拒绝 {invalid:?}");
    }
    let error =
        serde_json::from_str::<SessionId>(r#""../escape""#).expect_err("反序列化不能绕过标识校验");
    assert!(error.to_string().contains("非法会话标识"));
}

#[tokio::test]
async fn memory_store_supports_cas_lifecycle() {
    let store = MemorySessionStore::new();
    let created = store
        .save(record("session_a"), None)
        .await
        .expect("首次保存应该成功");
    assert_eq!(created.revision, 1);
    assert_eq!(
        store.load(&created.id).await.unwrap(),
        Some(created.clone())
    );

    let mut updated = created.clone();
    updated.title = Some("新标题".to_owned());
    let updated = store
        .save(updated, Some(created.revision))
        .await
        .expect("匹配修订号时应该更新成功");
    assert_eq!(updated.revision, 2);

    let error = store
        .save(created, Some(1))
        .await
        .expect_err("过期记录不能覆盖新记录");
    assert!(matches!(
        error,
        SessionStoreError::RevisionConflict {
            expected: Some(1),
            actual: Some(2),
            ..
        }
    ));

    store
        .delete(&updated.id, updated.revision)
        .await
        .expect("匹配修订号时应该删除成功");
    assert!(store.load(&updated.id).await.unwrap().is_none());
}

#[tokio::test]
async fn memory_store_lists_records_by_id() {
    let store = MemorySessionStore::new();
    store.save(record("z"), None).await.unwrap();
    store.save(record("a"), None).await.unwrap();

    let ids: Vec<_> = store
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|record| record.id.to_string())
        .collect();
    assert_eq!(ids, ["a", "z"]);
}

#[tokio::test]
async fn memory_store_lists_summaries_by_id() {
    let store = MemorySessionStore::new();
    let mut last = record("z");
    last.session.push_assistant_text("第二条消息");
    let last = store.save(last, None).await.unwrap();
    let mut first = record("a");
    first.title = Some("第一个会话".to_owned());
    let first = store.save(first, None).await.unwrap();

    let summaries = store.list_summaries().await.unwrap();

    assert_eq!(
        summaries,
        [SessionSummary::from(&first), SessionSummary::from(&last)]
    );
    assert_eq!(summaries[0].title.as_deref(), Some("第一个会话"));
    assert_eq!(summaries[1].message_count, 2);
}

#[tokio::test]
async fn file_store_persists_records_across_reopen() {
    let root = test_directory();
    let store = FileSessionStore::open(&root).await.unwrap();
    let saved = store.save(record("persisted"), None).await.unwrap();
    assert!(fs::try_exists(store.summary_index_path()).await.unwrap());
    drop(store);

    let reopened = FileSessionStore::open(&root).await.unwrap();
    assert_eq!(reopened.load(&saved.id).await.unwrap(), Some(saved));
    assert_eq!(reopened.list().await.unwrap().len(), 1);
    assert_eq!(
        reopened.list_summaries().await.unwrap(),
        [SessionSummary::from(
            &reopened.load(&id("persisted")).await.unwrap().unwrap()
        )]
    );

    remove_test_directory(&root).await;
}

#[tokio::test]
async fn file_store_summary_index_tracks_updates_and_deletes_across_reopen() {
    let root = test_directory();
    let store = FileSessionStore::open(&root).await.unwrap();
    let first = store.save(record("first"), None).await.unwrap();
    let second = store.save(record("second"), None).await.unwrap();
    drop(store);

    let reopened = FileSessionStore::open(&root).await.unwrap();
    let mut updated = first.clone();
    updated.title = Some("更新后的会话".to_owned());
    updated.session.push_assistant_text("新增回复");
    let updated = reopened.save(updated, Some(first.revision)).await.unwrap();
    assert_eq!(
        reopened.list_summaries().await.unwrap(),
        [
            SessionSummary::from(&updated),
            SessionSummary::from(&second)
        ]
    );

    reopened.delete(&second.id, second.revision).await.unwrap();
    drop(reopened);

    let reopened = FileSessionStore::open(&root).await.unwrap();
    assert_eq!(
        reopened.list_summaries().await.unwrap(),
        [SessionSummary::from(&updated)]
    );

    remove_test_directory(&root).await;
}

#[tokio::test]
async fn file_store_rebuilds_missing_index_from_legacy_records() {
    let root = test_directory();
    let store = FileSessionStore::open(&root).await.unwrap();
    let mut legacy = record("legacy");
    legacy.revision = 4;
    legacy.title = Some("旧会话".to_owned());
    legacy.session.push_assistant_text("旧回复");
    fs::write(
        store.record_path(&legacy.id),
        serde_json::to_vec_pretty(&legacy).unwrap(),
    )
    .await
    .unwrap();
    assert!(!fs::try_exists(store.summary_index_path()).await.unwrap());

    assert_eq!(
        store.list_summaries().await.unwrap(),
        [SessionSummary::from(&legacy)]
    );
    assert!(fs::try_exists(store.summary_index_path()).await.unwrap());
    drop(store);

    let reopened = FileSessionStore::open(&root).await.unwrap();
    assert_eq!(
        reopened.list_summaries().await.unwrap(),
        [SessionSummary::from(&legacy)]
    );

    remove_test_directory(&root).await;
}

#[tokio::test]
async fn file_store_rebuilds_corrupted_summary_index() {
    let root = test_directory();
    let store = FileSessionStore::open(&root).await.unwrap();
    let saved = store.save(record("corrupted_index"), None).await.unwrap();
    fs::write(store.summary_index_path(), "不是有效索引".as_bytes())
        .await
        .unwrap();

    assert_eq!(
        store.list_summaries().await.unwrap(),
        [SessionSummary::from(&saved)]
    );
    let rebuilt: StoredSessionSummaryIndex =
        serde_json::from_slice(&fs::read(store.summary_index_path()).await.unwrap()).unwrap();
    assert_eq!(rebuilt.schema_version, CURRENT_SUMMARY_INDEX_SCHEMA_VERSION);
    assert_eq!(rebuilt.summaries, [SessionSummary::from(&saved)]);

    remove_test_directory(&root).await;
}

#[tokio::test]
async fn file_store_summary_index_avoids_reading_session_records() {
    let root = test_directory();
    let store = FileSessionStore::open(&root).await.unwrap();
    let saved = store.save(record("indexed"), None).await.unwrap();
    fs::write(store.record_path(&saved.id), "不是有效会话记录".as_bytes())
        .await
        .unwrap();

    assert_eq!(
        store.list_summaries().await.unwrap(),
        [SessionSummary::from(&saved)]
    );
    assert!(matches!(
        store.list().await,
        Err(SessionStoreError::InvalidRecord { .. })
    ));

    remove_test_directory(&root).await;
}

#[tokio::test]
async fn file_store_summary_skips_full_message_deserialization() {
    let root = test_directory();
    let store = FileSessionStore::open(&root).await.unwrap();
    let session_id = id("lightweight");
    let path = store.record_path(&session_id);
    let malformed_messages = serde_json::json!({
        "schema_version": CURRENT_SESSION_SCHEMA_VERSION,
        "id": session_id,
        "revision": 7,
        "created_at_ms": 11,
        "updated_at_ms": 22,
        "title": "轻量摘要",
        "session": {
            "messages": [null, { "不是": "有效模型消息" }]
        }
    });
    fs::write(&path, serde_json::to_vec(&malformed_messages).unwrap())
        .await
        .unwrap();

    let summaries = store.list_summaries().await.unwrap();

    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id, id("lightweight"));
    assert_eq!(summaries[0].revision, 7);
    assert_eq!(summaries[0].updated_at_ms, 22);
    assert_eq!(summaries[0].title.as_deref(), Some("轻量摘要"));
    assert_eq!(summaries[0].message_count, 2);
    assert!(matches!(
        store.list().await,
        Err(SessionStoreError::InvalidRecord { .. })
    ));

    remove_test_directory(&root).await;
}

#[tokio::test]
async fn file_store_serializes_concurrent_cas_updates_across_instances() {
    let root = test_directory();
    let left = FileSessionStore::open(&root).await.unwrap();
    let right = FileSessionStore::open(&root).await.unwrap();
    let created = left.save(record("concurrent"), None).await.unwrap();
    let left_record = created.clone();
    let right_record = created.clone();

    let (left_result, right_result) = tokio::join!(
        left.save(left_record, Some(created.revision)),
        right.save(right_record, Some(created.revision))
    );
    let successes = usize::from(left_result.is_ok()) + usize::from(right_result.is_ok());
    assert_eq!(successes, 1);
    let error = left_result.err().or_else(|| right_result.err()).unwrap();
    assert!(matches!(error, SessionStoreError::RevisionConflict { .. }));

    remove_test_directory(&root).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_store_serializes_all_operations_with_another_process() {
    let root = test_directory();
    let store = FileSessionStore::open(&root).await.unwrap();
    let loaded = store.save(record("locked_load"), None).await.unwrap();
    let deleted = store.save(record("locked_delete"), None).await.unwrap();
    let saved = record("locked_save");
    let ready = root.join("helper-ready");
    let release = root.join("helper-release");
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("file_store::tests::file_store_cross_process_lock_holder_helper")
        .arg("--test-threads=1")
        .env(LOCK_HELPER_ROOT_ENV, &root)
        .env(LOCK_HELPER_READY_ENV, &ready)
        .env(LOCK_HELPER_RELEASE_ENV, &release)
        .kill_on_drop(true)
        .spawn()
        .expect("应该可以启动跨进程锁测试子进程");

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if fs::try_exists(&ready).await.unwrap() {
                break;
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "跨进程锁测试子进程在就绪前退出"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("等待跨进程锁测试子进程就绪超时");

    let barrier = Arc::new(tokio::sync::Barrier::new(6));
    let load_task = tokio::spawn({
        let store = store.clone();
        let barrier = Arc::clone(&barrier);
        let id = loaded.id.clone();
        async move {
            barrier.wait().await;
            store.load(&id).await
        }
    });
    let save_task = tokio::spawn({
        let store = store.clone();
        let barrier = Arc::clone(&barrier);
        async move {
            barrier.wait().await;
            store.save(saved, None).await
        }
    });
    let delete_task = tokio::spawn({
        let store = store.clone();
        let barrier = Arc::clone(&barrier);
        let id = deleted.id.clone();
        let revision = deleted.revision;
        async move {
            barrier.wait().await;
            store.delete(&id, revision).await
        }
    });
    let list_task = tokio::spawn({
        let store = store.clone();
        let barrier = Arc::clone(&barrier);
        async move {
            barrier.wait().await;
            store.list().await
        }
    });
    let summaries_task = tokio::spawn({
        let store = store.clone();
        let barrier = Arc::clone(&barrier);
        async move {
            barrier.wait().await;
            store.list_summaries().await
        }
    });
    barrier.wait().await;
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    assert!(!load_task.is_finished(), "load 必须等待跨进程锁");
    assert!(!save_task.is_finished(), "save 必须等待跨进程锁");
    assert!(!delete_task.is_finished(), "delete 必须等待跨进程锁");
    assert!(!list_task.is_finished(), "list 必须等待跨进程锁");
    assert!(
        !summaries_task.is_finished(),
        "list_summaries 必须等待跨进程锁"
    );

    fs::write(&release, b"release").await.unwrap();
    let (load_result, save_result, delete_result, list_result, summaries_result) =
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(load_task, save_task, delete_task, list_task, summaries_task)
        })
        .await
        .expect("跨进程锁释放后存储操作应该完成");

    assert_eq!(load_result.unwrap().unwrap(), Some(loaded));
    assert_eq!(save_result.unwrap().unwrap().revision, 1);
    delete_result.unwrap().unwrap();
    list_result.unwrap().unwrap();
    summaries_result.unwrap().unwrap();
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .expect("等待跨进程锁测试子进程退出超时")
        .unwrap();
    assert!(status.success());

    remove_test_directory(&root).await;
}

#[tokio::test]
async fn store_rejects_unsupported_schema_and_revision_mismatch() {
    let store = MemorySessionStore::new();
    let mut unsupported = record("unsupported");
    unsupported.schema_version = CURRENT_SESSION_SCHEMA_VERSION + 1;
    assert!(matches!(
        store.save(unsupported, None).await,
        Err(SessionStoreError::UnsupportedSchemaVersion { .. })
    ));

    let mut mismatched = record("mismatched");
    mismatched.revision = 3;
    assert!(matches!(
        store.save(mismatched, None).await,
        Err(SessionStoreError::RecordRevisionMismatch { .. })
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn file_store_rejects_symlinked_session_file() {
    use std::os::unix::fs::symlink;

    let root = test_directory();
    let store = FileSessionStore::open(&root).await.unwrap();
    let outside = root.with_extension("outside.json");
    fs::write(&outside, b"{}").await.unwrap();
    symlink(&outside, store.record_path(&id("linked"))).unwrap();

    assert!(matches!(
        store.load(&id("linked")).await,
        Err(SessionStoreError::UnsafePath { .. })
    ));

    remove_test_directory(&root).await;
    let _ = fs::remove_file(outside).await;
}

#[cfg(unix)]
#[tokio::test]
async fn file_store_rejects_symlinked_lock_file() {
    use std::os::unix::fs::symlink;

    let root = test_directory();
    fs::create_dir_all(&root).await.unwrap();
    let outside = root.with_extension("outside.lock");
    fs::write(&outside, b"").await.unwrap();
    symlink(&outside, root.join(STORE_LOCK_FILE_NAME)).unwrap();

    assert!(matches!(
        FileSessionStore::open(&root).await,
        Err(SessionStoreError::UnsafePath { .. })
    ));

    remove_test_directory(&root).await;
    let _ = fs::remove_file(outside).await;
}
