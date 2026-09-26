//! 对话框与动作：拆分字段/合并/哈希/复制/插入/删除/隐藏/关于/导出/拆分执行。

use eframe::egui;

use csv_editor_rust::core::column_ops::{HashAlgo, InsertPosition};
use csv_editor_rust::core::export;
use csv_editor_rust::core::filter::{FilterCondition, FilterOp, Logic};
use csv_editor_rust::core::merge::{self, HeaderCheck};
use csv_editor_rust::core::split::{self, OutputFormat, SplitOptions};

use super::background::BgMsg;
use super::{CsvEditorApp, DialogPosition};

/// 内嵌的微信收款码（自愿捐助窗口用；单文件发布无法依赖外部图片）
const WECHAT_QR_PNG: &[u8] = include_bytes!("../../assets/wechat_qr.png");

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

/// 按弹窗位置给 Window 应用默认位置（default_pos：首次定位、可拖动、egui 自动记忆拖动位置）
pub(super) fn apply_dialog_pos<'a>(
    w: egui::Window<'a>,
    pos: DialogPosition,
    ctx: &'a egui::Context,
) -> egui::Window<'a> {
    let screen = ctx.screen_rect();
    match pos {
        DialogPosition::Center => {
            w.default_pos(egui::pos2(screen.center().x - 200.0, screen.center().y - 200.0))
        }
        DialogPosition::RightTop => {
            w.default_pos(egui::pos2(screen.right() - 360.0, screen.top() + 80.0))
        }
        DialogPosition::LeftTop => {
            w.default_pos(egui::pos2(screen.left() + 300.0, screen.top() + 80.0))
        }
        DialogPosition::CenterTop => {
            w.default_pos(egui::pos2(screen.center().x - 300.0, screen.top() + 80.0))
        }
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
        // 数据行详情（双击单元格触发）
        let mut open = self.detail_row.is_some();
        self.dialog_row_detail(ctx, &mut open);
        if !open {
            self.detail_row = None;
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

                ui.label("新列名称前缀:");
                ui.add(egui::TextEdit::singleline(&mut self.dialogs.split_prefix));

                ui.add_space(4.0);
                if ui.button("确定").clicked() {
                    let sep = self.dialogs.split_sep.clone();
                    let num = self.dialogs.split_num.max(1);
                    let prefix = self.dialogs.split_prefix.clone();
                    match self.ops.as_ref().and_then(|o| o.view_to_base(col)) {
                        Some(base) => {
                            if let Some(ops) = self.ops_mut() {
                                match ops.split_field(base, &sep, num, &prefix) {
                                    Ok(()) => self.log(&format!(
                                        "字段拆分完成: {} -> {} 列",
                                        header[col], num
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
        apply_dialog_pos(
            egui::Window::new("数据筛选")
                .open(&mut keep)
                .collapsible(false)
                .resizable(true)
                .default_size([640.0, 460.0]),
            self.dialog_position,
            ctx,
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
        apply_dialog_pos(
            egui::Window::new("数据行详情")
                .open(&mut keep)
                .collapsible(false)
                .resizable(true)
                .default_size([460.0, 480.0]),
            self.dialog_position,
            ctx,
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
        apply_dialog_pos(
            egui::Window::new("合并多个CSV文件")
                .open(&mut keep)
                .collapsible(false)
                .resizable(true)
                .default_size([760.0, 470.0]),
            self.dialog_position,
            ctx,
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
                ui.set_min_width(700.0);
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
        apply_dialog_pos(
            egui::Window::new("使用说明")
                .open(&mut keep)
                .collapsible(false)
                .resizable(true)
                .default_size([800.0, 580.0]),
            self.dialog_position,
            ctx,
        )
        .show(ctx, |ui| {
            ui.label(egui::RichText::new("宿经03超大CSV文件实用编辑器 · 使用说明").heading());
            ui.weak("专为超大 CSV 设计（实测 6.5GB / 901 万行流畅操作）。下面按使用顺序讲解，跟着做一遍就能上手。");
            ui.separator();
            egui::ScrollArea::vertical()
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
                    ui.collapsing("九、常见问题", |ui| {
                        ui.label("• 打开很慢或卡住？ 读文件在后台进行，请看进度窗口与右下「操作日志」面板；");
                        ui.label("• 中文显示乱码？ 编码由软件自动识别；若结果异常，请在日志中确认识别到的编码；");
                        ui.label("• 中文输入只出现第一个字？ 该问题已在当前版本修复（若仍出现请反馈输入法名称）；");
                        ui.label("• 拆分产生文件太多？ 改用「按行数拆分」，或先筛选缩小数据范围；");
                        ui.label("• 程序启动无反应？ 请查看 exe 同目录的 csv_editor_rust.log（记录启动诊断信息）。");
                    });
                    ui.collapsing("十、支持开发者", |ui| {
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
            ui.set_min_width(360.0);
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
                Ok(count) => {
                    let _ = tx.send(BgMsg::SplitDone(count));
                }
                Err(e) => {
                    let _ = tx.send(BgMsg::Error(format!("拆分失败: {}", e)));
                }
            }
        }));
        self.set_busy("拆分数据", "正在拆分...", 0.0);
    }
}
