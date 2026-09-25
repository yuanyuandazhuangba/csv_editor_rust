//! 数据预览：egui_extras 虚拟化表格（900 万行仅渲染可见部分）。

use eframe::egui;
use egui_extras::{Column, TableBuilder};

/// 预览表格状态
pub struct PreviewTable {
    /// 需要滚动到的行（如统计后跳转）
    pub scroll_to_row: Option<usize>,
    /// 上次渲染的总行数（用于滚动条比例）
    pub total_rows: usize,
    /// 本帧被双击单元格所在的数据行号（供外部弹出行详情）
    pub clicked_row: Option<usize>,
}

impl PreviewTable {
    pub fn new() -> Self {
        Self {
            scroll_to_row: None,
            total_rows: 0,
            clicked_row: None,
        }
    }

    /// 渲染表格
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        model: Option<&std::sync::Arc<csv_editor_rust::core::model::DataModel>>,
        ops: Option<&csv_editor_rust::core::column_ops::ColumnOps>,
        rows: Option<&[u32]>,
    ) {
        let (Some(model), Some(ops)) = (model, ops) else {
            ui.centered_and_justified(|ui| {
                ui.label(
                    egui::RichText::new("尚未加载数据 — 请点击「选择CSV文件」")
                        .size(16.0)
                        .color(egui::Color32::GRAY),
                );
            });
            return;
        };
        // 每帧重置，供外部读取本帧双击
        self.clicked_row = None;
        let mut clicked_row: Option<usize> = None;

        let visible = ops.visible_columns();
        let header_names: Vec<String> = visible.iter().map(|&i| ops.header()[i].clone()).collect();
        let total = match rows {
            Some(v) => v.len(),
            None => model.row_count(),
        };
        self.total_rows = total;

        // 行高按需设置（中文场景取 20 像素）
        let row_height = 20.0;
        let text_height = ui.text_style_height(&egui::TextStyle::Body);

        egui::ScrollArea::horizontal()
            .auto_shrink([true, false])
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
            .show(ui, |ui| {
                let mut table = TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::initial(70.0).at_least(40.0).clip(true)) // 行号
            .min_scrolled_height(200.0);

                for _ in &header_names {
            table = table.column(Column::initial(110.0).at_least(40.0).clip(true));
        }

        table
            .header(row_height + 6.0, |mut header_row| {
                header_row.col(|ui| {
                    ui.strong("#");
                });
                        for h in &header_names {
                    header_row.col(|ui| {
                        ui.strong(h);
                    });
                }
            })
            .body(|body| {
                if self.scroll_to_row.is_some() {
                    // 触发行定位（egui 无直接 API，简化：忽略或通过初始滚动）
                    self.scroll_to_row = None;
                }
                body.rows(row_height, total, |mut row| {
                    let view_row = row.index();
                    let data_row = match rows {
                        Some(v) => v.get(view_row).copied().unwrap_or(0) as usize,
                        None => view_row,
                    };
                    row.col(|ui| {
                        ui.monospace(egui::RichText::new((data_row + 1).to_string()).weak());
                    });
                    for &vcol in &visible {
                        let cell = ops.field(model, data_row, vcol);
                        row.col(|ui| {
                            // 限制每单元格显示长度，避免超长文本拖慢渲染
                            let shown = truncate_cell(&cell, 80);
                            let is_numeric = cell
                                .trim()
                                .bytes()
                                .all(|b| b.is_ascii_digit() || b == b'-' || b == b'+')
                                && !cell.trim().is_empty();
                            let mut text = egui::RichText::new(shown);
                            if is_numeric {
                                text = text.monospace();
                            }
                            // 双击单元格 → 记录行号，供弹出行详情
                            if ui
                                .add(egui::Label::new(text).sense(egui::Sense::click()))
                                .double_clicked()
                            {
                                clicked_row = Some(data_row);
                            }
                        });
                    }
                });
            });
            });

        // 空状态提示
        if total == 0 {
            ui.label("（无数据）");
        }
        self.clicked_row = clicked_row;
        let _ = text_height;
    }
}

/// 截断超长单元格文本
fn truncate_cell(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let cut: String = s.chars().take(max_chars).collect();
    format!("{}…", cut)
}
