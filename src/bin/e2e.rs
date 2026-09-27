//! 端到端回归：打开 → 类型推断 → 筛选 → 列操作(undo/redo) → 拆分 → 导出。
//! 用法: e2e.exe <csv路径> [输出目录]

use std::time::Instant;

use csv_editor_rust::core::column_ops::{ColumnOps, HashAlgo};
use csv_editor_rust::core::export;
use csv_editor_rust::core::filter::{apply_filter, FilterCondition, FilterOp, Logic};
use csv_editor_rust::core::model::DataModel;
use csv_editor_rust::core::split::{self, OutputFormat, SplitOptions};
use csv_editor_rust::core::stats::{self, AggKind, AggSpec};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("用法: e2e <csv路径> [输出目录]");
        std::process::exit(2);
    }
    let path = &args[1];
    let out_dir = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| std::env::temp_dir().join("csv_e2e").to_string_lossy().into_owned());
    std::fs::create_dir_all(&out_dir).unwrap();

    println!("=== 端到端回归: {} ===", path);
    let mut fails = 0usize;

    // 1. 打开 + 类型推断
    let t = Instant::now();
    let m = match DataModel::open(path, 1000) {
        Ok(m) => m,
        Err(e) => {
            println!("FAIL 打开: {}", e);
            return;
        }
    };
    println!("OK  打开: {} 行/{} 列/{} ({:.2}s)", m.row_count(), m.column_count(), m.encoding_label(), t.elapsed().as_secs_f64());

    let header = m.header().to_vec();
    let col_of = |name: &str| header.iter().position(|h| h == name);

    // 2. 筛选：金额 > 100（若存在该列）
    let mut filtered: Option<Vec<u32>> = None;
    if let Some(c_amt) = col_of("交易金额") {
        let conds = vec![FilterCondition {
            column: c_amt,
            op: FilterOp::Greater,
            value: "100".into(),
            logic: Logic::And,
        }];
        let t = Instant::now();
        let hits = apply_filter(&m, &conds, None);
        println!("OK  筛选 交易金额>100: {} 行 ({:.2}s)", hits.len(), t.elapsed().as_secs_f64());
        filtered = Some(hits);
    } else if let Some(c_id) = col_of("ID") {
        let conds = vec![FilterCondition {
            column: c_id,
            op: FilterOp::Greater,
            value: "0".into(),
            logic: Logic::And,
        }];
        let hits = apply_filter(&m, &conds, None);
        println!("OK  筛选 ID>0: {} 行", hits.len());
        filtered = Some(hits);
    } else {
        println!("WARN 无可用筛选列，跳过筛选");
    }

    // 3. 列操作：复制列 + 哈希列 + undo/redo
    let mut ops = ColumnOps::new(m.header());
    ops.copy_column(0, "dup_id").unwrap();
    ops.generate_hash(&[0], "row_hash", HashAlgo::Md5).unwrap();
    assert_eq!(ops.column_count(), m.column_count() + 2);
    ops.undo().unwrap();
    ops.undo().unwrap();
    assert_eq!(ops.column_count(), m.column_count());
    ops.redo().unwrap();
    ops.redo().unwrap();
    assert_eq!(ops.column_count(), m.column_count() + 2);
    let h = ops.field(&m, 0, ops.column_count() - 1);
    println!("OK  列操作(复制+哈希+undo/redo): 哈希长度 {}", h.len());

    // 4. 拆分：按第一个文本列（若合适）
    let mut split_count = 0usize;
    if let Some(c_bank) = col_of("数据来源_银行名称").or_else(|| col_of("ID")) {
        let opts = SplitOptions {
            folder: out_dir.clone(),
            format: OutputFormat::Csv,
        };
        let t = Instant::now();
        match split::split_by_field(&m, None, &[c_bank], &opts, None) {
            Ok(rep) => {
                split_count = rep.files;
                println!(
                    "OK  按字段拆分: {} 个文件 / {} 行（来源 {} 行），完整性核对: {} ({:.2}s)",
                    rep.files,
                    rep.total_on_disk(),
                    rep.source_rows,
                    if rep.is_complete() { "通过" } else { "未通过" },
                    t.elapsed().as_secs_f64()
                );
            }
            Err(e) => {
                println!("FAIL 拆分: {}", e);
                fails += 1;
            }
        }
    }

    // 5. 导出：筛选结果导出 CSV 与 XLSX
    let csv_path = format!("{}\\e2e_filtered.csv", out_dir);
    match export::export_view_csv(&m, &ops, filtered.as_deref(), &csv_path, None) {
        Ok(n) => println!("OK  导出 CSV: {} 行 -> {}", n, csv_path),
        Err(e) => {
            println!("FAIL 导出CSV: {}", e);
            fails += 1;
        }
    }
    let xlsx_path = format!("{}\\e2e_filtered.xlsx", out_dir);
    match export::export_view_xlsx(&m, &ops, filtered.as_deref(), &xlsx_path, None) {
        Ok(n) => println!("OK  导出 XLSX: {} 行 -> {}", n, xlsx_path),
        Err(e) => {
            println!("FAIL 导出XLSX: {}", e);
            fails += 1;
        }
    }

    // 6. 统计分析：字段分组汇总（按银行名称分组，对交易金额求和/均值/最大/最小）
    let group_col = col_of("数据来源_银行名称").or_else(|| col_of("银行"));
    let value_col = col_of("交易金额").or_else(|| col_of("ID"));
    if let (Some(c_g), Some(c_v)) = (group_col, value_col) {
        let aggs = [
            AggSpec {
                column: c_v,
                kind: AggKind::Sum,
            },
            AggSpec {
                column: c_v,
                kind: AggKind::Avg,
            },
            AggSpec {
                column: c_v,
                kind: AggKind::Max,
            },
            AggSpec {
                column: c_v,
                kind: AggKind::Min,
            },
        ];
        let t = Instant::now();
        match stats::group_by(&m, filtered.as_deref(), &[c_g], &aggs, None) {
            Ok(tb) => {
                // 内置一致性核对：各组「行数」之和应等于统计范围行数
                let sum_rows: u64 = tb
                    .rows
                    .iter()
                    .filter_map(|r| r.get(tb.group_columns).and_then(|v| v.parse::<u64>().ok()))
                    .sum();
                let rows_ok = sum_rows as usize == tb.input_rows;
                println!(
                    "OK  分组汇总 {}: {} 行 → {} 组（分组字段「{}」），行数核对 {} ({:.2}s)",
                    if filtered.is_some() { "(当前筛选结果)" } else { "(全量)" },
                    tb.input_rows,
                    tb.group_count(),
                    header.get(c_g).cloned().unwrap_or_default(),
                    if rows_ok { "通过" } else { "未通过" },
                    t.elapsed().as_secs_f64()
                );
                if !rows_ok {
                    fails += 1;
                }
                // 统计结果导出：CSV 与 XLSX
                let sc = format!("{}\\e2e_stats.csv", out_dir);
                match export::export_rows_csv(&sc, &tb.headers, tb.rows.iter().cloned()) {
                    Ok(n) => println!("OK  导出统计结果 CSV: {} 组 -> {}", n, sc),
                    Err(e) => {
                        println!("FAIL 导出统计结果 CSV: {}", e);
                        fails += 1;
                    }
                }
                let sx = format!("{}\\e2e_stats.xlsx", out_dir);
                match export::export_rows_xlsx(&sx, &tb.headers, tb.rows.iter().cloned()) {
                    Ok(n) => println!("OK  导出统计结果 XLSX: {} 组 -> {}", n, sx),
                    Err(e) => {
                        println!("FAIL 导出统计结果 XLSX: {}", e);
                        fails += 1;
                    }
                }
            }
            Err(e) => {
                println!("FAIL 分组汇总: {}", e);
                fails += 1;
            }
        }
    } else {
        println!("WARN 无可用于分组汇总的列，跳过统计");
    }

    // 7. 数值列统计：对数值列（与分组字段）做描述统计
    let mut col_stats_cols: Vec<usize> = Vec::new();
    if let Some(c) = value_col {
        col_stats_cols.push(c);
    }
    if let Some(c) = group_col {
        if !col_stats_cols.contains(&c) {
            col_stats_cols.push(c);
        }
    }
    if !col_stats_cols.is_empty() {
        let cols = col_stats_cols.clone();
        let t = Instant::now();
        match stats::describe_columns(&m, filtered.as_deref(), &cols, None) {
            Ok(tb) => {
                // 内置一致性核对：每列「计数 + 空值数 == 行数」
                let rows_ok = tb.rows.iter().all(|r| {
                    match (
                        r.get(1).and_then(|v| v.parse::<u64>().ok()),
                        r.get(2).and_then(|v| v.parse::<u64>().ok()),
                        r.get(3).and_then(|v| v.parse::<u64>().ok()),
                    ) {
                        (Some(rows), Some(count), Some(empty)) => count + empty == rows,
                        _ => false,
                    }
                });
                println!(
                    "OK  列统计 {}: {} 行 × {} 列（列名/计数/空值/求和/均值/中位数…），行数核对 {} ({:.2}s)",
                    if filtered.is_some() { "(当前筛选结果)" } else { "(全量)" },
                    tb.input_rows,
                    cols.len(),
                    if rows_ok { "通过" } else { "未通过" },
                    t.elapsed().as_secs_f64()
                );
                for note in &tb.notes {
                    println!("     注意：{}", note);
                }
                if !rows_ok {
                    fails += 1;
                }
                let sc = format!("{}\\e2e_colstats.csv", out_dir);
                match export::export_rows_csv(&sc, &tb.headers, tb.rows.iter().cloned()) {
                    Ok(n) => println!("OK  导出列统计 CSV: {} 列 -> {}", n, sc),
                    Err(e) => {
                        println!("FAIL 导出列统计 CSV: {}", e);
                        fails += 1;
                    }
                }
                let sx = format!("{}\\e2e_colstats.xlsx", out_dir);
                match export::export_rows_xlsx(&sx, &tb.headers, tb.rows.iter().cloned()) {
                    Ok(n) => println!("OK  导出列统计 XLSX: {} 列 -> {}", n, sx),
                    Err(e) => {
                        println!("FAIL 导出列统计 XLSX: {}", e);
                        fails += 1;
                    }
                }
            }
            Err(e) => {
                println!("FAIL 数值列统计: {}", e);
                fails += 1;
            }
        }
    } else {
        println!("WARN 无可用于数值列统计的列，跳过统计");
    }

    // 8. 频次分布 / Top-N：找出取值出现次数最多的前 N 项
    let mut freq_cols: Vec<usize> = Vec::new();
    if let Some(c) = group_col {
        freq_cols.push(c);
    }
    if let Some(c) = col_of("交易是否成功") {
        freq_cols.push(c);
    }
    if !freq_cols.is_empty() {
        let cols = freq_cols.clone();
        let t = Instant::now();
        match stats::value_frequency(&m, filtered.as_deref(), &cols, 20, None) {
            Ok(tb) => {
                // 内置一致性核对（每列分别校验）：
                //   1) 各取值行（含「(其余 M 个取值)」）的出现次数之和 == 统计范围行数
                //   2) 末行「(合计)」的出现次数 == 统计范围行数（占比 100%）
                // 同一列的项在结果表中连续排列，因此按列名变化分段累加即可。
                let mut sums: Vec<u64> = Vec::new();
                let mut totals: Vec<u64> = Vec::new();
                let mut cur: Option<String> = None;
                for r in &tb.rows {
                    if cur.as_deref() != Some(r[0].as_str()) {
                        cur = Some(r[0].clone());
                        sums.push(0);
                    }
                    let n = r[3].parse::<u64>().unwrap_or(0);
                    if r[2] == "(合计)" {
                        totals.push(n);
                    } else {
                        *sums.last_mut().unwrap() += n;
                    }
                }
                let rows_ok = sums.iter().all(|s| *s as usize == tb.input_rows)
                    && totals.iter().all(|s| *s as usize == tb.input_rows);
                println!(
                    "OK  频次分布 {}: 统计范围 {} 行 × {} 列，共 {} 项，计数核对 {} ({:.2}s)",
                    if filtered.is_some() { "(当前筛选结果)" } else { "(全量)" },
                    tb.input_rows,
                    cols.len(),
                    tb.rows.len(),
                    if rows_ok { "通过" } else { "未通过" },
                    t.elapsed().as_secs_f64()
                );
                if let Some(first) = tb.rows.first() {
                    println!(
                        "      首项：{} 的「{}」出现 {} 次（占比 {}%）",
                        first[0], first[2], first[3], first[4]
                    );
                }
                for note in &tb.notes {
                    println!("      {}", note);
                }
                if !rows_ok {
                    fails += 1;
                }
                let sc = format!("{}\\e2e_freq.csv", out_dir);
                match export::export_rows_csv(&sc, &tb.headers, tb.rows.iter().cloned()) {
                    Ok(n) => println!("OK  导出频次分布 CSV: {} 项 -> {}", n, sc),
                    Err(e) => {
                        println!("FAIL 导出频次分布 CSV: {}", e);
                        fails += 1;
                    }
                }
                let sx = format!("{}\\e2e_freq.xlsx", out_dir);
                match export::export_rows_xlsx(&sx, &tb.headers, tb.rows.iter().cloned()) {
                    Ok(n) => println!("OK  导出频次分布 XLSX: {} 项 -> {}", n, sx),
                    Err(e) => {
                        println!("FAIL 导出频次分布 XLSX: {}", e);
                        fails += 1;
                    }
                }
            }
            Err(e) => {
                println!("FAIL 频次分布: {}", e);
                fails += 1;
            }
        }
    } else {
        println!("WARN 无可用于频次分布的列，跳过统计");
    }

    // 9. 重复情况统计：找重复键、各重复几次、重复行占比，并给出可落回视图的行号
    let dup_key_col = col_of("交易方户名").or(group_col);
    if let Some(c_key) = dup_key_col {
        let t = Instant::now();
        match stats::duplicate_stats(&m, filtered.as_deref(), &[c_key], 100, None) {
            Ok(rep) => {
                // 内置核对：
                //   1) 不同键 + 重复行 == 统计范围行数（恒等式）
                //   2) 「重复涉及行」∪「唯一行」恰好覆盖统计范围全部行
                let mut all: Vec<u32> = rep
                    .dup_rows
                    .iter()
                    .chain(rep.unique_rows.iter())
                    .copied()
                    .collect();
                all.sort_unstable();
                all.dedup();
                let ok = rep.distinct_keys + rep.duplicate_rows == rep.input_rows
                    && all.len() == rep.input_rows
                    && rep.unique_rows.len() == rep.distinct_keys;
                println!(
                    "OK  重复情况 {}: {}，可落回视图（重复 {} 行 / 唯一 {} 行），核对 {} ({:.2}s)",
                    if filtered.is_some() { "(当前筛选结果)" } else { "(全量)" },
                    rep.summary(),
                    rep.dup_rows.len(),
                    rep.unique_rows.len(),
                    if ok { "通过" } else { "未通过" },
                    t.elapsed().as_secs_f64()
                );
                if let Some(top) = rep.table.rows.first() {
                    println!("      重复最多的键：{:?}", top);
                }
                if !ok {
                    fails += 1;
                }
                let sc = format!("{}\\e2e_dup.csv", out_dir);
                match export::export_rows_csv(&sc, &rep.table.headers, rep.table.rows.iter().cloned())
                {
                    Ok(n) => println!("OK  导出重复情况 CSV: {} 项 -> {}", n, sc),
                    Err(e) => {
                        println!("FAIL 导出重复情况 CSV: {}", e);
                        fails += 1;
                    }
                }
                let sx = format!("{}\\e2e_dup.xlsx", out_dir);
                match export::export_rows_xlsx(&sx, &rep.table.headers, rep.table.rows.iter().cloned())
                {
                    Ok(n) => println!("OK  导出重复情况 XLSX: {} 项 -> {}", n, sx),
                    Err(e) => {
                        println!("FAIL 导出重复情况 XLSX: {}", e);
                        fails += 1;
                    }
                }
            }
            Err(e) => {
                println!("FAIL 重复情况统计: {}", e);
                fails += 1;
            }
        }
    } else {
        println!("WARN 无可用于判重的列，跳过重复情况统计");
    }

    // 10. 空值与数据质量统计：逐列体检（这里取前若干列）
    let quality_cols: Vec<usize> = (0..m.column_count().min(8)).collect();
    if !quality_cols.is_empty() {
        let t = Instant::now();
        match stats::describe_quality(&m, filtered.as_deref(), &quality_cols, None) {
            Ok(tb) => {
                // 内置核对：空值率 = (空值数 + 纯空白数) / 行数；最长长度 ≥ 最短长度
                let rows_total = tb.input_rows as f64;
                let ok = tb.rows.iter().all(|r| {
                    let empty: f64 = r[2].parse().unwrap_or(0.0);
                    let blank: f64 = r[3].parse().unwrap_or(0.0);
                    let rate: f64 = r[4].parse().unwrap_or(-1.0);
                    let min_len: f64 = r[6].parse().unwrap_or(0.0);
                    let max_len: f64 = r[7].parse().unwrap_or(0.0);
                    let expect = if rows_total > 0.0 {
                        (empty + blank) * 100.0 / rows_total
                    } else {
                        0.0
                    };
                    (rate - expect).abs() < 0.05 && max_len >= min_len
                });
                println!(
                    "OK  数据质量 {}: {} 行 × {} 列，核对 {} ({:.2}s)",
                    if filtered.is_some() { "(当前筛选结果)" } else { "(全量)" },
                    tb.input_rows,
                    tb.rows.len(),
                    if ok { "通过" } else { "未通过" },
                    t.elapsed().as_secs_f64()
                );
                for note in tb.notes.iter().take(2) {
                    println!("      {}", note);
                }
                if !ok {
                    fails += 1;
                }
                let sc = format!("{}\\e2e_quality.csv", out_dir);
                match export::export_rows_csv(&sc, &tb.headers, tb.rows.iter().cloned()) {
                    Ok(n) => println!("OK  导出数据质量 CSV: {} 列 -> {}", n, sc),
                    Err(e) => {
                        println!("FAIL 导出数据质量 CSV: {}", e);
                        fails += 1;
                    }
                }
                let sx = format!("{}\\e2e_quality.xlsx", out_dir);
                match export::export_rows_xlsx(&sx, &tb.headers, tb.rows.iter().cloned()) {
                    Ok(n) => println!("OK  导出数据质量 XLSX: {} 列 -> {}", n, sx),
                    Err(e) => {
                        println!("FAIL 导出数据质量 XLSX: {}", e);
                        fails += 1;
                    }
                }
            }
            Err(e) => {
                println!("FAIL 数据质量统计: {}", e);
                fails += 1;
            }
        }
    }

    // 11. 透视表：行 × 列 × 值聚合（用求和核对「合计 == 各行/各列之和」）
    if let (Some(c_row), Some(c_col), Some(c_val)) = (
        col_of("数据来源_银行名称").or(group_col),
        col_of("交易是否成功").or(col_of("收付标志")),
        value_col,
    ) {
        let t = Instant::now();
        match stats::pivot_table(
            &m,
            filtered.as_deref(),
            &[c_row],
            c_col,
            &[stats::PivotValue {
                column: c_val,
                kind: AggKind::Sum,
            }],
            stats::PivotLimits::default(),
            None,
        ) {
            Ok(tb) => {
                // 核对：末行（合计行）各列 == 上面各行的对应列之和；末列 == 各行末列之和
                let last = tb.rows.len().saturating_sub(1);
                let col_ok = if tb.rows.len() > 1 {
                    (1..tb.headers.len()).all(|ci| {
                        let sum: f64 = tb.rows[..last]
                            .iter()
                            .map(|r| r.get(ci).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0))
                            .sum();
                        let total: f64 = tb.rows[last]
                            .get(ci)
                            .and_then(|v| v.parse::<f64>().ok())
                            .unwrap_or(0.0);
                        (sum - total).abs() < 0.01
                    })
                } else {
                    true
                };
                println!(
                    "OK  透视表 {}: {} 行 × {} 列（行字段「{}」× 列字段「{}」× 金额求和），核对 {} ({:.2}s)",
                    if filtered.is_some() { "(当前筛选结果)" } else { "(全量)" },
                    tb.rows.len(),
                    tb.group_columns,
                    header.get(c_row).cloned().unwrap_or_default(),
                    header.get(c_col).cloned().unwrap_or_default(),
                    if col_ok { "通过" } else { "未通过" },
                    t.elapsed().as_secs_f64()
                );
                if !col_ok {
                    fails += 1;
                }
                let sc = format!("{}\\e2e_pivot.csv", out_dir);
                match export::export_rows_csv(&sc, &tb.headers, tb.rows.iter().cloned()) {
                    Ok(n) => println!("OK  导出透视表 CSV: {} 行 -> {}", n, sc),
                    Err(e) => {
                        println!("FAIL 导出透视表 CSV: {}", e);
                        fails += 1;
                    }
                }
                let sx = format!("{}\\e2e_pivot.xlsx", out_dir);
                match export::export_rows_xlsx(&sx, &tb.headers, tb.rows.iter().cloned()) {
                    Ok(n) => println!("OK  导出透视表 XLSX: {} 行 -> {}", n, sx),
                    Err(e) => {
                        println!("FAIL 导出透视表 XLSX: {}", e);
                        fails += 1;
                    }
                }
            }
            Err(e) => {
                println!("FAIL 透视表: {}", e);
                fails += 1;
            }
        }
    } else {
        println!("WARN 无可用于透视表的列，跳过透视表");
    }

    // 12. 交叉 / 关联统计：两个字段的对应关系
    if let (Some(c_a), Some(c_b)) = (col_of("交易时间").or(value_col), group_col) {
        let t = Instant::now();
        match stats::cross_stats(&m, filtered.as_deref(), c_a, c_b, 1000, None) {
            Ok(rep) => {
                // 核对：明细各行「对应 B 的不同值数」之和 == 组合数；各行「行数」之和 == 统计范围行数
                let sum_pairs: u64 = rep
                    .table
                    .rows
                    .iter()
                    .filter(|r| !r[0].starts_with("(其余"))
                    .filter_map(|r| r[2].parse::<u64>().ok())
                    .sum();
                let sum_rows: u64 = rep
                    .table
                    .rows
                    .iter()
                    .filter(|r| !r[0].starts_with("(其余"))
                    .filter_map(|r| r[1].parse::<u64>().ok())
                    .sum();
                let ok = sum_pairs as usize == rep.pairs
                    && sum_rows as usize == rep.input_rows
                    && rep.max_b_per_a <= rep.b_values
                    && rep.max_a_per_b <= rep.a_values;
                println!(
                    "OK  交叉统计 {}: {}，核对 {} ({:.2}s)",
                    if filtered.is_some() { "(当前筛选结果)" } else { "(全量)" },
                    rep.summary(),
                    if ok { "通过" } else { "未通过" },
                    t.elapsed().as_secs_f64()
                );
                if let Some(top) = rep.table.rows.first() {
                    println!("      明细首行：{:?}", top);
                }
                if !ok {
                    fails += 1;
                }
                let mut tb = rep.table.clone();
                tb.notes.insert(0, rep.summary());
                let sc = format!("{}\\e2e_cross.csv", out_dir);
                match export::export_rows_csv(&sc, &tb.headers, tb.rows.iter().cloned()) {
                    Ok(n) => println!("OK  导出交叉统计 CSV: {} 项 -> {}", n, sc),
                    Err(e) => {
                        println!("FAIL 导出交叉统计 CSV: {}", e);
                        fails += 1;
                    }
                }
                let sx = format!("{}\\e2e_cross.xlsx", out_dir);
                match export::export_rows_xlsx(&sx, &tb.headers, tb.rows.iter().cloned()) {
                    Ok(n) => println!("OK  导出交叉统计 XLSX: {} 项 -> {}", n, sx),
                    Err(e) => {
                        println!("FAIL 导出交叉统计 XLSX: {}", e);
                        fails += 1;
                    }
                }
            }
            Err(e) => {
                println!("FAIL 交叉/关联统计: {}", e);
                fails += 1;
            }
        }
    } else {
        println!("WARN 无可用于交叉统计的列，跳过交叉统计");
    }

    if fails == 0 {
        println!("=== 端到端回归: 全部通过 ===");    } else {
        println!("=== 端到端回归: {} 项失败 ===", fails);
        std::process::exit(1);
    }
    let _ = split_count;
}
