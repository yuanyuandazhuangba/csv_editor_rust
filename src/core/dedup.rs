//! 去重引擎：在给定行集合内按若干列的值组合筛出唯一行。
//!
//! 语义（与界面说明一致）：
//! - 作用范围是**传入的行集合**（界面传入的是当前筛选结果；未筛选时为全部数据行）；
//! - 按 key 列（调用方指定，空表示全部列）的取值组合判断重复，
//!   **保留每条重复记录中首次出现的行**，因此结果的行序与输入一致；
//! - 列值之间用 `\x1F` 连接（字段值中几乎不会出现），避免「a|b」与「a|b」这类拼接歧义。
//!
//! 性能：单线程 O(N)，复用 [`RowReader`]（整行只解析一次、字段文本零分配），
//! key 也用同一个复用 `String` 构建；只有首次出现的新唯一行才会 `clone` 一份 key
//! 存入去重表。筛选/去重属于「结果相互依赖」的扫描，无法像拆分那样按行块并行
//! （并行会破坏「保留首次出现」的顺序语义）。

use std::collections::HashMap;

use super::model::DataModel;
use super::reader::RowReader;

/// 去重结果统计
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DedupReport {
    /// 参与判重的输入行数
    pub input_rows: usize,
    /// 去重后剩余的唯一行数
    pub unique_rows: usize,
    /// 被判定为重复而丢弃的行数
    pub duplicates: usize,
    /// 参与判重的列数
    pub key_columns: usize,
}

/// 按 `columns`（空 = 全部列）的值组合去重，返回唯一行号（升序，保持输入顺序）。
///
/// `rows`：要判重的行号；`None` 表示全部数据行。
pub fn unique_rows(
    model: &DataModel,
    rows: Option<&[u32]>,
    columns: &[usize],
    progress: Option<&(dyn Fn(f64) + Sync)>,
) -> (Vec<u32>, DedupReport) {
    let (unique, report, _) = unique_rows_with_map(model, rows, columns, progress, false);
    (unique, report)
}

