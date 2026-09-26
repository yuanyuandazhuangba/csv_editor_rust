//! 宿经03超大CSV文件实用编辑器 — Rust 重构版
//!
//! Win7 单文件构建：见 .cargo/config.toml 与 build_win7.bat。
//! release 使用 Windows GUI 子系统（不弹命令行控制台），debug 保留 console 便于开发。
//!
//! 诊断日志：因为 release 是 GUI 子系统且 panic=abort（无控制台、无 stderr），
//! 任何启动错误都“无反应”。这里把 log::error!/warn!、panic、以及 run_native 返回的
//! Err 都写到 exe 同目录的 csv_editor_rust.log（不可写则回退 %TEMP%），便于在 Win7 上定位。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;

use std::io::Write;
use std::path::{Path, PathBuf};

use app::CsvEditorApp;
use eframe::egui;

fn main() -> eframe::Result<()> {
    install_diagnostics();
    log::info!("进入 main：加载窗口图标");

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1440.0, 900.0])
        .with_min_inner_size([900.0, 600.0])
        .with_title("宿经03超大CSV文件实用编辑器");
    if let Some(icon) = load_window_icon() {
        log::info!("窗口图标解码成功（{:?}），设置为窗口图标", icon_dimensions(&icon));
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    } else {
        log::warn!("窗口图标解码失败，将跳过设置图标（不影响启动）");
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    log::info!("调用 eframe::run_native …");
    let result = eframe::run_native(
        "宿经03超大CSV文件实用编辑器",
        options,
        Box::new(|cc| {
            log::info!("app 创建回调进入：安装字体与滚动条样式");
            app::install_fonts(&cc.egui_ctx);
            // 加宽滚动条（bar_width + 内边距），避免数据预览区右侧滚动条过窄、贴分割线难操作
            cc.egui_ctx.style_mut(|s| {
                s.spacing.scroll = egui::style::ScrollStyle::solid();
                s.spacing.scroll.bar_width = 12.0;
                s.spacing.scroll.bar_inner_margin = 8.0;
                s.spacing.scroll.bar_outer_margin = 4.0;
            });
            log::info!("创建 CsvEditorApp");
            Box::new(CsvEditorApp::new(cc))
        }),
    );

    if let Err(err) = &result {
        log::error!("eframe::run_native 返回错误：{err:?}");
    }
    log::info!("main 结束（正常路径）");
    result
}

/// 定位诊断日志文件：优先 exe 同目录（方便查找），不可写则回退 %TEMP%
fn log_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("csv_editor_rust.log");
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
    std::env::temp_dir().join("csv_editor_rust.log")
}

/// 诊断日志文件的大小上限：超过则归档为 `.1`（只保留一份历史），
/// 避免长期反复启动把文件无限写大（该文件每次启动都会追加十几行）。
const LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// 超过上限时轮转：`csv_editor_rust.log` → `csv_editor_rust.log.1`（覆盖旧的 .1）
fn rotate_log_if_needed(path: &Path) {
    let too_big = std::fs::metadata(path)
        .map(|m| m.len() > LOG_MAX_BYTES)
        .unwrap_or(false);
    if !too_big {
        return;
    }
    let mut bak = path.as_os_str().to_owned();
    bak.push(".1");
    let bak = PathBuf::from(bak);
    let _ = std::fs::remove_file(&bak);
    let _ = std::fs::rename(path, &bak);
}

/// 直接向日志文件追加一行（不依赖 log crate；用于 hook/首行）
fn log_line(msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    {
        let _ = writeln!(f, "{msg}");
    }
}

/// 把 `log` crate 的输出落盘（eframe/glow 用 log::error!/warn! 汇报底层错误）
struct FileLogger {
    path: PathBuf,
}

impl log::Log for FileLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(
                f,
                "[{} {}] {}",
                record.level(),
                record.target(),
                record.args()
            );
        }
    }
    fn flush(&self) {}
}

/// 安装诊断：文件 logger + panic hook。必须在任何可能失败的初始化前调用。
fn install_diagnostics() {
    let path = log_path();
    rotate_log_if_needed(&path);
    log_line("==================== csv_editor_rust 启动诊断 ====================");
    log_line(&format!("日志文件：{}", path.display()));
    log_line(&format!(
        "PID={}  pointer_bits={}  debug={}",
        std::process::id(),
        if cfg!(target_pointer_width = "64") {
            64
        } else {
            32
        },
        cfg!(debug_assertions)
    ));

    // 文件 logger（capture log::error!/warn!）
    let logger = FileLogger {
        path: path.clone(),
    };
    // set_boxed_logger 失败（如已有 logger）时忽略
    let _ = log::set_boxed_logger(Box::new(logger));
    log::set_max_level(log::LevelFilter::Info);

    // panic hook：panic=abort 下先执行 hook 再 abort，借此落盘崩溃信息
    std::panic::set_hook(Box::new(|info| {
        log_line(&format!("\n[PANIC] {info}"));
        let bt = std::backtrace::Backtrace::force_capture();
        log_line(&format!("[PANIC 栈] {bt}"));
    }));
}

