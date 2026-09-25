//! 数据预览：egui_extras 虚拟化表格（900 万行仅渲染可见部分）。

use std::sync::Arc;

use eframe::egui;
use egui_extras::{Column, TableBuilder};

use csv_editor_rust::core::column_ops::ColumnOps;
use csv_editor_rust::core::model::DataModel;

/// 预览表格状态
pub struct PreviewTable {
    /// 待滚动到的数据行（外部设置，仅生效一帧；如「刷新显示」回到首行）
    pub scroll_to_row: Option<usize>,
    /// 本帧被双击单元格所在的数据行号（供外部弹出行详情）
    pub clicked_row: Option<usize>,
}

impl PreviewTable {
    pub fn new() -> Self {
        Self {
            scroll_to_row: None,
            clicked_row: None,
        }
    }

    /// 请求滚动到指定数据行（下一帧渲染时生效一次）
    pub fn scroll_to(&mut self, row: usize) {
        self.scroll_to_row = Some(row);
    }

    /// 渲染表格
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        model: Option<&Arc<DataModel>>,
        ops: Option<&ColumnOps>,
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

        let row_height = 20.0;
        // 取走待滚动行：只在本次渲染生效，否则每帧都会把视图拉回该行而无法滚动
        let scroll_to = self.scroll_to_row.take();

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
                // 滚动定位：数据行号 → 视图行号（筛选视图下需查表，行号数组升序故用二分）
                if let Some(data_row) = scroll_to {
                    let view_row = match rows {
                        Some(list) => match list.binary_search(&(data_row as u32)) {
                            Ok(i) => i,
                            Err(i) => i.min(list.len().saturating_sub(1)),
                        },
                        None => data_row,
                    };
                    table = table.scroll_to_row(view_row, Some(egui::Align::Min));
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
                        body.rows(row_height, total, |mut row| {
                            let view_row = row.index();
                            let data_row = match rows {
                                Some(v) => v.get(view_row).copied().unwrap_or(0) as usize,
                                None => view_row,
                            };
                            row.col(|ui| {
                                ui.monospace(
                                    egui::RichText::new((data_row + 1).to_string()).weak(),
                                );
                            });
                            for &vcol in &visible {
                                let cell = ops.field(model, data_row, vcol);
                                row.col(|ui| {
                                    // 限制每单元格显示长度，避免超长文本拖慢渲染
                                    let shown = truncate_cell(&cell, 80);
                                    let trimmed = cell.trim();
                                    let is_numeric = !trimmed.is_empty()
                                        && trimmed.bytes().all(|b| {
                                            b.is_ascii_digit() || b == b'-' || b == b'+'
                                        });
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
