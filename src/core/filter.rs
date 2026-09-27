//! 筛选引擎：10 种条件 + 且/或组合，并行流式评估 → 命中行号集合。
//!
//! 语义与 Python 版对齐：
//! - 10 种条件：等于/包含/不包含/大于/小于/大于等于/小于等于/不等于/空值/非空值；
//! - 条件间用 且/或 组合：首个条件必选，后续条件按自身 logic 与累积结果组合；
//! - 数值比较优先（整数用 i128 精确比较，覆盖 19 位卡号；浮点回退 f64），
//!   双方均可数值化时数值比较，否则字符串比较（trim 后）。
//!
//! 性能：并行分块评估（行级），900 万行目标 < 10 秒；结果行号 u32。

use std::cmp::Ordering;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use super::model::DataModel;
use super::reader::RowReader;

/// 筛选并行度上限。筛选是纯 CPU + 内存带宽密集型（不像拆分那样受写入 IO 限制），
/// 但并发过高会让内存带宽成为瓶颈、收益递减；且行数很少时也没必要开满。
const MAX_FILTER_THREADS: usize = 32;

/// 条件间逻辑关系
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Logic {
    And,
    Or,
}

/// 筛选操作
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    Equal,
    Contains,
    NotContains,
    Greater,
    Less,
    GreaterEqual,
    LessEqual,
    NotEqual,
    IsEmpty,
    NotEmpty,
}

impl FilterOp {
    /// 中文标签（UI 显示）
    pub fn label(&self) -> &'static str {
        match self {
            FilterOp::Equal => "等于",
            FilterOp::Contains => "包含",
            FilterOp::NotContains => "不包含",
            FilterOp::Greater => "大于",
            FilterOp::Less => "小于",
            FilterOp::GreaterEqual => "大于等于",
            FilterOp::LessEqual => "小于等于",
            FilterOp::NotEqual => "不等于",
            FilterOp::IsEmpty => "空值",
            FilterOp::NotEmpty => "非空值",
        }
    }

    pub fn from_label(s: &str) -> Option<FilterOp> {
        match s {
            "等于" => Some(FilterOp::Equal),
            "包含" => Some(FilterOp::Contains),
            "不包含" => Some(FilterOp::NotContains),
            "大于" => Some(FilterOp::Greater),
            "小于" => Some(FilterOp::Less),
            "大于等于" => Some(FilterOp::GreaterEqual),
            "小于等于" => Some(FilterOp::LessEqual),
            "不等于" => Some(FilterOp::NotEqual),
            "空值" => Some(FilterOp::IsEmpty),
            "非空值" => Some(FilterOp::NotEmpty),
            _ => None,
        }
    }

    /// 是否需要输入值
    pub fn needs_value(&self) -> bool {
        !matches!(self, FilterOp::IsEmpty | FilterOp::NotEmpty)
    }
}

/// 单条筛选条件
#[derive(Debug, Clone)]
pub struct FilterCondition {
    /// 列索引（0 起始，不含表头）
    pub column: usize,
    pub op: FilterOp,
    pub value: String,
    /// 与前一条件的组合方式（首个条件忽略）
    pub logic: Logic,
}

/// 评估单行是否命中（逐条件累积，带 AND 短路）。
///
/// 字段值来自已载入的行缓存 `reader`：**整行只解析一次**，同行的多个条件共享结果。
/// 此前每个条件各调一次 `model.field(row, col)`，每次都要重新解析整行
/// （分配 `Vec<Vec<u8>>`、每字段一个 `Vec<u8>` 与一个 `String`），
/// 条件越多重复解析越多（即使只取一列也要解析全部列）。
fn eval_row(reader: &RowReader, conds: &[FilterCondition]) -> bool {
    let mut acc = eval_one(reader, &conds[0]);
    for c in &conds[1..] {
        if c.logic == Logic::And && !acc {
            return false; // AND 短路
        }
        let sub = eval_one(reader, c);
        acc = match c.logic {
            Logic::And => acc && sub,
            Logic::Or => acc || sub,
        };
    }
    acc
}

/// 单条件评估（字段值取自行缓存）
fn eval_one(reader: &RowReader, c: &FilterCondition) -> bool {
    let v = reader.value(c.column);
    let v = v.trim();
    match c.op {
        FilterOp::IsEmpty => v.is_empty(),
        FilterOp::NotEmpty => !v.is_empty(),
        FilterOp::Equal => cmp_equal(v, &c.value),
        FilterOp::NotEqual => !cmp_equal(v, &c.value),
        FilterOp::Contains => v.contains(c.value.trim()),
        FilterOp::NotContains => !v.contains(c.value.trim()),
        FilterOp::Greater => cmp_values(v, &c.value) == Some(Ordering::Greater),
        FilterOp::Less => cmp_values(v, &c.value) == Some(Ordering::Less),
        FilterOp::GreaterEqual => matches!(
            cmp_values(v, &c.value),
            Some(Ordering::Greater) | Some(Ordering::Equal)
        ),
        FilterOp::LessEqual => matches!(
            cmp_values(v, &c.value),
            Some(Ordering::Less) | Some(Ordering::Equal)
        ),
    }
}

