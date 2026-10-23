//! 流式链路诊断探针。
//!
//! 卡顿可能来自网络抖动，也可能来自渲染拖慢了事件消费，只看屏幕无法区分。设置
//! `LUCIA_STREAM_DEBUG=1` 后，探针在内存里累计两类样本，退出时一次性写出一份报告：
//!
//! - **到达侧**：模型增量之间、工具输出 chunk 之间的间隔分位数，用来判断网络节奏；
//! - **渲染侧**：帧数、单帧耗时、每帧揭示的字符数，用来判断渲染是否成为瓶颈。
//!
//! 只有一次文件写入（退出时），热路径上没有任何 IO 或系统调用：诊断本身不能变成新的
//! 卡顿来源。默认关闭，未设置环境变量时所有入口立即返回。

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 探针报告的固定文件名；多次运行追加写入，便于对比。
const REPORT_FILE: &str = "stream-debug.log";

/// 累计样本；由网络侧与渲染侧共享，因此用互斥锁保护。
#[derive(Default)]
struct Samples {
    /// 到达间隔（微秒），分别记录模型文本与工具输出。
    model_gaps_us: Vec<u64>,
    tool_gaps_us: Vec<u64>,
    /// 模型文本增量与工具输出 chunk 的总数与总字节数。
    model_deltas: usize,
    model_bytes: usize,
    tool_deltas: usize,
    /// 渲染帧数、累计与最坏单帧耗时（微秒）、揭示字符总数。
    frames: usize,
    frame_total_us: u64,
    frame_max_us: u64,
    revealed_chars: usize,
    /// 上一次到达的时间点，用于计算间隔。
    last_model_at: Option<Instant>,
    last_tool_at: Option<Instant>,
}

/// 流式链路诊断探针；未启用时 [`StreamProbe::from_env`] 返回 `None`。
#[derive(Clone)]
pub(crate) struct StreamProbe {
    samples: Arc<Mutex<Samples>>,
}

impl StreamProbe {
    /// 按 `LUCIA_STREAM_DEBUG` 创建探针；除 `1` 以外的取值都视为关闭。
    pub(crate) fn from_env() -> Option<Self> {
        matches!(std::env::var("LUCIA_STREAM_DEBUG").as_deref(), Ok("1")).then(|| Self {
            samples: Arc::new(Mutex::new(Samples::default())),
        })
    }

    /// 记录一段模型文本增量：调用方已经拿到解析后的文本，这里只关心大小与到达间隔。
    pub(crate) fn record_model_delta(&self, bytes: usize) {
        let Ok(mut samples) = self.samples.lock() else {
            return;
        };
        let now = Instant::now();
        if let Some(previous) = samples.last_model_at {
            samples
                .model_gaps_us
                .push(now.duration_since(previous).as_micros() as u64);
        }
        samples.last_model_at = Some(now);
        samples.model_deltas += 1;
        samples.model_bytes += bytes;
    }

    /// 记录一段工具输出 chunk 的到达。
    pub(crate) fn record_tool_delta(&self) {
        let Ok(mut samples) = self.samples.lock() else {
            return;
        };
        let now = Instant::now();
        if let Some(previous) = samples.last_tool_at {
            samples
                .tool_gaps_us
                .push(now.duration_since(previous).as_micros() as u64);
        }
        samples.last_tool_at = Some(now);
        samples.tool_deltas += 1;
    }

    /// 记录一帧的耗时，以及这一帧揭示的字符数。
    pub(crate) fn record_frame(&self, elapsed: Duration, revealed_chars: usize) {
        let Ok(mut samples) = self.samples.lock() else {
            return;
        };
        let micros = elapsed.as_micros() as u64;
        samples.frames += 1;
        samples.frame_total_us = samples.frame_total_us.saturating_add(micros);
        samples.frame_max_us = samples.frame_max_us.max(micros);
        samples.revealed_chars += revealed_chars;
    }

