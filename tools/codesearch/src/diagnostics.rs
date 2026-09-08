//! 同步搜索区间内的旁路诊断；不增加召回，不记录源码或模型向量，不改变公开响应。

use std::cell::RefCell;
use std::io::Write;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Mutex;

use serde_json::{json, Value};

thread_local! {
    static EVENTS: RefCell<Option<Vec<Value>>> = const { RefCell::new(None) };
}
static WRITER: Mutex<()> = Mutex::new(());

/// 只跨越无 await 的检索与组装区间；禁止在线程间移动，退出时恢复外层记录器。
pub(crate) struct Capture {
    path: Option<PathBuf>,
    previous: Option<Vec<Value>>,
    _same_thread: PhantomData<Rc<()>>,
}

impl Capture {
    /// 环境变量显式指定本地 JSONL 路径才启用；调用方负责目录存在和日志保留策略。
    pub(crate) fn start() -> Self {
        Self::at(
            std::env::var_os("CODESEARCH_TRACE_JSONL")
                .filter(|p| !p.is_empty())
                .map(PathBuf::from),
        )
    }

    /// 供内部确定性测试指定目的地，不修改进程环境；生产入口统一由 start 控制。
    pub(crate) fn at(path: Option<PathBuf>) -> Self {
        let previous = EVENTS.with(|events| events.replace(path.as_ref().map(|_| Vec::new())));
        Self {
            path,
            previous,
            _same_thread: PhantomData,
        }
    }

    /// 追加一条完整调用记录；文件错误只写警告，不影响搜索。新文件仅当前用户可读写。
    pub(crate) fn finish(self, query: &str, epoch: i64) {
        let Some(path) = &self.path else { return };
        let events = EVENTS
            .with(|events| events.borrow_mut().take())
            .unwrap_or_default();
        let write = || -> Result<(), Box<dyn std::error::Error>> {
            let mut bytes = serde_json::to_vec(
                &json!({"version":1,"query":query,"catalog_epoch":epoch,"stages":events}),
            )?;
            bytes.push(b'\n');
            let _lock = WRITER.lock().map_err(|_| "诊断写锁不可用")?;
            let mut options = std::fs::OpenOptions::new();
            options.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(path)?.write_all(&bytes)?;
            Ok(())
        };
        if let Err(error) = write() {
            tracing::warn!(%error, "搜索诊断写入失败");
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        EVENTS.with(|events| {
            events.replace(self.previous.take());
        });
    }
}

/// 懒构造阶段元数据；关闭诊断时不执行闭包，不访问数据库或分配快照。
pub(crate) fn record(stage: &str, data: impl FnOnce() -> Value) {
    EVENTS.with(|events| {
        if let Some(events) = events.borrow_mut().as_mut() {
            events.push(json!({"stage":stage,"data":data()}));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 关闭开关及异常退出不得遗留线程状态；内层调用不得污染外层。
    #[test]
    fn capture_is_lazy_and_scoped() {
        record("disabled", || panic!("关闭时不应执行"));
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("trace.jsonl");
        let outer = Capture::at(Some(path.clone()));
        record("outer", || json!([1]));
        {
            let _inner = Capture::at(Some(path.clone()));
            record("discarded", || json!([2]));
        }
        std::thread::spawn(|| record("other_thread", || panic!("跨线程泄漏")))
            .join()
            .unwrap();
        outer.finish("查询", 3);
        let value: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(value["stages"], json!([{"stage":"outer","data":[1]}]));
        record("disabled", || panic!("退出后应关闭"));
    }

    /// 写入失败不得传播成搜索失败，也不得破坏后续捕获。
    #[test]
    fn unavailable_destination_is_non_fatal() {
        let directory = tempfile::tempdir().unwrap();
        Capture::at(Some(directory.path().to_path_buf())).finish("查询", 0);
        record("disabled", || panic!("退出后应关闭"));
    }
}
