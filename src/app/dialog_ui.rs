//! 对话框与动作：拆分字段/合并/哈希/复制/插入/删除/隐藏/关于/导出/拆分执行。

use eframe::egui;
use egui_extras::{Column, TableBuilder};

use csv_editor_rust::core::column_ops::{HashAlgo, InsertPosition};
use csv_editor_rust::core::db::{self, DbKind};
use csv_editor_rust::core::export;
use csv_editor_rust::core::filter::{FilterCondition, FilterOp, Logic};
use csv_editor_rust::core::merge::{self, HeaderCheck};
use csv_editor_rust::core::split::{self, OutputFormat, SplitOptions};
use csv_editor_rust::core::stats::{self, AggKind};

use super::background::BgMsg;
use super::table::truncate_cell;
use super::{CsvEditorApp, DialogPosition};

/// 内嵌的微信收款码（自愿捐助窗口用；单文件发布无法依赖外部图片）
const WECHAT_QR_PNG: &[u8] = include_bytes!("../../assets/wechat_qr.png");

/// 透视表的行字段槽位数（1~2 个；与 `stats::MAX_GROUP_COLUMNS` 独立，界面固定 2 个槽位）
const MAX_PIVOT_ROW_SLOTS: usize = 2;

/// 数据库表预览最多显示的列数（更宽的表仍可由横向滚动查看，载入编辑器后不受限制）
const DB_PREVIEW_MAX_COLS: usize = 60;

/// 数据预览的默认列宽：窗口放不下时按这个宽度显示并出现横向滚动条
const DB_PREVIEW_COL_W: f32 = 78.0;

/// 「数据库表（只读）」窗口的尺寸度量（纯函数，便于测试）
pub(crate) struct DbBrowseMetrics {
    /// 窗口内容区可用高度（内容整体放进这个高度的滚动区，窗口因此不会超出屏幕）
    pub content_h: f32,
    pub win_w: f32,
    pub win_h: f32,
    /// 左侧「表 / 视图」列表宽度
    pub left_w: f32,
    /// 上段两个列表（表 / 字段）的高度
    pub lists_h: f32,
    /// 下段通栏数据预览的高度
    pub preview_h: f32,
}

/// 按屏幕尺寸算「数据库表（只读）」窗口的各区域尺寸。
///
/// 布局为「上段两栏列表 + 下段通栏预览」：左侧表列表不再从顶铺到底，
/// 数据预览横跨窗口左右两侧（因此预览高度给得比列表更大）。
pub(crate) fn db_browse_metrics(screen: egui::Rect) -> DbBrowseMetrics {
    let content_h = (screen.height() - 150.0).clamp(220.0, 660.0);
    let win_w = (screen.width() - 48.0).clamp(460.0, 1080.0);
    let win_h = content_h + 56.0;
    let left_w = (win_w * 0.28).clamp(160.0, 280.0);
    // 两处都是「高度上限」：内容少时按内容高度（布局紧凑），内容多时到此为止并出现滚动条
    let preview_h = (content_h * 0.38).clamp(110.0, 260.0);
    let lists_h = (content_h * 0.30).clamp(100.0, 240.0);
    DbBrowseMetrics {
        content_h,
        win_w,
        win_h,
        left_w,
        lists_h,
        preview_h,
    }
}

/// 渲染一行「规模上限」设置（标签 + 输入框 + 悬浮说明），返回是否被修改
fn limits_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut usize,
    range: std::ops::RangeInclusive<usize>,
    speed: f64,
    tip: &str,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(label);
        changed = ui
            .add(
                egui::DragValue::new(value)
                    .clamp_range(range)
                    .speed(speed),
            )
            .on_hover_text(tip)
            .changed();
    });
    changed
}

/// 每个参数窗打开时恢复一次「规模上限」（首次从 egui memory 读，之后沿用内存中的值）
fn ensure_stats_limits(app: &mut CsvEditorApp, ctx: &egui::Context) {
    if app.dialogs.stats_limits_loaded {
        return;
    }
    app.dialogs.stats_limits_loaded = true;
    if let Some(v) = ctx.memory(|m| {
        m.data
            .get_temp::<csv_editor_rust::core::stats::StatsLimits>(egui::Id::new("stats_limits"))
    }) {
        app.dialogs.stats_limits = v;
    }
}

/// 把「规模上限」写回 egui memory（重启后沿用）
fn save_stats_limits(ctx: &egui::Context, limits: csv_editor_rust::core::stats::StatsLimits) {
    ctx.memory_mut(|m| m.data.insert_temp(egui::Id::new("stats_limits"), limits));
}

/// 「规模上限」区的通用标题与说明
fn limits_header(ui: &mut egui::Ui, hint: &str) {
    ui.separator();
    ui.label("规模上限（按电脑内存调整；数值越大能处理的情况越多、占用内存越大）:");
    ui.weak(hint);
}

/// 程序化设置某个 egui 窗口的「折叠（最小化）」状态。
///
/// egui 0.27 的 `Window` 只有 `collapsible`，没有公开的 `minimized()`；
/// 它的折叠状态实际存在 `Id::new(标题).with("collapsing")` 的 `CollapsingState` 里
/// （见 egui `containers/window.rs`，窗口用 `area_id.with("collapsing")` 存取），
/// 这里按同一约定读写：折叠后窗口只剩标题栏，点标题栏左侧箭头可再展开。
/// `title` 必须与 `Window::new(..)` 的标题完全一致。
fn set_window_collapsed(ctx: &egui::Context, title: &str, collapsed: bool) {
    let id = egui::Id::new(title).with("collapsing");
    let mut state =
        egui::containers::collapsing_header::CollapsingState::load_with_default_open(ctx, id, true);
    state.set_open(!collapsed);
    state.store(ctx);
}

/// 筛选操作符到下拉列表索引
fn filter_op_idx(op: FilterOp) -> usize {
    match op {
        FilterOp::Equal => 0,
        FilterOp::Contains => 1,
        FilterOp::NotContains => 2,
        FilterOp::Greater => 3,
        FilterOp::Less => 4,
        FilterOp::GreaterEqual => 5,
        FilterOp::LessEqual => 6,
        FilterOp::NotEqual => 7,
        FilterOp::IsEmpty => 8,
        FilterOp::NotEmpty => 9,
    }
}

/// 按弹窗位置给 Window 应用默认位置（default_pos：首次定位、可拖动、egui 自动记忆拖动位置）。
///
/// 同时做两件「防超出屏幕」的事（小屏或界面放大 1.5 倍时尤其重要）：
/// 1. 限制窗口最大宽高不超过屏幕 —— 窗口比屏幕还宽时，标题栏会被裁到屏幕外，
///    右侧的「✕ 关闭」与滚动条都会点不到；
/// 2. 把默认位置限制在「窗口放得下」的范围内 —— 位置贴到屏幕下缘时，
///    底部按钮会被推到屏幕外（实测 egui 自身的 constrain 不足以处理这种情况）。
///
/// `est_size` 为该窗口的估计尺寸（一般用它的 `default_size`）：屏幕装不下时把窗口
/// 固定成屏幕内可容纳的尺寸，内容超出部分由窗口自身滚动（egui 的 `max_height`
/// 只能限制用户拖动，压不住内容把窗口撑大）。
pub(super) fn apply_dialog_pos<'a>(
    w: egui::Window<'a>,
    pos: DialogPosition,
    ctx: &'a egui::Context,
) -> egui::Window<'a> {
    apply_dialog_pos_sized(w, pos, ctx, egui::vec2(520.0, 480.0))
}

/// 同 [`apply_dialog_pos`]，但显式给出窗口的估计尺寸（较宽/较高的窗口应传入自己的 `default_size`）
pub(super) fn apply_dialog_pos_sized<'a>(
    w: egui::Window<'a>,
    pos: DialogPosition,
    ctx: &'a egui::Context,
    est_size: egui::Vec2,
) -> egui::Window<'a> {
    let screen = ctx.screen_rect();
    let max_w = (screen.width() - 40.0).max(320.0);
    let max_h = (screen.height() - 120.0).max(240.0);
    let est_w = est_size.x.clamp(240.0, max_w);
    // 估计尺寸超出屏幕可容纳范围时：把窗口固定为屏幕内尺寸，内容由窗口内部滚动，
    // 避免出现「窗口比屏幕还大 → 标题栏的关闭按钮跑到屏幕外」的情况
    let w = if est_size.x > max_w || est_size.y > max_h {
        w.default_size([est_size.x.min(max_w), est_size.y.min(max_h)])
            .resizable(false)
            .max_width(max_w)
            .max_height(max_h)
    } else {
        w.max_width(max_w).max_height(max_h)
    };

    let x_min = screen.left() + 8.0;
    let x_max = (screen.right() - est_w - 8.0).max(x_min);
    let y_min = screen.top() + 8.0;
    // 高度可能随内容增长到 max_h，故按最坏情况限制起始 y，确保底边不越界
    let y_max = (screen.bottom() - max_h - 8.0).max(y_min);
    let cx = |x: f32| x.clamp(x_min, x_max);
    let cy = |y: f32| y.clamp(y_min, y_max);
    match pos {
        DialogPosition::Center => w.default_pos(egui::pos2(
            cx(screen.center().x - 200.0),
            cy(screen.center().y - 200.0),
        )),
        DialogPosition::RightTop => w.default_pos(egui::pos2(
            cx(screen.right() - 360.0),
            cy(screen.top() + 80.0),
        )),
        DialogPosition::LeftTop => w.default_pos(egui::pos2(
            cx(screen.left() + 300.0),
            cy(screen.top() + 80.0),
        )),
        DialogPosition::CenterTop => w.default_pos(egui::pos2(
            cx(screen.center().x - 300.0),
            cy(screen.top() + 80.0),
        )),
        // 记忆位置：尊重用户拖动的位置，但仍然受上面的最大宽高限制
        DialogPosition::Remember => w,
    }
}

impl CsvEditorApp {
    /// 渲染所有打开的对话框
    pub fn show_dialogs(&mut self, ctx: &egui::Context) {
        let mut f = std::mem::take(&mut self.dialogs.split_field);
        self.dialog_split_field(ctx, &mut f);
        self.dialogs.split_field = f;
        let mut f = std::mem::take(&mut self.dialogs.merge_field);
        self.dialog_merge_field(ctx, &mut f);
        self.dialogs.merge_field = f;
        let mut f = std::mem::take(&mut self.dialogs.generate_hash);
        self.dialog_generate_hash(ctx, &mut f);
        self.dialogs.generate_hash = f;
        let mut f = std::mem::take(&mut self.dialogs.copy_column);
        self.dialog_copy_column(ctx, &mut f);
        self.dialogs.copy_column = f;
        let mut f = std::mem::take(&mut self.dialogs.insert_column);
        self.dialog_insert_column(ctx, &mut f);
        self.dialogs.insert_column = f;
        let mut f = std::mem::take(&mut self.dialogs.delete_column);
        self.dialog_delete_column(ctx, &mut f);
        self.dialogs.delete_column = f;
        let mut f = std::mem::take(&mut self.dialogs.hide_column);
        self.dialog_hide_show(ctx, &mut f, true);
        self.dialogs.hide_column = f;
        let mut f = std::mem::take(&mut self.dialogs.show_column);
        self.dialog_hide_show(ctx, &mut f, false);
        self.dialogs.show_column = f;
        let mut f = std::mem::take(&mut self.dialogs.about);
        self.dialog_about(ctx, &mut f);
        self.dialogs.about = f;
        let mut f = std::mem::take(&mut self.dialogs.filter_window);
        self.dialog_filter(ctx, &mut f);
        self.dialogs.filter_window = f;
        let mut f = std::mem::take(&mut self.dialogs.split_window);
        self.dialog_split(ctx, &mut f);
        self.dialogs.split_window = f;
        let mut f = std::mem::take(&mut self.dialogs.file_info);
        self.dialog_file_info(ctx, &mut f);
        self.dialogs.file_info = f;
        let mut f = std::mem::take(&mut self.dialogs.export_data);
        self.dialog_export(ctx, &mut f);
        self.dialogs.export_data = f;
        let mut f = std::mem::take(&mut self.dialogs.merge_window);
        self.dialog_merge_files(ctx, &mut f);
        self.dialogs.merge_window = f;
        let mut f = std::mem::take(&mut self.dialogs.help_guide);
        self.dialog_help_guide(ctx, &mut f);
        self.dialogs.help_guide = f;
        let mut f = std::mem::take(&mut self.dialogs.donate);
        self.dialog_donate(ctx, &mut f);
        self.dialogs.donate = f;
        let mut f = std::mem::take(&mut self.dialogs.dedup_window);
        self.dialog_dedup(ctx, &mut f);
        self.dialogs.dedup_window = f;
        let mut f = std::mem::take(&mut self.dialogs.dedup_done_window);
        self.dialog_dedup_done(ctx, &mut f);
        self.dialogs.dedup_done_window = f;
        let mut f = std::mem::take(&mut self.dialogs.stats_window);
        self.dialog_stats(ctx, &mut f);
        self.dialogs.stats_window = f;
        let mut f = std::mem::take(&mut self.dialogs.col_stats_window);
        self.dialog_column_stats(ctx, &mut f);
        self.dialogs.col_stats_window = f;
        let mut f = std::mem::take(&mut self.dialogs.freq_window);
        self.dialog_value_frequency(ctx, &mut f);
        self.dialogs.freq_window = f;
        let mut f = std::mem::take(&mut self.dialogs.dup_window);
        self.dialog_duplicate_stats(ctx, &mut f);
        self.dialogs.dup_window = f;
        let mut f = std::mem::take(&mut self.dialogs.quality_window);
        self.dialog_quality_stats(ctx, &mut f);
        self.dialogs.quality_window = f;
        let mut f = std::mem::take(&mut self.dialogs.pivot_window);
        self.dialog_pivot_table(ctx, &mut f);
        self.dialogs.pivot_window = f;
        let mut f = std::mem::take(&mut self.dialogs.cross_window);
        self.dialog_cross_stats(ctx, &mut f);
        self.dialogs.cross_window = f;
        let mut f = std::mem::take(&mut self.dialogs.stats_result_window);
        self.dialog_stats_result(ctx, &mut f);
        self.dialogs.stats_result_window = f;
        // 数据库：连接窗口 + 库/表浏览窗口
        let mut f = self.dialogs.db.connect_window;
        self.dialog_db_connect(ctx, &mut f);
        self.dialogs.db.connect_window = f;
        let mut f = self.dialogs.db.browse_window;
        self.dialog_db_browse(ctx, &mut f);
        self.dialogs.db.browse_window = f;
        // 数据行详情（双击单元格触发）
        let mut open = self.detail_row.is_some();
        self.dialog_row_detail(ctx, &mut open);
        if !open {
            self.detail_row = None;
        }
    }

