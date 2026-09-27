//! 主应用：egui/eframe 界面状态与事件处理。
//!
//! 架构：
//! - 数据层（core）与 UI 层分离；DataModel + ColumnOps（列视图）+ 筛选行号
//!   组成当前工作视图；
//! - 耗时操作（打开/筛选/拆分/导出）在后台线程执行，通过 mpsc 通道回传
//!   进度与结果，UI 不阻塞；
//! - 数据预览用 egui_extras 虚拟化表格，900 万行仅渲染可见行。

use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

use eframe::egui;

use csv_editor_rust::core::column_ops::ColumnOps;
use csv_editor_rust::core::db;

use csv_editor_rust::core::filter::{apply_filter, FilterCondition};
use csv_editor_rust::core::model::DataModel;
use csv_editor_rust::core::stats;


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
    // 是否显示「数据处理」工具栏（「工具」菜单可切换，默认显示）
    show_data_toolbar: bool,
    // 是否显示「数据分析」工具栏（同上，默认显示）
    show_analysis_toolbar: bool,
    // 微信收款码纹理（首次打开捐助窗口时解码一次并缓存）
    qr_texture: Option<egui::TextureHandle>,
    // 上一次合并输出的临时文件（下次合并前清理，避免临时目录堆积）
    merged_temp: Option<String>,
    /// 最近一次「提取唯一行」的结果（重复行对应表 + 判重字段）
    last_dedup: Option<LastDedup>,
    /// 最近一次统计的结果表（结果窗渲染与导出共用）
    last_stats: Option<Arc<stats::StatsTable>>,
    /// 最近一次「重复情况统计」可直接落回当前视图的行号（有结果窗内的「只看…」按钮用）
    stats_filter_rows: Option<Arc<StatsFilterRows>>,
    /// 最近一次「频次分布」统计的原始列（结果窗里"只看出现次数 ≥ N 的行"要用）
    freq_filter_cols: Vec<usize>,
    /// 是否把操作日志追加写入 exe 同目录的「操作日志.txt」（默认关闭）
    log_to_file: bool,
    // 界面缩放（按钮大小）的唯一真值源。拖动过程中不立即应用缩放：
    // set_zoom_factor 会改变 pixels_per_point，使指针的逻辑坐标在一帧内突变，
    // 导致 DragValue 的 drag_delta() 出现虚假位移、数值反向跳变并与界面震荡。
    ui_zoom: f32,
    /// 缩放控件当前是否处于拖动中（拖动期间不应用缩放，松手后一次性应用）
    zoom_dragging: bool,
    /// 当前数据库连接（只读；同一时刻只允许一个数据库任务使用）
    db_conn: Option<Arc<db::DbConn>>,
    /// 连接描述（类型 / 驱动 / 服务器），用于界面与日志
    db_conn_desc: String,
    /// 最近一次由数据库载入生成的临时 CSV（下次导出前清理，避免临时目录堆积）
    db_temp: Option<String>,
    /// 上一条数据库数据的来源描述（如「数据库表 orders」），用于界面显示数据来源
    db_source_label: String,
}

/// 「重复情况统计」结果里可直接落回当前视图的行号集合
pub struct StatsFilterRows {
    /// 重复键涉及的全部行（含各键首次出现的那行）
    pub dup: Vec<u32>,
    /// 每个键首次出现的行（与「提取唯一行（去重）」的结果一致）
    pub unique: Vec<u32>,
}

/// 最近一次「提取唯一行」的结果信息（供结果弹窗与「导出重复行对应表」使用）
pub struct LastDedup {
    /// (保留的首次出现行, 被剔除的重复行)
    pub map: Vec<(u32, u32)>,
    /// 判重字段（原始列索引）
    pub key_columns: Vec<usize>,
    /// 判重范围行数
    pub input_rows: usize,
    /// 去重后行数
    pub unique_rows: usize,
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
    /// 0..=1 为确定进度；**负数表示进度不确定**（总行数未知等），
    /// 界面据此显示动画进度条 + 「进行中…」，而不是虚假的百分比。
    pub progress: f32,
}