/// 数值化尝试：整数（i128，覆盖 19 位卡号）或浮点。
///
/// 与 `stats.rs`（分组汇总的求和/均值/极值）共用同一套解析与比较语义：
/// 筛选里"金额 > 100 命中"与统计里"金额求和包含该行"必须一致，
/// 因此这两处刻意复用同一实现，而不是各写一份。
#[derive(Debug, Clone, Copy)]
pub(crate) enum Num {
    Int(i128),
    Float(f64),
}

pub(crate) fn try_num(s: &str) -> Option<Num> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    if let Ok(i) = t.parse::<i128>() {
        return Some(Num::Int(i));
    }
    t.parse::<f64>().ok().map(Num::Float)
}

pub(crate) fn cmp_num(a: &Num, b: &Num) -> Ordering {
    match (a, b) {
        (Num::Int(x), Num::Int(y)) => x.cmp(y),
        (Num::Int(x), Num::Float(y)) => (*x as f64).partial_cmp(y).unwrap_or(Ordering::Equal),
        (Num::Float(x), Num::Int(y)) => x.partial_cmp(&(*y as f64)).unwrap_or(Ordering::Equal),
        (Num::Float(x), Num::Float(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
    }
}

/// 等于：双方可数值化时数值比较，否则字符串比较
fn cmp_equal(a: &str, b: &str) -> bool {
    match (try_num(a), try_num(b)) {
        (Some(x), Some(y)) => cmp_num(&x, &y) == Ordering::Equal,
        _ => a == b,
    }
}

/// 大小比较：双方可数值化时数值比较，否则回退字符串比较
fn cmp_values(a: &str, b: &str) -> Option<Ordering> {
    match (try_num(a), try_num(b)) {
        (Some(x), Some(y)) => Some(cmp_num(&x, &y)),
        _ => Some(a.cmp(b)),
    }
}

/// 应用筛选，返回命中行号（数据行，0 起始，升序）。
///
/// 并行分块评估；进度回调可选（行进度 0..=1.0）。
pub fn apply_filter(
    model: &DataModel,
    conds: &[FilterCondition],
    progress: Option<&(dyn Fn(f64) + Sync)>,
) -> Vec<u32> {
    let n = model.row_count();
    if n == 0 {
        return Vec::new();
    }
    if conds.is_empty() {
        return (0..n as u32).collect();
    }

    // 本次筛选涉及的所有列（去重后交给每线程的行读取器，做到每行只解析一次）
    let need_cols: Vec<usize> = conds.iter().map(|c| c.column).collect();

    let threads = std::thread::available_parallelism()
        .map(|x| x.get())
        .unwrap_or(4)
        .min(MAX_FILTER_THREADS)
        .min(n);
    let chunk = (n / threads).max(1);
    let done = AtomicUsize::new(0);

    let mut results: Vec<Vec<u32>> = Vec::with_capacity(threads + 1);
    std::thread::scope(|s| {
        let mut handles = Vec::new();
        let mut start = 0usize;
        while start < n {
            let end = (start + chunk).min(n);
            let model_ref = model;
            let conds_ref = conds;
            let cols = need_cols.clone();
            let done_ref = &done;
            let prog = progress;
            handles.push(s.spawn(move || {
                // 线程局部的行读取器：解析缓冲与字段文本都在线程内复用（零分配）
                let mut reader = RowReader::new(cols);
                let mut out = Vec::new();
                for row in start..end {
                    reader.load(model_ref.file(), row);
                    if eval_row(&reader, conds_ref) {
                        out.push(row as u32);
                    }
                }
                done_ref.fetch_add(end - start, AtomicOrdering::Relaxed);
                if let Some(p) = prog {
                    let d = done_ref.load(AtomicOrdering::Relaxed);
                    p(d as f64 / n as f64);
                }
                out
            }));
            start = end;
        }
        for h in handles {
            results.push(h.join().unwrap_or_default());
        }
    });

    let total: usize = results.iter().map(|v| v.len()).sum();
    let mut out = Vec::with_capacity(total);
    for r in results {
        out.extend(r);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::DataModel;

    fn write_temp(data: &[u8], suffix: &str) -> String {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("csv_filter_test_{}{}", std::process::id(), suffix));
        std::fs::write(&path, data).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn cond(col: usize, op: FilterOp, value: &str, logic: Logic) -> FilterCondition {
        FilterCondition {
            column: col,
            op,
            value: value.to_string(),
            logic,
        }
    }

    #[test]
    fn filter_equal_numeric_and_string() {
        let p = write_temp(
            b"ID,NAME\n1,alice\n2,bob\n10,carol\n100,dave\n",
            "_eq.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        // 数值等于：字段"10" == 值"10"
        let hits = apply_filter(&m, &[cond(0, FilterOp::Equal, "10", Logic::And)], None);
        assert_eq!(hits, vec![2]);
        // 字符串等于（NAME）
        let hits = apply_filter(&m, &[cond(1, FilterOp::Equal, "bob", Logic::And)], None);
        assert_eq!(hits, vec![1]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn filter_contains_notcontains() {
        let p = write_temp(b"ID,NAME\n1,alice\n2,bob\n3,carol\n", "_contains.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let hits = apply_filter(&m, &[cond(1, FilterOp::Contains, "a", Logic::And)], None);
        assert_eq!(hits, vec![0, 2]); // alice, carol
        let hits = apply_filter(&m, &[cond(1, FilterOp::NotContains, "o", Logic::And)], None);
        assert_eq!(hits, vec![0]); // alice
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn filter_comparisons() {
        let p = write_temp(b"ID,AMOUNT\n1,100\n2,200.5\n3,50\n", "_cmp.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let hits = apply_filter(&m, &[cond(1, FilterOp::Greater, "100", Logic::And)], None);
        assert_eq!(hits, vec![1]); // 200.5
        let hits = apply_filter(&m, &[cond(1, FilterOp::LessEqual, "100", Logic::And)], None);
        assert_eq!(hits, vec![0, 2]); // 100, 50
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn filter_text_format_numbers_still_work() {
        // 回归：读取全文本后，文本格式数字（金额/前导零/长数字）的筛选仍按数值比较
        use crate::core::reader::TypeKind;
        let p = write_temp(
            b"AMOUNT,CARD,CODE\n2553.34,6222020200112345678,007\n100,6222020200112345679,008\n50,6222020200112345680,009\n",
            "_textnum.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        // 读取全文本：列类型均为文本
        assert_eq!(m.columns()[0].type_kind, TypeKind::Text);
        assert_eq!(m.columns()[1].type_kind, TypeKind::Text);
        // 金额 > 200：字段是文本 "2553.34"/"100"，仍按数值比较
        let hits = apply_filter(&m, &[cond(0, FilterOp::Greater, "200", Logic::And)], None);
        assert_eq!(hits, vec![0]);
        // 前导零 "007" 与条件 "7" 数值比较相等（文本格式数字筛选不受影响）
        let hits = apply_filter(&m, &[cond(2, FilterOp::Equal, "7", Logic::And)], None);
        assert_eq!(hits, vec![0]);
        // 19 位卡号等于精确匹配（i128，无精度丢失）
        let hits = apply_filter(
            &m,
            &[cond(1, FilterOp::Equal, "6222020200112345680", Logic::And)],
            None,
        );
        assert_eq!(hits, vec![2]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn filter_long_number_exact() {
        // 19 位卡号精确相等（i128，无精度丢失）
        let p = write_temp(
            b"ID,CARD\n1,6222020200112345678\n2,6222020200112345679\n",
            "_long.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let hits = apply_filter(&m, &[cond(1, FilterOp::Equal, "6222020200112345679", Logic::And)], None);
        assert_eq!(hits, vec![1]);
        let hits = apply_filter(&m, &[cond(1, FilterOp::Greater, "6222020200112345678", Logic::And)], None);
        assert_eq!(hits, vec![1]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn filter_empty_notempty() {
        let p = write_temp(b"ID,NOTE\n1,\n2,abc\n3,  \n", "_empty.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let hits = apply_filter(&m, &[cond(1, FilterOp::IsEmpty, "", Logic::And)], None);
        assert_eq!(hits, vec![0, 2]);
        let hits = apply_filter(&m, &[cond(1, FilterOp::NotEmpty, "", Logic::And)], None);
        assert_eq!(hits, vec![1]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn filter_and_or_combination() {
        let p = write_temp(
            "ID,CITY,AGE\n1,北京,30\n2,上海,25\n3,北京,20\n".as_bytes(),
            "_combo.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        // 城市=北京 且 年龄>=25
        let hits = apply_filter(
            &m,
            &[
                cond(1, FilterOp::Equal, "北京", Logic::And),
                cond(2, FilterOp::GreaterEqual, "25", Logic::And),
            ],
            None,
        );
        assert_eq!(hits, vec![0]);
        // 城市=北京 或 年龄<25 → 北京(0,2) + 年龄<25(2)；上海25岁不命中
        let hits = apply_filter(
            &m,
            &[
                cond(1, FilterOp::Equal, "北京", Logic::And),
                cond(2, FilterOp::Less, "25", Logic::Or),
            ],
            None,
        );
        assert_eq!(hits, vec![0, 2]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn filter_empty_conditions_returns_all() {
        let p = write_temp(b"ID\n1\n2\n3\n", "_all.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let hits = apply_filter(&m, &[], None);
        assert_eq!(hits.len(), 3);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn filter_or_semantics() {
        // 验证"或"的组合：第二个条件为 OR 时，命中任一即可
        let p = write_temp(b"ID,V\n1,a\n2,b\n3,c\n", "_or.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let hits = apply_filter(
            &m,
            &[
                cond(1, FilterOp::Equal, "a", Logic::And),
                cond(1, FilterOp::Equal, "c", Logic::Or),
            ],
            None,
        );
        assert_eq!(hits, vec![0, 2]);
        std::fs::remove_file(&p).ok();
    }
}
