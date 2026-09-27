//! 性能探针：字段分组汇总在不同分组深度/聚合方式下的墙钟耗时、进程 CPU 时间
//! 与平均并行度（CPU/墙钟，接近 1.0 表示只用了单核）。
//!
//! 用法：cargo run --release --bin stats_bench [行数] [重复次数]
//! 复用 split_bench / filter_bench 生成的同一份合成数据（若不存在则生成）。

use std::io::Write;
use std::path::Path;
use std::time::Instant;

use csv_editor_rust::core::model::DataModel;
use csv_editor_rust::core::stats::{group_by, AggKind, AggSpec};

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
    let m = DataModel::open(&src.to_string_lossy().into_owned(), 100).unwrap();
    println!(
        "数据：{} 行 × {} 列；逻辑核数 {}",
        m.row_count(),
        m.column_count(),
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0)
    );
    println!(
        "列：{}",
        m.header()
            .iter()
            .enumerate()
            .map(|(i, h)| format!("{}={}", i, h))
            .collect::<Vec<_>>()
            .join("  ")
    );

    // 列索引（与 gen_csv 的表头一致）：1=银行 2=账号 3=交易金额 6=城市 11=备注1
    let sp = |column: usize, kind: AggKind| AggSpec { column, kind };
    let run = |gcols: &[usize], aggs: &[AggSpec]| -> String {
        match group_by(&m, None, gcols, aggs, None) {
            Ok(t) => format!("{} 组 / {} 聚合", t.group_count(), t.aggs),
            Err(e) => format!("失败：{}", e),
        }
    };

    let mut results = Vec::new();

    results.push(bench("1 字段分组（银行 20 组）×求和/均值/最大/最小", repeat, || {
        run(
            &[1],
            &[
                sp(3, AggKind::Sum),
                sp(3, AggKind::Avg),
                sp(3, AggKind::Max),
                sp(3, AggKind::Min),
            ],
        )
    }));
    results.push(bench("1 字段分组（银行）×计数/空值数（备注1）", repeat, || {
        run(&[1], &[sp(11, AggKind::Count), sp(11, AggKind::EmptyCount)])
    }));
    results.push(bench("1 字段分组（银行）×去重计数（账号）", repeat, || {
        run(&[1], &[sp(2, AggKind::DistinctCount)])
    }));
    results.push(bench("2 字段分组（银行×城市 ≤2000 组）×求和/计数", repeat, || {
        run(&[1, 6], &[sp(3, AggKind::Sum), sp(3, AggKind::Count)])
    }));
    results.push(bench("3 字段分组（银行×城市×备注1）×求和", repeat, || {
        run(&[1, 6, 11], &[sp(3, AggKind::Sum)])
    }));
    results.push(bench("2 字段分组 × 6 聚合列（金额+账号）", repeat, || {
        run(
            &[1, 6],
            &[
                sp(3, AggKind::Sum),
                sp(3, AggKind::Avg),
                sp(3, AggKind::Max),
                sp(3, AggKind::Min),
                sp(2, AggKind::DistinctCount),
                sp(2, AggKind::Count),
            ],
        )
    }));

    // 数值列统计（含中位数，需要保留并排序原始值）
    let describe = |cols: &[usize]| -> String {
        match csv_editor_rust::core::stats::describe_columns(&m, None, cols, None) {
            Ok(t) => format!("{} 列", t.rows.len()),
            Err(e) => format!("失败：{}", e),
        }
    };
    results.push(bench("列统计：1 列（金额，2 位小数）", repeat, || {
        describe(&[3])
    }));
    results.push(bench("列统计：4 列（金额/账号/网点/序号）", repeat, || {
        describe(&[3, 2, 7, 14])
    }));
    results.push(bench("列统计：10 列（含长数字列）", repeat, || {
        describe(&[0, 1, 2, 3, 4, 6, 7, 10, 11, 14])
    }));

    // 频次分布 / Top-N（每列需维护取值→次数表）
    let freq = |cols: &[usize], top_n: usize| -> String {
        match csv_editor_rust::core::stats::value_frequency(&m, None, cols, top_n, None) {
            Ok(t) => format!("{} 项", t.rows.len()),
            Err(e) => format!("失败：{}", e),
        }
    };
    results.push(bench("频次分布：1 列（银行，20 个取值）Top-20", repeat, || {
        freq(&[1], 20)
    }));
    results.push(bench("频次分布：3 列（银行/城市/网点）Top-20", repeat, || {
        freq(&[1, 6, 7], 20)
    }));
    results.push(bench("频次分布：5 列 Top-20", repeat, || {
        freq(&[1, 6, 7, 10, 11], 20)
    }));
    results.push(bench("频次分布：5 列 Top-1000", repeat, || {
        freq(&[1, 6, 7, 10, 11], 1000)
    }));
    // 护栏演示：账号是 19 位高基数列（200 万行几乎全是唯一值），超过每列不同取值上限
    // 时会拒绝统计（Top-N 依赖完整计数，宁可拒绝也不给不完整的结果）
    results.push(bench(
        "频次分布：1 列（账号，200 万唯一值）→ 预期拒绝",
        repeat,
        || {
            let s = freq(&[2], 20);
            let short: String = s.chars().take(24).collect();
            format!("{}…", short)
        },
    ));

    println!(
        "\n{:<44} {:>9} {:>9} {:>8}  {}",
        "场景", "耗时(s)", "CPU(s)", "并行度", "产出"
    );
    println!("{}", "-".repeat(96));
    for r in &results {
        let par = if r.wall > 0.0 { r.cpu / r.wall } else { 0.0 };
        println!(
            "{:<44} {:>9.3} {:>9.2} {:>8.2}  {}",
            r.name, r.wall, r.cpu, par, r.extra
        );
    }
    println!("\n并行度 = CPU时间 / 墙钟时间；明显 >1 说明多核并行生效。");
    println!("（每个场景重复 {repeat} 次，表中为最小墙钟时间）");
}
