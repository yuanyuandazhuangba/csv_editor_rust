//! 主应用：egui/eframe 界面状态与事件处理。
//!
//! 架构：
//! - 数据层（core）与 UI 层分离；DataModel + ColumnOps（列视图）+ 筛选行号
//!   组成当前工作视图；
//! - 耗时操作（打开/筛选/拆分/导出）在后台线程执行，通过 mpsc 通道回传
//!   进度与结果，UI 不阻塞；
//! - 数据预览用 egui_extras 虚拟化表格，900 万行仅渲染可见行。

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

use eframe::egui;

use csv_editor_rust::core::column_ops::ColumnOps;

use csv_editor_rust::core::filter::{apply_filter, FilterCondition};
use csv_editor_rust::core::model::DataModel;


pub mod background;
pub mod dialog_ui;
pub mod dialogs;
pub mod panels;
pub mod table;

use background::{BgMsg, Worker};
use panels::peak_working_set_mb;
use table::PreviewTable;

/// 主应用状态
pub struct CsvEditorApp {
    // 数据
    model: Option<Arc<DataModel>>,
    ops: Option<ColumnOps>,
    file_info: Option<FileInfo>,
    // 筛选
    filter_conditions: Vec<FilterCondition>,
    filtered_rows: Option<Vec<u32>>, // None = 全部
    // 弹出窗口位置
    dialog_position: DialogPosition,
    // 日志
    log: Vec<String>,
    show_log: bool,
    // 后台
    tx: Sender<BgMsg>,
    rx: Receiver<BgMsg>,
    worker: Option<Worker>,
    busy: Option<BusyState>,
    // 对话框标志
    pub dialogs: dialogs::DialogStates,
    // 预览表
    preview: PreviewTable,
    // 双击单元格要查看的数据行详情
    detail_row: Option<usize>,
    // 进程峰值内存
    peak_memory_mb: f64,
}

/// 弹出窗口位置（数据预览区右侧靠上为默认）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogPosition {
    /// 屏幕居中
    Center,
    /// 数据预览区右侧靠上
    RightTop,
    /// 数据预览区左侧靠上
    LeftTop,
    /// 数据预览区中间靠上
    CenterTop,
    /// 记忆上次拖动位置（egui persistence）
    Remember,
}

impl DialogPosition {
    pub fn label(&self) -> &'static str {
        match self {
            DialogPosition::Center => "居中",
            DialogPosition::RightTop => "数据预览区右侧靠上",
            DialogPosition::LeftTop => "数据预览区左侧靠上",
            DialogPosition::CenterTop => "数据预览区中间靠上",
            DialogPosition::Remember => "记忆上次位置",
        }
    }
}

pub struct FileInfo {
    pub path: String,
    pub rows: usize,
    pub cols: usize,
    pub encoding: String,
    pub size_bytes: u64,
}

#[derive(Clone)]
pub struct BusyState {
    pub title: String,
    pub detail: String,
    pub progress: f32, // 0..1
}