/// 与 [`unique_rows`] 相同，但当 `collect_map` 为真时**额外返回重复行对应关系**
/// `(保留的首次出现行, 被剔除的重复行)`（按被剔除行的出现顺序），
/// 供界面导出「重复行对应表」以便逐条核对。
pub fn unique_rows_with_map(
    model: &DataModel,
    rows: Option<&[u32]>,
    columns: &[usize],
    progress: Option<&(dyn Fn(f64) + Sync)>,
    collect_map: bool,
) -> (Vec<u32>, DedupReport, Vec<(u32, u32)>) {
    let column_count = model.column_count();
    // 空表示「全部字段」；同时丢弃越界列（界面可能传入陈旧索引）
    let cols: Vec<usize> = if columns.is_empty() {
        (0..column_count).collect()
    } else {
        let mut v: Vec<usize> = columns
            .iter()
            .copied()
            .filter(|&c| c < column_count)
            .collect();
        v.sort_unstable();
        v.dedup();
        if v.is_empty() {
            (0..column_count).collect()
        } else {
            v
        }
    };

    let total = rows.map(|v| v.len()).unwrap_or_else(|| model.row_count());
    // key -> 该 key 首次出现的行号（用行号而不是计数，便于导出对应关系）
    let mut seen: HashMap<String, u32> = HashMap::with_capacity(1024);
    let mut out: Vec<u32> = Vec::with_capacity(total);
    let mut duplicates: Vec<(u32, u32)> = Vec::new();
    let mut key = String::with_capacity(256);
    // 整行只解析一次，字段文本零分配
    let mut reader = RowReader::new(cols.clone());

    for i in 0..total {
        let row = match rows {
            Some(list) => list[i] as usize,
            None => i,
        };
        reader.load(model.file(), row);
        key.clear();
        for (k, &c) in cols.iter().enumerate() {
            if k > 0 {
                key.push('\u{1F}');
            }
            key.push_str(reader.value(c));
        }
        // 命中已有 key 时用 &str 借用查询，不分配；仅新唯一行 clone 一份
        match seen.get(key.as_str()) {
            Some(&kept) => {
                if collect_map {
                    duplicates.push((kept, row as u32));
                }
            }
            None => {
                seen.insert(key.clone(), row as u32);
                out.push(row as u32);
            }
        }
        if let Some(p) = progress {
            if i % 50_000 == 0 {
                p(i as f64 / total.max(1) as f64);
            }
        }
    }
    if let Some(p) = progress {
        p(1.0);
    }

    let report = DedupReport {
        input_rows: total,
        unique_rows: out.len(),
        duplicates: total.saturating_sub(out.len()),
        key_columns: cols.len(),
    };
    (out, report, duplicates)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(data: &[u8], suffix: &str) -> String {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "csv_dedup_test_{}_{}{}",
            std::process::id(),
            id,
            suffix
        ));
        std::fs::write(&p, data).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn dedup_all_columns_keeps_first_occurrence() {
        // 全部字段判重：完全相同的行只保留首次出现，行序不变
        let p = write_temp(
            b"ID,NAME,CITY\n1,a,bj\n2,b,sh\n1,a,bj\n3,c,bj\n2,b,sh\n",
            "_all.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let (rows, rep) = unique_rows(&m, None, &[], None);
        assert_eq!(rows, vec![0, 1, 3]);
        assert_eq!(rep.input_rows, 5);
        assert_eq!(rep.unique_rows, 3);
        assert_eq!(rep.duplicates, 2);
        assert_eq!(rep.key_columns, 3);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn dedup_by_selected_columns() {
        // 只按 CITY 判重：相同城市的行只保留第一条
        let p = write_temp(
            b"ID,NAME,CITY\n1,a,bj\n2,b,sh\n3,c,bj\n4,d,sh\n",
            "_cols.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let (rows, rep) = unique_rows(&m, None, &[2], None);
        assert_eq!(rows, vec![0, 1]);
        assert_eq!(rep.key_columns, 1);
        // 多列组合：CITY + NAME 都相同才算重复
        let (rows2, _) = unique_rows(&m, None, &[1, 2], None);
        assert_eq!(rows2, vec![0, 1, 2, 3]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn dedup_respects_row_subset() {
        // 在当前筛选结果（行子集）上判重：子集外的行不参与、也不影响结果
        let p = write_temp(
            b"ID,CITY\n1,bj\n2,sh\n3,bj\n4,sh\n5,bj\n",
            "_subset.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        // 只看第 0、2、4 行（都是 bj）→ 应只剩第一条
        let (rows, rep) = unique_rows(&m, Some(&[0, 2, 4]), &[1], None);
        assert_eq!(rows, vec![0]);
        assert_eq!(rep.input_rows, 3);
        // 子集为 1、3 行（sh）→ 只剩第一条
        let (rows2, _) = unique_rows(&m, Some(&[1, 3]), &[1], None);
        assert_eq!(rows2, vec![1]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn dedup_handles_empty_and_single_row() {
        let p = write_temp(b"ID,CITY\n1,bj\n", "_one.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let (rows, rep) = unique_rows(&m, None, &[], None);
        assert_eq!(rows, vec![0]);
        assert_eq!(rep.duplicates, 0);
        // 空行子集
        let (rows2, rep2) = unique_rows(&m, Some(&[]), &[], None);
        assert!(rows2.is_empty());
        assert_eq!(rep2.input_rows, 0);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn dedup_distinguishes_joined_key_ambiguity() {
        // key 用 \x1F 连接：("ab","c") 与 ("a","bc") 不应被判为相同
        let p = write_temp(b"A,B\nab,c\na,bc\n", "_join.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let (rows, rep) = unique_rows(&m, None, &[0, 1], None);
        assert_eq!(rows, vec![0, 1], "拼接歧义会导致误判为重复");
        assert_eq!(rep.duplicates, 0);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn dedup_ignores_out_of_range_columns() {
        // 越界列（界面可能传入陈旧索引）应被忽略；全部越界时退化为「全部字段」
        let p = write_temp(b"ID,CITY\n1,bj\n1,bj\n", "_range.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let (rows, rep) = unique_rows(&m, None, &[1, 99], None);
        assert_eq!(rows, vec![0]);
        assert_eq!(rep.key_columns, 1);
        let (rows2, rep2) = unique_rows(&m, None, &[99], None);
        assert_eq!(rows2, vec![0]);
        assert_eq!(rep2.key_columns, 2, "全部越界时应退化为全部字段");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn dedup_map_records_kept_and_dropped_rows() {
        // 对应关系：每个被剔除的重复行都指向它对应的首次保留行（行号从 0 起）
        let p = write_temp(
            b"ID,CITY\n1,bj\n2,sh\n1,bj\n3,bj\n2,sh\n",
            "_map.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        // 只按 CITY 判重：bj 首次在第 0 行、sh 首次在第 1 行
        let (unique, rep, map) = unique_rows_with_map(&m, None, &[1], None, true);
        assert_eq!(unique, vec![0, 1]);
        assert_eq!(rep.duplicates, 3);
        assert_eq!(map, vec![(0, 2), (0, 3), (1, 4)]);
        // 不收集时对应表为空（省内存）
        let (_, _, empty) = unique_rows_with_map(&m, None, &[1], None, false);
        assert!(empty.is_empty());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn dedup_chinese_and_quoted_fields() {
        // 中文与含逗号的引号字段参与判重
        let p = write_temp(
            "城市,备注\n北京,\"a,b\"\n上海,c\n北京,\"a,b\"\n".as_bytes(),
            "_cn.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let (rows, rep) = unique_rows(&m, None, &[], None);
        assert_eq!(rows, vec![0, 1]);
        assert_eq!(rep.duplicates, 1);
        std::fs::remove_file(&p).ok();
    }
}
