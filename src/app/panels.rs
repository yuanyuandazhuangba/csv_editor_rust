//! 主界面面板：菜单、顶部控制、筛选、日志、预览表格、进度窗口。

use eframe::egui;

use super::{CsvEditorApp, DialogPosition};

impl CsvEditorApp {
    /// 菜单栏
    pub fn menu_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("文件", |ui| {
                    if ui.button("打开CSV文件").clicked() {
                        self.pick_and_open_file();
                        ui.close_menu();
                    }
                    if ui.button("打开多个CSV文件（合并）").clicked() {
                        self.log(
                            "多文件合并尚未实现：可先按字段/行数拆分，或分别打开后用「导出数据」汇总",
                        );
                        ui.close_menu();
                    }
                    if ui.button("导出数据").clicked() {
                        self.dialogs.export_data = true;
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("退出").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button("编辑", |ui| {
                    let (can_undo, can_redo) = self
                        .ops
                        .as_ref()
                        .map_or((false, false), |o| (o.can_undo(), o.can_redo()));
                    if ui
                        .add_enabled(can_undo, egui::Button::new("撤销"))
                        .clicked()
                    {
                        self.do_undo();
                        ui.close_menu();
                    }
                    if ui
                        .add_enabled(can_redo, egui::Button::new("恢复"))
                        .clicked()
                    {
                        self.do_redo();
                        ui.close_menu();
                    }
                });
                ui.menu_button("视图", |ui| {
                    if ui.button("显示/隐藏日志").clicked() {
                        self.show_log = !self.show_log;
                        ui.close_menu();
                    }
                    if ui.button("刷新视图").clicked() {
                        self.refresh_view();
                        ui.close_menu();
                    }
                });
                ui.menu_button("工具", |ui| {
                    if ui.button("加载示例数据").clicked() {
                        self.load_sample_data();
                        ui.close_menu();
                    }
                });
                ui.menu_button("数据处理", |ui| {
                    if ui.button("拆分数据…").clicked() {
                        self.dialogs.split_window = true;
                        ui.close_menu();
                    }
                    ui.menu_button("列操作", |ui| {
                        if ui.button("拆分字段…").clicked() {
                            self.dialogs.split_field = true;
                            ui.close_menu();
                        }
                        if ui.button("合并字段…").clicked() {
                            self.dialogs.merge_field = true;
                            ui.close_menu();
                        }
                        if ui.button("生成行哈希…").clicked() {
                            self.dialogs.generate_hash = true;
                            ui.close_menu();
                        }
                        if ui.button("复制列…").clicked() {
                            self.dialogs.copy_column = true;
                            ui.close_menu();
                        }
                        if ui.button("插入列…").clicked() {
                            self.dialogs.insert_column = true;
                            ui.close_menu();
                        }
                        if ui.button("删除列…").clicked() {
                            self.dialogs.delete_column = true;
                            ui.close_menu();
                        }
                        if ui.button("隐藏列…").clicked() {
                            self.dialogs.hide_column = true;
                            ui.close_menu();
                        }
                        if ui.button("显示列…").clicked() {
                            self.dialogs.show_column = true;
                            ui.close_menu();
                        }
                    });
                    ui.separator();
                    let (can_undo, can_redo) = self
                        .ops
                        .as_ref()
                        .map_or((false, false), |o| (o.can_undo(), o.can_redo()));
                    if ui
                        .add_enabled(can_undo, egui::Button::new("撤销"))
                        .clicked()
                    {
                        self.do_undo();
                        ui.close_menu();
                    }
                    if ui
                        .add_enabled(can_redo, egui::Button::new("恢复"))
                        .clicked()
                    {
                        self.do_redo();
                        ui.close_menu();
                    }
                    ui.separator();
                    ui.menu_button("高级功能", |ui| {
                        if ui.button("清理筛选缓存").clicked() {
                            self.filtered_rows = None;
                            self.log("筛选缓存已清理");
                            ui.close_menu();
                        }
                        if ui.button("文件信息…").clicked() {
                            self.dialogs.file_info = true;
                            ui.close_menu();
                        }
                    });
                });
                ui.menu_button("设置", |ui| {
                    ui.menu_button("弹出窗口位置", |ui| {
                        let mut changed = false;
                        for pos in [
                            DialogPosition::Center,
                            DialogPosition::RightTop,
                            DialogPosition::LeftTop,
                            DialogPosition::CenterTop,
                            DialogPosition::Remember,
                        ] {
                            if ui
                                .selectable_value(&mut self.dialog_position, pos, pos.label())
                                .changed()
                            {
                                changed = true;
                            }
                        }
                        // 切换位置后清除弹窗已记忆位置，使 default_pos 重新生效
                        if changed {
                            ui.ctx().memory_mut(|m| m.reset_areas());
                        }
                    });
                });
                ui.menu_button("帮助", |ui| {
                    if ui.button("关于").clicked() {
                        self.dialogs.about = true;
                        ui.close_menu();
                    }
                });
            });
        });
    }

    /// 顶部控制区
    pub fn controls_panel(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("controls").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                // 文件操作
                if ui.button("📂 选择CSV文件").clicked() {
                    self.pick_and_open_file();
                }
                if ui.button("💾 导出数据").clicked() {
                    if self.model.is_some() {
                        self.dialogs.export_data = true;
                    } else {
                        self.log("请先加载数据");
                    }
                }
                if ui.button("🧪 加载示例数据").clicked() {
                    self.load_sample_data();
                }
                if ui.button("🗑 清空数据").clicked() {
                    self.clear_data();
                }
                if ui.button("🔍 数据筛选").clicked() {
                    self.dialogs.filter_window = true;
                }
                ui.separator();
                if ui.button("刷新显示").clicked() {
                    self.refresh_view();
                }
                if ui.button("日志").clicked() {
                    self.show_log = !self.show_log;
                }
            });
            ui.add_space(2.0);
            // 统计信息
            ui.horizontal(|ui| {
                let total = self.model.as_ref().map(|m| m.row_count()).unwrap_or(0);
                let filtered = self
                    .visible_rows()
                    .map(|v| v.len())
                    .unwrap_or(total);
                ui.label(
                    egui::RichText::new(format!(
                        "总行数: {}  |  筛选后: {}  |  列数: {}  |  内存: {:.0} MB",
                        total,
                        filtered,
                        self.ops.as_ref().map(|o| o.column_count()).unwrap_or(0),
                        self.peak_memory_mb
                    ))
                    .color(egui::Color32::from_rgb(0, 110, 200)),
                );
                if let Some(fi) = &self.file_info {
                    ui.separator();
                    ui.label(
                        egui::RichText::new(format!(
                            "文件: {} ({:.1} MB, {})",
                            std::path::Path::new(&fi.path)
                                .file_name()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                            fi.size_bytes as f64 / 1024.0 / 1024.0,
                            fi.encoding
                        ))
                        .color(egui::Color32::from_rgb(90, 150, 90)),
                    );
                }
            });
            ui.add_space(4.0);
        });
    }

    /// 数据预览表格（中央面板）
    pub fn central_panel(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("数据预览");
            // 提取数据快照，避免 preview 与 self 的借用冲突
            let model = self.model.clone();
            let ops = self.ops.clone();
            let rows = self.visible_rows().map(|v| v.to_vec());
            self.preview.show(ui, model.as_ref(), ops.as_ref(), rows.as_deref());
            // 双击单元格 → 弹出该行详情
            if let Some(r) = self.preview.clicked_row.take() {
                self.detail_row = Some(r);
            }
        });
    }

    /// 日志面板（右侧）
    pub fn log_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("log_panel")
            .resizable(true)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.heading("操作日志");
                ui.horizontal(|ui| {
                    if ui.button("清空").clicked() {
                        self.log.clear();
                    }
                    if ui.button("复制").clicked() {
                        let text = self.log.join("\n");
                        ui.ctx().copy_text(text);
                    }
                    if ui.button("导出").clicked() {
                        self.export_log();
                    }
                });
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .max_height(ui.available_height() - 14.0)
                    .show(ui, |ui| {
                        for line in &self.log {
                            ui.monospace(egui::RichText::new(line).size(11.0));
                        }
                    });
            });
    }

    /// 导出操作日志到文本文件
    pub fn export_log(&mut self) {
        if self.log.is_empty() {
            self.log("没有日志可导出");
            return;
        }
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("文本文件", &["txt", "log"])
            .set_file_name("操作日志.txt")
            .save_file()
        {
            let text = self.log.join("\n");
            match std::fs::write(&path, text) {
                Ok(()) => self.log(&format!("日志已导出: {}", path.display())),
                Err(e) => self.log(&format!("日志导出失败: {}", e)),
            }
        }
    }

    /// 进度/取消窗口（用 egui::Area 而非浮动 Window，内容更可靠地绘制；可拖动、记忆位置）
    pub fn progress_window(&mut self, ctx: &egui::Context) {
        let Some(busy) = self.busy.clone() else {
            return;
        };
        let screen = ctx.screen_rect();
        // 首次默认位置（默认右上；之后 egui 记忆拖动位置）
        let default_pos = match self.dialog_position {
            DialogPosition::Center => egui::pos2(screen.center().x - 200.0, screen.center().y - 70.0),
            DialogPosition::RightTop => egui::pos2(screen.right() - 400.0, screen.top() + 100.0),
            DialogPosition::LeftTop => egui::pos2(screen.left() + 260.0, screen.top() + 100.0),
            DialogPosition::CenterTop => egui::pos2(screen.center().x - 230.0, screen.top() + 100.0),
            DialogPosition::Remember => egui::pos2(screen.center().x - 200.0, screen.center().y - 70.0),
        };
        egui::Area::new(egui::Id::new("export_progress_window"))
            .movable(true)
            .default_pos(default_pos)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::window(&ctx.style()).show(ui, |ui| {
                    ui.set_min_width(360.0);
                    ui.set_min_height(118.0);
                    ui.horizontal(|ui| {
                        ui.strong(&busy.title);
                        ui.spinner();
                    });
                    ui.add_space(4.0);
                    ui.label(&busy.detail);
                    ui.add_space(6.0);
                    ui.add(egui::ProgressBar::new(busy.progress).desired_width(320.0));
                    ui.weak(format!("{:.0}%", busy.progress * 100.0));
                });
            });
    }

    /// 忙碌时持续重绘以更新进度
    pub fn request_repaint_if_busy(&self, ctx: &egui::Context) {
        if self.busy.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    // ===== 动作 =====

    /// 文件选择对话框并打开
    pub fn pick_and_open_file(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("CSV 文件", &["csv", "txt"])
            .add_filter("所有文件", &["*"])
            .pick_file()
        {
            let p = path.to_string_lossy().into_owned();
            if self.busy.is_none() {
                self.open_file(p);
            } else {
                self.log("已有任务在执行，请稍候");
            }
        }
    }

    /// 撤销/恢复
    pub fn do_undo(&mut self) {
        let result = match self.ops.as_mut() {
            Some(ops) => ops.undo(),
            None => Err("没有可撤销的操作".to_string()),
        };
        match result {
            Ok(()) => self.log("撤销操作"),
            Err(e) => self.log(&e),
        }
    }

    pub fn do_redo(&mut self) {
        let result = match self.ops.as_mut() {
            Some(ops) => ops.redo(),
            None => Err("没有可恢复的操作".to_string()),
        };
        match result {
            Ok(()) => self.log("恢复操作"),
            Err(e) => self.log(&e),
        }
    }

    /// 刷新预览视图：滚动回首行（表格内容本身每帧重渲染）
    pub fn refresh_view(&mut self) {
        self.preview.scroll_to(0);
        if self.model.is_some() {
            self.log("视图已刷新（滚动回首行）");
        }
    }

    /// 示例数据（写入临时文件后加载）
    pub fn load_sample_data(&mut self) {
        let dir = std::env::temp_dir();
        let path = dir.join("csv_editor_sample.csv");
        let mut content = String::from("ID,姓名,年龄,城市,部门,工资,入职日期\n");
        for i in 1..=1000u32 {
            content.push_str(&format!(
                "{},用户{},{},城市{},部门{},{},2020-01-{:02}\n",
                i,
                i,
                i % 47 + 18,
                i % 5 + 1,
                i % 4 + 1,
                i % 30 + 5000,
                i % 28 + 1
            ));
        }
        if std::fs::write(&path, content).is_ok() {
            let p = path.to_string_lossy().into_owned();
            self.open_file(p);
        } else {
            self.log("示例数据写入失败");
        }
    }
}

/// 进程峰值工作集（Windows）
pub fn peak_working_set_mb() -> f64 {
    #[cfg(windows)]
    {
        use winapi::um::processthreadsapi::GetCurrentProcess;
        use winapi::um::psapi::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
        unsafe {
            let mut pmc: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            let ok = GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut pmc,
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            );
            if ok != 0 {
                return pmc.WorkingSetSize as f64 / 1024.0 / 1024.0;
            }
        }
    }
    0.0
}
