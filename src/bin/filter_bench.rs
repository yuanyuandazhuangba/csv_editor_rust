//! 临时性能探针：数据筛选在不同条件类型/数量下的耗时、CPU 时间与平均并行度。
//!
//! 用法：cargo run --release --bin filter_bench [行数] [重复次数]
//! 复用 split_bench 生成的同一份合成数据（若不存在则生成）。

use std::io::Write;
use std::path::Path;
use std::time::Instant;

use csv_editor_rust::core::filter::{apply_filter, FilterCondition, FilterOp, Logic};
use csv_editor_rust::core::model::DataModel;

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
            return (t(k) + t(u)) / 1e7;
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
        let sz = gen_csv(&src, rows);
        println!("生成测试数据：{:.1} MB", sz as f64 / 1e6);
    }
    let m = DataModel::open(&src.to_string_lossy().into_owned(), 100).unwrap();
    println!(
        "数据：{} 行 × {} 列；逻辑核数 {}",
        m.row_count(),
        m.column_count(),
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0)
    );

    let c = |column: usize, op: FilterOp, value: &str, logic: Logic| FilterCondition {
        column,
        op,
        value: value.to_string(),
        logic,
    };
    let run = |conds: Vec<FilterCondition>| -> String {
        let hits = apply_filter(&m, &conds, None);
        format!("命中 {}", hits.len())
    };

    let mut results = Vec::new();

    results.push(bench("1 条件 数值比较（金额 > 100）", repeat, || {
        run(vec![c(3, FilterOp::Greater, "100", Logic::And)])
    }));
    results.push(bench("1 条件 字符串相等（银行 = 银行5）", repeat, || {
        run(vec![c(1, FilterOp::Equal, "银行5", Logic::And)])
    }));
    results.push(bench("1 条件 包含（摘要 contains）", repeat, || {
        run(vec![c(5, FilterOp::Contains, "内容1", Logic::And)])
    }));
    results.push(bench("1 条件 长数字精确相等（账号）", repeat, || {
        run(vec![c(
            2,
            FilterOp::Equal,
            "6222020200112345678",
            Logic::And,
        )])
    }));
    results.push(bench("1 条件 非空值（备注1）", repeat, || {
        run(vec![c(11, FilterOp::NotEmpty, "", Logic::And)])
    }));
    results.push(bench("3 条件 且（银行+金额+状态）", repeat, || {
        run(vec![
            c(1, FilterOp::Equal, "银行5", Logic::And),
            c(3, FilterOp::Greater, "100", Logic::And),
            c(10, FilterOp::Equal, "成功", Logic::And),
        ])
    }));
    results.push(bench("5 条件 混合 且/或", repeat, || {
        run(vec![
            c(1, FilterOp::Equal, "银行5", Logic::And),
            c(3, FilterOp::Greater, "100", Logic::And),
            c(5, FilterOp::Contains, "内容1", Logic::And),
            c(6, FilterOp::Equal, "城市3", Logic::Or),
            c(11, FilterOp::NotEmpty, "", Logic::Or),
        ])
    }));

    println!(
        "\n{:<34} {:>9} {:>9} {:>8}  {}",
        "场景", "耗时(s)", "CPU(s)", "并行度", "产出"
    );
    println!("{}", "-".repeat(88));
    for r in &results {
        let par = if r.wall > 0.0 { r.cpu / r.wall } else { 0.0 };
        println!(
            "{:<34} {:>9.3} {:>9.2} {:>8.2}  {}",
            r.name, r.wall, r.cpu, par, r.extra
        );
    }
    println!("\n并行度 = CPU时间 / 墙钟时间；≈1.0 说明只利用了单核。");
    println!("（每个场景重复 {repeat} 次，表中为最小墙钟时间）");
}
