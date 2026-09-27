//! 后台工作线程：耗时操作（打开/筛选/拆分/导出）与 UI 通过 mpsc 通道通信。

use std::sync::mpsc::Sender;
use std::thread::JoinHandle;

use csv_editor_rust::core::dedup::DedupReport;
use csv_editor_rust::core::merge::{FileHeader, HeaderCheck, MergeReport};
use csv_editor_rust::core::model::DataModel;
use csv_editor_rust::core::split::SplitReport;
use csv_editor_rust::core::stats::StatsTable;

/// 后台 → UI 消息
pub enum BgMsg {
    /// 进度 (0..=1, 详情)
    Progress(f32, String),
    /// 文件加载完成
    FileLoaded(String, DataModel),
    /// 筛选完成（命中行号）
    Filtered(Vec<u32>),
    /// 拆分完成（含逐文件行数与完整性核对结论；第二个是该目录下生成的核对表路径）
    SplitDone(SplitReport, Result<String, String>),
    /// 导出完成（行数, 路径）
    ExportDone(usize, String),
    /// 多文件表头读取完成（表头列表, 一致性判定, 差异字段）
    MergeHeaders(Vec<FileHeader>, HeaderCheck, Vec<String>),
    /// 多文件合并完成
    Merged(MergeReport),
    /// 提取唯一行完成（唯一行号, 统计, 重复行对应关系, 判重字段原始列索引）
    DedupDone(Vec<u32>, DedupReport, Vec<(u32, u32)>, Vec<usize>),
    /// 统计完成（分组汇总 / 数值列统计；结果表用 Arc 便于结果窗每帧零成本共享）
    StatsDone(std::sync::Arc<StatsTable>),
    /// 错误
    Error(String),
}

/// 后台线程控制句柄（可报告进度）
pub struct WorkerCtl {
    tx: Sender<BgMsg>,
}

impl WorkerCtl {
    pub fn report_progress(&self, p: f32, detail: &str) {
        let _ = self.tx.send(BgMsg::Progress(p.clamp(0.0, 1.0), detail.to_string()));
    }
}

/// 后台任务句柄
pub struct Worker {
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    /// 启动后台任务；`tx` 用于向 UI 发送消息。
    /// 线程内部 catch_unwind：任务 panic 时补发错误消息，避免 UI 永久卡在忙碌状态
    /// （release 为 panic=abort 时进程直接终止，同样不会卡死 UI）。
    pub fn spawn<F>(tx: Sender<BgMsg>, f: F) -> Worker
    where
        F: FnOnce(&WorkerCtl) + Send + 'static,
    {
        let ctl = WorkerCtl { tx };
        let handle = std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                f(&ctl);
            }));
            if result.is_err() {
                let _ = ctl
                    .tx
                    .send(BgMsg::Error("后台任务异常终止（内部错误）".to_string()));
            }
        });
        Worker {
            handle: Some(handle),
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // 不 join：任务在后台自然结束（避免 UI 线程被长任务阻塞）。
        // 线程持有 Sender，任务完成后发送消息并退出。
        if let Some(h) = self.handle.take() {
            drop(h);
        }
    }
}