impl CsvEditorApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let (tx, rx) = mpsc::channel::<BgMsg>();
        Self {
            model: None,
            ops: None,
            file_info: None,
            filter_conditions: Vec::new(),
            filtered_rows: None,
            dialog_position: DialogPosition::RightTop,
            log: Vec::new(),
            show_log: true,
            tx,
            rx,
            worker: None,
            busy: None,
            dialogs: dialogs::DialogStates::default(),
            preview: PreviewTable::new(),
            detail_row: None,
            peak_memory_mb: 0.0,
        }
    }

    pub fn log(&mut self, msg: &str) {
        let ts = format!("[{}]", now_str());
        self.log.push(format!("{} {}", ts, msg));
        if self.log.len() > 5000 {
            self.log.drain(0..self.log.len() - 5000);
        }
    }

    pub fn set_busy(&mut self, title: &str, detail: &str, progress: f32) {
        self.busy = Some(BusyState {
            title: title.to_string(),
            detail: detail.to_string(),
            progress,
        });
    }

    /// 打开文件（后台线程）
    pub fn open_file(&mut self, path: String) {
        self.log(&format!("开始读取文件: {}", path));
        let tx = self.tx.clone();
        self.worker = Some(Worker::spawn(tx.clone(), move |worker| {
            worker.report_progress(0.05, "正在检测编码并建立行索引...");
            let result = DataModel::open(&path, 1000);
            match result {
                Ok(model) => {
                    worker.report_progress(1.0, "读取完成");
                    let _ = tx.send(BgMsg::FileLoaded(path, model));
                }
                Err(e) => {
                    let _ = tx.send(BgMsg::Error(format!("读取文件失败: {}", e)));
                }
            }
        }));
        self.busy = Some(BusyState {
            title: "读取CSV文件".to_string(),
            detail: "正在处理...".to_string(),
            progress: 0.0,
        });
    }

    /// 应用筛选（后台线程）
    pub fn apply_filters(&mut self) {
        if self.busy.is_some() {
            self.log("已有任务在执行，请稍候");
            return;
        }
        let Some(model) = self.model.clone() else {
            return;
        };
        let conds = self.filter_conditions.clone();
        if conds.is_empty() {
            self.log("请先添加筛选条件");
            return;
        }
        self.log("开始应用筛选条件...");
        let tx = self.tx.clone();
        self.worker = Some(Worker::spawn(tx.clone(), move |worker| {
            let hits = apply_filter(&model, &conds, Some(&|p| {
                worker.report_progress(p as f32, "正在筛选...");
            }));
            let _ = tx.send(BgMsg::Filtered(hits));
        }));
        self.set_busy("筛选数据", "正在筛选...", 0.0);
    }

    /// 从后台消息队列取消息（每帧调用）
    pub fn poll_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                BgMsg::Progress(p, detail) => {
                    if let Some(b) = self.busy.as_mut() {
                        b.progress = p;
                        b.detail = detail;
                    }
                }
                BgMsg::FileLoaded(path, model) => {
                    self.busy = None;
                    let rows = model.row_count();
                    let cols = model.column_count();
                    let encoding = model.encoding_label().to_string();
                    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    self.file_info = Some(FileInfo {
                        path,
                        rows,
                        cols,
                        encoding,
                        size_bytes: size,
                    });
                    self.ops = Some(ColumnOps::new(model.header()));
                    self.filtered_rows = None;
                    self.filter_conditions.clear();
                    self.model = Some(Arc::new(model));
                    self.log(&format!(
                        "文件读取完成，总行数: {}，列数: {}",
                        rows, cols
                    ));
                }
                BgMsg::Filtered(hits) => {
                    self.busy = None;
                    self.filtered_rows = Some(hits.clone());
                    self.log(&format!("筛选完成，命中行数: {}", hits.len()));
                }
                BgMsg::SplitDone(count) => {
                    self.busy = None;
                    self.log(&format!("拆分完成，共生成 {} 个文件", count));
                }
                BgMsg::ExportDone(n, path) => {
                    self.busy = None;
                    self.log(&format!("导出完成: {} 行 -> {}", n, path));
                }
                BgMsg::Error(e) => {
                    self.busy = None;
                    self.log(&format!("错误: {}", e));
                }
            }
        }
    }

    /// 当前可见行集合（筛选后或全部）
    pub fn visible_rows(&self) -> Option<&[u32]> {
        self.filtered_rows.as_deref()
    }

    pub fn ops_mut(&mut self) -> Option<&mut ColumnOps> {
        self.ops.as_mut()
    }

    /// 清空已加载的所有数据与相关状态（不退出程序）
    pub fn clear_data(&mut self) {
        if self.busy.is_some() {
            self.log("有任务在进行，暂不能清空数据");
            return;
        }
        if self.model.is_none() && self.ops.is_none() && self.file_info.is_none() {
            self.log("当前没有已加载的数据");
            return;
        }
        self.model = None;
        self.ops = None;
        self.file_info = None;
        self.filter_conditions.clear();
        self.filtered_rows = None;
        self.log("已清空所有数据");
    }
}

