//! 端到端回归：打开 → 类型推断 → 筛选 → 列操作(undo/redo) → 拆分 → 导出。
//! 用法: e2e.exe <csv路径> [输出目录]

use std::time::Instant;

use csv_editor_rust::core::column_ops::{ColumnOps, HashAlgo};
use csv_editor_rust::core::export;
use csv_editor_rust::core::filter::{apply_filter, FilterCondition, FilterOp, Logic};
use csv_editor_rust::core::model::DataModel;
use csv_editor_rust::core::split::{self, OutputFormat, SplitOptions};

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
            Ok(n) => {
                split_count = n;
                println!("OK  按字段拆分: {} 个文件 ({:.2}s)", n, t.elapsed().as_secs_f64());
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

    if fails == 0 {
        println!("=== 端到端回归: 全部通过 ===");
    } else {
        println!("=== 端到端回归: {} 项失败 ===", fails);
        std::process::exit(1);
    }
    let _ = split_count;
}
