//! 对照验证工具：与 Python 参照脚本（testdata/verify_ref.py）输出相同结构的
//! JSON 结果（筛选行号 + 行哈希），用于验证 Rust 实现与 Python 语义一致。
//! 用法: verify.exe <csv路径>

use csv_editor_rust::core::column_ops::HashAlgo;
use csv_editor_rust::core::filter::{apply_filter, FilterCondition, FilterOp, Logic};
use csv_editor_rust::core::hash::compute_row_hash;
use csv_editor_rust::core::model::DataModel;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("用法: verify <csv路径>");
        std::process::exit(2);
    }
    let m = DataModel::open(&args[1], 1000).unwrap();
    let header = m.header().to_vec();
    let col = |name: &str| header.iter().position(|h| h == name).unwrap_or(usize::MAX);

    let c_zh = col("交易账号");
    let c_amt = col("交易金额");
    let c_inv = col("无效标签_新增");
    let c_bank = col("数据来源_银行名称");

    let cond = |column: usize, op: FilterOp, value: &str| FilterCondition {
        column,
        op,
        value: value.to_string(),
        logic: Logic::And,
    };

    let hits1 = apply_filter(&m, &[cond(c_zh, FilterOp::Equal, "15270188000168848")], None);
    let hits2 = apply_filter(&m, &[cond(c_amt, FilterOp::Greater, "100")], None);
    let hits3 = apply_filter(&m, &[cond(c_inv, FilterOp::Equal, "无效")], None);
    let hits4 = apply_filter(&m, &[cond(c_bank, FilterOp::Contains, "银行")], None);
    let hashes: Vec<String> = (0..50)
        .map(|r| compute_row_hash(&m, r, &[0, c_zh], HashAlgo::Md5))
        .collect();

    println!(
        "{{\"rows\":{},\"cols\":{},\"hits1\":{:?},\"hits2\":{:?},\"hits3\":{:?},\"hits4\":{:?},\"hashes\":{:?}}}",
        m.row_count(),
        m.column_count(),
        hits1,
        hits2,
        hits3,
        hits4,
        hashes
    );
}