/// 当前时间字符串
fn now_str() -> String {
    // 不引入 chrono，用系统时间秒级
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 粗略本地时间（UTC+8）
    let local = secs + 8 * 3600;
    let days = local / 86400;
    let rem = local % 86400;
    let (y, m, d) = civil_from_days(days as i64);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, m, d, hh, mm, ss)
}

/// 天数转公历日期
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 工具：加载微软雅黑字体
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for (name, path) in [
        ("msyh", r"C:\Windows\Fonts\msyh.ttc"),
        ("msyhbd", r"C:\Windows\Fonts\msyhbd.ttc"),
        ("simhei", r"C:\Windows\Fonts\simhei.ttf"),
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert(name.to_string(), egui::FontData::from_owned(bytes));
        }
    }
    for family in [
        egui::FontFamily::Proportional,
        egui::FontFamily::Monospace,
    ] {
        for name in ["msyh", "msyhbd", "simhei"] {
            if fonts.font_data.contains_key(name) {
                fonts
                    .families
                    .entry(family.clone())
                    .or_default()
                    .push(name.into());
            }
        }
    }
    ctx.set_fonts(fonts);
    // 默认字号稍大，方便中文阅读
    ctx.style_mut(|s| {
        s.text_styles.insert(
            egui::TextStyle::Body,
            egui::FontId::new(14.0, egui::FontFamily::Proportional),
        );
    });
}

// ==================== IME 修补（中文输入）====================

/// 修补 egui 0.27 的 IME 提交缺陷（Windows 中文输入只出第一个字）。
///
/// 现象：筛选条件等输入框里输入中文，只有第一个字（词）能上屏，之后的字词
/// 输入后不显示；字母、数字、符号一切正常（它们走 `Event::Text`，不经过 IME）。
///
/// 根因（已在 headless 测试中复现）：egui 0.27.2 的 `TextEdit` 处理
/// `Event::CompositionEnd(prediction)` 时，要求当前光标与 `state.ime_cursor_range`
/// 的 `secondary` 端一致才插入文本；而 `ime_cursor_range` **只在
/// `Event::CompositionUpdate` 中更新**。Windows 输入法连续上屏时，winit 会发出
/// 没有 preedit 的 `Ime::Commit`，egui-winit 于是只推送一个 `CompositionEnd`——
/// 此时 `ime_cursor_range` 仍是上一次的值，与当前光标不符，提交被静默丢弃，
/// 于是“只有第一个字能输入”。
///
/// 修补：对每个“前面没有 `CompositionUpdate` 的 `CompositionEnd`”，先补发
/// `CompositionStart` + 一个**空的** `CompositionUpdate`。空 preedit 不插入任何字符，
/// 只让 egui 把 `ime_cursor_range` 同步到当前光标，随后该次提交即可正常落字。
pub fn patch_ime_events(events: &mut Vec<egui::Event>) {
    if !events
        .iter()
        .any(|e| matches!(e, egui::Event::CompositionEnd(_)))
    {
        return; // 绝大多数帧不含 IME 提交，直接返回
    }
    let mut out: Vec<egui::Event> = Vec::with_capacity(events.len() + 4);
    let mut saw_composition_update = false;
    for ev in events.drain(..) {
        match ev {
            egui::Event::CompositionUpdate(_) => {
                saw_composition_update = true;
                out.push(ev);
            }
            egui::Event::CompositionEnd(_) => {
                if !saw_composition_update {
                    out.push(egui::Event::CompositionStart);
                    out.push(egui::Event::CompositionUpdate(String::new()));
                }
                saw_composition_update = false;
                out.push(ev);
            }
            other => out.push(other),
        }
    }
    *events = out;
}

// ==================== eframe::App 实现 ====================