impl CsvEditorApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        Self::new_with_ctx(&cc.egui_ctx)
    }

    /// 与 [`Self::new`] 等价，但不依赖 eframe 的创建上下文（便于 headless 测试）。
    pub fn new_with_ctx(ctx: &egui::Context) -> Self {
        // 界面缩放的唯一入口是工具栏「按钮大小」控件：关闭 egui 内置的键盘缩放
        // （Ctrl + / Ctrl -），避免出现两个真值源互相覆盖。
        ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let (tx, rx) = mpsc::channel::<BgMsg>();
        // 启动时先清理上次遗留的数据库临时 CSV：这类文件可能很大（6GB 的表 → 5GB 文件），
        // 异常结束或直接关闭窗口时来不及删除，累积下来会吃掉大量磁盘空间
        let (cleaned, freed) = db::cleanup_temp_csvs();
        let mut app = Self {
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
            show_data_toolbar: true,
            show_analysis_toolbar: true,
            qr_texture: None,
            merged_temp: None,
            last_dedup: None,
            last_stats: None,
            stats_filter_rows: None,
            freq_filter_cols: Vec::new(),
            log_to_file: ctx.memory(|m| {
                m.data
                    .get_temp::<bool>(egui::Id::new("ops_log_to_file"))
                    .unwrap_or(false)
            }),
            ui_zoom: ctx.zoom_factor().clamp(1.0, 1.5),
            zoom_dragging: false,
            db_conn: None,
            db_conn_desc: String::new(),
            db_temp: None,
            db_source_label: String::new(),
        };
        if cleaned > 0 {
            app.log(&format!(
                "已清理上次遗留的数据库临时文件：{} 个，释放 {:.1} MB",
                cleaned,
                freed as f64 / 1024.0 / 1024.0
            ));
        }
        app
    }

    pub fn log(&mut self, msg: &str) {
        let ts = format!("[{}]", now_str());
        let line = format!("{} {}", ts, msg);
        // 可选落盘：纯文本追加，便于事后检索与审计（大小上限内自动轮转）
        if self.log_to_file {
            append_ops_log(&line);
        }
        self.log.push(line);
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
                    // 这次加载的是不是「数据库取数产生的临时文件」
                    let from_db = self.db_temp.as_deref() == Some(path.as_str());
                    // 数据库载入的数据：显示来源（而不是内部临时文件路径）
                    let shown = if from_db && !self.db_source_label.is_empty() {
                        format!("{}（数据库只读载入）", self.db_source_label)
                    } else {
                        path
                    };
                    // 若不是数据库临时文件，说明之前那份临时文件已不再使用
                    // （内存映射已随旧 model 释放），顺手删掉，避免留在磁盘上
                    if !from_db {
                        if let Some(old) = self.db_temp.take() {
                            let _ = std::fs::remove_file(&old);
                        }
                        self.db_source_label.clear();
                    }
                    self.file_info = Some(FileInfo {
                        path: shown,
                        rows,
                        cols,
                        encoding,
                        size_bytes: size,
                    });
                    self.ops = Some(ColumnOps::new(model.header()));
                    self.filtered_rows = None;
                    self.filter_conditions.clear();
                    // 换了数据源：上一次的统计结果不再对应当前数据，清掉
                    self.last_stats = None;
                    self.stats_filter_rows = None;
                    self.freq_filter_cols.clear();
                    self.dialogs.stats_result_window = false;
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
                BgMsg::SplitDone(report, check) => {
                    self.busy = None;
                    // 逐文件列出「不计表头的数据行数」，并给出与数据来源总行数的核对结论，
                    // 让用户能直观确认拆分导出完整、无遗漏。
                    self.log(&format!(
                        "拆分完成：共 {} 个文件（数据来源 {} 行，均不含表头）",
                        report.files, report.source_rows
                    ));
                    const MAX_LIST: usize = 10; // 文件很多时避免刷屏，其余汇总显示
                    for s in report.stats.iter().take(MAX_LIST) {
                        self.log(&format!("    {}：{} 行（不含表头）", s.name, s.disk_rows));
                    }
                    if report.stats.len() > MAX_LIST {
                        let rest: usize = report.stats[MAX_LIST..]
                            .iter()
                            .map(|s| s.disk_rows)
                            .sum();
                        self.log(&format!(
                            "    ……（其余 {} 个文件略，合计 {} 行）",
                            report.stats.len() - MAX_LIST,
                            rest
                        ));
                    }
                    self.log(&format!(
                        "    各文件合计：{} 行（不含表头）；数据来源总行数：{} 行",
                        report.total_on_disk(),
                        report.source_rows
                    ));
                    self.log(&report.verdict());
                    match check {
                        Ok(p) => self.log(&format!("已生成拆分完整性核对表：{}", p)),
                        Err(e) => self.log(&format!("⚠ 拆分完整性核对表生成失败：{}", e)),
                    }
                }
                BgMsg::ExportDone(n, path) => {
                    self.busy = None;
                    self.log(&format!("导出完成: {} 行 -> {}", n, path));
                }
                BgMsg::MergeHeaders(headers, check, diff) => {
                    self.busy = None;
                    self.dialogs.merge_scanning = false;
                    self.log(&format!(
                        "多文件表头校验：{}（差异字段：{}）",
                        check.label(),
                        if diff.is_empty() {
                            "无".to_string()
                        } else {
                            diff.join("、")
                        }
                    ));
                    self.dialogs.merge_headers = headers;
                    self.dialogs.merge_check = Some(check);
                    self.dialogs.merge_diff = diff;
                    // 表头有差异时，默认勾选“只保留相同字段名”，让用户一眼看到可用方案
                    if check == csv_editor_rust::core::merge::HeaderCheck::Differ {
                        self.dialogs.merge_keep_common = true;
                    }
                }
                BgMsg::Merged(report) => {
                    self.busy = None;
                    self.log(&format!(
                        "多文件合并完成：{} 个文件 → {} 行 × {} 列（{}）",
                        report.files,
                        report.rows,
                        report.columns,
                        report.mode.label()
                    ));
                    // 清理上一次的临时文件（避免临时目录堆积）
                    if let Some(old) = self.merged_temp.take() {
                        if old != report.out_path {
                            let _ = std::fs::remove_file(&old);
                        }
                    }
                    let path = report.out_path.clone();
                    self.merged_temp = Some(path.clone());
                    self.dialogs.merge_window = false;
                    self.dialogs.merge_files.clear();
                    self.dialogs.merge_headers.clear();
                    // 合并结果作为当前数据打开（复用既有加载流程）
                    self.open_file(path);
                }
                BgMsg::DedupDone(rows, report, map, key_cols) => {
                    self.busy = None;
                    // 去重结果作为新的当前视图：后续导出/拆分/统计都基于唯一行
                    self.filtered_rows = Some(rows);
                    self.log(&format!(
                        "提取唯一行完成：{} 行 → {} 行（丢弃重复 {} 行；按 {} 个字段判重）",
                        report.input_rows,
                        report.unique_rows,
                        report.duplicates,
                        report.key_columns
                    ));
                    // 保存「保留行 ↔ 被剔除行」的对应关系，并弹出结果提示窗
                    self.last_dedup = Some(LastDedup {
                        map,
                        key_columns: key_cols,
                        input_rows: report.input_rows,
                        unique_rows: report.unique_rows,
                    });
                    self.dialogs.dedup_done_window = true;
                }
                BgMsg::StatsDone(table) => {
                    self.busy = None;
                    // 记录参数与结果规模（便于事后审计），并弹出结果窗
                    self.log(&format!(
                        "{}完成：{}",
                        table.kind.label(),
                        table.summary()
                    ));
                    for note in &table.notes {
                        self.log(&format!("    注意：{}", note));
                    }
                    // 只有「频次分布」的结果才支持"按出现次数筛回视图"；其它统计清空该上下文
                    if table.kind != stats::StatsKind::ValueFrequency {
                        self.freq_filter_cols.clear();
                    }
                    self.last_stats = Some(table);
                    // 换了一次非重复情况统计：结果窗里的「只看重复行/唯一行」不再适用
                    self.stats_filter_rows = None;
                    // 新结果 → 结果窗打开并（若上次被最小化）强制展开
                    self.dialogs.stats_result_expand_pending = true;
                    self.dialogs.stats_result_window = true;
                }
                BgMsg::DuplicateDone(report) => {
                    self.busy = None;
                    self.log(&format!("重复情况统计完成：{}", report.summary()));
                    for note in &report.table.notes {
                        self.log(&format!("    注意：{}", note));
                    }
                    // 保留行号，供结果窗里的「只看重复的行 / 只看唯一行」直接落回视图
                    self.stats_filter_rows = Some(Arc::new(StatsFilterRows {
                        dup: report.dup_rows.clone(),
                        unique: report.unique_rows.clone(),
                    }));
                    self.last_stats = Some(Arc::new(report.table.clone()));
                    self.dialogs.stats_result_expand_pending = true;
                    self.dialogs.stats_result_window = true;
                }
                BgMsg::DbConnected(conn, desc, scopes) => {
                    self.busy = None;
                    self.dialogs.db.connecting = false;
                    self.log(&format!("已连接数据库：{}（只读）", desc));
                    if self.dialogs.db.test_only {
                        // 测试连接：验证通了即释放，不保留连接、不打开浏览窗口
                        self.dialogs.db.test_only = false;
                        self.dialogs.db.status = format!("连接测试成功：{}", desc);
                    } else {
                        self.dialogs.db.status = format!("已连接：{}", desc);
                        self.db_conn = Some(conn);
                        self.db_conn_desc = desc;
                        // 打开浏览窗口并载入第一级（库/模式）
                        self.dialogs.db.scopes = scopes.clone();
                        self.dialogs.db.scope = scopes.first().cloned().unwrap_or_default();
                        self.dialogs.db.tables.clear();
                        self.dialogs.db.selected_table = None;
                        self.dialogs.db.columns.clear();
                        self.dialogs.db.col_checked.clear();
                        self.dialogs.db.preview = None;
                        self.dialogs.db.preview_note.clear();
                        self.dialogs.db.browse_window = true;
                        // 连接成功即拉取表清单，省去用户再点一次
                        self.db_load_tables(scopes.first().cloned().unwrap_or_default());
                    }
                }
                BgMsg::DbTables(scopes, tables) => {
                    self.busy = None;
                    self.dialogs.db.connecting = false;
                    if !scopes.is_empty() {
                        self.dialogs.db.scopes = scopes;
                    }
                    self.log(&format!("已读取到 {} 张表/视图", tables.len()));
                    self.dialogs.db.tables = tables;
                }
                BgMsg::DbColumns(cols) => {
                    self.busy = None;
                    self.dialogs.db.connecting = false;
                    self.log(&format!("已读取到 {} 个字段", cols.len()));
                    self.dialogs.db.col_checked = vec![true; cols.len()];
                    self.dialogs.db.columns = cols;
                }
                BgMsg::DbPreview(header, rows, note) => {
                    self.busy = None;
                    self.dialogs.db.connecting = false;
                    self.log(&format!(
                        "数据库预览完成：{} 行 × {} 列（{}）",
                        rows.len(),
                        header.len(),
                        note
                    ));
                    self.dialogs.db.preview_note = note;
                    self.dialogs.db.preview = Some((header, rows));
                }
                BgMsg::DbExported(path, rows) => {
                    self.busy = None;
                    self.dialogs.db.connecting = false;
                    self.log(&format!(
                        "已从数据库读取 {} 行到本机临时文件（数据库数据未被修改），正在加载到编辑器...",
                        rows
                    ));
                    // 清理旧的数据库临时文件（保留刚生成的这个；更早的遗留一并清掉）
                    self.db_temp = None;
                    let (cleaned, freed) =
                        db::cleanup_temp_csvs_except(std::path::Path::new(&path));
                    if cleaned > 0 {
                        self.log(&format!(
                            "已清理旧的数据库临时文件：{} 个，释放 {:.1} MB",
                            cleaned,
                            freed as f64 / 1024.0 / 1024.0
                        ));
                    }
                    self.db_temp = Some(path.clone());
                    self.dialogs.db.browse_window = false;
                    self.open_file(path);
                }
                BgMsg::Error(e) => {
                    self.busy = None;
                    // 数据库任务失败时把原因显示在连接窗口里（其它任务只写日志）
                    if self.dialogs.db.connecting {
                        self.dialogs.db.connecting = false;
                        self.dialogs.db.status = format!("失败：{}", e);
                    }
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
        // 数据来自数据库时，其临时 CSV 已不再被使用 → 一并清理
        if let Some(p) = self.db_temp.take() {
            let _ = std::fs::remove_file(&p);
        }
        self.db_source_label.clear();
        self.filter_conditions.clear();
        self.filtered_rows = None;
        self.last_stats = None;
        self.stats_filter_rows = None;
        self.freq_filter_cols.clear();
        self.dialogs.stats_result_window = false;
        self.log("已清空所有数据");
    }
}

