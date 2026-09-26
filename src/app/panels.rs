//! 主界面面板：菜单、顶部控制、筛选、日志、预览表格、进度窗口。

use eframe::egui;

use csv_editor_rust::core::dedup;
use csv_editor_rust::core::merge::{self, HeaderCheck, MergeMode};

use super::background::{BgMsg, Worker};
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
                        self.pick_merge_files(false);
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
                    ui.separator();
                    // 显示/隐藏「数据处理」工具栏：显示时命令后带对号。
                    // 用「尾部空格占位」而不是 add_sized 固定宽度：add_sized 会把文字居中，
                    // 而菜单项应与「加载示例数据」一样左对齐；两态文本长度相同，
                    // 因此对号出现/消失时菜单宽度也不会跳动。
                    let label = if self.show_data_toolbar {
                        "数据处理工具栏  ✔"
                    } else {
                        "数据处理工具栏    "
                    };
                    if ui.button(label).clicked() {
                        self.show_data_toolbar = !self.show_data_toolbar;
                        let msg = if self.show_data_toolbar {
                            "已显示数据处理工具栏"
                        } else {
                            "已隐藏数据处理工具栏"
                        };
                        self.log(msg);
                        ui.close_menu();
                    }
                    let label = if self.show_analysis_toolbar {
                        "数据分析工具栏  ✔"
                    } else {
                        "数据分析工具栏    "
                    };
                    if ui.button(label).clicked() {
                        self.show_analysis_toolbar = !self.show_analysis_toolbar;
                        let msg = if self.show_analysis_toolbar {
                            "已显示数据分析工具栏"
                        } else {
                            "已隐藏数据分析工具栏"
                        };
                        self.log(msg);
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
                ui.menu_button("数据分析", |ui| {
                    if ui.button("提取唯一行（去重）…").clicked() {
                        self.dialogs.dedup_window = true;
                        ui.close_menu();
                    }
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
                    ui.separator();
                    // 操作日志落盘开关（两态文本等长，保证左对齐且宽度不跳动）
                    let label = if self.log_to_file {
                        "操作日志落盘  ✔"
                    } else {
                        "操作日志落盘    "
                    };
                    if ui
                        .button(label)
                        .on_hover_text(
                            "开启后，每条操作日志会追加写入 exe 同目录的「操作日志.txt」\
                             （超过 2MB 自动轮转为 .1），便于事后检索与审计。默认关闭，设置会被记住。",
                        )
                        .clicked()
                    {
                        self.log_to_file = !self.log_to_file;
                        let on = self.log_to_file;
                        ui.ctx().memory_mut(|m| {
                            m.data.insert_temp(egui::Id::new("ops_log_to_file"), on)
                        });
                        self.log(if on {
                            "已开启操作日志落盘（追加到 exe 同目录的「操作日志.txt」）"
                        } else {
                            "已关闭操作日志落盘"
                        });
                        ui.close_menu();
                    }
                });
                ui.menu_button("帮助", |ui| {
                    if ui.button("使用说明").clicked() {
                        self.dialogs.help_guide = true;
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("关于").clicked() {
                        self.dialogs.about = true;
                        ui.close_menu();
                    }
                });
            });
        });
    }

    /// 「数据分析」工具栏：目前提供「提取唯一行（去重）」；
    /// 由「工具 → 数据分析工具栏」控制显示/隐藏（默认显示）。
    pub fn analysis_toolbar_panel(&mut self, ctx: &egui::Context) {
        if !self.show_analysis_toolbar {
            return;
        }
        egui::TopBottomPanel::top("analysis_toolbar").show(ctx, |ui| {
            ui.add_space(2.0);
            egui::ScrollArea::horizontal()
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .button("🔍 提取唯一行（去重）…")
                            .on_hover_text(
                                "在当前筛选结果（未筛选则为全部数据）中，按指定字段或全部字段判重，保留唯一行",
                            )
                            .clicked()
                        {
                            self.dialogs.dedup_window = true;
                        }
                    });
                });
            ui.add_space(2.0);
        });
    }

    /// 底部状态栏：总行数 / 筛选后行数 / 列数 / 内存占用 / 文件信息。
    /// （原先显示在主工具栏第二行，现移到底部，避免占用工具栏垂直空间。）
    pub fn status_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let total = self.model.as_ref().map(|m| m.row_count()).unwrap_or(0);
                let filtered = self.visible_rows().map(|v| v.len()).unwrap_or(total);
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
            ui.add_space(2.0);
        });
    }

    /// 「数据处理」工具栏：把「数据处理」菜单里的每条命令做成按钮（功能与菜单完全一致）。
    /// 由「工具 → 数据处理工具栏」控制显示/隐藏（默认显示）。
    pub fn data_toolbar_panel(&mut self, ctx: &egui::Context) {
        if !self.show_data_toolbar {
            return;
        }
        egui::TopBottomPanel::top("data_toolbar").show(ctx, |ui| {
            ui.add_space(2.0);
            // 按钮较多：与主工具栏同样用横向滚动区兜底，界面放大后仍可访问全部按钮
            egui::ScrollArea::horizontal()
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        // —— 数据拆分 ——
                        if ui
                            .button("📊 拆分数据…")
                            .on_hover_text("按行数或按字段把数据拆分成多个文件")
                            .clicked()
                        {
                            self.dialogs.split_window = true;
                        }
                        ui.separator();
                        // —— 列操作（与「数据处理 → 列操作」子菜单一一对应）——
                        if ui.button("✂ 拆分字段…").clicked() {
                            self.dialogs.split_field = true;
                        }
                        if ui.button("🔗 合并字段…").clicked() {
                            self.dialogs.merge_field = true;
                        }
                        if ui.button("🔢 生成行哈希…").clicked() {
                            self.dialogs.generate_hash = true;
                        }
                        if ui.button("📋 复制列…").clicked() {
                            self.dialogs.copy_column = true;
                        }
                        if ui.button("➕ 插入列…").clicked() {
                            self.dialogs.insert_column = true;
                        }
                        if ui.button("➖ 删除列…").clicked() {
                            self.dialogs.delete_column = true;
                        }
                        if ui.button("🕶 隐藏列…").clicked() {
                            self.dialogs.hide_column = true;
                        }
                        if ui.button("👁 显示列…").clicked() {
                            self.dialogs.show_column = true;
                        }
                        ui.separator();
                        // —— 撤销 / 恢复（与菜单一致：不可用时置灰）——
                        let (can_undo, can_redo) = self
                            .ops
                            .as_ref()
                            .map_or((false, false), |o| (o.can_undo(), o.can_redo()));
                        if ui
                            .add_enabled(can_undo, egui::Button::new("↩ 撤销"))
                            .clicked()
                        {
                            self.do_undo();
                        }
                        if ui
                            .add_enabled(can_redo, egui::Button::new("↪ 恢复"))
                            .clicked()
                        {
                            self.do_redo();
                        }
                        ui.separator();
                        // —— 高级功能 ——
                        if ui
                            .button("♻ 清理筛选缓存")
                            .on_hover_text("清除筛选结果缓存，恢复显示完整数据视图")
                            .clicked()
                        {
                            self.filtered_rows = None;
                            self.log("筛选缓存已清理");
                        }
                        if ui.button("ℹ 文件信息…").clicked() {
                            self.dialogs.file_info = true;
                        }
                    });
                });
            ui.add_space(2.0);
        });
    }

    /// 顶部控制区
    pub fn controls_panel(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("controls").show(ctx, |ui| {
            ui.add_space(4.0);
            // 工具栏按钮较多，界面放大到 1.5 倍时可能超出窗口宽度：
            // 用横向滚动区域包住，溢出时可横向滚动访问（保证各按钮始终可达）。
            egui::ScrollArea::horizontal()
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                    // 文件操作
                    if ui.button("📂 选择CSV文件").clicked() {
                        self.pick_and_open_file();
                    }
                    if ui
                        .button("📚 选择多个CSV文件（合并）")
                        .on_hover_text("一次性选择或追加多个同表头 CSV，先校验表头再合并为一个数据集")
                        .clicked()
                    {
                        self.pick_merge_files(false);
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
                    ui.separator();
                    if ui.button("🔍 数据筛选").clicked() {
                        self.dialogs.filter_window = true;
                    }
                    if ui
                        .button("🧹 清除筛选")
                        .on_hover_text("清除全部筛选条件并显示完整数据（等同筛选窗口中的「清除筛选」）")
                        .clicked()
                    {
                        self.clear_filters();
                    }
                    ui.separator();
                    if ui.button("刷新显示").clicked() {
                        self.refresh_view();
                    }
                    if ui.button("日志").clicked() {
                        self.show_log = !self.show_log;
                    }
                    // 自愿捐助 + 按钮大小：顺序排在工具栏按钮之后（不右对齐）
                    ui.separator();
                    if ui
                        .button("❤ 自愿捐助")
                        .on_hover_text("扫码支持开发者，万分感谢")
                        .clicked()
                    {
                        self.dialogs.donate = true;
                    }
                    ui.separator();
                    ui.label("按钮大小");
                    let resp = ui
                        .add(
                            egui::DragValue::new(&mut self.ui_zoom)
                                .clamp_range(1.0f32..=1.5f32)
                                .speed(0.002)
                                .fixed_decimals(2),
                        )
                        .on_hover_text(
                            "拖动或输入界面放大倍数（1.0 ~ 1.5）。拖动时先显示倍数，松开鼠标后应用，避免界面抖动。",
                        );
                    self.zoom_dragging = resp.dragged();
                    // 拖动中只累积数值、不改动 pixels_per_point：缩放会让指针的逻辑坐标
                    // 在一帧内突变，DragValue 依此算出的 drag_delta() 出现虚假位移，
                    // 数值会反向跳变并与界面来回震荡（即“跳动厉害”）。
                    // 松手（drag_stopped）或键盘输入完成（changed 且未拖动）后一次性平滑应用。
                    if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                        let z = self.ui_zoom.clamp(1.0, 1.5);
                        self.ui_zoom = z;
                        ui.ctx().set_zoom_factor(z);
                        self.log(&format!("按钮大小：{:.2} 倍（界面等比缩放）", z));
                    }
                    ui.label("倍");
                    if self.zoom_dragging {
                        ui.weak("（松开鼠标后应用）");
                    }
                    });
                });
            ui.add_space(2.0);
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
                // 虚拟化渲染：日志上限是 5000 条，若每帧为全部条目创建文本布局，
                // 在放大界面时会有明显开销。show_rows 只渲染可见的若干行，
                // 滚动时再按需布局，因此日志再多也不影响帧率。
                let row_height = 14.0;
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .max_height(ui.available_height() - 14.0)
                    .show_rows(ui, row_height, self.log.len(), |ui, range| {
                        for i in range {
                            if let Some(line) = self.log.get(i) {
                                ui.monospace(egui::RichText::new(line).size(11.0));
                            }
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
            // Area 默认不约束在屏幕内（`Area::constrain` 默认 false），而它的位置在首次之后
            // 一律取自 memory（持久化的“上次拖动位置”）。一旦界面缩放（按钮大小）或窗口尺寸
            // 变化，逻辑屏幕变小，这个记住的位置就可能落到可视区之外 —— 进度窗口于是“不见了”。
            // 对话框用的是 Window，默认 constrain = true，所以不受影响。
            .constrain_to(screen)
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

    /// 清除筛选（与筛选窗口中的「清除筛选」等价）：清空条件并回到完整数据视图
    pub fn clear_filters(&mut self) {
        if self.filter_conditions.is_empty() && self.filtered_rows.is_none() {
            self.log("当前没有筛选条件");
            return;
        }
        self.filter_conditions.clear();
        self.filtered_rows = None;
        self.log("已清除所有筛选条件，显示完整数据");
    }

    /// 选择多个 CSV 文件（合并）：`append=false` 重新选择，`true` 追加选择
    pub fn pick_merge_files(&mut self, append: bool) {
        let picked = rfd::FileDialog::new()
            .add_filter("CSV 文件", &["csv", "txt"])
            .add_filter("所有文件", &["*"])
            .pick_files();
        let Some(paths) = picked else { return };
        if !append {
            self.dialogs.merge_files.clear();
        }
        let mut added = 0usize;
        for p in paths {
            let s = p.to_string_lossy().into_owned();
            if !self.dialogs.merge_files.contains(&s) {
                self.dialogs.merge_files.push(s);
                added += 1;
            }
        }
        self.log(&format!(
            "已选择 {} 个待合并文件（本次新增 {} 个）",
            self.dialogs.merge_files.len(),
            added
        ));
        self.dialogs.merge_window = true;
        self.scan_merge_headers();
    }

    /// 后台读取各文件表头并校验一致性（**读取数据之前**完成校验）
    pub fn scan_merge_headers(&mut self) {
        let files = self.dialogs.merge_files.clone();
        if files.is_empty() {
            self.dialogs.merge_headers.clear();
            self.dialogs.merge_check = None;
            self.dialogs.merge_diff.clear();
            self.dialogs.merge_scanning = false;
            return;
        }
        self.dialogs.merge_scanning = true;
        let tx = self.tx.clone();
        self.worker = Some(Worker::spawn(tx.clone(), move |_ctl| {
            let mut headers = Vec::with_capacity(files.len());
            for f in &files {
                match merge::read_file_header(f) {
                    Ok(h) => headers.push(h),
                    Err(e) => {
                        let _ = tx.send(BgMsg::Error(format!("读取表头失败：{}", e)));
                        return;
                    }
                }
            }
            let hs: Vec<Vec<String>> = headers.iter().map(|h| h.header.clone()).collect();
            let check = merge::check_headers(&hs);
            let diff = merge::exclusive_fields(&hs);
            let _ = tx.send(BgMsg::MergeHeaders(headers, check, diff));
        }));
    }

    /// 更新待合并文件列表（移除某项后重新校验表头）
    pub fn set_merge_files(&mut self, keep: Vec<String>) {
        self.dialogs.merge_files = keep;
        self.dialogs.merge_keep_common = false;
        self.scan_merge_headers();
    }

    /// 执行合并（后台）：结果写入临时文件，完成后自动作为当前数据打开
    pub fn run_merge(&mut self) {
        if self.busy.is_some() {
            self.log("已有任务在执行，请稍候");
            return;
        }
        let files = self.dialogs.merge_files.clone();
        if files.is_empty() {
            self.log("请先选择要合并的 CSV 文件");
            return;
        }
        let mode = match self.dialogs.merge_check {
            Some(HeaderCheck::Differ) => {
                if self.dialogs.merge_keep_common {
                    MergeMode::KeepCommonFields
                } else {
                    self.log("表头不一致：请勾选「只保留相同字段名」，或移除不一致的文件后再合并");
                    return;
                }
            }
            _ => MergeMode::RequireIdentical,
        };
        // 输出到临时目录：合并结果随后按普通单文件打开，需要留存时用「导出数据」另存
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let out_path = std::env::temp_dir()
            .join(format!("csv_merged_{}_{}.csv", std::process::id(), ts))
            .to_string_lossy()
            .into_owned();
        self.log(&format!(
            "开始合并 {} 个文件（{}）...",
            files.len(),
            mode.label()
        ));
        let tx = self.tx.clone();
        self.worker = Some(Worker::spawn(tx.clone(), move |ctl| {
            let result = merge::merge_to_file(&files, mode, &out_path, Some(&|p| {
                ctl.report_progress(p as f32, "正在合并文件...")
            }));
            match result {
                Ok(rep) => {
                    let _ = tx.send(BgMsg::Merged(rep));
                }
                Err(e) => {
                    let _ = tx.send(BgMsg::Error(format!("合并失败：{}", e)));
                }
            }
        }));
        self.set_busy("合并CSV文件", "正在合并...", 0.0);
    }

    /// 执行「提取唯一行（去重）」（后台线程）。
    /// 作用范围是当前筛选结果（未筛选则为全部数据）；结果作为新的当前视图，
    /// 后续导出 / 拆分 / 统计都基于唯一行。
    pub fn run_dedup(&mut self) {
        if self.busy.is_some() {
            self.log("已有任务在执行，请稍候");
            return;
        }
        let Some(model) = self.model.clone() else {
            self.log("请先加载数据");
            return;
        };
        // 判重字段：视图列 → 原始列（派生列不参与，与拆分 / 行哈希保持一致）
        let cols: Vec<usize> = self
            .dialogs
            .dedup_cols
            .iter()
            .filter_map(|&c| self.ops.as_ref().and_then(|o| o.view_to_base(c)))
            .collect();
        let rows = self.visible_rows().map(|v| v.to_vec());
        self.log("开始提取唯一行...");
        let tx = self.tx.clone();
        self.worker = Some(Worker::spawn(tx.clone(), move |ctl| {
            // 收集「保留行 ↔ 被剔除行」对应关系，供完成后导出「重复行对应表」核对
            let (unique, report, map) = dedup::unique_rows_with_map(
                &model,
                rows.as_deref(),
                &cols,
                Some(&|p| ctl.report_progress(p as f32, "正在提取唯一行...")),
                true,
            );
            let _ = tx.send(BgMsg::DedupDone(unique, report, map, cols));
        }));
        self.set_busy("提取唯一行（去重）", "正在判重...", 0.0);
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