impl eframe::App for CsvEditorApp {
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        // 在 egui 处理输入前修补 IME 事件序列（见 patch_ime_events）
        patch_ime_events(&mut raw_input.events);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_messages();
        self.peak_memory_mb = peak_working_set_mb();

        self.menu_bar(ctx);
        self.controls_panel(ctx);
        if self.show_log {
            self.log_panel(ctx);
        }
        self.central_panel(ctx);
        self.progress_window(ctx);
        self.show_dialogs(ctx);
        self.request_repaint_if_busy(ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{pos2, vec2, Event, Rect};

    /// 用 headless egui 模拟一串 IME 事件，返回 TextEdit 最终内容。
    /// `patched=true` 时先经过 `patch_ime_events`（等价于 app 的 raw_input_hook）。
    fn type_ime_events(patched: bool, per_frame: &[Vec<Event>]) -> String {
        let ctx = egui::Context::default();
        let mut text = String::new();
        let step = |mut events: Vec<Event>, text: &mut String| {
            if patched {
                patch_ime_events(&mut events);
            }
            let mut input = egui::RawInput::default();
            input.screen_rect = Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(600.0, 400.0)));
            input.time = Some(1.0);
            input.events = events;
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let resp = ui.add(egui::TextEdit::singleline(text));
                    if !resp.has_focus() {
                        resp.request_focus();
                    }
                });
            });
        };
        step(Vec::new(), &mut text); // 先聚焦
        for events in per_frame {
            step(events.clone(), &mut text);
        }
        text
    }

    fn upd(s: &str) -> Event {
        Event::CompositionUpdate(s.to_string())
    }
    fn end(s: &str) -> Event {
        Event::CompositionEnd(s.to_string())
    }

    #[test]
    fn ime_bug_is_real_without_patch() {
        // 记录上游缺陷：无 preedit 的连续 commit 只会保留第一个字（修补前）
        let case = vec![vec![end("北")], vec![end("京")]];
        assert_eq!(type_ime_events(false, &case), "北");
    }

    #[test]
    fn ime_patch_restores_consecutive_commits() {
        // 回归：修补后，连续上屏（无 preedit 的 commit）应完整落字
        let cases: Vec<(Vec<Vec<Event>>, &str)> = vec![
            // 每帧一次 commit（最常见：连续输入单字）
            (vec![vec![end("北")], vec![end("京")]], "北京"),
            // 同帧多次 commit（连续选词）
            (vec![vec![end("北"), end("京"), end("市")]], "北京市"),
            // 带 preedit 的常规两轮输入
            (
                vec![
                    vec![Event::CompositionStart, upd("bei")],
                    vec![end("北")],
                    vec![Event::CompositionStart, upd("jing")],
                    vec![end("京")],
                ],
                "北京",
            ),
            // 同一帧内 preedit 与 commit 混合
            (
                vec![
                    vec![Event::CompositionStart, upd("bei"), end("北")],
                    vec![end("京")],
                ],
                "北京",
            ),
        ];
        for (case, expect) in cases {
            assert_eq!(
                type_ime_events(true, &case),
                expect,
                "修补后应得到 {expect:?}，事件序列 {case:?}"
            );
        }
    }

    #[test]
    fn ime_patch_keeps_normal_preedit_and_text_events() {
        // 正常 preedit 一次成词不受影响
        let case = vec![vec![Event::CompositionStart, upd("beijing"), end("北京")]];
        assert_eq!(type_ime_events(true, &case), "北京");
        // 字母/数字等非 IME 输入不受影响
        let case = vec![vec![Event::Text("a".into()), Event::Text("1".into())]];
        assert_eq!(type_ime_events(true, &case), "a1");
    }

    #[test]
    fn patch_leaves_event_stream_untouched_without_commit() {
        // 不含 CompositionEnd 时不应改动事件列表（零开销路径）
        let mut events = vec![Event::Text("x".into()), Event::CompositionUpdate("y".into())];
        let before = events.len();
        patch_ime_events(&mut events);
        assert_eq!(events.len(), before);
    }
}