/// 操作日志落盘文件名（exe 同目录；不可写则回退 %TEMP%）
const OPS_LOG_FILE: &str = "操作日志.txt";

/// 操作日志文件大小上限：超过则轮转为 `.1`，避免无限增长
const OPS_LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// 操作日志落盘路径：优先 exe 同目录（与诊断日志一致），不可写则回退 %TEMP%
fn ops_log_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join(OPS_LOG_FILE);
            if std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p)
                .is_ok()
            {
                return p;
            }
        }
    }
    std::env::temp_dir().join(OPS_LOG_FILE)
}

/// 追加一条操作日志到文本文件（零依赖、纯文本可搜索；超限自动轮转）
fn append_ops_log(line: &str) {
    let path = ops_log_path();
    let too_big = std::fs::metadata(&path)
        .map(|m| m.len() > OPS_LOG_MAX_BYTES)
        .unwrap_or(false);
    if too_big {
        let mut bak = path.as_os_str().to_owned();
        bak.push(".1");
        let bak = PathBuf::from(bak);
        let _ = std::fs::remove_file(&bak);
        let _ = std::fs::rename(&path, &bak);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{line}");
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
    /// 退出前清掉本次读取数据库产生的临时文件（正在被内存映射的文件删除会失败，
    /// 那种情况由下次启动时的清理兜底）
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.db_temp = None;
        let (n, freed) = db::cleanup_temp_csvs();
        log::info!(
            "退出清理数据库临时文件：{} 个，{:.1} MB",
            n,
            freed as f64 / 1024.0 / 1024.0
        );
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        // 在 egui 处理输入前修补 IME 事件序列（见 patch_ime_events）
        patch_ime_events(&mut raw_input.events);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_messages();
        self.peak_memory_mb = peak_working_set_mb();

        self.menu_bar(ctx);
        self.controls_panel(ctx);
        // 「数据处理」工具栏：紧跟在主工具栏下方（可经「工具」菜单隐藏）
        self.data_toolbar_panel(ctx);
        self.analysis_toolbar_panel(ctx);
        // 底部状态栏：先于右侧日志面板渲染，使其横跨窗口底部（而不是只到日志面板左侧）
        self.status_bar(ctx);
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

    /// 「数据库表（只读）」窗口必须在各种屏幕尺寸下都完整落在屏幕内
    ///（此前默认位置按小窗口设计 + 尺寸过大，会跑出屏幕导致点不到标题栏/滚动条）
    #[test]
    fn db_browse_window_fits_various_screens() {
        use csv_editor_rust::core::db::TableInfo;

        for (w, h) in [
            (1440.0_f32, 900.0_f32),
            (1280.0, 800.0),
            (1024.0, 768.0),
            (800.0, 600.0),
            (640.0, 480.0),
        ] {
            let ctx = egui::Context::default();
            let mut app = CsvEditorApp::new_with_ctx(&ctx);
            app.dialogs.db.browse_window = true;
            app.dialogs.db.scopes = vec!["demo".to_string()];
            app.dialogs.db.scope = "demo".to_string();
            app.dialogs.db.tables = vec![TableInfo {
                catalog: "demo".into(),
                schema: String::new(),
                name: "orders".into(),
                kind: "TABLE".into(),
            }];
            app.dialogs.db.selected_table = Some(0);
            app.dialogs.db.columns = vec!["编号".to_string(), "金额".to_string()];
            app.dialogs.db.col_checked = vec![true, true];
            app.dialogs.db.preview = Some((
                vec!["编号".to_string(), "金额".to_string()],
                vec![vec!["1".to_string(), "3.5".to_string()]],
            ));
            app.dialogs.db.preview_note = "表 orders 共 1 行".to_string();

            let input = || {
                let mut i = egui::RawInput::default();
                i.screen_rect = Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(w, h)));
                i
            };
            // 渲染若干帧（窗口位置在第二帧起稳定）
            for _ in 0..3 {
                let _ = ctx.run(input(), |ctx| app.show_dialogs(ctx));
            }
            let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(w, h));
            let rect = ctx
                .memory(|m| m.area_rect(egui::Id::new("数据库表（只读）")))
                .unwrap_or_else(|| panic!("{}×{}：窗口未渲染出来", w, h));
            assert!(
                rect.min.x >= screen.left() - 1.0
                    && rect.min.y >= screen.top() - 1.0
                    && rect.max.x <= screen.right() + 1.0
                    && rect.max.y <= screen.bottom() + 1.0,
                "{}×{}：窗口 {:?} 超出屏幕 {:?}",
                w,
                h,
                rect,
                screen
            );
            // 标题栏与按钮所在的顶部必须可见（窗口不能顶到屏幕外）
            assert!(rect.top() >= screen.top() - 1.0, "{}×{}：标题栏在屏幕外", w, h);
            assert!(
                rect.height() <= screen.height(),
                "{}×{}：窗口比屏幕还高（无法滚动/关闭）",
                w,
                h
            );
        }
    }

    /// 各主要对话框都不能超出屏幕（否则标题栏的关闭按钮会落在屏幕外）
    ///
    /// 用示例数据打开一遍常用窗口，覆盖「有数据才会变大」的那些窗口。
    #[test]
    fn dialogs_fit_small_screens() {
        for (w, h) in [
            (1280.0_f32, 800.0_f32),
            (1024.0, 768.0),
            (800.0, 600.0),
            (640.0, 480.0),
        ] {
            let ctx = egui::Context::default();
            let mut app = CsvEditorApp::new_with_ctx(&ctx);
            app.load_sample_data();
            assert!(
                wait_for(&mut app, |a| a.model.is_some(), 20_000),
                "示例数据加载超时"
            );

            let d = &mut app.dialogs;
            d.about = true;
            d.help_guide = true;
            d.file_info = true;
            d.export_data = true;
            d.filter_window = true;
            d.split_window = true;
            d.dedup_window = true;
            d.stats_window = true;
            d.col_stats_window = true;
            d.freq_window = true;
            d.dup_window = true;
            d.quality_window = true;
            d.pivot_window = true;
            d.cross_window = true;
            d.merge_window = true;
            d.split_field = true;
            d.merge_field = true;
            d.insert_column = true;
            d.delete_column = true;
            d.db.connect_window = true;
            d.db.browse_window = true;
            d.db.tables = vec![csv_editor_rust::core::db::TableInfo {
                catalog: "demo".into(),
                schema: String::new(),
                name: "orders".into(),
                kind: "TABLE".into(),
            }];
            d.db.selected_table = Some(0);
            d.db.columns = vec!["编号".to_string(), "金额".to_string()];
            d.db.col_checked = vec![true, true];
            // 模拟一个宽表预览（50 列 × 100 行），贴近真实数据库表
            let header: Vec<String> = (0..50).map(|i| format!("列{}", i)).collect();
            let rows: Vec<Vec<String>> = (0..100)
                .map(|r| (0..50).map(|c| format!("值{}-{}", r, c)).collect())
                .collect();
            d.db.preview = Some((header, rows));
            d.db.preview_note = "表 orders 仅显示前 100 行".to_string();

            let input = || {
                let mut i = egui::RawInput::default();
                i.screen_rect = Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(w, h)));
                i
            };
            for _ in 0..3 {
                let _ = ctx.run(input(), |ctx| app.show_dialogs(ctx));
            }

            let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(w, h));
            let mut checked = 0;
            for title in [
                "连接数据库（只读）",
                "数据库表（只读）",
                "使用说明",
                "关于",
                "文件信息",
                "导出数据",
                "数据筛选",
                "数据拆分",
                "提取唯一行（去重）",
                "字段分组汇总",
                "数值列统计",
                "透视表",
                "交叉 / 关联统计",
                "重复情况统计",
                "空值与数据质量统计",
                "取值频次分布 / Top-N",
                "合并多个CSV文件",
                "拆分字段",
                "插入列",
                "删除列",
                "删除列",
            ] {
                let Some(rect) = ctx.memory(|m| m.area_rect(egui::Id::new(title))) else {
                    continue; // 该窗口本帧未显示
                };
                checked += 1;
                assert!(
                    rect.min.x >= screen.left() - 1.0
                        && rect.min.y >= screen.top() - 1.0
                        && rect.max.x <= screen.right() + 1.0
                        && rect.max.y <= screen.bottom() + 1.0,
                    "{}×{}：「{}」窗口 {:?} 超出屏幕",
                    w,
                    h,
                    title,
                    rect
                );
            }
            assert!(checked >= 10, "{}×{}：只检查到 {} 个窗口", w, h, checked);
        }
    }

    /// 「数据库表（只读）」窗口的布局度量：下段预览通栏且比上段列表更高，
    /// 整体尺寸始终在屏幕内（左侧表列表底部上提，预览区横跨窗口左右）
    #[test]
    fn db_browse_metrics_layout() {
        for (w, h) in [
            (1920.0_f32, 1080.0_f32),
            (1440.0, 900.0),
            (1280.0, 800.0),
            (1024.0, 768.0),
            (800.0, 600.0),
        ] {
            let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(w, h));
            let m = crate::app::dialog_ui::db_browse_metrics(screen);
            // 窗口（含标题栏与边距）必须放得进屏幕
            assert!(
                m.win_w <= screen.width() - 24.0 && m.win_h + 40.0 <= screen.height(),
                "{}×{}：窗口 {}×{} 放不进屏幕",
                w,
                h,
                m.win_w,
                m.win_h
            );
            // 左侧表列表宽度明显窄于预览区（预览通栏）
            assert!(
                m.left_w * 2.0 < m.win_w,
                "{}×{}：左侧列表宽度 {} 占窗口 {} 太宽",
                w,
                h,
                m.left_w,
                m.win_w
            );
            // 上段列表底部上提：列表高度不超过预览高度，且合计不超出内容区
            assert!(
                m.lists_h <= m.preview_h,
                "{}×{}：上段列表 {} 应不高于下段预览 {}",
                w,
                h,
                m.lists_h,
                m.preview_h
            );
            assert!(m.lists_h >= 80.0, "{}×{}：列表太矮 {}", w, h, m.lists_h);
        }
    }

    /// Esc 应能关闭「数据库表（只读）」窗口（窗口过大时标题栏可能不便点击）
    #[test]
    fn db_browse_window_closes_with_escape() {
        let ctx = egui::Context::default();
        let mut app = CsvEditorApp::new_with_ctx(&ctx);
        app.dialogs.db.browse_window = true;
        let mut input = egui::RawInput::default();
        input.screen_rect = Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 700.0)));
        let _ = ctx.run(input, |ctx| app.show_dialogs(ctx));
        assert!(app.dialogs.db.browse_window);

        // 第二帧注入 Esc
        let mut input = egui::RawInput::default();
        input.screen_rect = Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 700.0)));
        input.events.push(Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        });
        let _ = ctx.run(input, |ctx| app.show_dialogs(ctx));
        assert!(
            !app.dialogs.db.browse_window,
            "Esc 应当关闭数据库表窗口"
        );
    }

    /// 「数据库表（只读）」窗口内的空白带不得超过 60px
    ///（曾因滚动区 `auto_shrink` 的高度方向设为 false，两栏列表与预览之间留出约 265px 空白）
    #[test]
    fn db_browse_window_has_no_large_gap() {
        use csv_editor_rust::core::db::TableInfo;
        let (w, h) = (1440.0_f32, 900.0_f32);
        let ctx = egui::Context::default();
        let mut app = CsvEditorApp::new_with_ctx(&ctx);
        app.dialogs.db.browse_window = true;
        app.dialogs.db.scopes = vec!["cbd2026".to_string()];
        app.dialogs.db.scope = "cbd2026".to_string();
        app.dialogs.db.tables = (0..20)
            .map(|i| TableInfo {
                catalog: "cbd2026".into(),
                schema: String::new(),
                name: format!("table_{:02}", i),
                kind: "TABLE".into(),
            })
            .collect();
        app.dialogs.db.selected_table = Some(0);
        app.dialogs.db.columns = (0..55).map(|i| format!("字段_{}", i)).collect();
        app.dialogs.db.col_checked = vec![true; 55];
        let header: Vec<String> = (0..55).map(|i| format!("列{:02}", i)).collect();
        let rows: Vec<Vec<String>> = (0..100)
            .map(|r| (0..55).map(|c| format!("值{}-{}", r, c)).collect())
            .collect();
        app.dialogs.db.preview = Some((header, rows));
        app.dialogs.db.preview_note = "表 table_00 仅显示前 100 行".to_string();

        let input = || {
            let mut i = egui::RawInput::default();
            i.screen_rect = Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(w, h)));
            i
        };
        let mut out = None;
        for _ in 0..3 {
            out = Some(ctx.run(input(), |ctx| app.show_dialogs(ctx)));
        }
        let out = out.unwrap();
        let win = ctx
            .memory(|m| m.area_rect(egui::Id::new("数据库表（只读）")))
            .expect("窗口");
        println!("窗口 = {:?}（高 {:.0}）", win, win.height());

        // 收集窗口内「非整窗背景、非细竖线」的图元 y 区间。
        // 注意要排除竖直分隔线/细线段：它们常常纵向跨越整段空白，
        // 若一并计入就会把真正的空白带掩盖掉（这条测试曾因此漏掉 360px 的空白）。
        let mut spans: Vec<(f32, f32)> = Vec::new();
        for cs in &out.shapes {
            let r = cs.shape.visual_bounding_rect();
            if !r.is_finite() || r.height() < 2.0 || r.width() < 3.0 {
                continue;
            }
            let covers_window = r.width() >= win.width() * 0.8 && r.height() >= win.height() * 0.5;
            if covers_window {
                continue;
            }
            if r.bottom() < win.top() || r.top() > win.bottom() {
                continue;
            }
            spans.push((r.top(), r.bottom()));
        }
        spans.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let mut merged: Vec<(f32, f32)> = Vec::new();
        for (t, b) in spans {
            match merged.last_mut() {
                Some(last) if t <= last.1 + 1.0 => last.1 = last.1.max(b),
                _ => merged.push((t, b)),
            }
        }
        let mut gaps: Vec<(f32, f32)> = Vec::new();
        for pair in merged.windows(2) {
            let gap = pair[1].0 - pair[0].1;
            if gap > 2.0 {
                gaps.push((pair[0].1, gap));
            }
        }
        gaps.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        println!("图元 y 区间数 = {}，最大空档：", merged.len());
        for (y, g) in gaps.iter().take(5) {
            println!("   空档 y={:.0} 高 {:.0}", y, g);
        }
        // 窗口内不应出现超过 60px 的连续空白带
        if let Some((y, g)) = gaps.first() {
            println!("最大空白带 = {:.0}px（位于 y={:.0}）", g, y);
            assert!(*g <= 60.0, "存在 {}px 空白带（y={:.0}）", g, y);
        } else {
            println!("窗口内没有 > 2px 的空白带");
        }
    }

    /// 程序启动时应清理上次遗留的数据库临时文件（大表会留下数 GB 的文件）
    #[test]
    fn startup_cleans_leftover_db_temp_files() {
        let prefix = csv_editor_rust::core::db::DB_TEMP_PREFIX;
        let leftover = std::env::temp_dir().join(format!("{}999999_0.csv", prefix));
        std::fs::write(&leftover, b"leftover from previous run").unwrap();
        assert!(leftover.exists());

        let ctx = egui::Context::default();
        let _app = CsvEditorApp::new_with_ctx(&ctx);

        // 只断言结果：启动后不再有遗留文件
        //（清理动作本身的计数与日志由 core::db 的单元测试精确覆盖；
        //  这里是端到端确认「启动即清理」这条链路确实接上了）
        assert!(
            !leftover.exists(),
            "启动时应自动清理上次遗留的数据库临时文件"
        );
    }

    /// 轮询后台消息直到条件成立或超时（等待时间以毫秒计）
    fn wait_for(app: &mut CsvEditorApp, pred: impl Fn(&CsvEditorApp) -> bool, ms: u64) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        loop {
            app.poll_messages();
            if pred(app) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// 端到端：连接本机 ODBC 驱动（Access 文本驱动）→ 读表/字段 → 预览 → 只读载入编辑器。
    /// 本机未安装任何可用的“目录型”ODBC 驱动时自动跳过（不影响其它环境跑测试）。
    #[test]
    fn db_end_to_end_with_local_odbc_driver() {
        use csv_editor_rust::core::db::{self, DbKind};

        let drivers = db::installed_drivers();
        let Some(text_driver) = drivers.iter().find(|d| d.contains("Text Driver")).cloned() else {
            eprintln!("跳过：本机没有 Access 文本 ODBC 驱动（仅影响该端到端用例）");
            return;
        };
        // 准备数据目录：Access 文本驱动把目录当库、CSV 文件当表，按系统 ANSI 代码页读文件
        let dir = std::env::temp_dir().join(format!("csv_editor_db_e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let content = "编号,姓名,金额\n1,甲,3.5\n2,乙,7\n";
        let (gbk, _, _) = encoding_rs::GBK.encode(content);
        std::fs::write(dir.join("orders.csv"), &gbk[..]).unwrap();

        let ctx = egui::Context::default();
        let mut app = CsvEditorApp::new_with_ctx(&ctx);
        app.dialogs.db.drivers = drivers.clone();
        app.dialogs.db.odbc_loaded = true;
        app.dialogs.db.kind_idx = DbKind::ALL
            .iter()
            .position(|k| *k == DbKind::Generic)
            .unwrap();
        app.dialogs.db.manual_conn_str = true;
        app.dialogs.db.conn_str = format!(
            "DRIVER={{{}}};DBQ={};Extensions=csv;HDR=Yes;",
            text_driver,
            dir.display()
        );

        // 1) 连接（只读）
        let cfg = app.dialogs.db.config();
        app.db_connect(cfg);
        assert!(
            wait_for(&mut app, |a| !a.dialogs.db.connecting, 30_000),
            "连接超时"
        );
        assert!(app.db_conn.is_some(), "连接后应保留连接：{:?}", app.dialogs.db.status);
        assert!(!app.dialogs.db.tables.is_empty(), "应读到表清单");

        // 2) 读表：字段 + 预览
        app.db_open_table(0);
        assert!(
            wait_for(
                &mut app,
                |a| !a.dialogs.db.columns.is_empty() && a.dialogs.db.preview.is_some(),
                30_000
            ),
            "读取字段/预览超时"
        );
        assert_eq!(app.dialogs.db.columns, vec!["编号", "姓名", "金额"]);
        let (_, rows) = app.dialogs.db.preview.clone().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][1], "甲");
        assert!(app.dialogs.db.col_checked.iter().all(|c| *c), "字段默认全选");

        // 3) 只读载入编辑器（全部行）
        app.dialogs.db.max_rows_text = "0".to_string();
        app.db_load_selected_into_editor();
        assert!(
            wait_for(&mut app, |a| a.model.is_some(), 60_000),
            "载入编辑器超时"
        );
        {
            let model = app.model.as_ref().unwrap();
            assert_eq!(model.row_count(), 2);
            assert_eq!(model.column_count(), 3);
            assert_eq!(model.field(0, 1), "甲");
            assert_eq!(model.header(), &["编号", "姓名", "金额"]);
        }
        // 数据来源标注：显示为「数据库表 …」而不是内部临时文件路径
        let info = app.file_info.as_ref().unwrap();
        assert!(info.path.contains("数据库表"), "来源标注：{}", info.path);
        // 4) 断开连接并清理临时文件
        app.db_disconnect();
        assert!(app.db_conn.is_none());
        if let Some(p) = app.db_temp.take() {
            let _ = std::fs::remove_file(p);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真实读取本机 ODBC 环境（驱动/DSN/已保存连接）并检查「连接数据库」窗口的预置逻辑
    #[test]
    fn db_odbc_env_and_connect_defaults() {
        use csv_editor_rust::core::db::DbKind;

        let ctx = egui::Context::default();
        let mut app = CsvEditorApp::new_with_ctx(&ctx);
        app.ensure_db_odbc();
        // 调用不应 panic；Win7/10 都自带 odbc32.dll（驱动列表可能为空，取决于是否装了驱动）
        assert!(app.dialogs.db.odbc_loaded);

        app.open_db_connect(0);
        assert_eq!(app.dialogs.db.kind(), DbKind::MySql);
        assert_eq!(app.dialogs.db.port, "3306");
        app.open_db_connect(2);
        assert_eq!(app.dialogs.db.kind(), DbKind::SqlServer);
        assert_eq!(app.dialogs.db.port, "1433");
        app.open_db_connect(3);
        assert_eq!(app.dialogs.db.kind(), DbKind::Oracle);
        assert_eq!(app.dialogs.db.port, "1521");
        app.open_db_connect(4);
        assert_eq!(app.dialogs.db.kind(), DbKind::Generic);
        // 用户改过的端口不应被类型切换覆盖
        app.dialogs.db.port = "9999".to_string();
        app.open_db_connect(1);
        assert_eq!(app.dialogs.db.port, "9999");

        // 配置构造与脱敏：密码不得出现在脱敏后的连接串里
        app.dialogs.db.password = "秘密口令".to_string();
        app.dialogs.db.user = "root".to_string();
        let cfg = app.dialogs.db.config();
        assert_eq!(cfg.kind, DbKind::PostgreSql);
        assert_eq!(cfg.password, "秘密口令");
        let shown = cfg.redacted_conn_str(&app.dialogs.db.drivers);
        assert!(!shown.contains("秘密口令"));
    }

    /// headless 渲染两个数据库窗口：确保界面代码不 panic
    /// （连接窗口的表单/下拉/已保存列表，浏览窗口的表列表/字段勾选/预览表格）
    #[test]
    fn db_dialogs_render_headless() {
        use csv_editor_rust::core::db::{DbKind, TableInfo};

        let ctx = egui::Context::default();
        let mut app = CsvEditorApp::new_with_ctx(&ctx);
        let screen = || {
            let mut input = egui::RawInput::default();
            input.screen_rect = Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1280.0, 800.0)));
            input
        };

        // 1) 连接窗口：五种数据库类型各渲染一帧（含通用 ODBC 的 DSN 分支）
        for idx in 0..DbKind::ALL.len() {
            app.open_db_connect(idx);
            app.dialogs.db.use_dsn = idx == DbKind::ALL.len() - 1;
            app.dialogs.db.dsns = vec!["MyDSN".to_string()];
            app.dialogs.db.drivers = vec!["SQL Server".to_string(), "MySQL ODBC 8.0 Unicode Driver".to_string()];
            app.dialogs.db.manual_conn_str = idx % 2 == 0;
            app.dialogs.db.conn_str = "DRIVER={x};SERVER=y;".to_string();
            app.dialogs.db.saved = vec![csv_editor_rust::core::db::DbConfig {
                name: "已保存的库".to_string(),
                kind: DbKind::MySql,
                ..Default::default()
            }];
            app.dialogs.db.status = "（测试）".to_string();
            let input = screen();
            let _ = ctx.run(input, |ctx| app.show_dialogs(ctx));
        }

        // 2) 浏览窗口：有库/表/字段/预览数据
        app.dialogs.db.connect_window = false;
        app.dialogs.db.browse_window = true;
        app.dialogs.db.scopes = vec!["demo".to_string(), "other".to_string()];
        app.dialogs.db.scope = "demo".to_string();
        app.dialogs.db.tables = vec![
            TableInfo {
                catalog: "demo".into(),
                schema: String::new(),
                name: "orders".into(),
                kind: "TABLE".into(),
            },
            TableInfo {
                catalog: "demo".into(),
                schema: String::new(),
                name: "v_orders".into(),
                kind: "VIEW".into(),
            },
        ];
        app.dialogs.db.table_filter = "order".to_string();
        app.dialogs.db.selected_table = Some(0);
        app.dialogs.db.columns = vec!["编号".to_string(), "金额".to_string()];
        app.dialogs.db.col_checked = vec![true, false];
        app.dialogs.db.preview = Some((
            vec!["编号".to_string(), "金额".to_string()],
            vec![vec!["1".to_string(), "3.5".to_string()]],
        ));
        app.dialogs.db.preview_note = "表 orders 共 1 行".to_string();
        let input = screen();
        let _ = ctx.run(input, |ctx| app.show_dialogs(ctx));

        // 3) 无预览、无字段时也要能渲染
        app.dialogs.db.preview = None;
        app.dialogs.db.columns.clear();
        app.dialogs.db.col_checked.clear();
        app.dialogs.db.tables.clear();
        app.dialogs.db.scopes.clear();
        let input = screen();
        let _ = ctx.run(input, |ctx| app.show_dialogs(ctx));
    }

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
