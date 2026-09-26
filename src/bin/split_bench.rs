//! 临时性能探针（用完即删）：数据拆分（按行数 / 按字段 / 按字段 XLSX）的
//! 墙钟耗时、进程 CPU 时间与平均并行度（CPU/墙钟，接近 1.0 表示只用了单核）。
//!
//! 用法：cargo run --release --bin split_bench [行数]

use std::io::Write;
use std::path::Path;
use std::time::Instant;

use csv_editor_rust::core::model::DataModel;
use csv_editor_rust::core::split::{self, OutputFormat, SplitOptions};

/// 进程累计 CPU 时间（内核 + 用户，单位秒）
fn process_cpu_seconds() -> f64 {
    #[cfg(windows)]
    unsafe {
        use winapi::shared::minwindef::FILETIME;
        use winapi::um::processthreadsapi::{GetCurrentProcess, GetProcessTimes};
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut c, mut e, mut k, mut u) = (zero, zero, zero, zero);
        if GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) != 0 {
            let t = |f: FILETIME| {
                (((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64) as f64
            };
            return (t(k) + t(u)) / 1e7; // 100ns 单位 → 秒
        }
    }
    0.0
}

struct Case {
    name: String,
    wall: f64,
    cpu: f64,
    extra: String,
}

/// 重复 repeat 次，取**最小**墙钟时间（单次测量有 ±10% 抖动，最小值最稳定）
fn bench(name: &str, repeat: usize, f: impl Fn() -> String) -> Case {
    let mut best = f64::MAX;
    let mut best_cpu = 0.0;
    let mut extra = String::new();
    for _ in 0..repeat.max(1) {
        let w0 = Instant::now();
        let c0 = process_cpu_seconds();
        extra = f();
        let wall = w0.elapsed().as_secs_f64();
        let cpu = process_cpu_seconds() - c0;
        if wall < best {
            best = wall;
            best_cpu = cpu;
        }
    }
    Case {
        name: name.to_string(),
        wall: best,
        cpu: best_cpu,
        extra,
    }
}

fn gen_csv(path: &Path, rows: usize) -> u64 {
    let f = std::fs::File::create(path).unwrap();
    let mut w = std::io::BufWriter::with_capacity(1 << 20, f);
    w.write_all(
        "ID,银行,账号,交易金额,交易时间,摘要,城市,网点,币种,渠道,状态,备注1,备注2,备注3,序号\n"
            .as_bytes(),
    )
    .unwrap();
    for i in 0..rows {
        writeln!(
            w,
            "{},银行{},62220202001{:09},{}  ,2026-01-{:02} 10:{:02}:{:02},摘要文本内容{},城市{},网点{},CNY,手机银行,成功,备注{},备注{},备注{},{}",
            i + 1,
            i % 20 + 1,
            123456789u64 % 1000000000 + i as u64 % 1000000000,
            (i % 100000) as f64 / 100.0,
            i % 28 + 1,
            i % 60,
            i % 60,
            i % 1000,
            i % 100,
            i % 500,
            i % 7,
            i % 31,
            i % 97,
            i
        )
        .unwrap();
    }
    w.flush().unwrap();
    std::fs::metadata(path).unwrap().len()
}

fn main() {
    let rows: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000_000);
    let repeat: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    let dir = std::env::temp_dir().join("split_bench");
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("bench_src.csv");
    if !src.exists() {
        let t = Instant::now();
        let sz = gen_csv(&src, rows);
        println!(
            "生成测试数据：{} 行，{:.1} MB，{:.1} 秒",
            rows,
            sz as f64 / 1e6,
            t.elapsed().as_secs_f64()
        );
    }
    let src_s = src.to_string_lossy().into_owned();

    let t = Instant::now();
    let m = DataModel::open(&src_s, 100).unwrap();
    println!(
        "打开文件：{} 行 × {} 列，{:.2} 秒（含建行索引）",
        m.row_count(),
        m.column_count(),
        t.elapsed().as_secs_f64()
    );
    println!(
        "逻辑核数：{}\n",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0)
    );

    let out_rows = dir.join("out_rows");
    let out_field = dir.join("out_field");
    let out_xlsx = dir.join("out_xlsx");
    for d in [&out_rows, &out_field, &out_xlsx] {
        let _ = std::fs::remove_dir_all(d);
        std::fs::create_dir_all(d).unwrap(); // 拆分不会自动建目录
    }

    let mut results = Vec::new();

    results.push(bench("按行数拆分 CSV（每批 10 万行）", repeat, || {
        let opts = SplitOptions {
            folder: out_rows.to_string_lossy().into_owned(),
            format: OutputFormat::Csv,
        };
        let rep = split::split_by_rows(&m, None, 100_000, &opts, None).unwrap();
        format!(
            "{} 个文件 / {} 行{}",
            rep.files,
            rep.total_on_disk(),
            if rep.is_complete() { "（核对通过）" } else { "（核对未通过）" }
        )
    }));

    results.push(bench("按字段拆分 CSV（银行列，20 个唯一值）", repeat, || {
        let opts = SplitOptions {
            folder: out_field.to_string_lossy().into_owned(),
            format: OutputFormat::Csv,
        };
        let rep = split::split_by_field(&m, None, &[1], &opts, None).unwrap();
        format!(
            "{} 个文件 / {} 行{}",
            rep.files,
            rep.total_on_disk(),
            if rep.is_complete() { "（核对通过）" } else { "（核对未通过）" }
        )
    }));

    results.push(bench("按字段拆分 XLSX（两阶段，20 组）", repeat, || {
        let opts = SplitOptions {
            folder: out_xlsx.to_string_lossy().into_owned(),
            format: OutputFormat::Xlsx,
        };
        let rep = split::split_by_field(&m, None, &[1], &opts, None).unwrap();
        format!(
            "{} 个文件 / {} 行{}",
            rep.files,
            rep.total_on_disk(),
            if rep.is_complete() { "（核对通过）" } else { "（核对未通过）" }
        )
    }));

    // 参考项：并行筛选（项目里少数已并行的路径），用于对照计算密集型的并行度
    results.push(bench("参考：并行筛选（金额 > 100）", repeat, || {
        use csv_editor_rust::core::filter::{apply_filter, FilterCondition, FilterOp, Logic};
        let hits = apply_filter(
            &m,
            &[FilterCondition {
                column: 3,
                op: FilterOp::Greater,
                value: "100".into(),
                logic: Logic::And,
            }],
            None,
        );
        format!("命中 {} 行", hits.len())
    }));

    println!(
        "\n{:<38} {:>9} {:>9} {:>8}  {}",
        "场景", "耗时(s)", "CPU(s)", "并行度", "产出"
    );
    println!("{}", "-".repeat(92));
    for r in &results {
        let par = if r.wall > 0.0 { r.cpu / r.wall } else { 0.0 };
        println!(
            "{:<38} {:>9.2} {:>9.2} {:>8.2}  {}",
            r.name, r.wall, r.cpu, par, r.extra
        );
    }
    println!("\n并行度 = CPU时间 / 墙钟时间；≈1.0 说明只利用了单核。");
    println!("（每个场景重复 {repeat} 次，表中为最小墙钟时间）");
}