/// 便捷：读取图标尺寸（用于日志）
fn icon_dimensions(icon: &egui::IconData) -> (u32, u32) {
    (icon.width, icon.height)
}

/// 从内嵌 ico 解码窗口图标（标题栏/任务栏）。
/// 优先选取最大 32bpp 的 BMP DIB 条目（本图标为 256×256），
/// PNG 压缩条目用 image 解码兜底；失败返回 None（不设图标、不崩溃）。
fn load_window_icon() -> Option<egui::IconData> {
    decode_ico(include_bytes!("../csv_editor_icon.ico"))
}

/// 解析 ICO 容器（ICONDIR + 条目表），选面积最大的 32bpp 条目解码
fn decode_ico(data: &[u8]) -> Option<egui::IconData> {
    if data.len() < 6 {
        return None;
    }
    let count = u16::from_le_bytes(data[4..6].try_into().ok()?) as usize;
    let entries = data.get(6..6 + count * 16)?;

    // 选取面积最大的 32bpp 条目（(面积, 条目表内偏移)）
    let mut best: Option<(u32, usize)> = None;
    for i in 0..count {
        let off = i * 16;
        let w = if entries[off] == 0 {
            256
        } else {
            entries[off] as u32
        };
        let h = if entries[off + 1] == 0 {
            256
        } else {
            entries[off + 1] as u32
        };
        let bpp = u16::from_le_bytes([entries[off + 6], entries[off + 7]]);
        if bpp >= 32 {
            let area = w * h;
            if best.map_or(true, |(a, _)| area > a) {
                best = Some((area, off));
            }
        }
    }
    let (_, entry) = best?;

    let data_off = u32::from_le_bytes(entries[entry + 12..entry + 16].try_into().ok()?) as usize;
    let data_len = u32::from_le_bytes(entries[entry + 8..entry + 12].try_into().ok()?) as usize;
    let img = data.get(data_off..data_off + data_len)?;

    // PNG 压缩条目（PNG 签名）用 image 解码
    if img.starts_with(&[0x89, b'P', b'N', b'G']) {
        let dyn_img = image::load_from_memory(img).ok()?;
        let rgba = dyn_img.to_rgba8();
        let (w, h) = rgba.dimensions();
        return Some(egui::IconData {
            rgba: rgba.into_raw(),
            width: w,
            height: h,
        });
    }

    decode_dib(img)
}

/// 解析 32bpp 未压缩 DIB（BITMAPINFOHEADER + BGRA 像素 + AND 掩码）为 RGBA
fn decode_dib(img: &[u8]) -> Option<egui::IconData> {
    if img.len() < 40 {
        return None;
    }
    let header_size = u32::from_le_bytes(img[0..4].try_into().ok()?) as usize;
    if header_size < 40 {
        return None;
    }
    let width = i32::from_le_bytes(img[4..8].try_into().ok()?);
    let height = i32::from_le_bytes(img[8..12].try_into().ok()?);
    let bit_count = u16::from_le_bytes(img[14..16].try_into().ok()?);
    if width <= 0 || height == 0 || bit_count != 32 {
        return None;
    }
    let w = width as usize;
    let row_bytes = w * 4;
    let declared = height.unsigned_abs() as usize;
    // ICO 的 DIB biHeight 通常为像素高度 × 2（含 AND 掩码行），
    // 当声明行数超出可用像素行数时按一半取，得到实际像素高度
    let max_rows = img.len().saturating_sub(header_size) / row_bytes;
    let h = if declared > max_rows { declared / 2 } else { declared };
    if w == 0 || h == 0 {
        return None;
    }
    let bottom_up = height > 0;
    let pixels = img.get(header_size..header_size + row_bytes * h)?;

    let mut rgba = Vec::with_capacity(row_bytes * h);
    for row in 0..h {
        // 自底向上（biHeight 为正）时翻转行
        let src_row = if bottom_up { h - 1 - row } else { row };
        let line = &pixels[src_row * row_bytes..(src_row + 1) * row_bytes];
        for px in line.chunks_exact(4) {
            rgba.push(px[2]); // R
            rgba.push(px[1]); // G
            rgba.push(px[0]); // B
            rgba.push(px[3]); // A
        }
    }
    Some(egui::IconData {
        rgba,
        width: w as u32,
        height: h as u32,
    })
}