    /// 把累计样本写成一份可读报告；`dir` 通常是 Lucia 主目录。
    ///
    /// 返回写出的文件路径，便于在启动日志里提示用户去哪里查看。IO 失败不影响主流程。
    pub(crate) fn write_report(&self, dir: &Path) -> Option<std::path::PathBuf> {
        let samples = self.samples.lock().ok()?;
        let path = dir.join("logs").join(REPORT_FILE);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok()?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        let report = samples.render();
        file.write_all(report.as_bytes()).ok()?;
        Some(path)
    }
}

impl Samples {
    /// 生成报告正文：先给结论指标，再给分位数，便于直接对照"网络抖动 vs 渲染瓶颈"。
    fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("\n=== lucia stream debug ===\n");
        out.push_str(&format!(
            "model deltas: {} ({} bytes)\n",
            self.model_deltas, self.model_bytes
        ));
        out.push_str(&format!("tool output chunks: {}\n", self.tool_deltas));
        if let Some((p50, p95, max)) = percentiles(&self.model_gaps_us) {
            out.push_str(&format!("model gap: p50={p50}us p95={p95}us max={max}us\n"));
        }
        if let Some((p50, p95, max)) = percentiles(&self.tool_gaps_us) {
            out.push_str(&format!("tool gap: p50={p50}us p95={p95}us max={max}us\n"));
        }
        if self.frames > 0 {
            let average = self.frame_total_us / self.frames as u64;
            out.push_str(&format!(
                "frames: {} avg={}us max={}us ({} chars revealed)\n",
                self.frames, average, self.frame_max_us, self.revealed_chars
            ));
            // 平均帧耗时换算成"每秒能出多少帧"，一眼看出是否还有余量。
            let capacity = if average == 0 { 0 } else { 1_000_000 / average };
            out.push_str(&format!("frame capacity: ~{capacity} fps\n"));
        }
        out
    }
}

/// 计算 p50/p95/max；没有样本时返回 `None`。
fn percentiles(values: &[u64]) -> Option<(u64, u64, u64)> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let pick = |ratio: f64| {
        let index = ((sorted.len() - 1) as f64 * ratio).round() as usize;
        sorted[index.min(sorted.len() - 1)]
    };
    Some((pick(0.5), pick(0.95), *sorted.last().unwrap_or(&0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 间隔分位数必须按真实顺序给出中位数、p95 与最大值。
    #[test]
    fn percentiles_report_median_p95_and_max() {
        let values: Vec<u64> = (1..=100).collect();

        let (p50, p95, max) = percentiles(&values).expect("应有分位数");

        assert_eq!(max, 100);
        assert!((49..=51).contains(&p50), "中位数应落在区间中点附近：{p50}");
        assert!((94..=96).contains(&p95), "p95 应落在高位：{p95}");
        assert!(percentiles(&[]).is_none());
    }

    /// 报告应给出帧耗时与能力换算，便于判断渲染是否还有余量。
    #[test]
    fn report_summarizes_frame_cost() {
        let probe = StreamProbe {
            samples: Arc::new(Mutex::new(Samples::default())),
        };
        probe.record_model_delta(8);
        probe.record_model_delta(8);
        probe.record_frame(Duration::from_micros(400), 12);
        probe.record_frame(Duration::from_micros(600), 0);

        let report = probe.samples.lock().expect("探针锁不应中毒").render();

        assert!(report.contains("model deltas: 2 (16 bytes)"), "{report}");
        assert!(report.contains("frames: 2 avg=500us max=600us"), "{report}");
        assert!(report.contains("frame capacity: ~2000 fps"), "{report}");
    }

    /// 未设置环境变量时不创建探针，热路径上不产生任何样本。
    #[test]
    fn probe_is_off_without_env_var() {
        // 测试进程可能已设置该变量，这里显式清空再断言，避免受外部环境影响。
        std::env::remove_var("LUCIA_STREAM_DEBUG");
        assert!(StreamProbe::from_env().is_none());
    }
}
