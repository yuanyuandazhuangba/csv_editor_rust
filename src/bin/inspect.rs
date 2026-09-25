//! 命令行实测工具：打开 CSV 并报告行数/列数/编码/耗时/峰值内存/采样预览。
//! 用法：inspect.exe <csv路径> [采样行数]

use std::time::Instant;

use csv_editor_rust::core::model::{DataModel, DEFAULT_SAMPLE_ROWS};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("用法: inspect <csv路径> [采样行数]");
        std::process::exit(2);
    }
    let path = &args[1];
    let sample: usize = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SAMPLE_ROWS);

    let t0 = Instant::now();
    let m = match DataModel::open(path, sample) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("打开失败: {}", e);
            std::process::exit(1);
        }
    };
    let open_ms = t0.elapsed().as_millis();

    println!("=== CSV 实测 ===");
    println!("文件: {}", path);
    println!("编码: {}", m.encoding_label());
    println!("总行数(不含表头): {}", m.row_count());
    println!("列数: {}", m.column_count());
    println!("类型推断采样行数: {}", m.sampled_rows());
    println!("打开耗时: {:.3} 秒", open_ms as f64 / 1000.0);
    println!("峰值工作集内存: {:.1} MB", peak_working_set_mb());

    println!("--- 表头 ---");
    let header = m.header();
    for (i, name) in header.iter().take(10).enumerate() {
        println!("  [{}] {} ({})", i, name, m.columns()[i].type_kind.label());
    }
    if header.len() > 10 {
        println!("  ... 共 {} 列", header.len());
    }

    println!("--- 前 3 行采样 ---");
    for r in 0..3.min(m.row_count()) {
        let row = m.row(r);
        let shown: Vec<&str> = row.iter().take(6).map(|s| s.as_str()).collect();
        println!("  行{}: {}", r + 1, shown.join(" | "));
    }
    println!("--- 第 {} 行采样 ---", m.row_count());
    let last = m.row_count().saturating_sub(1);
    if last > 0 {
        let row = m.row(last);
        let shown: Vec<&str> = row.iter().take(6).map(|s| s.as_str()).collect();
        println!("  行{}: {}", last + 1, shown.join(" | "));
    }
}

/// 进程峰值工作集（Windows）
fn peak_working_set_mb() -> f64 {
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
                return pmc.PeakWorkingSetSize as f64 / 1024.0 / 1024.0;
            }
        }
    }
    0.0
}