    /// 「连接数据库」窗口：选择数据库类型、填写连接参数、测试/保存/连接（全部只读）
    fn dialog_db_connect(&mut self, ctx: &egui::Context, open: &mut bool) {
        self.ensure_db_odbc();
        let mut keep = *open;
        let mut close_now = false;
        let mut act_connect = false;
        let mut act_test = false;
        let mut act_save = false;
        let mut act_delete: Option<String> = None;
        let mut act_use_saved: Option<usize> = None;
        // 尺寸自适应：窗口与内部滚动区都不超出屏幕（小屏 / 界面放大后仍可操作与关闭）
        let screen = ctx.screen_rect();
        let win_w = (screen.width() - 80.0).clamp(420.0, 620.0);
        let scroll_h = (screen.height() - 190.0).clamp(220.0, 560.0);
        if keep && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            close_now = true;
        }
        apply_dialog_pos(
            egui::Window::new("连接数据库（只读）")
                .open(&mut keep)
                .collapsible(false)
                .resizable(true)
                .default_width(win_w)
                .max_width((screen.width() - 24.0).max(420.0))
                .max_height((screen.height() - 48.0).max(300.0)),
            self.dialog_position,
            ctx,
        )
        .show(ctx, |ui| {
            // 内容较多（含已保存连接列表），小屏或界面放大后需要滚动查看
            egui::ScrollArea::vertical()
                .max_height(scroll_h)
                .auto_shrink([false, false])
                .show(ui, |ui| {
            ui.weak(
                "本软件只读取数据库（SELECT / 元数据），不会修改、删除或新建数据库中的任何数据。",
            );
            ui.separator();
            let d = &mut self.dialogs.db;

            // —— 数据库类型 ——
            ui.horizontal(|ui| {
                ui.label("数据库类型:");
                let mut idx = d.kind_idx;
                egui::ComboBox::from_id_source("db_kind")
                    .selected_text(d.kind().label())
                    .show_ui(ui, |ui| {
                        for (i, k) in DbKind::ALL.iter().enumerate() {
                            ui.selectable_value(&mut idx, i, k.label());
                        }
                    });
                if idx != d.kind_idx {
                    d.kind_idx = idx;
                    let k = d.kind();
                    d.port = k.default_port().to_string();
                    d.driver = db::pick_driver(k, &d.drivers).unwrap_or_default();
                    if k != DbKind::Generic {
                        d.use_dsn = false;
                    }
                    d.status.clear();
                }
            });

            // —— 通用 ODBC：可选 DSN ——
            if d.kind() == DbKind::Generic {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut d.use_dsn, "使用已配置的数据源（DSN）");
                    if d.use_dsn {
                        let sel = if d.dsn.is_empty() {
                            "（请选择）".to_string()
                        } else {
                            d.dsn.clone()
                        };
                        egui::ComboBox::from_id_source("db_dsn_pick")
                            .selected_text(sel)
                            .show_ui(ui, |ui| {
                                if d.dsns.is_empty() {
                                    ui.label("（本机没有已配置的 DSN）");
                                }
                                for name in d.dsns.clone() {
                                    ui.selectable_value(&mut d.dsn, name.clone(), name);
                                }
                            });
                    }
                });
            }

            // —— ODBC 驱动 ——
            ui.horizontal(|ui| {
                ui.label("ODBC 驱动:");
                let shown = if d.driver.is_empty() {
                    "（自动选择）".to_string()
                } else {
                    d.driver.clone()
                };
                egui::ComboBox::from_id_source("db_driver_pick")
                    .selected_text(shown)
                    .width(320.0)
                    .show_ui(ui, |ui| {
                        if d.drivers.is_empty() {
                            ui.label("（本机未检测到 ODBC 驱动，请先安装对应数据库的 ODBC 驱动）");
                        }
                        let auto = db::pick_driver(d.kind(), &d.drivers);
                        let label = match &auto {
                            Some(a) => format!("（自动：{}）", a),
                            None => "（不指定，按连接串里的 DRIVER 项）".to_string(),
                        };
                        ui.selectable_value(&mut d.driver, String::new(), label);
                        for name in d.drivers.clone() {
                            ui.selectable_value(&mut d.driver, name.clone(), name);
                        }
                    });
            });
            // 驱动缺失时给醒目提示：这是「连接失败」最常见的原因
            let missing_driver = d.kind() != DbKind::Generic
                && d.driver.trim().is_empty()
                && db::pick_driver(d.kind(), &d.drivers).is_none();
            if missing_driver {
                ui.colored_label(
                    egui::Color32::from_rgb(190, 60, 60),
                    format!(
                        "⚠ 本机未安装 {} 的 ODBC 驱动，直接连接会失败（ODBC 报 IM002「未发现数据源名称」）。",
                        d.kind().label()
                    ),
                );
                ui.colored_label(egui::Color32::from_rgb(190, 60, 60), db::driver_hint(d.kind()));
                if !d.drivers.is_empty() {
                    ui.weak(format!(
                        "本机现有驱动：{}",
                        d.drivers.join("、")
                    ));
                }
            } else if !d.drivers.is_empty() {
                ui.weak(format!(
                    "本机已安装 {} 个 ODBC 驱动；未列出目标数据库时，请先安装对应驱动或直接手写连接串。",
                    d.drivers.len()
                ));
            }

            // —— 连接参数 ——
            ui.separator();
            let generic = d.kind() == DbKind::Generic;
            if !generic {
                egui::Grid::new("db_params")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("主机 / 服务器:");
                        ui.text_edit_singleline(&mut d.host);
                        ui.end_row();
                        ui.label("端口:");
                        ui.add(egui::TextEdit::singleline(&mut d.port).desired_width(80.0));
                        ui.end_row();
                        ui.label(if d.kind() == DbKind::Oracle {
                            "服务名 / SID:"
                        } else {
                            "数据库名:"
                        });
                        ui.text_edit_singleline(&mut d.database);
                        ui.end_row();
                        ui.label("用户名:");
                        ui.text_edit_singleline(&mut d.user);
                        ui.end_row();
                        ui.label("密码:");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut d.password)
                                    .password(!d.show_password)
                                    .desired_width(200.0),
                            );
                            ui.checkbox(&mut d.show_password, "显示");
                        });
                        ui.end_row();
                    });
            } else {
                ui.label("通用 ODBC 模式：直接填写参数或手写连接串（DSN / 其它数据库用）。");
                egui::Grid::new("db_params_generic")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("主机 / 服务器:");
                        ui.text_edit_singleline(&mut d.host);
                        ui.end_row();
                        ui.label("用户名:");
                        ui.text_edit_singleline(&mut d.user);
                        ui.end_row();
                        ui.label("密码:");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut d.password)
                                    .password(!d.show_password)
                                    .desired_width(200.0),
                            );
                            ui.checkbox(&mut d.show_password, "显示");
                        });
                        ui.end_row();
                    });
            }

            // —— 连接串 ——
            ui.separator();
            ui.checkbox(&mut d.manual_conn_str, "手动编辑连接串（勾选后按下面填写的内容连接）");
            if d.manual_conn_str {
                ui.add(
                    egui::TextEdit::multiline(&mut d.conn_str)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .hint_text("例：DRIVER={ODBC Driver 17 for SQL Server};SERVER=...;DATABASE=...;UID=...;PWD=..."),
                );
                ui.weak("提示：连接串里的密码不会被保存，除非另外勾选「同时保存密码」。");
            } else {
                let preview = d.config().redacted_conn_str(&d.drivers);
                ui.add(egui::Label::new(egui::RichText::new(preview).monospace()).wrap(true));
                ui.weak("以上为将使用的连接串（密码显示为 ***）。需要覆盖时勾选上面的「手动编辑连接串」。");
            }

            // —— 保存 ——
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("连接名称:");
                ui.add(
                    egui::TextEdit::singleline(&mut d.conn_name)
                        .desired_width(200.0)
                        .hint_text("留空则按「类型 主机/库」自动命名"),
                );
                ui.checkbox(&mut d.save_password, "同时保存密码");
            });
            ui.weak("保存的配置写入 exe 同目录的「数据库连接配置.ini」；勾选保存密码时密码为明文保存。");

            // —— 状态与按钮 ——
            ui.separator();
            if !d.status.is_empty() {
                ui.label(&d.status);
            }
            ui.horizontal(|ui| {
                let busy = d.connecting;
                if ui
                    .add_enabled(!busy, egui::Button::new("连接并浏览"))
                    .on_hover_text("建立只读连接，成功后打开「数据库表」窗口")
                    .clicked()
                {
                    act_connect = true;
                }
                if ui
                    .add_enabled(!busy, egui::Button::new("测试连接"))
                    .on_hover_text("只验证能否连通，不保留连接")
                    .clicked()
                {
                    act_test = true;
                }
                if ui
                    .add_enabled(!busy, egui::Button::new("保存此连接"))
                    .on_hover_text("保存到本机配置文件，之后可从「数据库 → 已保存的连接」一键连接")
                    .clicked()
                {
                    act_save = true;
                }
                if busy {
                    ui.spinner();
                    ui.weak("正在连接...");
                }
            });

            // —— 已保存的连接 ——
            if !self.dialogs.db.saved.is_empty() {
                ui.separator();
                ui.label("已保存的连接：");
                egui::ScrollArea::vertical()
                    .max_height(140.0)
                    .show(ui, |ui| {
                        let saved = self.dialogs.db.saved.clone();
                        for (i, c) in saved.iter().enumerate() {
                            ui.horizontal(|ui| {
                                if ui.button("连接").clicked() {
                                    act_use_saved = Some(i);
                                }
                                if ui.button("删除").clicked() {
                                    act_delete = Some(c.name.clone());
                                }
                                ui.label(format!("{}（{}）", c.display_name(), c.kind.label()));
                            });
                        }
                    });
            }
                });
        });
        *open = keep && !close_now;
        if close_now {
            return;
        }

        // 闭包外执行动作（避免与界面借用冲突）
        if act_save {
            let mut cfg = self.dialogs.db.config();
            if cfg.name.trim().is_empty() {
                cfg.name = cfg.display_name();
            }
            let mut list = self.dialogs.db.saved.clone();
            db::upsert_config(&mut list, cfg.clone());
            match db::save_configs(&list) {
                Ok(()) => {
                    self.dialogs.db.saved = list;
                    self.dialogs.db.status = format!("已保存连接：{}", cfg.name);
                    self.log(&format!("已保存数据库连接配置：{}", cfg.name));
                }
                Err(e) => {
                    self.dialogs.db.status = format!("保存失败：{}", e);
                    self.log(&format!("保存数据库连接配置失败：{}", e));
                }
            }
        }
        if let Some(name) = act_delete {
            let mut list = self.dialogs.db.saved.clone();
            if db::remove_config(&mut list, &name) {
                match db::save_configs(&list) {
                    Ok(()) => {
                        self.dialogs.db.saved = list;
                        self.dialogs.db.status = format!("已删除连接：{}", name);
                        self.log(&format!("已删除数据库连接配置：{}", name));
                    }
                    Err(e) => self.log(&format!("删除后保存失败：{}", e)),
                }
            }
        }
        if let Some(i) = act_use_saved {
            if let Some(cfg) = self.dialogs.db.saved.get(i).cloned() {
                self.dialogs.db.apply_config(&cfg);
                act_connect = true;
            }
        }
        if act_test {
            self.dialogs.db.test_only = true;
            let cfg = self.dialogs.db.config();
            self.db_connect(cfg);
        } else if act_connect {
            self.dialogs.db.test_only = false;
            let cfg = self.dialogs.db.config();
            self.db_connect(cfg);
        }
    }

    /// 「数据库表」窗口：选择库/表/字段 → 预览 → 只读载入编辑器。
    ///
    /// 尺寸与位置都按当前屏幕自适应：本窗口内容较多（库/表 + 字段 + 预览），
    /// 若沿用「右上角」这类为小窗口设计的默认位置，窗口会跑出屏幕，导致标题栏、
    /// 滚动条与按钮点不到、关不掉。这里按其实际宽高居中偏上，并限制最大尺寸
    /// （超出部分在窗口内部滚动），另设「关闭」按钮与 Esc 作为兜底。
    fn dialog_db_browse(&mut self, ctx: &egui::Context, open: &mut bool) {
        let mut keep = *open;
        let mut close_now = false;
        let mut pick_table: Option<usize> = None;
        let mut act_load = false;
        let mut act_preview = false;
        let mut change_scope: Option<String> = None;
        let mut select_all: Option<bool> = None;

        // —— 尺寸自适应 ——
        // 注意：egui 的 Window 会被内容撑大（max_height 压不住），所以窗口内容整体
        // 放进一个高度受限的滚动区（content_h），窗口高度 = content_h + 标题栏/边距，
        // 这样在任何屏幕上窗口都不会超出屏幕（否则标题栏与滚动条可能点不到、关不掉）。
        let screen = ctx.screen_rect();
        let m = db_browse_metrics(screen);
        let (content_h, win_w, win_h) = (m.content_h, m.win_w, m.win_h);
        let (left_w, lists_h, preview_h) = (m.left_w, m.lists_h, m.preview_h);

        // Esc 关闭（窗口较大时标题栏可能不便点，键盘兜底）
        if keep && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            close_now = true;
        }

        // 收窄窗口内边距：让下段的通栏数据预览左右两侧紧贴窗口边缘
        let frame = egui::Frame::window(&ctx.style())
            .inner_margin(egui::Margin::symmetric(4.0, 6.0));
        let mut win = egui::Window::new("数据库表（只读）")
            .frame(frame)
            .open(&mut keep)
            .collapsible(false)
            .resizable(true)
            .default_size([win_w, win_h])
            .min_width(460.0)
            .min_height(320.0)
            .max_width((screen.width() - 24.0).max(460.0))
            .max_height((screen.height() - 48.0).max(320.0));
        if self.dialog_position != DialogPosition::Remember {
            // 居中偏上：本窗口较大，小窗口用的「右上角」定位会让它超出屏幕
            let x = (screen.center().x - win_w * 0.5).clamp(
                screen.left() + 8.0,
                (screen.right() - win_w - 8.0).max(screen.left() + 8.0),
            );
            let y = (screen.center().y - win_h * 0.5).clamp(
                screen.top() + 28.0,
                (screen.bottom() - win_h - 8.0).max(screen.top() + 8.0),
            );
            win = win.default_pos(egui::pos2(x, y));
        }
        win.show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .max_height(content_h)
                // y 收缩：内容少时窗口跟着变矮（紧凑），内容多时才出现滚动条
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    self.db_browse_body(
                        ui,
                        left_w,
                        lists_h,
                        preview_h,
                        &mut pick_table,
                        &mut act_load,
                        &mut act_preview,
                        &mut change_scope,
                        &mut select_all,
                    );
                });
        });
        *open = keep && !close_now;
        if close_now {
            return; // 用户关窗：本帧不再执行任何动作
        }
        if let Some(true) = select_all {
            for f in self.dialogs.db.col_checked.iter_mut() {
                *f = true;
            }
        }
        if let Some(false) = select_all {
            for f in self.dialogs.db.col_checked.iter_mut() {
                *f = false;
            }
        }
        if let Some(scope) = change_scope {
            self.dialogs.db.scope = scope.clone();
            self.dialogs.db.selected_table = None;
            self.dialogs.db.columns.clear();
            self.dialogs.db.col_checked.clear();
            self.dialogs.db.preview = None;
            self.db_load_tables(scope);
        }
        match pick_table {
            Some(usize::MAX) => {
                let scope = self.dialogs.db.scope.clone();
                self.db_load_tables(scope);
            }
            Some(i) => self.db_open_table(i),
            None => {}
        }
        if act_preview {
            if let Some(i) = self.dialogs.db.selected_table {
                self.db_open_table(i);
            }
        }
        if act_load {
            self.db_load_selected_into_editor();
        }
    }

    /// 「数据库表」窗口的正文（放在高度受限的滚动区里渲染，见 `dialog_db_browse`）
    #[allow(clippy::too_many_arguments)]
    fn db_browse_body(
        &mut self,
        ui: &mut egui::Ui,
        left_w: f32,
        lists_h: f32,
        preview_h: f32,
        pick_table: &mut Option<usize>,
        act_load: &mut bool,
        act_preview: &mut bool,
        change_scope: &mut Option<String>,
        select_all: &mut Option<bool>,
    ) {
        let pick_table = &mut *pick_table;
        let act_load = &mut *act_load;
        let act_preview = &mut *act_preview;
        let change_scope = &mut *change_scope;
        let select_all = &mut *select_all;

        // —— 顶部：连接信息 + 显式「关闭」——
        ui.horizontal(|ui| {
            let desc = if self.db_conn_desc.is_empty() {
                "（未连接）".to_string()
            } else {
                self.db_conn_desc.clone()
            };
            ui.strong(truncate_cell(&desc, 56)).on_hover_text(format!(
                "{}\n只读：本软件只执行查询（SELECT 与库表元数据），不会改动数据库数据",
                desc
            ));
        });

        // —— 库 / 模式选择 ——
        {
            let d = &mut self.dialogs.db;
            if !d.scopes.is_empty() {
                ui.horizontal(|ui| {
                    ui.label(format!("{}:", d.kind().scope_label()));
                    let cur = if d.scope.is_empty() {
                        "（全部）".to_string()
                    } else {
                        d.scope.clone()
                    };
                    let mut sel = d.scope.clone();
                    egui::ComboBox::from_id_source("db_scope_pick")
                        .selected_text(cur)
                        .width(260.0)
                        .show_ui(ui, |ui| {
                            for sc in d.scopes.clone() {
                                ui.selectable_value(&mut sel, sc.clone(), sc);
                            }
                        });
                    if sel != d.scope {
                        *change_scope = Some(sel);
                    }
                });
            }
        }
        ui.separator();

        // —— 上段：左＝表 / 视图列表，右＝字段列表（两个列表等高，底部不铺满窗口）——
        // —— 上段：左＝表 / 视图列表，右＝字段列表 ——
        // 用「限定高度的容器」承载两栏：水平布局里的 ui.separator()（竖分隔线）会按
        // 「可用高度」绘制并计入布局，若不限定高度，它会把上段撑到接近整窗高，
        // 两栏下方就留下一大块消不掉的空白（窗口显得松散、不紧凑）。
        let row_h = lists_h + 66.0; // 标签 + 筛选/操作行 + 列表 + 计数行
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), row_h),
            egui::Layout::left_to_right(egui::Align::Min),
            |ui| {
                // 左：表 / 视图
                let d = &mut self.dialogs.db;
                ui.vertical(|ui| {
                    ui.set_min_width(left_w);
                    ui.set_max_width(left_w);
                    ui.label("表 / 视图（点击选择）:");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut d.table_filter)
                                .desired_width((left_w - 118.0).max(60.0))
                                .hint_text("按名称过滤"),
                        );
                        if ui
                            .button("刷新")
                            .on_hover_text("重新读取当前库/模式的表清单")
                            .clicked()
                        {
                            *pick_table = Some(usize::MAX);
                        }
                    });
                    let filter = d.table_filter.trim().to_lowercase();
                    let rows: Vec<(usize, String)> = d
                        .tables
                        .iter()
                        .enumerate()
                        .filter(|(_, t)| filter.is_empty() || t.name.to_lowercase().contains(&filter))
                        .map(|(i, t)| (i, t.display()))
                        .collect();
                    let selected = d.selected_table;
                    egui::ScrollArea::vertical()
                        .id_source("db_tables")
                        .max_height(lists_h)
                        // y 方向收缩到内容高度：auto_shrink 的 y=false 会让滚动区占满可用高度，
                        // 结果两栏下方留出一大块空白（窗口显得松散）
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            if rows.is_empty() {
                                ui.weak("（没有符合条件的表；可点「刷新」重新读取）");
                            }
                            for (i, label) in &rows {
                                if ui
                                    .selectable_label(selected == Some(*i), truncate_cell(label, 28))
                                    .on_hover_text(label.as_str())
                                    .clicked()
                                {
                                    *pick_table = Some(*i);
                                }
                            }
                        });
                    ui.weak(format!("共 {} 个表/视图", d.tables.len()));
                });
                ui.separator();

                // 右：字段
                let d = &mut self.dialogs.db;
                ui.vertical(|ui| {
                    ui.label("字段（勾选要读取的列）:");
                    ui.horizontal(|ui| {
                        if ui.button("全选").clicked() {
                            *select_all = Some(true);
                        }
                        if ui.button("全不选").clicked() {
                            *select_all = Some(false);
                        }
                        ui.label("读取行数上限:");
                        ui.add(
                            egui::TextEdit::singleline(&mut d.max_rows_text)
                                .desired_width(90.0)
                                .hint_text("0=全部"),
                        )
                        .on_hover_text("从数据库读取多少行：填 0 表示读取全部行");
                    });
                    egui::ScrollArea::vertical()
                        .id_source("db_columns")
                        .max_height(lists_h)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            if d.columns.is_empty() {
                                ui.weak("（先选择一张表）");
                            }
                            for (i, name) in d.columns.iter().enumerate() {
                                if let Some(flag) = d.col_checked.get_mut(i) {
                                    ui.checkbox(flag, truncate_cell(name, 34))
                                        .on_hover_text(name.as_str());
                                }
                            }
                        });
                });
        },
        );

        // —— 操作行 ——
        ui.separator();
        let d = &mut self.dialogs.db;
        ui.horizontal(|ui| {
            let can_act = d.selected_table.is_some() && !d.connecting;
            if ui
                .add_enabled(can_act, egui::Button::new("⬇ 载入到编辑器"))
                .on_hover_text("把选中的字段与数据读入编辑器，随后可用筛选/统计/拆分/导出等全部功能")
                .clicked()
            {
                *act_load = true;
            }
            if ui
                .add_enabled(can_act, egui::Button::new("🔄 刷新预览"))
                .clicked()
            {
                *act_preview = true;
            }
            if d.connecting {
                ui.spinner();
                ui.weak("正在读取...");
            }
        });

        // —— 下段：数据预览（通栏；列多时可横向滚动）——
        ui.separator();
        ui.label(if d.preview_note.is_empty() {
            "数据预览:".to_string()
        } else {
            format!("数据预览（{}）:", d.preview_note)
        });
        if let Some((header, rows)) = &d.preview {
            let header = header.clone();
            let rows = rows.clone();
            let n = header.len().min(DB_PREVIEW_MAX_COLS).max(1);
            // 列宽：窗口放得下就铺满整宽（左右贴住窗口），放不下就按默认列宽显示，
            // 由横向滚动条查看其它列 —— 表格宽度不再影响窗口尺寸。
            let gap = 8.0;
            let avail_w = (ui.available_width() - 18.0).max(160.0); // 18 = 预留垂直滚动条
            let per_col = (avail_w - gap * n as f32) / n as f32;
            let col_w = if per_col >= DB_PREVIEW_COL_W {
                per_col
            } else {
                DB_PREVIEW_COL_W
            };
            if header.len() > n {
                ui.weak(format!(
                    "（该表共 {} 列，这里显示前 {} 列；可拖动横向滚动条查看，载入编辑器后可查看全部列）",
                    header.len(),
                    n
                ));
            }
            egui::ScrollArea::horizontal()
                .id_source("db_preview_hscroll")
                .max_height(preview_h + 22.0) // 预留横向滚动条
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    TableBuilder::new(ui)
                        .striped(true)
                        .auto_shrink([true, true])
                        .columns(Column::exact(col_w).clip(true), n)
                        .max_scroll_height(preview_h)
                        .header(20.0, |mut h| {
                            for name in header.iter().take(n) {
                                h.col(|ui| {
                                    ui.strong(truncate_cell(name, 40));
                                });
                            }
                        })
                        .body(|mut body| {
                            for row in &rows {
                                body.row(18.0, |mut r| {
                                    for c in row.iter().take(n) {
                                        r.col(|ui| {
                                            ui.label(truncate_cell(c, 40));
                                        });
                                    }
                                });
                            }
                        });
                });
        } else {
            ui.weak("（选择表后自动预览前 100 行）");
        }

        // —— 动作处理（与界面渲染分开，避免借用冲突）——
        if let Some(true) = *select_all {
            for f in self.dialogs.db.col_checked.iter_mut() {
                *f = true;
            }
        }
        if let Some(false) = *select_all {
            for f in self.dialogs.db.col_checked.iter_mut() {
                *f = false;
            }
        }
        if let Some(scope) = change_scope.clone() {
            self.dialogs.db.scope = scope.clone();
            self.dialogs.db.selected_table = None;
            self.dialogs.db.columns.clear();
            self.dialogs.db.col_checked.clear();
            self.dialogs.db.preview = None;
            self.db_load_tables(scope);
        }
        match *pick_table {
            Some(usize::MAX) => {
                let scope = self.dialogs.db.scope.clone();
                self.db_load_tables(scope);
            }
            Some(i) => self.db_open_table(i),
            None => {}
        }
        if *act_preview {
            if let Some(i) = self.dialogs.db.selected_table {
                self.db_open_table(i);
            }
        }
        if *act_load {
            self.db_load_selected_into_editor();
        }
    }

    /// 列名列表（用于对话框下拉）
    fn header_names(&self) -> Vec<String> {
        self.ops
            .as_ref()
            .map(|o| o.header().to_vec())
            .unwrap_or_default()
    }

    fn dialog_split_field(&mut self, ctx: &egui::Context, open: &mut bool) {
        let mut keep = *open;
        let mut done = false;
        apply_dialog_pos(
            egui::Window::new("拆分字段")
                .open(&mut keep)
                .collapsible(false)
                .resizable(false),
            self.dialog_position,
            ctx,
        )
        .show(ctx, |ui| {
                ui.weak("例如：使用空格拆分 '2021-12-30 00:00:00' 得到两列");
                let header = self.header_names();
                if header.is_empty() {
                    ui.label("请先加载数据");
                    return;
                }
                ui.label("选择要拆分的字段:");
                let mut col = self.dialogs.split_col.min(header.len() - 1);
                egui::ComboBox::from_id_source("dlg_split_col")
                    .selected_text(header[col].clone())
                    .show_ui(ui, |ui| {
                        for (i, h) in header.iter().enumerate() {
                            ui.selectable_value(&mut col, i, h);
                        }
                    });
                self.dialogs.split_col = col;

                ui.label("分隔符:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.dialogs.split_sep)
                        .hint_text("空格、逗号(,)、分号(;)等"),
                );

                ui.label("新列数量:");
                ui.add(egui::DragValue::new(&mut self.dialogs.split_num).clamp_range(1..=10));
                let num = self.dialogs.split_num.clamp(1, 10);

                ui.label("新列名称前缀（自动命名、以及自定义留空时使用）:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.dialogs.split_prefix)
                        .hint_text("例：ts_ → ts_1、ts_2；留空则用 1、2、3…"),
                );

                ui.label("新列命名方式:");
                ui.horizontal(|ui| {
                    ui.selectable_value(
                        &mut self.dialogs.split_name_mode,
                        0,
                        "自动命名（前缀 + 序号）",
                    );
                    ui.selectable_value(&mut self.dialogs.split_name_mode, 1, "逐个自定义命名");
                });
                if self.dialogs.split_name_mode == 1 {
                    // 输入框数量跟随「新列数量」
                    if self.dialogs.split_names.len() < num {
                        self.dialogs.split_names.resize(num, String::new());
                    }
                    let prefix = self.dialogs.split_prefix.clone();
                    ui.weak("为每个新列取名（留空则用「前缀 + 序号」）:");
                    egui::ScrollArea::vertical()
                        .max_height(170.0)
                        .id_source("dlg_split_names")
                        .show(ui, |ui| {
                            for i in 0..num {
                                ui.horizontal(|ui| {
                                    ui.label(format!("第 {} 列:", i + 1));
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.dialogs.split_names[i])
                                            .hint_text(format!("{}{}", prefix, i + 1))
                                            .desired_width(170.0),
                                    );
                                });
                            }
                        });
                }

                ui.add_space(4.0);
                if ui.button("确定").clicked() {
                    let sep = self.dialogs.split_sep.clone();
                    let num = self.dialogs.split_num.clamp(1, 10);
                    let prefix = self.dialogs.split_prefix.clone();
                    // 仅「逐个自定义命名」模式下携带自定义名称；自动模式留空走默认命名
                    let names: Vec<String> = if self.dialogs.split_name_mode == 1 {
                        self.dialogs.split_names.clone()
                    } else {
                        Vec::new()
                    };
                    let src_name = header[col].clone();
                    match self.ops.as_ref().and_then(|o| o.view_to_base(col)) {
                        Some(base) => {
                            if let Some(ops) = self.ops_mut() {
                                match ops.split_field_named(base, &sep, num, &names, &prefix) {
                                    Ok(new_names) => self.log(&format!(
                                        "字段拆分完成: {} -> {} 列（{}）",
                                        src_name,
                                        num,
                                        new_names.join("、")
                                    )),
                                    Err(e) => self.log(&format!("拆分字段失败: {}", e)),
                                }
                            }
                        }
                        None => self.log("所选字段为派生列，不能作为拆分源"),
                    }
                    done = true;
                }
            });
        if done {
            keep = false;
        }
        *open = keep;
    }

    fn dialog_merge_field(&mut self, ctx: &egui::Context, open: &mut bool) {
        let mut keep = *open;
        let mut done = false;
        apply_dialog_pos(
            egui::Window::new("合并字段")
                .open(&mut keep)
                .collapsible(false)
                .resizable(false),
            self.dialog_position,
            ctx,
        )
        .show(ctx, |ui| {
                let header = self.header_names();
                if header.is_empty() {
                    ui.label("请先加载数据");
                    return;
                }
                ui.label("选择要合并的字段:");
                let mut sel = self.dialogs.merge_cols.clone();
                egui::ScrollArea::vertical().max_height(140.0).show(ui, |ui| {
                    for (i, h) in header.iter().enumerate() {
                        let mut on = sel.contains(&i);
                        ui.checkbox(&mut on, h);
                        if on && !sel.contains(&i) {
                            sel.push(i);
                        } else if !on {
                            sel.retain(|&x| x != i);
                        }
                    }
                });
                self.dialogs.merge_cols = sel;

                ui.label("新字段名称:");
                ui.add(egui::TextEdit::singleline(&mut self.dialogs.merge_name));
                ui.label("连接符:");
                ui.add(egui::TextEdit::singleline(&mut self.dialogs.merge_sep));

                if ui.button("确定").clicked() {
                    let cols = self.dialogs.merge_cols.clone();
                    let name = self.dialogs.merge_name.clone();
                    let sep = self.dialogs.merge_sep.clone();
                    // 视图列 → 原始列映射（派生列剔除）
                    let base_cols: Vec<usize> = cols
                        .iter()
                        .filter_map(|&c| self.ops.as_ref().and_then(|o| o.view_to_base(c)))
                        .collect();
                    if base_cols.len() != cols.len() {
                        self.log("合并字段包含派生列，已忽略");
                    }
                    if !base_cols.is_empty() && !name.is_empty() {
                        if let Some(ops) = self.ops_mut() {
                            match ops.merge_fields(&base_cols, &name, &sep) {
                                Ok(()) => self.log(&format!("字段合并完成: {} 列 -> {}", base_cols.len(), name)),
                                Err(e) => self.log(&format!("合并字段失败: {}", e)),
                            }
                        }
                    } else {
                        self.log("请选择字段并填写名称");
                    }
                    done = true;
                }
            });
        if done {
            keep = false;
        }
        *open = keep;
    }

    fn dialog_generate_hash(&mut self, ctx: &egui::Context, open: &mut bool) {
        let mut keep = *open;
        let mut done = false;
        apply_dialog_pos(
            egui::Window::new("生成行哈希值")
                .open(&mut keep)
                .collapsible(false)
                .resizable(false),
            self.dialog_position,
            ctx,
        )
        .show(ctx, |ui| {
                let header = self.header_names();
                if header.is_empty() {
                    ui.label("请先加载数据");
                    return;
                }
                ui.label("哈希算法:");
                let algos = ["md5", "sha1", "sha256", "sha512"];
                let mut a = self.dialogs.hash_algo.min(algos.len() - 1);
                egui::ComboBox::from_id_source("dlg_hash_algo")
                    .selected_text(algos[a])
                    .show_ui(ui, |ui| {
                        for (i, x) in algos.iter().enumerate() {
                            ui.selectable_value(&mut a, i, *x);
                        }
                    });
                self.dialogs.hash_algo = a;

                ui.label("选择字段（默认全部）:");
                let mut sel = self.dialogs.hash_cols.clone();
                egui::ScrollArea::vertical().max_height(140.0).show(ui, |ui| {
                    for (i, h) in header.iter().enumerate() {
                        let mut on = sel.contains(&i);
                        ui.checkbox(&mut on, h);
                        if on && !sel.contains(&i) {
                            sel.push(i);
                        } else if !on {
                            sel.retain(|&x| x != i);
                        }
                    }
                });
                self.dialogs.hash_cols = sel;

                ui.label("哈希值字段名称:");
                ui.add(egui::TextEdit::singleline(&mut self.dialogs.hash_name));

                if ui.button("确定").clicked() {
                    let cols = if self.dialogs.hash_cols.is_empty() {
                        (0..header.len()).collect()
                    } else {
                        self.dialogs.hash_cols.clone()
                    };
                    let name = self.dialogs.hash_name.clone();
                    let algo = HashAlgo::from_label(algos[a]).unwrap_or(HashAlgo::Md5);
                    // 视图列 → 原始列映射（派生列剔除）
                    let base_cols: Vec<usize> = cols
                        .iter()
                        .filter_map(|&c| self.ops.as_ref().and_then(|o| o.view_to_base(c)))
                        .collect();
                    if let Some(ops) = self.ops_mut() {
                        match ops.generate_hash(&base_cols, &name, algo) {
                            Ok(()) => self.log(&format!("生成行哈希完成: {}", name)),
                            Err(e) => self.log(&format!("生成哈希失败: {}", e)),
                        }
                    }
                    done = true;
                }
            });
        if done {
            keep = false;
        }
        *open = keep;
    }

    fn dialog_copy_column(&mut self, ctx: &egui::Context, open: &mut bool) {
        let mut keep = *open;
        let mut done = false;
        apply_dialog_pos(
            egui::Window::new("复制列")
                .open(&mut keep)
                .collapsible(false)
                .resizable(false),
            self.dialog_position,
            ctx,
        )
        .show(ctx, |ui| {
                let header = self.header_names();
                if header.is_empty() {
                    ui.label("请先加载数据");
                    return;
                }
                ui.label("选择要复制的列:");
                let mut col = self.dialogs.copy_col.min(header.len() - 1);
                egui::ComboBox::from_id_source("dlg_copy_col")
                    .selected_text(header[col].clone())
                    .show_ui(ui, |ui| {
                        for (i, h) in header.iter().enumerate() {
                            ui.selectable_value(&mut col, i, h);
                        }
                    });
                self.dialogs.copy_col = col;
                ui.label("新列名称:");
                ui.add(egui::TextEdit::singleline(&mut self.dialogs.copy_name));
                if ui.button("确定").clicked() {
                    let name = self.dialogs.copy_name.clone();
                    match self.ops.as_ref().and_then(|o| o.view_to_base(col)) {
                        Some(base) => {
                            if name.is_empty() {
                                self.log("请输入新列名称");
                            } else if let Some(ops) = self.ops_mut() {
                                match ops.copy_column(base, &name) {
                                    Ok(()) => self.log(&format!("复制列完成: {} -> {}", header[col], name)),
                                    Err(e) => self.log(&format!("复制列失败: {}", e)),
                                }
                            }
                        }
                        None => self.log("所选字段为派生列，不能作为复制源"),
                    }
                    done = true;
                }
            });
        if done {
            keep = false;
        }
        *open = keep;
    }

    fn dialog_insert_column(&mut self, ctx: &egui::Context, open: &mut bool) {
        let mut keep = *open;
        let mut done = false;
        apply_dialog_pos(
            egui::Window::new("插入列")
                .open(&mut keep)
                .collapsible(false)
                .resizable(false),
            self.dialog_position,
            ctx,
        )
        .show(ctx, |ui| {
                let header = self.header_names();
                if header.is_empty() {
                    ui.label("请先加载数据");
                    return;
                }
                ui.label("新列名称:");
                ui.add(egui::TextEdit::singleline(&mut self.dialogs.insert_name));
                ui.label("插入位置:");
                let pos_labels = ["开头", "末尾", "指定列前"];
                let mut p = self.dialogs.insert_pos.min(2);
                egui::ComboBox::from_id_source("dlg_insert_pos")
                    .selected_text(pos_labels[p])
                    .show_ui(ui, |ui| {
                        for (i, x) in pos_labels.iter().enumerate() {
                            ui.selectable_value(&mut p, i, *x);
                        }
                    });
                self.dialogs.insert_pos = p;
                if p == 2 {
                    ui.label("参考列:");
                    let mut refc = self.dialogs.insert_ref.min(header.len() - 1);
                    egui::ComboBox::from_id_source("dlg_insert_ref")
                        .selected_text(header[refc].clone())
                        .show_ui(ui, |ui| {
                            for (i, h) in header.iter().enumerate() {
                                ui.selectable_value(&mut refc, i, h);
                            }
                        });
                    self.dialogs.insert_ref = refc;
                }
                ui.label("默认值:");
                ui.add(egui::TextEdit::singleline(&mut self.dialogs.insert_default));

                if ui.button("确定").clicked() {
                    let name = self.dialogs.insert_name.clone();
                    let default = self.dialogs.insert_default.clone();
                    let pos = match p {
                        0 => InsertPosition::Start,
                        1 => InsertPosition::End,
                        _ => InsertPosition::Before(self.dialogs.insert_ref.min(header.len() - 1)),
                    };
                    if name.is_empty() {
                        self.log("请输入新列名称");
                    } else if let Some(ops) = self.ops_mut() {
                        match ops.insert_column(&name, pos, &default) {
                            Ok(()) => self.log(&format!("插入列完成: {}", name)),
                            Err(e) => self.log(&format!("插入列失败: {}", e)),
                        }
                    }
                    done = true;
                }
            });
        if done {
            keep = false;
        }
        *open = keep;
    }

    fn dialog_delete_column(&mut self, ctx: &egui::Context, open: &mut bool) {
        let mut keep = *open;
        let mut done = false;
        apply_dialog_pos(
            egui::Window::new("删除列")
                .open(&mut keep)
                .collapsible(false)
                .resizable(false),
            self.dialog_position,
            ctx,
        )
        .show(ctx, |ui| {
                let header = self.header_names();
                if header.is_empty() {
                    ui.label("请先加载数据");
                    return;
                }
                ui.label("选择要删除的列:");
                // 丢弃超出当前列数的陈旧索引：列数会因之前的删除/撤销/生成列而变化，
                // 若把旧索引原样提交，delete_columns 会报「列索引 N 不存在」。
                self.dialogs.delete_cols.retain(|&i| i < header.len());
                let mut sel = self.dialogs.delete_cols.clone();
                egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                    for (i, h) in header.iter().enumerate() {
                        let mut on = sel.contains(&i);
                        ui.checkbox(&mut on, h);
                        if on && !sel.contains(&i) {
                            sel.push(i);
                        } else if !on {
                            sel.retain(|&x| x != i);
                        }
                    }
                });
                self.dialogs.delete_cols = sel.clone();
                if ui.button("确定删除").clicked() {
                    if sel.is_empty() {
                        self.log("请选择要删除的列");
                    } else if let Some(ops) = self.ops_mut() {
                        match ops.delete_columns(&sel) {
                            Ok(()) => {
                                self.log(&format!("删除列完成: {} 列", sel.len()));
                                // 删除后必须清空选中：否则这些列索引残留到下次打开，
                                // 列数已变少时会越界（曾表现为「删除列失败：列索引 N 不存在」）。
                                self.dialogs.delete_cols.clear();
                            }
                            Err(e) => self.log(&format!("删除列失败: {}", e)),
                        }
                    }
                    done = true;
                }
            });
        if done {
            keep = false;
        }
        *open = keep;
    }

    fn dialog_hide_show(&mut self, ctx: &egui::Context, open: &mut bool, hide: bool) {
        let mut keep = *open;
        let mut done = false;
        let title = if hide { "隐藏列" } else { "显示列" };
        apply_dialog_pos(
            egui::Window::new(title)
                .open(&mut keep)
                .collapsible(false)
                .resizable(false),
            self.dialog_position,
            ctx,
        )
        .show(ctx, |ui| {
                let header = self.header_names();
                if header.is_empty() {
                    ui.label("请先加载数据");
                    return;
                }
                ui.label(if hide {
                    "选择要隐藏的列（视图层）："
                } else {
                    "选择要显示的列："
                });
                let sel_key = if hide {
                    &mut self.dialogs.hide_cols
                } else {
                    &mut self.dialogs.show_cols
                };
                let mut sel = sel_key.clone();
                egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                    for (i, h) in header.iter().enumerate() {
                        let hidden_now = self.ops.as_ref().map(|o| o.is_hidden(i)).unwrap_or(false);
                        if hide && hidden_now {
                            continue;
                        }
                        if !hide && !hidden_now {
                            continue;
                        }
                        let mut on = sel.contains(&i);
                        ui.checkbox(&mut on, h);
                        if on && !sel.contains(&i) {
                            sel.push(i);
                        } else if !on {
                            sel.retain(|&x| x != i);
                        }
                    }
                });
                *sel_key = sel;
                if ui.button("确定").clicked() {
                    let cols = sel_key.clone();
                    if let Some(ops) = self.ops_mut() {
                        if hide {
                            let _ = ops.hide_columns(&cols);
                            self.log(&format!("隐藏列: {} 列", cols.len()));
                        } else {
                            let _ = ops.show_columns(&cols);
                            self.log(&format!("显示列: {} 列", cols.len()));
                        }
                    }
                    done = true;
                }
            });
        if done {
            keep = false;
        }
        *open = keep;
    }

    fn dialog_about(&mut self, ctx: &egui::Context, open: &mut bool) {
        let mut keep = *open;
        let mut done = false;
        apply_dialog_pos(
            egui::Window::new("关于")
                .open(&mut keep)
                .collapsible(false)
                .resizable(false),
            self.dialog_position,
            ctx,
        )
        .show(ctx, |ui| {
                ui.heading("宿经03超大CSV文件实用编辑器");
                ui.label("Rust 重构版（原 Python/PyQt6 版 1.3_nodask）");
                ui.label("作者：SZJZ-王安工作室");
                ui.separator();
                ui.label("功能：读取/筛选/拆分/列操作/哈希/导出（CSV/XLSX）");
                ui.label("支持 6.5GB 级超大 CSV，流式处理内存占用 < 500MB");
                ui.label("Win7 兼容单文件（x86_64-win7-windows-gnu + build-std）");
                ui.add_space(6.0);
                if ui.button("关闭").clicked() {
                    done = true;
                }
            });
        if done {
            keep = false;
        }
        *open = keep;
    }

    /// 数据筛选弹出窗口（原左侧筛选面板逻辑，功能不变）
    fn dialog_filter(&mut self, ctx: &egui::Context, open: &mut bool) {
        let mut keep = *open;
        apply_dialog_pos_sized(
            egui::Window::new("数据筛选")
                .open(&mut keep)
                .collapsible(false)
                .resizable(true)
                .default_size([640.0, 460.0]),
            self.dialog_position,
            ctx,
            egui::vec2(640.0, 460.0),
        )
        .show(ctx, |ui| {
                let header: Vec<String> = self
                    .ops
                    .as_ref()
                    .map(|o| o.header().to_vec())
                    .unwrap_or_default();
                if header.is_empty() {
                    ui.label("请先加载数据");
                    return;
                }

                let op_labels = [
                    "等于",
                    "包含",
                    "不包含",
                    "大于",
                    "小于",
                    "大于等于",
                    "小于等于",
                    "不等于",
                    "空值",
                    "非空值",
                ];

                ui.add_space(2.0);
                // 可编辑条件列表（take/回写，避免借用冲突）
                let mut conds = std::mem::take(&mut self.filter_conditions);
                let mut remove: Option<usize> = None;
                let mut move_up: Option<usize> = None;
                let mut move_down: Option<usize> = None;

                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        if conds.is_empty() {
                            ui.weak("（暂无筛选条件，点击下方「+ 添加条件」）");
                        }
                        for i in 0..conds.len() {
                            ui.horizontal(|ui| {
                                // 逻辑关系：首条无前置关系，显示「当」
                                if i > 0 {
                                    let mut lg = conds[i].logic == Logic::And;
                                    egui::ComboBox::from_id_source(("fcond_lg", i))
                                        .selected_text(if lg { "且" } else { "或" })
                                        .width(46.0)
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(&mut lg, true, "且");
                                            ui.selectable_value(&mut lg, false, "或");
                                        });
                                    conds[i].logic = if lg { Logic::And } else { Logic::Or };
                                } else {
                                    ui.weak("当");
                                }

                                // 字段
                                let mut col = conds[i].column.min(header.len().saturating_sub(1));
                                egui::ComboBox::from_id_source(("fcond_col", i))
                                    .selected_text(header.get(col).cloned().unwrap_or_default())
                                    .width(130.0)
                                    .show_ui(ui, |ui| {
                                        for (j, h) in header.iter().enumerate() {
                                            ui.selectable_value(&mut col, j, h);
                                        }
                                    });
                                conds[i].column = col;

                                // 操作符
                                let mut op = conds[i].op;
                                let mut op_idx = filter_op_idx(op);
                                egui::ComboBox::from_id_source(("fcond_op", i))
                                    .selected_text(op.label())
                                    .width(88.0)
                                    .show_ui(ui, |ui| {
                                        for (k, l) in op_labels.iter().enumerate() {
                                            ui.selectable_value(&mut op_idx, k, *l);
                                        }
                                    });
                                if let Some(o) = FilterOp::from_label(op_labels[op_idx]) {
                                    op = o;
                                }
                                conds[i].op = op;

                                // 值（空值/非空值无需输入）
                                if op.needs_value() {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut conds[i].value)
                                            .hint_text("值")
                                            .desired_width(140.0),
                                    );
                                } else {
                                    ui.add_enabled(
                                        false,
                                        egui::TextEdit::singleline(&mut conds[i].value)
                                            .hint_text("无需输入值")
                                            .desired_width(140.0),
                                    );
                                }

                                // 调整顺序与删除
                                if i > 0 && ui.button("↑").on_hover_text("上移").clicked() {
                                    move_up = Some(i);
                                }
                                if i + 1 < conds.len()
                                    && ui.button("↓").on_hover_text("下移").clicked()
                                {
                                    move_down = Some(i);
                                }
                                if ui.button("×").on_hover_text("删除此条件").clicked() {
                                    remove = Some(i);
                                }
                            });
                            ui.separator();
                        }
                    });

                // 增删/排序后回写（remove 先行，索引可能已失效，swap 前补长度检查）
                if let Some(i) = remove {
                    conds.remove(i);
                }
                if let Some(i) = move_up {
                    if i > 0 && i < conds.len() {
                        conds.swap(i, i - 1);
                    }
                }
                if let Some(i) = move_down {
                    if i + 1 < conds.len() {
                        conds.swap(i, i + 1);
                    }
                }
                self.filter_conditions = conds;

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("+ 添加条件").clicked() {
                        self.filter_conditions.push(FilterCondition {
                            column: 0,
                            op: FilterOp::Equal,
                            value: String::new(),
                            logic: Logic::And,
                        });
                    }
                    if ui.button("应用筛选").clicked() {
                        self.apply_filters();
                    }
                    if ui.button("清除筛选").clicked() {
                        self.filter_conditions.clear();
                        self.filtered_rows = None;
                        self.log("清除所有筛选条件");
                    }
                });
            });
        *open = keep;
    }

    /// 数据拆分弹出窗口（原左侧拆分标签页逻辑）
    fn dialog_split(&mut self, ctx: &egui::Context, open: &mut bool) {
        let mut keep = *open;
        apply_dialog_pos(
            egui::Window::new("数据拆分")
                .open(&mut keep)
                .collapsible(false)
                .resizable(false),
            self.dialog_position,
            ctx,
        )
        .show(ctx, |ui| {
                ui.label("拆分方式:");
                let mut method = self.dialogs.split_method;
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut method, 0, "按行数拆分");
                    ui.selectable_value(&mut method, 1, "按字段拆分");
                });
                self.dialogs.split_method = method;

                if method == 0 {
                    ui.label("每批行数:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.dialogs.split_batch)
                            .hint_text("100000"),
                    );
                } else {
                    ui.label("拆分字段(最多3个):");
                    let header = self
                        .ops
                        .as_ref()
                        .map(|o| o.header().to_vec())
                        .unwrap_or_default();
                    let mut selected = self.dialogs.split_cols.clone();
                    egui::ScrollArea::vertical().max_height(120.0).show(ui, |ui| {
                        for (i, h) in header.iter().enumerate() {
                            let mut is_sel = selected.contains(&i);
                            ui.checkbox(&mut is_sel, h);
                            if is_sel && !selected.contains(&i) {
                                if selected.len() < 3 {
                                    selected.push(i);
                                }
                            } else if !is_sel {
                                selected.retain(|&x| x != i);
                            }
                        }
                    });
                    self.dialogs.split_cols = selected;
                }

                ui.label("输出格式:");
                let mut fmt = self.dialogs.split_format;
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut fmt, 0, "CSV");
                    ui.selectable_value(&mut fmt, 1, "XLSX");
                });
                self.dialogs.split_format = fmt;

                ui.add_space(6.0);
                if ui.button("执行拆分").clicked() {
                    self.run_split();
                }
            });
        *open = keep;
    }

    /// 文件信息弹出窗口（原左侧高级功能标签页内容）
    fn dialog_file_info(&mut self, ctx: &egui::Context, open: &mut bool) {
        let mut keep = *open;
        let mut done = false;
        apply_dialog_pos(
            egui::Window::new("文件信息")
                .open(&mut keep)
                .collapsible(false)
                .resizable(false),
            self.dialog_position,
            ctx,
        )
        .show(ctx, |ui| {
                if let Some(fi) = &self.file_info {
                    egui::Grid::new("file_info").num_columns(2).show(ui, |ui| {
                        ui.label("路径:");
                        ui.label(&fi.path);
                        ui.end_row();
                        ui.label("行数:");
                        ui.label(format!("{}", fi.rows));
                        ui.end_row();
                        ui.label("列数:");
                        ui.label(fi.cols.to_string());
                        ui.end_row();
                        ui.label("编码:");
                        ui.label(&fi.encoding);
                        ui.end_row();
                        ui.label("大小:");
                        ui.label(format!("{:.1} MB", fi.size_bytes as f64 / 1024.0 / 1024.0));
                        ui.end_row();
                    });
                } else {
                    ui.weak("未加载文件");
                }
                ui.add_space(6.0);
                if ui.button("关闭").clicked() {
                    done = true;
                }
            });
        if done {
            keep = false;
        }
        *open = keep;
    }

    fn dialog_export(&mut self, ctx: &egui::Context, open: &mut bool) {
        let mut keep = *open;
        let mut done = false;
        apply_dialog_pos(
            egui::Window::new("导出数据")
                .open(&mut keep)
                .collapsible(false)
                .resizable(false),
            self.dialog_position,
            ctx,
        )
        .show(ctx, |ui| {
                ui.label("导出范围: 当前筛选结果（未筛选则全部）");
                ui.label("导出格式:");
                let mut fmt = self.dialogs.split_format; // 复用
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut fmt, 0, "CSV");
                    ui.selectable_value(&mut fmt, 1, "XLSX");
                });
                self.dialogs.split_format = fmt;
                ui.add_space(4.0);
                if ui.button("选择保存位置并导出").clicked() {
                    let ext = if fmt == 0 { "csv" } else { "xlsx" };
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter(if fmt == 0 { "CSV" } else { "XLSX" }, &[ext])
                        .set_file_name(&format!("export.{}", ext))
                        .save_file()
                    {
                        let p = path.to_string_lossy().into_owned();
                        self.export_view(p, fmt == 0);
                        done = true;
                    }
                }
            });
        if done {
            keep = false;
        }
        *open = keep;
    }

    /// 数据行详情窗口（展示被双击单元格所在行的所有数据）
    fn dialog_row_detail(&mut self, ctx: &egui::Context, open: &mut bool) {
        let Some(row) = self.detail_row else {
            *open = false;
            return;
        };
        let mut keep = *open;
        apply_dialog_pos_sized(
            egui::Window::new("数据行详情")
                .open(&mut keep)
                .collapsible(false)
                .resizable(true)
                .default_size([460.0, 480.0]),
            self.dialog_position,
            ctx,
            egui::vec2(460.0, 480.0),
        )
        .show(ctx, |ui| {
            let (Some(model), Some(ops)) = (self.model.clone(), self.ops.clone()) else {
                ui.label("无数据");
                return;
            };
            let header = ops.header().to_vec();
            let visible = ops.visible_columns();
            ui.label(format!("第 {} 行", row + 1));
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::Grid::new("row_detail")
                    .num_columns(2)
                    .striped(true)
                    .show(ui, |ui| {
                        for &vc in &visible {
                            let name = header.get(vc).cloned().unwrap_or_default();
                            let val = ops.field(&model, row, vc);
                            ui.label(&name);
                            ui.label(val);
                            ui.end_row();
                        }
                    });
            });
        });
        *open = keep;
    }

    /// 多文件合并对话框：选择/追加文件 → 表头校验 → 合并为一个数据集
    fn dialog_merge_files(&mut self, ctx: &egui::Context, open: &mut bool) {
        let mut keep = *open;
        let mut closed = false;
        apply_dialog_pos_sized(
            egui::Window::new("合并多个CSV文件")
                .open(&mut keep)
                .collapsible(false)
                .resizable(true)
                .default_size([760.0, 470.0]),
            self.dialog_position,
            ctx,
            egui::vec2(760.0, 470.0),
        )
        .show(ctx, |ui| {
            ui.label("选择多个表头相同的 CSV 文件（可反复追加），校验表头后合并为一个数据集。");
            ui.weak("合并结果会作为当前数据打开；需要留存时用「💾 导出数据」另存到指定位置。");
            ui.separator();

            ui.horizontal(|ui| {
                if ui.button("➕ 选择文件（可多选）").clicked() {
                    self.pick_merge_files(false);
                }
                if ui.button("➕ 追加选择文件").clicked() {
                    self.pick_merge_files(true);
                }
                let has_files = !self.dialogs.merge_files.is_empty();
                if ui
                    .add_enabled(has_files, egui::Button::new("🗑 清空列表"))
                    .clicked()
                {
                    self.set_merge_files(Vec::new());
                }
                if self.dialogs.merge_scanning {
                    ui.spinner();
                    ui.weak("正在读取表头...");
                }
            });
            ui.add_space(4.0);

            // 待合并文件列表
            let files = self.dialogs.merge_files.clone();
            let headers = self.dialogs.merge_headers.clone();
            let mut remove_idx: Option<usize> = None;
            ui.group(|ui| {
                // 固定最小宽度在窄屏上会把窗口撑出屏幕 → 不超过屏幕可用宽度
                ui.set_min_width(700.0_f32.min(ui.ctx().screen_rect().width() - 80.0));
                egui::ScrollArea::vertical()
                    .max_height(170.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if files.is_empty() {
                            ui.weak("（尚未选择文件，请用上方按钮选择）");
                        }
                        for (i, f) in files.iter().enumerate() {
                            ui.horizontal(|ui| {
                                let short = std::path::Path::new(f)
                                    .file_name()
                                    .map(|s| s.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| f.clone());
                                let text = match headers.get(i) {
                                    Some(h) => {
                                        format!("{}. {}（{} 列）", i + 1, short, h.header.len())
                                    }
                                    None => format!("{}. {}", i + 1, short),
                                };
                                ui.label(text).on_hover_text(f.as_str());
                                if ui.small_button("移除").clicked() {
                                    remove_idx = Some(i);
                                }
                            });
                        }
                    });
            });
            if let Some(i) = remove_idx {
                let mut remaining = files.clone();
                remaining.remove(i);
                self.set_merge_files(remaining);
            }

            ui.add_space(4.0);
            // 表头校验结果（读取数据之前完成）
            let hs: Vec<Vec<String>> = headers.iter().map(|h| h.header.clone()).collect();
            match self.dialogs.merge_check {
                Some(HeaderCheck::Identical) => {
                    ui.colored_label(
                        egui::Color32::from_rgb(0, 120, 0),
                        format!(
                            "✓ 表头校验通过：{} 个文件表头完全一致（{} 列），可直接合并",
                            files.len(),
                            hs.first().map(|h| h.len()).unwrap_or(0)
                        ),
                    );
                }
                Some(HeaderCheck::Reordered) => {
                    ui.colored_label(
                        egui::Color32::from_rgb(0, 120, 0),
                        "✓ 表头校验通过：字段完全相同（仅列顺序不同），将按字段名对齐后合并",
                    );
                }
                Some(HeaderCheck::Differ) => {
                    let common = merge::common_fields(&hs);
                    ui.colored_label(egui::Color32::from_rgb(190, 60, 60), "⚠ 表头不一致");
                    if !self.dialogs.merge_diff.is_empty() {
                        ui.label(format!(
                            "差异字段（并非所有文件都有）：{}",
                            self.dialogs.merge_diff.join("、")
                        ));
                    }
                    if ui
                        .checkbox(
                            &mut self.dialogs.merge_keep_common,
                            format!("只保留相同字段名的字段进行合并（共有 {} 列）", common.len()),
                        )
                        .changed()
                    {
                        // 选项变化只是重算显示，无需重新读表头
                    }
                    if common.is_empty() {
                        ui.colored_label(
                            egui::Color32::from_rgb(190, 60, 60),
                            "所选文件没有任何共同字段，无法合并",
                        );
                    }
                }
                None => {
                    if !files.is_empty() && !self.dialogs.merge_scanning {
                        ui.weak("（表头校验未完成）");
                    }
                }
            }

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let differ_blocked = matches!(self.dialogs.merge_check, Some(HeaderCheck::Differ))
                    && !self.dialogs.merge_keep_common;
                let can_merge = !files.is_empty() && !self.dialogs.merge_scanning && !differ_blocked;
                if ui
                    .add_enabled(can_merge, egui::Button::new("开始合并"))
                    .clicked()
                {
                    self.run_merge();
                }
                if ui.button("取消").clicked() {
                    closed = true;
                }
                if differ_blocked {
                    ui.colored_label(
                        egui::Color32::from_rgb(190, 60, 60),
                        "表头不一致：请勾选「只保留相同字段名」，或移除不一致的文件",
                    );
                }
            });
        });
        if closed {
            keep = false;
        }
        *open = keep;
    }

    /// 使用说明对话框：面向首次使用者，按使用顺序分节讲解
    fn dialog_help_guide(&mut self, ctx: &egui::Context, open: &mut bool) {
        let mut keep = *open;
        let mut closed = false;
        // 正文滚动区高度按屏幕计算：窗口高度随内容增长，若不限制，
        // 小屏（或界面放大 1.5 倍）时窗口会顶到屏幕外，底部按钮点不到。
        let help_scroll_h = (ctx.screen_rect().height() - 300.0).clamp(160.0, 520.0);
        apply_dialog_pos_sized(
            egui::Window::new("使用说明")
                .open(&mut keep)
                .collapsible(false)
                .resizable(true)
                .default_size([800.0, 580.0]),
            self.dialog_position,
            ctx,
            egui::vec2(800.0, 580.0),
        )
        .show(ctx, |ui| {
            ui.label(egui::RichText::new("宿经03超大CSV文件实用编辑器 · 使用说明").heading());
            ui.weak("专为超大 CSV 设计（实测 6.5GB / 901 万行流畅操作）。下面按使用顺序讲解，跟着做一遍就能上手。");
            ui.separator();
            egui::ScrollArea::vertical()
                .max_height(help_scroll_h)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.collapsing("一、三步上手", |ui| {
                        ui.label("第 1 步：点工具栏「📂 选择CSV文件」选择一个 CSV（或 .txt）文件；");
                        ui.label("第 2 步：打开后中间区域是数据预览，工具栏下方显示总行数/筛选后行数/列数/内存；");
                        ui.label("第 3 步：需要保存时点「💾 导出数据」，选择 CSV 或 XLSX 格式与保存位置。");
                        ui.weak("读取在后台线程进行，会弹出进度窗口；6.5GB 文件约 4 秒打开，内存占用约 150MB。");
                    });
                    ui.collapsing("二、打开文件：单个文件 / 多个文件合并", |ui| {
                        ui.label("• 单个文件：工具栏「📂 选择CSV文件」，或菜单「文件 → 打开CSV文件」。");
                        ui.label("• 多个同表头文件合并：工具栏「📚 选择多个CSV文件（合并）」，或菜单「文件 → 打开多个CSV文件（合并）」。");
                        ui.label("    1) 弹窗中可一次性多选，也可反复点「追加选择文件」逐个添加；");
                        ui.label("    2) 软件会先读取各文件表头并校验一致性（此时尚未读取数据行）；");
                        ui.label("    3) 表头不一致时会列出差异字段，勾选「只保留相同字段名的字段」即按共同字段合并；");
                        ui.label("    4) 合并完成后结果自动作为当前数据打开，可用「💾 导出数据」另存。");
                        ui.weak("编码自动识别：BOM → 严格 UTF-8 → UTF-16 → GB18030，ANSI/GBK 中文文件与 UTF-16 都能正确打开。");
                    });
                    ui.collapsing("三、数据预览", |ui| {
                        ui.label("• 表格只渲染屏幕内的行（虚拟化），900 万行滚动依然流畅；");
                        ui.label("• 双击任意单元格，会弹出该行的「数据行详情」，逐列查看完整内容；");
                        ui.label("• 顶部「刷新显示」可把视图滚回第一行。");
                    });
                    ui.collapsing("四、数据筛选", |ui| {
                        ui.label("• 点工具栏「🔍 数据筛选」打开筛选窗口；");
                        ui.label("• 每个条件选：字段 + 条件 + 值，条件共 10 种：等于/包含/不包含/大于/小于/大于等于/小于等于/不等于/空值/非空值；");
                        ui.label("• 多个条件之间可选「且 / 或」组合，点「应用筛选」执行（超大文件为并行计算，秒级完成）；");
                        ui.label("• 数值比较用高精度整数实现，19 位银行卡号不会因精度丢失而误判；");
                        ui.label("• 点「清除筛选」恢复显示全部数据；工具栏的「🧹 清除筛选」按钮作用相同，不用打开窗口。");
                    });
                    ui.collapsing("五、数据拆分", |ui| {
                        ui.label("• 菜单「数据处理 → 拆分数据…」或筛选后执行；");
                        ui.label("• 按行数拆分：指定每批行数，生成 part_1、part_2…；");
                        ui.label("• 按字段拆分：勾选 1~3 个字段，按字段取值分组，每组一个文件；");
                        ui.label("• 输出格式可选 CSV 或 XLSX，保存文件夹自选；");
                        ui.weak("按字段拆分对唯一值过多的字段（>20000）会拒绝，避免产生海量小文件；当前筛选结果会作为拆分范围。");
                    });
                    ui.collapsing("六、列操作（支持撤销/恢复）", |ui| {
                        ui.label("菜单「数据处理 → 列操作」下：拆分字段、合并字段、生成行哈希（md5/sha1/sha256/sha512）、复制列、插入列、删除列、隐藏列、显示列；");
                        ui.label("拆分字段的新列命名可选「自动命名（前缀 + 序号，如 ts_1、ts_2）」或「逐个自定义命名」（每个新列单独取名；留空回退为前缀 + 序号；与已有列重名时自动追加 _1、_2…，不会覆盖原列）；");
                        ui.label("所有这些操作只记录“操作描述”，不会复制整表数据，对超大文件也是瞬时完成；");
                        ui.label("菜单「编辑 → 撤销 / 恢复」（不可用时为灰色）可逐步回退或重做。");
                    });
                    ui.collapsing("七、导出数据", |ui| {
                        ui.label("• 点「💾 导出数据」，选择 CSV 或 XLSX 与保存位置；导出范围是当前筛选结果（未筛选则全部）；");
                        ui.label("• XLSX 为全文本导出：卡号、账号、长数字、前导零都不会变成科学计数法或丢精度（代价是在 Excel 中不参与自动求和）；");
                        ui.label("• 超过 Excel 单表 1,048,576 行会自动分多个工作表；");
                        ui.label("• CSV 输出带 BOM（utf-8-sig），用 Excel 打开中文不乱码。");
                    });
                    ui.collapsing("八、界面缩放（按钮大小）", |ui| {
                        ui.label("• 工具栏「日志」右侧有「按钮大小」输入框，范围 1.0 ~ 1.5 倍，默认 1.0；可直接输入数值，也可按住拖动（拖动时松开鼠标后界面才缩放，避免抖动）；");
                        ui.label("• 输入或拖动数值即可把菜单、菜单命令、工具栏按钮、表格文字整体等比放大，方便视力不佳的用户；");
                        ui.weak("设置会被记住，下次启动仍然生效。");
                    });
                    ui.collapsing("九、数据分析（去重 / 分组汇总 / 列统计 / 频次分布 / 重复情况 / 数据质量 / 透视表 / 交叉统计）", |ui| {
                        ui.label("八条命令都在「数据分析」工具栏与菜单「数据分析」下；作用范围都是当前筛选结果（未筛选则为全部数据）。");
                        ui.label("• 提取唯一行（去重）：勾选判重字段（一个都不勾 = 按整行判重），执行后保留唯一行并作为当前视图，可继续导出、拆分、统计；完成后可导出「重复行对应表」。");
                        ui.label("• 字段分组汇总：选 1~3 个分组字段，再为若干列添加汇总方式（求和/均值/最大/最小/计数/去重计数/空值数），点「开始汇总」；结果是每组一行的汇总表（含「行数」）。");
                        ui.label("• 数值列统计：勾选要统计的列（可多选），点「开始统计」；给出每列的行数、计数、空值数、非数值数、求和、均值、中位数、最小、最大、标准差（总体）。");
                        ui.label("• 频次分布 / Top-N：勾选列并设置「每列显示前 N 项」，点「开始统计」；列出各取值的出现次数与占比（按次数从多到少），空值作为「(空)」一类参与排名，超出前 N 项的汇总为一行「(其余 M 个取值)」，末行「(合计)」用于核对总量（占比 100%）。常用法：找出卡号/账号出现次数最多的前 20 条。");
                        ui.label("• 重复情况统计：选判重字段（不勾 = 按整行判重），点「开始统计」；给出不同键/重复键个数与重复行占比，明细按重复次数降序列出前 N 个重复键。结果窗里可一键「只看重复的行」或「只看唯一行」，把结果直接落到当前数据视图（用工具栏「清除筛选」可恢复全部）。");
                        ui.label("• 空值与数据质量统计：勾选列（想知道整份数据质量就点「全选」），点「开始检查」；逐列给出空值数、纯空白数、空值率、含控制字符数、最短/最长/平均长度、超长值数（>255 字符）。");
                        ui.label("• 透视表：选 1~2 个行字段 + 1 个列字段，再添加若干项「指标」（值列 + 聚合方式，可多项，如金额求和+均值+笔数计数），点「生成透视表」；得到交叉表，行末与末行给出「(合计)」（整体聚合，均值/计数在合计处也正确）。窗口底部「规模上限」三项（行组合数 / 列字段取值 / 单元格数）可按电脑内存调整并会记住，界面会给出粗估内存。");
                        ui.label("• 交叉/关联统计：选 A、B 两个字段，点「开始统计」；给出关系判定（一对一 / 一对多 / 多对一 / 多对多）、两侧最大对应数，以及每个 A 值对应的 B 值个数与示例（如「同一卡号对应多少个账号」）。");
                        ui.label("    金额等按数值精确计算（定点十进制累加，两位小数的合计不会出现 3613134567.580001 这类多余尾数）；某列若混有文字会被跳过，并在结果窗提示「N 个非数值未参与计算」；");
                        ui.label("    中位数需要保留该列的原始数值：数据量特别大时（超过用内存上限）该列中位数显示「—」，并提示先用筛选缩小范围；");
                        ui.label("    结果窗可「最小化」成一条标题栏（结果不丢），稍后点标题栏左侧三角展开即可继续查看或导出；");
                        ui.weak("除「重复情况统计」的一键筛选外，统计本身不改变当前数据视图；只能选原始列统计（拆分/合并/复制/哈希产生的新列不参与）；分组数或不同取值超过上限时会提示改用取值较少的字段或先筛选缩小范围。");
                    });
                    ui.collapsing("十、连接数据库（只读）", |ui| {
                        ui.label("• 菜单「数据库」下可连接各主要数据库：MySQL / MariaDB、PostgreSQL、SQL Server、Oracle，以及「连接其它数据库（ODBC / DSN）」通用入口；");
                        ui.label("• 在「连接数据库（只读）」窗口选择数据库类型（自动填入默认端口与已安装驱动），填写主机 / 端口 / 数据库名 / 用户名 / 密码；驱动名特殊的库（Oracle、达梦等）可勾选「手动编辑连接串」直接写 ODBC 连接串；");
                        ui.label("• 「测试连接」只验证能否连通；「连接并浏览」成功后打开「数据库表（只读）」窗口：选库/模式 → 选表/视图 → 勾选字段 → 设置读取行数上限（默认 10 万，填 0 = 全部）→「载入到编辑器」；");
                        ui.label("• 载入后的数据就是当前数据，可继续使用筛选、拆分、列操作、各类统计与导出等全部功能；");
                        ui.label("• 「数据库表」窗口分两段：上段左侧是表/视图列表、右侧是字段列表（都按内容自适应高度，不会铺到底部），下段是通栏的数据预览（横跨窗口左右两侧，列多时可拖动横向滚动条查看；载入编辑器后不受限制）；");
                        ui.label("• 取数是批量进行的（一次读取多行），百万行级表约 10 秒、千万行级约 1~2 分钟；读取行数上限越小越快，可用「上限」先试读；");
                        ui.label("• 进度窗口会显示「已读取 N / 约 M 行（百分比）」（M 由数据库统计信息快速估算）；取不到估算时显示「已读取 N 行」+ 动画进度条；");
                        ui.label("• 读出的数据先放在系统临时目录，软件在启动/退出/每次读取前会自动清理这些临时文件，也可用「设置 → 清理数据库临时文件」手动清理；");
                        ui.label("• 窗口会按屏幕自适应、不会超出屏幕；关闭可点标题栏 ✕ 或按 Esc 键；");
                        ui.label("• 「保存此连接」把配置存到 exe 同目录的「数据库连接配置.ini」，之后可从「数据库 → 已保存的连接」一键连接（只有勾选「同时保存密码」才会保存密码，且为明文）；");
                        ui.weak("只读保证（两道保险）：连接以只读模式建立、软件只执行查询（SELECT 与库表元数据读取），且不提供 SQL 编辑框；MySQL / PostgreSQL 还会把会话设为只读，由数据库自己拒绝写操作。导出/拆分只写本机文件。");
                        ui.weak("前置条件：需在本机安装目标数据库的 64 位 ODBC 驱动（MySQL Connector/ODBC、PostgreSQL psqlODBC、ODBC Driver 17/18 for SQL Server、Oracle ODBC Driver 等）；SQL Server / Access / Excel 驱动为 Windows 自带。");
                    });
                    ui.collapsing("十一、常见问题", |ui| {
                        ui.label("• 连接数据库要装什么？ 需要目标数据库的 64 位 ODBC 驱动；连接窗口会列出本机已安装的驱动名。");
                        ui.label("• 打开很慢或卡住？ 读文件在后台进行，请看进度窗口与右下「操作日志」面板；");
                        ui.label("• 中文显示乱码？ 编码由软件自动识别；若结果异常，请在日志中确认识别到的编码；");
                        ui.label("• 中文输入只出现第一个字？ 该问题已在当前版本修复（若仍出现请反馈输入法名称）；");
                        ui.label("• 拆分产生文件太多？ 改用「按行数拆分」，或先筛选缩小数据范围；");
                        ui.label("• 程序启动无反应？ 请查看 exe 同目录的 csv_editor_rust.log（记录启动诊断信息）。");
                    });
                    ui.collapsing("十二、支持开发者", |ui| {
                        ui.label("工具栏「❤ 自愿捐助」按钮中有微信收款码，感谢您的支持。");
                    });
                });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("知道了").clicked() {
                    closed = true;
                }
            });
        });
        if closed {
            keep = false;
        }
        *open = keep;
    }

    /// 「提取唯一行（去重）」对话框：选择判重字段后在当前筛选结果上判重
    fn dialog_dedup(&mut self, ctx: &egui::Context, open: &mut bool) {
        let mut keep = *open;
        let mut done = false;
        apply_dialog_pos(
            egui::Window::new("提取唯一行（去重）")
                .open(&mut keep)
                .collapsible(false)
                .resizable(false),
            self.dialog_position,
            ctx,
        )
        .show(ctx, |ui| {
            let header = self.header_names();
            if header.is_empty() {
                ui.label("请先加载数据");
                return;
            }
            let scope = match self.visible_rows() {
                Some(v) => format!("当前筛选结果（{} 行）", v.len()),
                None => format!(
                    "全部数据（{} 行）",
                    self.model.as_ref().map(|m| m.row_count()).unwrap_or(0)
                ),
            };
            ui.label(format!("判重范围：{scope}"));
            ui.weak(
                "按所选字段的值组合判断重复行，保留每条重复记录中首次出现的行。\
                 结果会作为新的当前数据视图，可继续导出、拆分或统计。",
            );
            ui.separator();
            ui.label("判重字段（不勾选 = 按全部字段判重）:");
            // 丢弃超出当前列数的陈旧索引（列数可能因之前的增删而变化）
            self.dialogs.dedup_cols.retain(|&i| i < header.len());
            let mut sel = self.dialogs.dedup_cols.clone();
            ui.horizontal(|ui| {
                if ui.small_button("全选").clicked() {
                    sel = (0..header.len()).collect();
                }
                if ui.small_button("全不选").clicked() {
                    sel.clear();
                }
                ui.weak(format!("已选 {} / {} 列", sel.len(), header.len()));
            });
            egui::ScrollArea::vertical()
                .max_height(180.0)
                .show(ui, |ui| {
                    for (i, h) in header.iter().enumerate() {
                        let mut on = sel.contains(&i);
                        ui.checkbox(&mut on, h);
                        if on && !sel.contains(&i) {
                            sel.push(i);
                        } else if !on {
                            sel.retain(|&x| x != i);
                        }
                    }
                });
            self.dialogs.dedup_cols = sel.clone();

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("提取唯一行").clicked() {
                    self.run_dedup();
                    done = true;
                }
                if ui.button("取消").clicked() {
                    done = true;
                }
                ui.weak(if sel.is_empty() {
                    "将按全部字段判重"
                } else {
                    "将按所选字段判重"
                });
            });
        });
        if done {
            keep = false;
        }
        *open = keep;
    }

    /// 去重完成后的结果提示窗：告知统计结果，并询问是否导出「重复行对应表」
    fn dialog_dedup_done(&mut self, ctx: &egui::Context, open: &mut bool) {
        let mut keep = *open;
        let mut done = false;
        apply_dialog_pos(
            egui::Window::new("提取唯一行完成")
                .open(&mut keep)
                .collapsible(false)
                .resizable(false),
            self.dialog_position,
            ctx,
        )
        .show(ctx, |ui| {
            let Some((dup_count, input_rows, unique_rows, key_count)) = self
                .last_dedup
                .as_ref()
                .map(|d| (d.map.len(), d.input_rows, d.unique_rows, d.key_columns.len()))
            else {
                ui.label("没有可显示的去重结果");
                if ui.button("关闭").clicked() {
                    done = true;
                }
                return;
            };

            let removed = input_rows.saturating_sub(unique_rows);
            ui.label(format!(
                "判重范围 {} 行 → 唯一 {} 行，剔除重复 {} 行（按 {} 个字段判重）",
                input_rows, unique_rows, removed, key_count
            ));
            ui.weak("去重结果已成为当前数据视图：之后的查看、导出、拆分都基于唯一行。");
            ui.separator();
            if dup_count == 0 {
                ui.label("本次没有发现重复行，无需导出对应表。");
                ui.add_space(4.0);
                if ui.button("关闭").clicked() {
                    done = true;
                }
            } else {
                ui.label(format!("是否导出「重复行对应表」？（共 {dup_count} 条对应关系）"));
                ui.weak(
                    "表中逐条记录：保留的首次出现行号 ↔ 被剔除的重复行号，并附判重字段的取值，\
                     便于你细致核对每条去重是否正确；行号从 1 开始（与表格显示一致）。",
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("导出为 CSV…").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("CSV", &["csv"])
                            .set_file_name("重复行对应表.csv")
                            .save_file()
                        {
                            self.export_dup_map(path.to_string_lossy().into_owned(), false);
                        }
                        done = true;
                    }
                    if ui.button("导出为 XLSX…").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("XLSX", &["xlsx"])
                            .set_file_name("重复行对应表.xlsx")
                            .save_file()
                        {
                            self.export_dup_map(path.to_string_lossy().into_owned(), true);
                        }
                        done = true;
                    }
                    if ui.button("暂不导出").clicked() {
                        done = true;
                    }
                });
            }
        });
        if done {
            keep = false;
        }
        *open = keep;
    }

    /// 导出「重复行对应表」（后台线程）
    pub fn export_dup_map(&mut self, path: String, is_xlsx: bool) {
        if self.busy.is_some() {
            self.log("已有任务在执行，请稍候");
            return;
        }
        let Some(model) = self.model.clone() else {
            self.log("请先加载数据");
            return;
        };
        let Some(d) = self.last_dedup.as_ref() else {
            self.log("没有可导出的重复行信息");
            return;
        };
        let map = d.map.clone();
        let key_cols = d.key_columns.clone();
        let tx = self.tx.clone();
        self.worker = Some(super::background::Worker::spawn(tx.clone(), move |ctl| {
            let result = export::export_dup_map(
                &model,
                &map,
                &key_cols,
                &path,
                is_xlsx,
                Some(&|p| ctl.report_progress(p as f32, "正在导出重复行对应表...")),
            );
            match result {
                Ok(n) => {
                    let _ = tx.send(BgMsg::ExportDone(n, path));
                }
                Err(e) => {
                    let _ = tx.send(BgMsg::Error(format!("导出重复行对应表失败: {}", e)));
                }
            }
        }));
        self.set_busy("导出重复行对应表", "正在导出...", 0.0);
    }

    /// 「字段分组汇总」参数对话框：选 1~3 个分组字段，并为若干列选择聚合方式。
    /// 派生列（拆分/合并/复制/哈希等新列）不作为统计字段：其值不是文件里的原始数据，
    /// 且统计走的是 `RowReader` 的零拷贝原列读取。
    fn dialog_stats(&mut self, ctx: &egui::Context, open: &mut bool) {
        ensure_stats_limits(self, ctx);
        let mut keep = *open;
        let mut done = false;
        apply_dialog_pos_sized(
            egui::Window::new("字段分组汇总")
                .open(&mut keep)
                .collapsible(false)
                .resizable(true)
                .default_size([680.0, 540.0]),
            self.dialog_position,
            ctx,
            egui::vec2(680.0, 540.0),
        )
        .show(ctx, |ui| {
            let header = self.header_names();
            if header.is_empty() {
                ui.label("请先加载数据");
                return;
            }
            // 可作为统计字段的视图列（派生列 = false）
            let base_ok: Vec<bool> = (0..header.len())
                .map(|i| self.ops.as_ref().and_then(|o| o.view_to_base(i)).is_some())
                .collect();
            let scope = match self.visible_rows() {
                Some(v) => format!("当前筛选结果（{} 行）", v.len()),
                None => format!(
                    "全部数据（{} 行）",
                    self.model.as_ref().map(|m| m.row_count()).unwrap_or(0)
                ),
            };
            ui.label(format!("统计范围：{scope}"));
            ui.weak(
                "按 1~3 个字段分组，对选定的列做聚合；每组都给出「行数」。\
                 整数用 i128 精确求和（大额金额/长数字不丢精度）；结果可查看并导出 CSV / XLSX。\
                 派生列不参与统计。",
            );
            ui.separator();

            // —— 分组字段（固定 3 个槽位，便于增删改）——
            ui.label("分组字段（1~3 个）:");
            let cur = self.dialogs.stats_group_cols.clone();
            let mut slots: Vec<Option<usize>> = Vec::with_capacity(stats::MAX_GROUP_COLUMNS);
            for i in 0..stats::MAX_GROUP_COLUMNS {
                // 丢弃越界/陈旧/派生列索引（列数可能因之前的增删而变化）
                slots.push(
                    cur.get(i)
                        .copied()
                        .filter(|&c| c < header.len() && base_ok[c]),
                );
            }
            for (slot_i, slot) in slots.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(format!("分组 {}", slot_i + 1));
                    let text = match *slot {
                        Some(c) => header[c].clone(),
                        None => "（不使用）".to_string(),
                    };
                    egui::ComboBox::from_id_source(("stats_gcol", slot_i))
                        .selected_text(text)
                        .width(220.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(slot, None, "（不使用）");
                            for (i, h) in header.iter().enumerate() {
                                if !base_ok[i] {
                                    continue; // 派生列不列出
                                }
                                ui.selectable_value(slot, Some(i), h);
                            }
                        });
                });
            }
            // 回写：滤掉「不使用」与重复选中的列，保持槽位顺序（第一个槽位 = 主分组字段）
            let mut picked: Vec<usize> = Vec::new();
            for s in slots.iter().flatten() {
                if !picked.contains(s) {
                    picked.push(*s);
                }
            }
            self.dialogs.stats_group_cols = picked;

            ui.separator();
            // —— 聚合列（一列 × 一种聚合 = 一个条目）——
            ui.label("聚合列（同一列可添加多个聚合方式，如同时求和与去重计数）:");
            let mut aggs = std::mem::take(&mut self.dialogs.stats_aggs);
            // 丢弃越界/陈旧/派生列的条目（列数可能已变化）
            aggs.retain(|&(c, _)| c < header.len() && base_ok[c]);
            let mut remove: Option<usize> = None;
            egui::ScrollArea::vertical()
                .max_height(180.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    if aggs.is_empty() {
                        ui.weak("（暂无聚合列：请在下方选择列与聚合方式，再点「+ 添加」）");
                    }
                    for i in 0..aggs.len() {
                        ui.horizontal(|ui| {
                            ui.label(header[aggs[i].0].clone());
                            let mut kind = aggs[i].1;
                            egui::ComboBox::from_id_source(("stats_agg_kind", i))
                                .selected_text(kind.label())
                                .width(96.0)
                                .show_ui(ui, |ui| {
                                    for k in AggKind::ALL {
                                        ui.selectable_value(&mut kind, k, k.label());
                                    }
                                });
                            aggs[i].1 = kind;
                            if ui.button("×").on_hover_text("删除此聚合").clicked() {
                                remove = Some(i);
                            }
                        });
                        ui.separator();
                    }
                });
            if let Some(i) = remove {
                aggs.remove(i);
            }
            self.dialogs.stats_aggs = aggs;

            // 新增一个聚合条目
            ui.horizontal(|ui| {
                let first_ok = (0..header.len()).find(|&i| base_ok[i]);
                let mut col = self.dialogs.stats_new_col;
                if col >= header.len() || !base_ok[col] {
                    col = first_ok.unwrap_or(0);
                }
                egui::ComboBox::from_id_source("stats_new_col")
                    .selected_text(header.get(col).cloned().unwrap_or_default())
                    .width(200.0)
                    .show_ui(ui, |ui| {
                        for (i, h) in header.iter().enumerate() {
                            if !base_ok[i] {
                                continue;
                            }
                            ui.selectable_value(&mut col, i, h);
                        }
                    });
                self.dialogs.stats_new_col = col;
                let mut k = self.dialogs.stats_new_kind.min(AggKind::ALL.len() - 1);
                egui::ComboBox::from_id_source("stats_new_kind")
                    .selected_text(AggKind::ALL[k].label())
                    .width(96.0)
                    .show_ui(ui, |ui| {
                        for (i, kind) in AggKind::ALL.iter().enumerate() {
                            ui.selectable_value(&mut k, i, kind.label());
                        }
                    });
                self.dialogs.stats_new_kind = k;
                if ui.button("+ 添加").clicked() {
                    self.dialogs.stats_aggs.push((col, AggKind::ALL[k]));
                }
            });

            limits_header(
                ui,
                "分组数超过上限时会报错；内存富余就把它调大，或先筛选缩小范围",
            );
            let mut limits_changed = false;
            limits_changed |= limits_row(
                ui,
                "分组数上限",
                &mut self.dialogs.stats_limits.max_groups,
                100..=5_000_000,
                500.0,
                "分组汇总最多允许多少个分组（默认 20000）；也用作交叉统计的取值上限",
            );
            if limits_changed {
                save_stats_limits(ctx, self.dialogs.stats_limits);
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("开始汇总").clicked() {
                    self.run_stats();
                    done = true;
                }
                if ui.button("取消").clicked() {
                    done = true;
                }
                ui.weak(format!(
                    "已选 {} 个分组字段 / {} 个聚合",
                    self.dialogs.stats_group_cols.len(),
                    self.dialogs.stats_aggs.len()
                ));
            });
        });
        if done {
            keep = false;
        }
        *open = keep;
    }

    /// 「数值列统计」参数对话框：勾选要统计的原始列。
    /// 与分组汇总参数窗一致，只列出原始列（派生列的值不是文件原始数据）。
    fn dialog_column_stats(&mut self, ctx: &egui::Context, open: &mut bool) {
        ensure_stats_limits(self, ctx);
        let mut keep = *open;
        let mut done = false;
        apply_dialog_pos_sized(
            egui::Window::new("数值列统计")
                .open(&mut keep)
                .collapsible(false)
                .resizable(true)
                .default_size([600.0, 520.0]),
            self.dialog_position,
            ctx,
            egui::vec2(600.0, 520.0),
        )
        .show(ctx, |ui| {
            let header = self.header_names();
            if header.is_empty() {
                ui.label("请先加载数据");
                return;
            }
            // 可作为统计字段的视图列（派生列 = false）
            let base_ok: Vec<bool> = (0..header.len())
                .map(|i| self.ops.as_ref().and_then(|o| o.view_to_base(i)).is_some())
                .collect();
            let scope = match self.visible_rows() {
                Some(v) => format!("当前筛选结果（{} 行）", v.len()),
                None => format!(
                    "全部数据（{} 行）",
                    self.model.as_ref().map(|m| m.row_count()).unwrap_or(0)
                ),
            };
            ui.label(format!("统计范围：{scope}"));
            ui.weak(
                "对勾选的列计算：计数（非空）、空值数、非数值数、求和、均值、中位数、最小、最大、\
                 标准差（总体，分母 n）。数值判定与筛选一致（整数 i128 精确、金额定点十进制精确累加）；\
                 结果可查看并导出 CSV / XLSX。派生列不参与统计。",
            );
            ui.separator();

            ui.label("选择要统计的列:");
            // 丢弃越界/陈旧/派生列索引（列数可能因之前的增删而变化）
            self.dialogs
                .col_stats_cols
                .retain(|&i| i < header.len() && base_ok[i]);
            let mut sel = self.dialogs.col_stats_cols.clone();
            let selectable = base_ok.iter().filter(|b| **b).count();
            ui.horizontal(|ui| {
                if ui.small_button("全选").clicked() {
                    sel = (0..header.len()).filter(|&i| base_ok[i]).collect();
                }
                if ui.small_button("全不选").clicked() {
                    sel.clear();
                }
                ui.weak(format!("已选 {} / {} 列（可统计列）", sel.len(), selectable));
            });
            egui::ScrollArea::vertical()
                .max_height(240.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for (i, h) in header.iter().enumerate() {
                        if !base_ok[i] {
                            continue; // 派生列不列出
                        }
                        let mut on = sel.contains(&i);
                        ui.checkbox(&mut on, h);
                        if on && !sel.contains(&i) {
                            sel.push(i);
                        } else if !on {
                            sel.retain(|&x| x != i);
                        }
                    }
                });
            self.dialogs.col_stats_cols = sel.clone();

            limits_header(
                ui,
                "中位数需要保留该列的原始数值：预算越大，能精确算中位数的数据量越大（约 8 字节/个）",
            );
            let mut limits_changed = false;
            limits_changed |= limits_row(
                ui,
                "中位数数值保留总量",
                &mut self.dialogs.stats_limits.median_budget,
                1_000_000..=200_000_000,
                1_000_000.0,
                "所有统计列共享、按列数均分（默认 2000 万 ≈ 160MB）；超出时该列中位数显示「—」",
            );
            if limits_changed {
                save_stats_limits(ctx, self.dialogs.stats_limits);
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("开始统计").clicked() {
                    self.run_column_stats();
                    done = true;
                }
                if ui.button("取消").clicked() {
                    done = true;
                }
                ui.weak(format!("已选 {} 列", sel.len()));
            });
        });
        if done {
            keep = false;
        }
        *open = keep;
    }

    /// 「透视表」参数对话框：行字段（1~2）+ 列字段 + 多项指标（值列 × 聚合方式）
    /// + 规模上限（行组合/列字段取值/单元格，可按硬件情况调大）。
    fn dialog_pivot_table(&mut self, ctx: &egui::Context, open: &mut bool) {
        let mut keep = *open;
        let mut done = false;
        apply_dialog_pos_sized(
            egui::Window::new("透视表")
                .open(&mut keep)
                .collapsible(false)
                .resizable(true)
                .default_size([680.0, 660.0]),
            self.dialog_position,
            ctx,
            egui::vec2(680.0, 660.0),
        )
        .show(ctx, |ui| {
            let header = self.header_names();
            if header.is_empty() {
                ui.label("请先加载数据");
                return;
            }
            // 可作为统计字段的视图列（派生列 = false）
            let base_ok: Vec<bool> = (0..header.len())
                .map(|i| self.ops.as_ref().and_then(|o| o.view_to_base(i)).is_some())
                .collect();
            let raw: Vec<usize> = (0..header.len()).filter(|&i| base_ok[i]).collect();
            if raw.is_empty() {
                ui.label("没有可用于统计的原始列");
                return;
            }
            let scope = match self.visible_rows() {
                Some(v) => format!("当前筛选结果（{} 行）", v.len()),
                None => format!(
                    "全部数据（{} 行）",
                    self.model.as_ref().map(|m| m.row_count()).unwrap_or(0)
                ),
            };
            ui.label(format!("统计范围：{scope}"));
            ui.weak(
                "行字段 × 列字段 × 指标（值列 + 聚合方式）的交叉表：行是行字段的取值组合，\
                 列是列字段的各个取值，格子是指标在该组合下的聚合结果；可一次放多项指标\
                 （如「金额求和 + 金额均值 + 笔数计数」）。每行末尾与末行是「(合计)」\
                 （整体聚合，均值/计数在合计处也正确）。派生列不参与统计。",
            );
            ui.separator();

            // —— 行字段（2 个槽位）——
            ui.label("行字段（1~2 个）:");
            self.dialogs
                .pivot_row_cols
                .retain(|&i| i < header.len() && base_ok[i]);
            let cur = self.dialogs.pivot_row_cols.clone();
            let mut slots: Vec<Option<usize>> = (0..MAX_PIVOT_ROW_SLOTS)
                .map(|i| cur.get(i).copied().filter(|&c| c < header.len()))
                .collect();
            for (slot_i, slot) in slots.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(format!("行 {}", slot_i + 1));
                    let text = match *slot {
                        Some(c) => header[c].clone(),
                        None => "（不使用）".to_string(),
                    };
                    egui::ComboBox::from_id_source(("pivot_row", slot_i))
                        .selected_text(text)
                        .width(230.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(slot, None, "（不使用）");
                            for &i in &raw {
                                ui.selectable_value(slot, Some(i), &header[i]);
                            }
                        });
                });
            }
            let mut picked: Vec<usize> = Vec::new();
            for s in slots.iter().flatten() {
                if !picked.contains(s) {
                    picked.push(*s);
                }
            }
            self.dialogs.pivot_row_cols = picked;

            // —— 列字段 ——
            ui.horizontal(|ui| {
                ui.label("列字段");
                let mut col = self.dialogs.pivot_col.min(header.len().saturating_sub(1));
                if !base_ok.get(col).copied().unwrap_or(false) {
                    col = raw[0];
                }
                egui::ComboBox::from_id_source("pivot_col")
                    .selected_text(header[col].clone())
                    .width(230.0)
                    .show_ui(ui, |ui| {
                        for &i in &raw {
                            ui.selectable_value(&mut col, i, &header[i]);
                        }
                    });
                self.dialogs.pivot_col = col;
            });

            ui.separator();
            // —— 指标列表（值列 × 聚合方式，可多项）——
            ui.label(format!(
                "指标（值列 × 聚合方式，最多 {} 项）:",
                stats::PIVOT_MAX_VALUES
            ));
            self.dialogs
                .pivot_values
                .retain(|&(c, _)| c < header.len() && base_ok[c]);
            let mut values = std::mem::take(&mut self.dialogs.pivot_values);
            let mut remove: Option<usize> = None;
            egui::ScrollArea::vertical()
                .max_height(120.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    if values.is_empty() {
                        ui.weak("（暂无指标：请在下方选择值列与聚合方式，再点「+ 添加」）");
                    }
                    for i in 0..values.len() {
                        ui.horizontal(|ui| {
                            ui.label(header[values[i].0].clone());
                            let mut k = values[i].1.min(stats::PIVOT_KINDS.len() - 1);
                            egui::ComboBox::from_id_source(("pivot_value_kind", i))
                                .selected_text(stats::PIVOT_KINDS[k].label())
                                .width(96.0)
                                .show_ui(ui, |ui| {
                                    for (j, kind) in stats::PIVOT_KINDS.iter().enumerate() {
                                        ui.selectable_value(&mut k, j, kind.label());
                                    }
                                });
                            values[i].1 = k;
                            if ui.button("×").on_hover_text("删除此指标").clicked() {
                                remove = Some(i);
                            }
                        });
                    }
                });
            if let Some(i) = remove {
                values.remove(i);
            }
            // 新增指标
            ui.horizontal(|ui| {
                let mut col = self.dialogs.pivot_new_col;
                if col >= header.len() || !base_ok[col] {
                    col = raw[0];
                }
                egui::ComboBox::from_id_source("pivot_new_col")
                    .selected_text(header.get(col).cloned().unwrap_or_default())
                    .width(200.0)
                    .show_ui(ui, |ui| {
                        for &i in &raw {
                            ui.selectable_value(&mut col, i, &header[i]);
                        }
                    });
                self.dialogs.pivot_new_col = col;
                let mut k = self
                    .dialogs
                    .pivot_new_kind
                    .min(stats::PIVOT_KINDS.len() - 1);
                egui::ComboBox::from_id_source("pivot_new_kind")
                    .selected_text(stats::PIVOT_KINDS[k].label())
                    .width(96.0)
                    .show_ui(ui, |ui| {
                        for (i, kind) in stats::PIVOT_KINDS.iter().enumerate() {
                            ui.selectable_value(&mut k, i, kind.label());
                        }
                    });
                self.dialogs.pivot_new_kind = k;
                if ui
                    .add_enabled(
                        values.len() < stats::PIVOT_MAX_VALUES,
                        egui::Button::new("+ 添加"),
                    )
                    .clicked()
                {
                    values.push((col, k));
                }
                ui.weak(format!("已添加 {} 项", values.len()));
            });
            self.dialogs.pivot_values = values;

            ui.separator();
            // —— 规模上限（可按硬件情况调整）——
            ui.label("规模上限（按电脑内存调整；数值越大能处理的情况越多、占用内存越大）:");
            if self.dialogs.pivot_max_rows == 0 {
                let saved = ctx.memory(|m| {
                    m.data
                        .get_temp::<(usize, usize, usize)>(egui::Id::new("pivot_limits"))
                });
                let (r, c, cell) = saved.unwrap_or((
                    stats::PIVOT_DEFAULT_MAX_ROWS,
                    stats::PIVOT_DEFAULT_MAX_COLUMNS,
                    stats::PIVOT_DEFAULT_MAX_CELLS,
                ));
                self.dialogs.pivot_max_rows = r;
                self.dialogs.pivot_max_cols = c;
                self.dialogs.pivot_max_cells = cell;
            }
            let mut limits_changed = false;
            ui.horizontal(|ui| {
                ui.label("行组合数上限");
                limits_changed |= ui
                    .add(
                        egui::DragValue::new(&mut self.dialogs.pivot_max_rows)
                            .clamp_range(100usize..=2_000_000usize)
                            .speed(500.0),
                    )
                    .on_hover_text("行字段取值组合的最大个数（默认 20000）；调大后可处理更细的行维度")
                    .changed();
                ui.label("　列字段取值上限");
                limits_changed |= ui
                    .add(
                        egui::DragValue::new(&mut self.dialogs.pivot_max_cols)
                            .clamp_range(10usize..=16_384usize)
                            .speed(50.0),
                    )
                    .on_hover_text("列字段不同取值的最大个数（默认 200，Excel 单表列上限 16384）")
                    .changed();
            });
            ui.horizontal(|ui| {
                ui.label("单元格数上限");
                limits_changed |= ui
                    .add(
                        egui::DragValue::new(&mut self.dialogs.pivot_max_cells)
                            .clamp_range(10_000usize..=20_000_000usize)
                            .speed(100_000.0),
                    )
                    .on_hover_text("行组合 × 列取值的有数据格子数上限（默认 500000）")
                    .changed();
                if ui
                    .small_button("恢复默认")
                    .on_hover_text("恢复为软件默认上限（20000 / 200 / 500000）")
                    .clicked()
                {
                    self.dialogs.pivot_max_rows = stats::PIVOT_DEFAULT_MAX_ROWS;
                    self.dialogs.pivot_max_cols = stats::PIVOT_DEFAULT_MAX_COLUMNS;
                    self.dialogs.pivot_max_cells = stats::PIVOT_DEFAULT_MAX_CELLS;
                    limits_changed = true;
                }
            });
            // 粗略内存预估：每个格子每项指标一个聚合器，约 100 字节
            let values_len = self.dialogs.pivot_values.len().max(1);
            let est_mb =
                self.dialogs.pivot_max_cells as f64 * values_len as f64 * 100.0 / 1024.0 / 1024.0;
            ui.weak(format!(
                "粗估最大内存占用 ≈ {:.0} MB（{} 个指标 × {} 个格子 × 约 100 字节；实际通常远小于此）",
                est_mb, values_len, self.dialogs.pivot_max_cells
            ));
            if limits_changed {
                let v = (
                    self.dialogs.pivot_max_rows,
                    self.dialogs.pivot_max_cols,
                    self.dialogs.pivot_max_cells,
                );
                ctx.memory_mut(|m| m.data.insert_temp(egui::Id::new("pivot_limits"), v));
            }

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let clash = self.dialogs.pivot_row_cols.contains(&self.dialogs.pivot_col);
                let ready = !clash && !self.dialogs.pivot_values.is_empty();
                if ui
                    .add_enabled(ready, egui::Button::new("生成透视表"))
                    .clicked()
                {
                    self.run_pivot_table();
                    done = true;
                }
                if ui.button("取消").clicked() {
                    done = true;
                }
                if clash {
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 120, 0),
                        "⚠ 列字段不能与行字段相同",
                    );
                } else if self.dialogs.pivot_values.is_empty() {
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 120, 0),
                        "⚠ 请至少添加一项指标",
                    );
                } else {
                    ui.weak(format!(
                        "行 {} 个字段 / 列 {} / 指标 {} 项",
                        self.dialogs.pivot_row_cols.len(),
                        header.get(self.dialogs.pivot_col).cloned().unwrap_or_default(),
                        self.dialogs.pivot_values.len()
                    ));
                }
            });
        });
        if done {
            keep = false;
        }
        *open = keep;
    }

    /// 「交叉 / 关联统计」参数对话框：A 字段 × B 字段 + 明细前 N 项。
    fn dialog_cross_stats(&mut self, ctx: &egui::Context, open: &mut bool) {
        ensure_stats_limits(self, ctx);
        let mut keep = *open;
        let mut done = false;
        apply_dialog_pos_sized(
            egui::Window::new("交叉 / 关联统计")
                .open(&mut keep)
                .collapsible(false)
                .resizable(true)
                .default_size([600.0, 460.0]),
            self.dialog_position,
            ctx,
            egui::vec2(600.0, 460.0),
        )
        .show(ctx, |ui| {
            let header = self.header_names();
            if header.is_empty() {
                ui.label("请先加载数据");
                return;
            }
            let base_ok: Vec<bool> = (0..header.len())
                .map(|i| self.ops.as_ref().and_then(|o| o.view_to_base(i)).is_some())
                .collect();
            let raw: Vec<usize> = (0..header.len()).filter(|&i| base_ok[i]).collect();
            if raw.is_empty() {
                ui.label("没有可用于统计的原始列");
                return;
            }
            let scope = match self.visible_rows() {
                Some(v) => format!("当前筛选结果（{} 行）", v.len()),
                None => format!(
                    "全部数据（{} 行）",
                    self.model.as_ref().map(|m| m.row_count()).unwrap_or(0)
                ),
            };
            ui.label(format!("统计范围：{scope}"));
            ui.weak(
                "看两个字段的对应关系，例如「同一卡号对应多少个账号」：结果会给出关系判定\
                 （一对一 / 一对多 / 多对一 / 多对多）、两侧的最大对应数，以及每个 A 值对应的\
                 B 值个数与示例。按原值比较（与去重一致），空值也是一种取值。",
            );
            ui.separator();

            ui.horizontal(|ui| {
                ui.label("A 字段");
                let mut a = self.dialogs.cross_a.min(header.len().saturating_sub(1));
                if !base_ok.get(a).copied().unwrap_or(false) {
                    a = raw[0];
                }
                egui::ComboBox::from_id_source("cross_a")
                    .selected_text(header[a].clone())
                    .width(220.0)
                    .show_ui(ui, |ui| {
                        for &i in &raw {
                            ui.selectable_value(&mut a, i, &header[i]);
                        }
                    });
                self.dialogs.cross_a = a;
            });
            ui.horizontal(|ui| {
                ui.label("B 字段");
                let mut b = self.dialogs.cross_b.min(header.len().saturating_sub(1));
                if !base_ok.get(b).copied().unwrap_or(false) {
                    b = raw.get(1).copied().unwrap_or(raw[0]);
                }
                egui::ComboBox::from_id_source("cross_b")
                    .selected_text(header[b].clone())
                    .width(220.0)
                    .show_ui(ui, |ui| {
                        for &i in &raw {
                            ui.selectable_value(&mut b, i, &header[i]);
                        }
                    });
                self.dialogs.cross_b = b;
            });

            ui.separator();
            if self.dialogs.cross_top_n == 0 {
                self.dialogs.cross_top_n = 100; // 懒初始化默认值
            }
            ui.horizontal(|ui| {
                ui.label("明细列出前");
                ui.add(
                    egui::DragValue::new(&mut self.dialogs.cross_top_n)
                        .clamp_range(1..=stats::MAX_CROSS_TOP_N)
                        .speed(1.0),
                );
                ui.label("个 A 取值（按「对应的 B 个数」从多到少）");
            });

            limits_header(
                ui,
                "取值数或组合数超出上限时会报错；内存富余可调大，或先筛选缩小范围",
            );
            let mut limits_changed = false;
            limits_changed |= limits_row(
                ui,
                "A 取值上限",
                &mut self.dialogs.stats_limits.max_groups,
                100..=5_000_000,
                500.0,
                "A 字段最多允许多少个不同取值（默认 20000，与分组汇总之分组数上限共用）",
            );
            limits_changed |= limits_row(
                ui,
                "组合数上限",
                &mut self.dialogs.stats_limits.max_cross_pairs,
                100_000..=50_000_000,
                500_000.0,
                "不同 (A,B) 组合数上限（默认 100 万；每条约 60~100 字节）",
            );
            if limits_changed {
                save_stats_limits(ctx, self.dialogs.stats_limits);
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let same = self.dialogs.cross_a == self.dialogs.cross_b;
                if ui
                    .add_enabled(!same, egui::Button::new("开始统计"))
                    .clicked()
                {
                    self.run_cross_stats();
                    done = true;
                }
                if ui.button("取消").clicked() {
                    done = true;
                }
                if same {
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 120, 0),
                        "⚠ 两个字段不能相同",
                    );
                } else {
                    ui.weak(format!(
                        "{} × {}",
                        header.get(self.dialogs.cross_a).cloned().unwrap_or_default(),
                        header.get(self.dialogs.cross_b).cloned().unwrap_or_default()
                    ));
                }
            });
        });
        if done {
            keep = false;
        }
        *open = keep;
    }

    /// 「重复情况统计」参数对话框：选判重字段（不勾 = 全部字段）+ 明细前 N 项。
    fn dialog_duplicate_stats(&mut self, ctx: &egui::Context, open: &mut bool) {
        ensure_stats_limits(self, ctx);
        let mut keep = *open;
        let mut done = false;
        apply_dialog_pos_sized(
            egui::Window::new("重复情况统计")
                .open(&mut keep)
                .collapsible(false)
                .resizable(true)
                .default_size([600.0, 540.0]),
            self.dialog_position,
            ctx,
            egui::vec2(600.0, 540.0),
        )
        .show(ctx, |ui| {
            let header = self.header_names();
            if header.is_empty() {
                ui.label("请先加载数据");
                return;
            }
            let base_ok: Vec<bool> = (0..header.len())
                .map(|i| self.ops.as_ref().and_then(|o| o.view_to_base(i)).is_some())
                .collect();
            let scope = match self.visible_rows() {
                Some(v) => format!("当前筛选结果（{} 行）", v.len()),
                None => format!(
                    "全部数据（{} 行）",
                    self.model.as_ref().map(|m| m.row_count()).unwrap_or(0)
                ),
            };
            ui.label(format!("统计范围：{scope}"));
            ui.weak(
                "按所选字段的取值组合找出重复的键：各重复几次、重复行占比，明细按重复次数降序。\
                 执行后可在结果窗里一键「只看重复的行」或「只看唯一行」把结果落到当前视图。\
                 判重口径与「提取唯一行（去重）」完全一致（按原值比较，保留首次出现的行）。",
            );
            ui.separator();

            ui.label("判重字段（不勾选 = 按整行判重）:");
            self.dialogs
                .dup_cols
                .retain(|&i| i < header.len() && base_ok[i]);
            let mut sel = self.dialogs.dup_cols.clone();
            let selectable = base_ok.iter().filter(|b| **b).count();
            ui.horizontal(|ui| {
                if ui.small_button("全选").clicked() {
                    sel = (0..header.len()).filter(|&i| base_ok[i]).collect();
                }
                if ui.small_button("全不选").clicked() {
                    sel.clear();
                }
                ui.weak(format!("已选 {} / {} 列", sel.len(), selectable));
            });
            egui::ScrollArea::vertical()
                .max_height(220.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for (i, h) in header.iter().enumerate() {
                        if !base_ok[i] {
                            continue; // 派生列不列出
                        }
                        let mut on = sel.contains(&i);
                        ui.checkbox(&mut on, h);
                        if on && !sel.contains(&i) {
                            sel.push(i);
                        } else if !on {
                            sel.retain(|&x| x != i);
                        }
                    }
                });
            self.dialogs.dup_cols = sel.clone();

            ui.separator();
            if self.dialogs.dup_top_n == 0 {
                self.dialogs.dup_top_n = 100; // 懒初始化默认值
            }
            ui.horizontal(|ui| {
                ui.label("明细列出前");
                ui.add(
                    egui::DragValue::new(&mut self.dialogs.dup_top_n)
                        .clamp_range(1..=stats::MAX_DUP_TOP_N)
                        .speed(1.0),
                );
                ui.label("个重复键（按重复次数降序；其余汇总为一行）");
            });

            limits_header(
                ui,
                "判重键的不同取值超出上限时会报错；内存富余可调大，或减少判重字段 / 先筛选",
            );
            let mut limits_changed = false;
            limits_changed |= limits_row(
                ui,
                "不同键个数上限",
                &mut self.dialogs.stats_limits.max_dup_keys,
                100_000..=50_000_000,
                500_000.0,
                "重复情况统计最多容纳多少个不同的键（默认 200 万；每条约 60~100 字节）",
            );
            if limits_changed {
                save_stats_limits(ctx, self.dialogs.stats_limits);
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("开始统计").clicked() {
                    self.run_duplicate_stats();
                    done = true;
                }
                if ui.button("取消").clicked() {
                    done = true;
                }
                ui.weak(if sel.is_empty() {
                    "将按整行判重"
                } else {
                    "将按所选字段判重"
                });
            });
        });
        if done {
            keep = false;
        }
        *open = keep;
    }

    /// 「空值与数据质量统计」参数对话框：勾选要体检的列（可全选）。
    fn dialog_quality_stats(&mut self, ctx: &egui::Context, open: &mut bool) {
        let mut keep = *open;
        let mut done = false;
        apply_dialog_pos_sized(
            egui::Window::new("空值与数据质量统计")
                .open(&mut keep)
                .collapsible(false)
                .resizable(true)
                .default_size([600.0, 540.0]),
            self.dialog_position,
            ctx,
            egui::vec2(600.0, 540.0),
        )
        .show(ctx, |ui| {
            let header = self.header_names();
            if header.is_empty() {
                ui.label("请先加载数据");
                return;
            }
            let base_ok: Vec<bool> = (0..header.len())
                .map(|i| self.ops.as_ref().and_then(|o| o.view_to_base(i)).is_some())
                .collect();
            let scope = match self.visible_rows() {
                Some(v) => format!("当前筛选结果（{} 行）", v.len()),
                None => format!(
                    "全部数据（{} 行）",
                    self.model.as_ref().map(|m| m.row_count()).unwrap_or(0)
                ),
            };
            ui.label(format!("统计范围：{scope}"));
            ui.weak(
                "逐列体检：空值数、纯空白数、空值率、含控制字符数（制表符/换行等）、\
                 最短/最长/平均长度（按字符数）、超长值数（>255 字符）。\
                 想知道整份数据的质量，点「全选」再统计即可。派生列不参与统计。",
            );
            ui.separator();

            ui.label("选择要检查的列:");
            self.dialogs
                .quality_cols
                .retain(|&i| i < header.len() && base_ok[i]);
            let mut sel = self.dialogs.quality_cols.clone();
            let selectable = base_ok.iter().filter(|b| **b).count();
            ui.horizontal(|ui| {
                if ui.small_button("全选").clicked() {
                    sel = (0..header.len()).filter(|&i| base_ok[i]).collect();
                }
                if ui.small_button("全不选").clicked() {
                    sel.clear();
                }
                ui.weak(format!("已选 {} / {} 列（可检查列）", sel.len(), selectable));
            });
            egui::ScrollArea::vertical()
                .max_height(240.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for (i, h) in header.iter().enumerate() {
                        if !base_ok[i] {
                            continue; // 派生列不列出
                        }
                        let mut on = sel.contains(&i);
                        ui.checkbox(&mut on, h);
                        if on && !sel.contains(&i) {
                            sel.push(i);
                        } else if !on {
                            sel.retain(|&x| x != i);
                        }
                    }
                });
            self.dialogs.quality_cols = sel.clone();

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("开始检查").clicked() {
                    self.run_quality_stats();
                    done = true;
                }
                if ui.button("取消").clicked() {
                    done = true;
                }
                ui.weak(format!("已选 {} 列", sel.len()));
            });
        });
        if done {
            keep = false;
        }
        *open = keep;
    }

    /// 「取值频次分布 / Top-N」参数对话框：勾选要统计的列 + 每列显示前 N 项。
    fn dialog_value_frequency(&mut self, ctx: &egui::Context, open: &mut bool) {
        ensure_stats_limits(self, ctx);
        let mut keep = *open;
        let mut done = false;
        apply_dialog_pos_sized(
            egui::Window::new("取值频次分布 / Top-N")
                .open(&mut keep)
                .collapsible(false)
                .resizable(true)
                .default_size([600.0, 540.0]),
            self.dialog_position,
            ctx,
            egui::vec2(600.0, 540.0),
        )
        .show(ctx, |ui| {
            let header = self.header_names();
            if header.is_empty() {
                ui.label("请先加载数据");
                return;
            }
            let base_ok: Vec<bool> = (0..header.len())
                .map(|i| self.ops.as_ref().and_then(|o| o.view_to_base(i)).is_some())
                .collect();
            let scope = match self.visible_rows() {
                Some(v) => format!("当前筛选结果（{} 行）", v.len()),
                None => format!(
                    "全部数据（{} 行）",
                    self.model.as_ref().map(|m| m.row_count()).unwrap_or(0)
                ),
            };
            ui.label(format!("统计范围：{scope}"));
            ui.weak(
                "统计勾选列各取值的出现次数与占比，按次数从多到少排出前 N 项（例：找出卡号出现次数最多的前 20 条）。\
                 按原值统计（区分大小写与首尾空格，与去重一致）；空值归为一类「(空)」参与排名；\
                 超出前 N 项的取值汇总为一行「(其余 M 个取值)」；每列末行是「(合计)」（次数 = 统计范围行数、占比 100%）\
                 可用来核对总量。占比按 4 位有效数字显示（如 0.004748），低频值不会被显示成 0.00。派生列不参与统计。",
            );
            ui.separator();

            ui.label("选择要统计的列:");
            self.dialogs
                .freq_cols
                .retain(|&i| i < header.len() && base_ok[i]);
            let mut sel = self.dialogs.freq_cols.clone();
            let selectable = base_ok.iter().filter(|b| **b).count();
            ui.horizontal(|ui| {
                if ui.small_button("全选").clicked() {
                    sel = (0..header.len()).filter(|&i| base_ok[i]).collect();
                }
                if ui.small_button("全不选").clicked() {
                    sel.clear();
                }
                ui.weak(format!("已选 {} / {} 列（可统计列）", sel.len(), selectable));
            });
            egui::ScrollArea::vertical()
                .max_height(220.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for (i, h) in header.iter().enumerate() {
                        if !base_ok[i] {
                            continue; // 派生列不列出
                        }
                        let mut on = sel.contains(&i);
                        ui.checkbox(&mut on, h);
                        if on && !sel.contains(&i) {
                            sel.push(i);
                        } else if !on {
                            sel.retain(|&x| x != i);
                        }
                    }
                });
            self.dialogs.freq_cols = sel.clone();

            ui.separator();
            if self.dialogs.freq_top_n == 0 {
                self.dialogs.freq_top_n = 20; // 懒初始化默认值
            }
            ui.horizontal(|ui| {
                ui.label("每列显示前");
                ui.add(
                    egui::DragValue::new(&mut self.dialogs.freq_top_n)
                        .clamp_range(1..=stats::MAX_FREQ_TOP_N)
                        .speed(1.0),
                );
                ui.label("项（Top-N）");
                ui.weak("N 越大结果表越长；常见的「找出最多的前 20 条」填 20 即可");
            });

            limits_header(
                ui,
                "不同取值超出上限时会拒绝统计（Top-N 依赖完整计数）；内存富余可调大，或先筛选缩小范围",
            );
            let mut limits_changed = false;
            limits_changed |= limits_row(
                ui,
                "不同取值总量上限",
                &mut self.dialogs.stats_limits.freq_budget,
                100_000..=50_000_000,
                500_000.0,
                "所有统计列共享、按列数均分（默认 300 万；每个不同取值约 60~100 字节）",
            );
            if limits_changed {
                save_stats_limits(ctx, self.dialogs.stats_limits);
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("开始统计").clicked() {
                    self.run_value_frequency();
                    done = true;
                }
                if ui.button("取消").clicked() {
                    done = true;
                }
                ui.weak(format!("已选 {} 列", sel.len()));
            });
        });
        if done {
            keep = false;
        }
        *open = keep;
    }

    /// 统计结果窗（分组汇总 / 数值列统计 / 频次分布共用）：虚拟化表格展示 +
    /// 导出 CSV / XLSX（可最小化为标题栏稍后再导出）。三种统计各有不同的窗口标题，
    /// 因此位置、最小化状态互不干扰。
    fn dialog_stats_result(&mut self, ctx: &egui::Context, open: &mut bool) {
        let mut keep = *open;
        let mut done = false;
        let mut export: Option<(String, bool)> = None;
        // 「重复情况统计」才有的行号集合（用于把结果落到当前数据视图）
        let filter_rows = self.stats_filter_rows.clone();
        let mut apply_rows: Option<(Vec<u32>, &'static str)> = None;
        // 「频次分布」才有的"按出现次数筛选"上下文（单列时可用）
        let freq_filter_col = self.freq_filter_cols.first().copied();
        let mut run_freq_filter = false;
        // 结果类型决定窗口标题（标题同时是 egui 窗口 Id）
        let kind = self.last_stats.as_ref().map(|t| t.kind).unwrap_or_default();
        let title = kind.title();
        // 新结果、或用户点「查看上次统计结果…」时，强制从最小化状态展开
        if self.dialogs.stats_result_expand_pending {
            self.dialogs.stats_result_expand_pending = false;
            set_window_collapsed(ctx, title, false);
        }
        // Arc：结果窗每帧渲染都共享同一份结果表，不复制数据
        let table = self.last_stats.clone();
        apply_dialog_pos_sized(
            egui::Window::new(title)
                .open(&mut keep)
                .collapsible(true)
                .default_open(true)
                .resizable(true)
                .default_size([780.0, 540.0]),
            self.dialog_position,
            ctx,
            egui::vec2(780.0, 540.0),
        )
        .show(ctx, |ui| {
            let Some(table) = table.as_ref() else {
                ui.label("没有可显示的统计结果");
                if ui.button("关闭").clicked() {
                    done = true;
                }
                return;
            };
            ui.label(table.summary());
            // 结果级提示（非数值未参与计算、中位数未计算、标准差口径等）
            for note in &table.notes {
                ui.label(
                    egui::RichText::new(format!("⚠ {}", note))
                        .color(egui::Color32::from_rgb(200, 120, 0)),
                );
            }
            ui.horizontal(|ui| {
                let stem = kind.file_stem();
                if ui.button("导出 CSV…").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("CSV", &["csv"])
                        .set_file_name(format!("{}.csv", stem))
                        .save_file()
                    {
                        export = Some((path.to_string_lossy().into_owned(), false));
                    }
                }
                if ui.button("导出 XLSX…").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("XLSX", &["xlsx"])
                        .set_file_name(format!("{}.xlsx", stem))
                        .save_file()
                    {
                        export = Some((path.to_string_lossy().into_owned(), true));
                    }
                }
                // 重复情况统计：一键把结果落到当前数据视图（与「清除筛选」配合可恢复）
                if let Some(rows) = filter_rows.as_ref() {
                    if ui
                        .button("只看重复的行")
                        .on_hover_text(format!(
                            "把当前视图切换为「重复键涉及的全部行」（{} 行，含首次出现的那行）；\
                             已按重复键分组排列：同一个键的各行紧挨着，一眼就能看出谁和谁重复\
                             （行号列显示原始行号，便于回原表核对）",
                            rows.dup.len()
                        ))
                        .clicked()
                    {
                        apply_rows = Some((rows.dup.clone(), "重复的行"));
                    }
                    if ui
                        .button("只看唯一行")
                        .on_hover_text(format!(
                            "把当前视图切换为「每个键首次出现的行」（{} 行，等同「提取唯一行（去重）」的结果）",
                            rows.unique.len()
                        ))
                        .clicked()
                    {
                        apply_rows = Some((rows.unique.clone(), "唯一行"));
                    }
                }
                if ui
                    .button("最小化")
                    .on_hover_text(
                        "把结果窗收起成一条标题栏（统计结果仍保留在程序里）；以后点标题栏左侧的三角或双击标题栏即可展开，继续导出",
                    )
                    .clicked()
                {
                    set_window_collapsed(ctx, title, true);
                }
                if ui.button("关闭").clicked() {
                    done = true;
                }
                ui.weak("行号从 1 开始；表格可拖动列宽、滚动查看全部行");
            });
            // 频次分布：把"高频取值对应的行"一键筛回当前视图
            if kind == stats::StatsKind::ValueFrequency && freq_filter_col.is_some() {
                if self.dialogs.freq_min_count == 0 {
                    self.dialogs.freq_min_count = 2; // 懒初始化默认值
                }
                ui.horizontal(|ui| {
                    ui.label("只看出现次数 ≥");
                    ui.add(
                        egui::DragValue::new(&mut self.dialogs.freq_min_count)
                            .clamp_range(1usize..=1_000_000usize)
                            .speed(1.0),
                    )
                    .on_hover_text(
                        "填 2 即「只看重复/高频的值」；填 1 等于把全部行按取值分组重排",
                    );
                    ui.label("的行");
                    if ui
                        .button("筛出这些行")
                        .on_hover_text(
                            "把当前视图切换为这些取值对应的行（按取值分组排列，同值相邻）；\
                             用工具栏「清除筛选」可恢复全部",
                        )
                        .clicked()
                    {
                        run_freq_filter = true;
                    }
                    ui.weak("仅统计单列时可用");
                });
            }
            ui.separator();

            // 结果行数可能上万：沿用预览表的虚拟化渲染（只布局可见行）
            let row_height = 20.0;
            egui::ScrollArea::horizontal()
                .auto_shrink([true, false])
                .show(ui, |ui| {
                    let mut tb = TableBuilder::new(ui)
                        .striped(true)
                        .resizable(true)
                        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                        .column(Column::initial(60.0).at_least(40.0).clip(true))
                        .min_scrolled_height(200.0);
                    for _ in &table.headers {
                        tb = tb.column(Column::initial(110.0).at_least(50.0).clip(true));
                    }
                    tb.header(row_height + 6.0, |mut header_row| {
                        header_row.col(|ui| {
                            ui.strong("#");
                        });
                        for h in &table.headers {
                            header_row.col(|ui| {
                                ui.strong(h);
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(row_height, table.rows.len(), |mut row| {
                            let i = row.index();
                            row.col(|ui| {
                                ui.monospace(
                                    egui::RichText::new((i + 1).to_string()).weak(),
                                );
                            });
                            if let Some(r) = table.rows.get(i) {
                                for v in r {
                                    // 限制单元格显示长度，避免超长文本拖慢渲染
                                    let shown = truncate_cell(v, 80);
                                    let trimmed = v.trim();
                                    let is_numeric = !trimmed.is_empty()
                                        && trimmed
                                            .bytes()
                                            .all(|b| b.is_ascii_digit() || b == b'-' || b == b'+' || b == b'.');
                                    let mut text = egui::RichText::new(shown);
                                    if is_numeric {
                                        text = text.monospace();
                                    }
                                    row.col(|ui| {
                                        ui.label(text);
                                    });
                                }
                            }
                        });
                    });
                });
            if table.rows.is_empty() {
                ui.label("（没有分组结果：统计范围内没有数据行）");
            }
        });
        if let Some((path, is_xlsx)) = export {
            // 导出是「额外动作」：结果窗保持打开，方便继续查看或再导出另一种格式
            self.export_stats(path, is_xlsx);
        }
        if run_freq_filter {
            self.run_frequency_filter();
        }
        if let Some((rows, what)) = apply_rows {
            let n = rows.len();
            self.filtered_rows = Some(rows);
            self.log(&format!(
                "已把当前数据视图切换为「只看{}」：{} 行{}（用工具栏「清除筛选」可恢复全部）",
                what,
                n,
                if what == "重复的行" {
                    "，已按重复键分组排列（同一键的各行相邻）"
                } else {
                    ""
                }
            ));
        }
        if done {
            keep = false;
        }
        *open = keep;
    }

    /// 导出分组汇总结果（后台线程）
    pub fn export_stats(&mut self, path: String, is_xlsx: bool) {
        if self.busy.is_some() {
            self.log("已有任务在执行，请稍候");
            return;
        }
        let Some(table) = self.last_stats.clone() else {
            self.log("没有可导出的统计结果");
            return;
        };
        let tx = self.tx.clone();
        self.worker = Some(super::background::Worker::spawn(tx.clone(), move |ctl| {
            ctl.report_progress(0.1, "正在写出统计结果...");
            // 结果表本来就是文本：直接按行写出（CSV 带公式注入防护、XLSX 全文本）
            let result = if is_xlsx {
                export::export_rows_xlsx(&path, &table.headers, table.rows.iter().cloned())
            } else {
                export::export_rows_csv(&path, &table.headers, table.rows.iter().cloned())
            };
            match result {
                Ok(n) => {
                    let _ = tx.send(BgMsg::ExportDone(n, path));
                }
                Err(e) => {
                    let _ = tx.send(BgMsg::Error(format!("导出统计结果失败: {}", e)));
                }
            }
        }));
        self.set_busy("导出统计结果", "正在导出...", 0.0);
    }

    /// 自愿捐助对话框（内嵌微信收款码）
    fn dialog_donate(&mut self, ctx: &egui::Context, open: &mut bool) {
        let mut keep = *open;
        let mut closed = false;
        apply_dialog_pos(
            egui::Window::new("捐助")
                .open(&mut keep)
                .collapsible(false)
                .resizable(false),
            self.dialog_position,
            ctx,
        )
        .show(ctx, |ui| {
            ui.set_min_width(360.0_f32.min(ui.ctx().screen_rect().width() - 80.0));
            ui.label("如果您愿意捐助钱款帮助开发者改进产品请微信扫码，万分感谢！");
            ui.add_space(6.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("捐助").size(26.0));
                ui.add_space(4.0);
                if let Some(tex) = self.qr_texture(ctx) {
                    let sz = tex.size_vec2();
                    let w = 300.0;
                    let h = if sz.x > 0.0 { w * (sz.y / sz.x) } else { w };
                    ui.add(egui::Image::from_texture(&tex).fit_to_exact_size(egui::vec2(w, h)));
                } else {
                    ui.colored_label(
                        egui::Color32::from_rgb(190, 60, 60),
                        "收款码图片加载失败（详见日志）",
                    );
                }
            });
            ui.add_space(8.0);
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("开发人员：周玉明");
                ui.add_space(20.0);
                ui.label("邮箱：443149727@qq.com");
            });
            ui.label("王安工作室");
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("关闭").clicked() {
                    closed = true;
                }
            });
        });
        if closed {
            keep = false;
        }
        *open = keep;
    }

    /// 懒加载微信收款码纹理（只解码一次，之后复用）
    fn qr_texture(&mut self, ctx: &egui::Context) -> Option<egui::TextureHandle> {
        if self.qr_texture.is_none() {
            match image::load_from_memory(WECHAT_QR_PNG) {
                Ok(img) => {
                    let rgba = img.to_rgba8();
                    let (w, h) = rgba.dimensions();
                    let color = egui::ColorImage::from_rgba_unmultiplied(
                        [w as usize, h as usize],
                        rgba.as_raw(),
                    );
                    self.qr_texture =
                        Some(ctx.load_texture("wechat_qr", color, egui::TextureOptions::LINEAR));
                }
                Err(e) => {
                    self.log(&format!("收款码图片解码失败：{}", e));
                }
            }
        }
        self.qr_texture.clone()
    }

    /// 导出当前视图（后台线程）
    pub fn export_view(&mut self, path: String, is_csv: bool) {
        if self.busy.is_some() {
            self.log("已有任务在执行，请稍候");
            return;
        }
        let Some(model) = self.model.clone() else {
            self.log("没有数据可导出");
            return;
        };
        let Some(ops) = self.ops.clone() else {
            return;
        };
        let rows = self.visible_rows().map(|v| v.to_vec());
        let tx = self.tx.clone();
        self.worker = Some(super::background::Worker::spawn(tx.clone(), move |ctl| {
            let result = if is_csv {
                export::export_view_csv(
                    &model,
                    &ops,
                    rows.as_deref(),
                    &path,
                    Some(&|p| ctl.report_progress(p as f32, "正在导出 CSV...")),
                )
            } else {
                export::export_view_xlsx(
                    &model,
                    &ops,
                    rows.as_deref(),
                    &path,
                    Some(&|p| ctl.report_progress(p as f32, "正在导出 XLSX...")),
                )
            };
            match result {
                Ok(n) => {
                    let _ = tx.send(BgMsg::ExportDone(n, path));
                }
                Err(e) => {
                    let _ = tx.send(BgMsg::Error(format!("导出失败: {}", e)));
                }
            }
        }));
        self.set_busy("导出数据", "正在导出...", 0.0);
    }

    /// 执行拆分（后台线程）
    pub fn run_split(&mut self) {
        if self.busy.is_some() {
            self.log("已有任务在执行，请稍候");
            return;
        }
        let Some(model) = self.model.clone() else {
            self.log("请先加载数据");
            return;
        };
        let folder = match rfd::FileDialog::new().pick_folder() {
            Some(f) => f.to_string_lossy().into_owned(),
            None => return,
        };
        let format = if self.dialogs.split_format == 0 {
            OutputFormat::Csv
        } else {
            OutputFormat::Xlsx
        };
        let opts = SplitOptions { folder, format };
        let method = self.dialogs.split_method;
        let batch: usize = self
            .dialogs
            .split_batch
            .trim()
            .parse()
            .unwrap_or(100000);
        // 视图列 → 原始列映射（派生列剔除）
        let cols: Vec<usize> = self
            .dialogs
            .split_cols
            .iter()
            .filter_map(|&c| self.ops.as_ref().and_then(|o| o.view_to_base(c)))
            .collect();
        if method != 0 && cols.is_empty() {
            self.log("请选择原始列作为拆分字段");
            return;
        }
        // 拆分范围：始终跟随筛选结果（无筛选时 filtered_rows 为 None = 全量）
        let rows = self.visible_rows().map(|v| v.to_vec());
        // 核对表所需的描述信息（拆分类型 / 依据 / 输出格式 / 生成时间）
        let split_type = if method == 0 {
            "按行数拆分"
        } else {
            "按字段拆分"
        };
        let split_basis = if method == 0 {
            format!("每批 {} 行", batch)
        } else {
            let names: Vec<String> = cols
                .iter()
                .filter_map(|&c| model.header().get(c).cloned())
                .collect();
            format!("字段：{}", names.join("、"))
        };
        let format_label = if format == OutputFormat::Csv {
            "CSV"
        } else {
            "XLSX"
        };
        let generated_at = super::now_str();
        // 文件名带时间戳（去掉空格与冒号——Windows 文件名不允许冒号），
        // 避免后一次拆分覆盖上一次的核对表，便于按时间归档比对。
        let stamp = generated_at.replace(' ', "_").replace(':', "");
        let check_path = format!("{}\\拆分完整性核对表_{}.csv", opts.folder, stamp);
        let tx = self.tx.clone();
        self.worker = Some(super::background::Worker::spawn(tx.clone(), move |ctl| {
            let result = if method == 0 {
                split::split_by_rows(&model, rows.as_deref(), batch, &opts, Some(&|p| {
                    ctl.report_progress(p as f32, "正在按行数拆分...")
                }))
            } else {
                split::split_by_field(&model, rows.as_deref(), &cols, &opts, Some(&|p| {
                    ctl.report_progress(p as f32, "正在按字段拆分...")
                }))
            };
            match result {
                Ok(report) => {
                    // 拆分完成后，在同一目录自动生成「拆分完整性核对表」
                    let check = split::write_split_checkfile(
                        &check_path,
                        &generated_at,
                        split_type,
                        &split_basis,
                        format_label,
                        &report,
                    )
                    .map(|_| check_path.clone());
                    let _ = tx.send(BgMsg::SplitDone(report, check));
                }
                Err(e) => {
                    let _ = tx.send(BgMsg::Error(format!("拆分失败: {}", e)));
                }
            }
        }));
        self.set_busy("拆分数据", "正在拆分...", 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// headless 渲染：返回 `Some(内容)` = 窗口展开了；`None` = 处于最小化（折叠）状态。
    fn render(title: &str, collapsed: bool) -> Option<bool> {
        let ctx = egui::Context::default();
        set_window_collapsed(&ctx, title, collapsed);
        let mut inner = None;
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            let r = egui::Window::new(title)
                .collapsible(true)
                .default_open(true)
                .show(ctx, |ui| {
                    ui.label("统计结果内容");
                    true
                });
            inner = r.and_then(|r| r.inner);
        });
        inner
    }

    #[test]
    fn set_window_collapsed_matches_egui_window_id_convention() {
        // 「最小化」依赖 egui 把折叠状态存在 `Id::new(标题).with("collapsing")` 这一约定，
        // 用 headless 渲染验证：折叠 ⇒ 不渲染内容，展开 ⇒ 渲染内容（守护该约定的正确性）
        for kind in [stats::StatsKind::GroupBy, stats::StatsKind::ColumnSummary] {
            let title = kind.title();
            assert_eq!(render(title, false), Some(true), "{title}: 应渲染内容");
            assert_eq!(render(title, true), None, "{title}: 最小化后不渲染内容");
        }
    }

    #[test]
    fn collapse_state_persists_across_frames() {
        // 同一 Context 连续渲染：状态由 egui memory 记忆（窗口不会自己弹回展开）
        let ctx = egui::Context::default();
        let title = stats::StatsKind::ColumnSummary.title();
        let mut shown = Vec::new();
        for collapsed in [true, true, false, true] {
            set_window_collapsed(&ctx, title, collapsed);
            let mut inner = None;
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                let r = egui::Window::new(title)
                    .collapsible(true)
                    .default_open(true)
                    .show(ctx, |_ui| true);
                inner = r.and_then(|r| r.inner);
            });
            shown.push(inner.is_some());
        }
        assert_eq!(shown, vec![false, false, true, false]);
    }
}
