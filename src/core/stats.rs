//! 统计分析：**字段分组汇总** 与 **数值列统计**。
//!
//! ### 字段分组汇总（[`group_by`]）
//!
//! 语义（与界面说明一致）：
//! - 作用范围是**传入的行集合**（界面传入的是当前筛选结果；未筛选时为全部数据行）；
//! - 按 1~3 个字段的取值组合分组，对指定列做聚合：
//!   行数（每组自动给出）、计数（非空）、求和、均值、最大、最小、去重计数、空值数；
//! - 分组 key 用 `\x1F` 连接（字段值中几乎不会出现），避免「a|b」这类拼接歧义；
//! - **输出顺序 = 分组在行序中首次出现的顺序**（稳定、可复现，与输入行序一致）。
//!
//! ### 数值列统计（[`describe_columns`]）
//!
//! 对每个选定的原始列给出：行数、计数（非空）、空值数、非数值数、求和、均值、
//! 中位数、最小、最大、标准差（总体，分母 n）。一次并行扫描算完所有列。
//!
//! ### 共同的数值语义
//!
//! 与筛选一致（复用 `filter.rs` 的 `try_num`/`cmp_num`）：
//! 整数用 `i128` 精确累加（19 位卡号/大金额不丢精度）；**求和与均值的分子用
//! 「定点十进制」累加**（把各值按小数位数放大成 `i128` 整数再相加，见 [`parse_dec`]），
//! 因此金额列不会出现 `3613134567.580001` 这类 f64 累积误差——只有当某个值无法用
//! 十进制字面量精确表示（科学计数法、超过 [`DEC_MAX_SCALE`] 位小数或超长数字）时，
//! 才整体回退到 `f64` 累加。
//! 非空但**无法解析为数值**的值（含 `NaN`/`±inf`）不计入求和/均值/最大/最小，
//! 而是单独计数，并通过结果表的 `notes` 提示"该列有 N 个非数值未参与计算"。
//!
//! 性能：聚合是可交换的（局部聚合可合并），因此按行块并行 ——
//! 各线程用 [`RowReader`] 零分配逐行取值、构建局部 `HashMap<key, Acc>`，
//! 再按块序合并；合并顺序即首次出现顺序，输出稳定。

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use super::filter::{cmp_num, try_num, Num};
use super::model::DataModel;
use super::reader::RowReader;

/// 分组字段上限（与「拆分字段」一致）
pub const MAX_GROUP_COLUMNS: usize = 3;

/// 分组数上限：超过则拒绝并提示改用其他字段，避免生成海量结果表
/// （与 `split.rs::MAX_SPLIT_FILES` 同样的保护思路）
pub const MAX_GROUPS: usize = 20_000;

/// 单个「去重计数」聚合最多保存多少个不同值；超出后结果标注 `>=N`
/// （防止单个聚合把内存吃光，例如按 1 个大字段分组、对 19 位卡号去重计数）
pub const MAX_DISTINCT_VALUES: usize = 200_000;

/// 定点十进制求和允许的最大小数位数；超过则整体回退 `f64` 累加
/// （金额/数量类数据远不到 9 位小数，回退只影响极端数据）
const DEC_MAX_SCALE: u32 = 9;

/// 定点十进制求和允许的最大十进制位数（`i128` 上限约 39 位，留出放大余量）
const DEC_MAX_DIGITS: usize = 30;

/// 统计并行度上限（纯 CPU + 内存带宽密集型，同筛选）
const MAX_STATS_THREADS: usize = 32;

/// 聚合方式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggKind {
    /// 非空值计数
    Count,
    /// 求和
    Sum,
    /// 均值（非空值的算术平均）
    Avg,
    /// 最小值
    Min,
    /// 最大值
    Max,
    /// 去重计数（不同值个数）
    DistinctCount,
    /// 空值数（trim 后为空）
    EmptyCount,
}

impl AggKind {
    /// 全部聚合方式（界面下拉顺序）
    pub const ALL: [AggKind; 7] = [
        AggKind::Sum,
        AggKind::Avg,
        AggKind::Max,
        AggKind::Min,
        AggKind::Count,
        AggKind::DistinctCount,
        AggKind::EmptyCount,
    ];

    /// 中文标签（UI 显示 / 结果列表头）
    pub fn label(&self) -> &'static str {
        match self {
            AggKind::Count => "计数",
            AggKind::Sum => "求和",
            AggKind::Avg => "均值",
            AggKind::Min => "最小",
            AggKind::Max => "最大",
            AggKind::DistinctCount => "去重计数",
            AggKind::EmptyCount => "空值数",
        }
    }

    pub fn from_label(s: &str) -> Option<AggKind> {
        AggKind::ALL.iter().copied().find(|k| k.label() == s)
    }

    /// 是否需要把值当数值处理（求和/均值/最大/最小；非数值不计入）
    pub fn is_numeric(&self) -> bool {
        matches!(
            self,
            AggKind::Sum | AggKind::Avg | AggKind::Min | AggKind::Max
        )
    }
}

/// 一条聚合规格：对原始列 `column` 施加 `kind`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AggSpec {
    /// 原始列索引（0 起始）
    pub column: usize,
    pub kind: AggKind,
}

/// 统计结果表的来源（界面据此显示标题、日志口径与导出文件名）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatsKind {
    /// 字段分组汇总
    #[default]
    GroupBy,
    /// 数值列描述统计
    ColumnSummary,
    /// 取值频次分布 / Top-N
    ValueFrequency,
    /// 重复情况统计（哪些键重复、各重复几次、重复行占比）
    DuplicateCheck,
    /// 空值与数据质量统计
    DataQuality,
    /// 透视表（行字段 × 列字段 × 值聚合）
    Pivot,
    /// 交叉/关联统计（A 与 B 的对应关系）
    CrossTab,
}

impl StatsKind {
    /// 功能名（日志与提示口径）
    pub fn label(&self) -> &'static str {
        match self {
            StatsKind::GroupBy => "分组汇总",
            StatsKind::ColumnSummary => "列统计",
            StatsKind::ValueFrequency => "频次分布",
            StatsKind::DuplicateCheck => "重复情况",
            StatsKind::DataQuality => "数据质量",
            StatsKind::Pivot => "透视表",
            StatsKind::CrossTab => "交叉统计",
        }
    }

    /// 结果窗标题（同时是 egui 窗口 Id，见 `app::dialog_ui::set_window_collapsed`）
    pub fn title(&self) -> &'static str {
        match self {
            StatsKind::GroupBy => "分组汇总结果",
            StatsKind::ColumnSummary => "列统计结果",
            StatsKind::ValueFrequency => "频次分布结果",
            StatsKind::DuplicateCheck => "重复情况结果",
            StatsKind::DataQuality => "数据质量结果",
            StatsKind::Pivot => "透视表结果",
            StatsKind::CrossTab => "交叉统计结果",
        }
    }

    /// 导出默认文件名（不含扩展名）
    pub fn file_stem(&self) -> &'static str {
        match self {
            StatsKind::GroupBy => "分组汇总",
            StatsKind::ColumnSummary => "列统计",
            StatsKind::ValueFrequency => "频次分布",
            StatsKind::DuplicateCheck => "重复情况",
            StatsKind::DataQuality => "数据质量",
            StatsKind::Pivot => "透视表",
            StatsKind::CrossTab => "交叉统计",
        }
    }
}

/// 统计结果表（可直接交给 `export.rs` 的 CsvWriter/XlsxWriter 导出）
#[derive(Debug, Clone, Default)]
pub struct StatsTable {
    /// 结果来源（分组汇总 / 数值列统计）
    pub kind: StatsKind,
    /// 表头：分组汇总为「分组字段名… + 行数 + 每个聚合列」；列统计为固定的指标列
    pub headers: Vec<String>,
    /// 数据行：分组汇总每行一个分组；列统计每行一列
    pub rows: Vec<Vec<String>>,
    /// 参与统计的输入行数（筛选结果行数或全部数据行数）
    pub input_rows: usize,
    /// 分组字段数（列统计为 0）
    pub group_columns: usize,
    /// 分组汇总的聚合列数 / 列统计的列数
    pub aggs: usize,
    /// 结果级补充说明（非数值个数、中位数是否可用等），界面与日志逐条提示
    pub notes: Vec<String>,
}

impl StatsTable {
    /// 结果行数（分组汇总 = 分组数；列统计 = 统计的列数）
    pub fn group_count(&self) -> usize {
        self.rows.len()
    }

    /// 结果行数的口径名（用于界面摘要文字）
    pub fn row_unit(&self) -> &'static str {
        match self.kind {
            StatsKind::GroupBy => "组",
            StatsKind::ColumnSummary => "列",
            StatsKind::ValueFrequency => "项",
            StatsKind::DuplicateCheck => "项",
            StatsKind::DataQuality => "列",
            StatsKind::Pivot => "行",
            StatsKind::CrossTab => "项",
        }
    }

    /// 结果窗顶部的一行摘要
    pub fn summary(&self) -> String {
        match self.kind {
            StatsKind::GroupBy => format!(
                "统计范围 {} 行 → {} 组（分组字段 {} 个，聚合列 {} 个）",
                self.input_rows,
                self.rows.len(),
                self.group_columns,
                self.aggs
            ),
            StatsKind::ColumnSummary => {
                format!("统计范围 {} 行 → {} 列", self.input_rows, self.rows.len())
            }
            StatsKind::ValueFrequency => format!(
                "统计范围 {} 行 → {} 列的取值频次（共 {} 项）",
                self.input_rows,
                self.aggs,
                self.rows.len()
            ),
            StatsKind::DuplicateCheck => format!(
                "统计范围 {} 行 → {} 项的重复明细（按重复次数降序）",
                self.input_rows,
                self.rows.len()
            ),
            StatsKind::DataQuality => {
                format!("统计范围 {} 行 → {} 列", self.input_rows, self.rows.len())
            }
            StatsKind::Pivot => format!(
                "统计范围 {} 行 → {} 行 × {} 列（含合计行列）",
                self.input_rows,
                self.rows.len().saturating_sub(1),
                self.group_columns
            ),
            StatsKind::CrossTab => format!(
                "统计范围 {} 行 → {} 个 {}（含汇总说明）",
                self.input_rows,
                self.rows.len(),
                self.row_unit()
            ),
        }
    }
}

/// 按 `group_cols` 的取值组合分组，对 `aggs` 指定的列做聚合。
///
/// `rows`：参与统计的行号；`None` 表示全部数据行。
/// 出错情形：无有效分组字段 / 无有效聚合列 / 分组数超过 [`MAX_GROUPS`]。
pub fn group_by(
    model: &DataModel,
    rows: Option<&[u32]>,
    group_cols: &[usize],
    aggs: &[AggSpec],
    progress: Option<&(dyn Fn(f64) + Sync)>,
) -> Result<StatsTable, String> {
    let column_count = model.column_count();
    // 分组字段：丢弃越界列（界面可能传入陈旧的列索引）、去重、保持用户选择顺序
    let mut gcols: Vec<usize> = Vec::new();
    for &c in group_cols {
        if c < column_count && !gcols.contains(&c) {
            gcols.push(c);
        }
    }
    if gcols.is_empty() {
        return Err("请选择至少一个有效的分组字段（列可能已删除或隐藏）".to_string());
    }
    if gcols.len() > MAX_GROUP_COLUMNS {
        return Err(format!("分组字段最多 {} 个", MAX_GROUP_COLUMNS));
    }
    // 聚合列：同样丢弃越界列
    let specs: Vec<AggSpec> = aggs
        .iter()
        .copied()
        .filter(|a| a.column < column_count)
        .collect();
    if specs.is_empty() {
        return Err("请至少为一个列选择聚合方式（列可能已删除或隐藏）".to_string());
    }

    // 结果表头（先建好，空输入时也能返回只有表头的表）
    let mut headers: Vec<String> = Vec::with_capacity(gcols.len() + 1 + specs.len());
    for &c in &gcols {
        headers.push(column_name(model, c));
    }
    headers.push("行数".to_string());
    for a in &specs {
        headers.push(format!(
            "{}({})",
            column_name(model, a.column),
            a.kind.label()
        ));
    }

    let total = rows.map(|v| v.len()).unwrap_or_else(|| model.row_count());
    let mut table = StatsTable {
        kind: StatsKind::GroupBy,
        headers,
        rows: Vec::new(),
        input_rows: total,
        group_columns: gcols.len(),
        aggs: specs.len(),
        notes: Vec::new(),
    };
    if total == 0 {
        if let Some(p) = progress {
            p(1.0);
        }
        return Ok(table);
    }

    // 本次统计需要读取的所有列（去重后交给每线程的行读取器，做到每行只解析一次）
    let mut need: Vec<usize> = gcols.clone();
    need.extend(specs.iter().map(|a| a.column));

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(MAX_STATS_THREADS)
        .min(total);
    let chunk = (total / threads).max(1);

    let done = AtomicUsize::new(0);
    // 各块结果（块内按首次出现顺序）→ 按块序合并即得全局首次出现顺序
    let mut blocks: Vec<Vec<(String, Vec<Acc>)>> = Vec::new();
    std::thread::scope(|s| {
        let mut handles = Vec::new();
        let mut start = 0usize;
        while start < total {
            let end = (start + chunk).min(total);
            let model_ref = model;
            let gcols_ref = &gcols;
            let specs_ref = &specs;
            let need_ref = &need;
            let rows_ref = rows;
            let done_ref = &done;
            let prog = progress;
            handles.push(s.spawn(move || {
                let mut reader = RowReader::new(need_ref.clone());
                let mut index: HashMap<String, usize> = HashMap::with_capacity(1024);
                let mut out: Vec<(String, Vec<Acc>)> = Vec::new();
                let mut key = String::with_capacity(128);
                for i in start..end {
                    let row = match rows_ref {
                        Some(list) => list[i] as usize,
                        None => i,
                    };
                    reader.load(model_ref.file(), row);
                    key.clear();
                    for (k, &c) in gcols_ref.iter().enumerate() {
                        if k > 0 {
                            key.push('\u{1F}');
                        }
                        key.push_str(reader.value(c));
                    }
                    let idx = match index.get(key.as_str()) {
                        Some(&i) => i,
                        None => {
                            let i = out.len();
                            index.insert(key.clone(), i);
                            out.push((key.clone(), new_accs(specs_ref)));
                            i
                        }
                    };
                    let accs = &mut out[idx].1;
                    for (acc, spec) in accs.iter_mut().zip(specs_ref.iter()) {
                        acc.push(spec.kind, reader.value(spec.column));
                    }
                }
                done_ref.fetch_add(end - start, AtomicOrdering::Relaxed);
                if let Some(p) = prog {
                    p(done_ref.load(AtomicOrdering::Relaxed) as f64 / total as f64);
                }
                out
            }));
            start = end;
        }
        for h in handles {
            blocks.push(h.join().unwrap_or_default());
        }
    });

    // 合并各块局部聚合（单线程，内存操作）：新 key 追加到末尾，保持首次出现顺序
    let mut index: HashMap<String, usize> = HashMap::with_capacity(1024);
    let mut keys: Vec<String> = Vec::new();
    let mut accs: Vec<Vec<Acc>> = Vec::new();
    for block in blocks {
        for (key, acc) in block {
            match index.get(key.as_str()) {
                Some(&i) => {
                    for (a, b) in accs[i].iter_mut().zip(acc.iter()) {
                        a.merge(b);
                    }
                }
                None => {
                    if keys.len() >= MAX_GROUPS {
                        return Err(format!(
                            "分组数超过上限 {}（当前已超过），请改用取值较少的字段分组，\
                             或先用筛选缩小范围",
                            MAX_GROUPS
                        ));
                    }
                    index.insert(key.clone(), keys.len());
                    keys.push(key);
                    accs.push(acc);
                }
            }
        }
    }

    // 输出：分组值按 `\x1F` 还原（`splitn` 限制段数，字段值含分隔符时也能正确切分）
    table.rows = Vec::with_capacity(keys.len());
    for (key, acc) in keys.iter().zip(accs.iter()) {
        let mut row: Vec<String> = Vec::with_capacity(table.headers.len());
        for part in key.splitn(gcols.len(), '\u{1F}') {
            row.push(part.to_string());
        }
        while row.len() < gcols.len() {
            row.push(String::new());
        }
        // 每组行数取第一个聚合的 total（所有聚合的 total 相同）
        let group_rows = acc.first().map(|a| a.total).unwrap_or(0);
        row.push(group_rows.to_string());
        for (a, spec) in acc.iter().zip(specs.iter()) {
            row.push(a.finish(spec.kind));
        }
        table.rows.push(row);
    }
    // 非数值提示：逐聚合列给出「有多少个非数值未参与计算」
    for (i, spec) in specs.iter().enumerate() {
        let n: u64 = accs.iter().map(|a| a[i].non_numeric).sum();
        if n > 0 {
            table.notes.push(format!(
                "{}({})：{} 个非数值未参与计算（求和/均值/最大/最小忽略它们）",
                column_name(model, spec.column),
                spec.kind.label(),
                n
            ));
        }
    }
    if let Some(p) = progress {
        p(1.0);
    }
    Ok(table)
}

fn column_name(model: &DataModel, col: usize) -> String {
    model
        .header()
        .get(col)
        .cloned()
        .unwrap_or_else(|| format!("列{}", col + 1))
}

/// 「数值列统计」的指标表头（每列一行）
const COLUMN_SUMMARY_HEADERS: [&str; 11] = [
    "列名",
    "行数",
    "计数",
    "空值数",
    "非数值数",
    "求和",
    "均值",
    "中位数",
    "最小",
    "最大",
    "标准差",
];

/// 精确中位数可保留的数值总数预算（约 160MB；在所有统计列之间均分）
const MEDIAN_TOTAL_BUDGET: usize = 20_000_000;
/// 单列可保留的数值个数下限（列很多时也尽量能算中位数）
const MEDIAN_MIN_PER_COLUMN: usize = 200_000;
/// 单列可保留的数值个数上限
const MEDIAN_MAX_PER_COLUMN: usize = 10_000_000;

/// 数值列描述统计：对每个选定的原始列计算
/// 行数 / 计数（非空）/ 空值数 / 非数值数 / 求和 / 均值 / 中位数 / 最小 / 最大 / 标准差。
///
/// 设计取舍：
/// - **一次并行扫描同时算所有列**（不按列反复读表），每列用线程局部的
///   [`ColAcc`]，块结果按列合并；
/// - 求和与均值分子用与分组汇总同一套**定点十进制**累加（金额不出现 f64 尾差）；
/// - 标准差用 **Welford 在线算法**（数值稳定，避免 `Σx² − n·mean²` 的灾难性抵消），
///   分块合并用 Chan 的并行合并公式；输出**总体标准差**（分母 n）；
/// - **中位数**必须保留数值本身：单列最多保留 [`MEDIAN_MAX_PER_COLUMN`] 个、
///   总预算 [`MEDIAN_TOTAL_BUDGET`] 个（按列数均分）。若某列数值个数超过保留上限，
///   该列中位数输出 `—` 并在 `notes` 里说明 —— 不静默给出近似值；
/// - `NaN` / `±inf` 不算用户意义上的数值（与 `model::classify` 一致），计入非数值。
pub fn describe_columns(
    model: &DataModel,
    rows: Option<&[u32]>,
    cols: &[usize],
    progress: Option<&(dyn Fn(f64) + Sync)>,
) -> Result<StatsTable, String> {
    let column_count = model.column_count();
    // 丢弃越界列（界面可能传入陈旧的列索引）、去重、保持用户选择顺序
    let mut list: Vec<usize> = Vec::new();
    for &c in cols {
        if c < column_count && !list.contains(&c) {
            list.push(c);
        }
    }
    if list.is_empty() {
        return Err("请至少选择一个有效的列（列可能已删除或隐藏）".to_string());
    }
    // 每列保留多少原始数值（用于精确中位数）
    let cap = (MEDIAN_TOTAL_BUDGET / list.len()).clamp(MEDIAN_MIN_PER_COLUMN, MEDIAN_MAX_PER_COLUMN);

    let total = rows.map(|v| v.len()).unwrap_or_else(|| model.row_count());
    let mut table = StatsTable {
        kind: StatsKind::ColumnSummary,
        headers: COLUMN_SUMMARY_HEADERS
            .iter()
            .map(|s| s.to_string())
            .collect(),
        rows: Vec::new(),
        input_rows: total,
        group_columns: 0,
        aggs: list.len(),
        notes: Vec::new(),
    };
    if total == 0 {
        for &c in &list {
            let mut acc = ColAcc::new(cap);
            table.rows.push(acc.finish(model, c, 0));
        }
        if let Some(p) = progress {
            p(1.0);
        }
        return Ok(table);
    }

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(MAX_STATS_THREADS)
        .min(total);
    let chunk = (total / threads).max(1);
    let done = AtomicUsize::new(0);
    let mut blocks: Vec<Vec<ColAcc>> = Vec::new();
    std::thread::scope(|s| {
        let mut handles = Vec::new();
        let mut start = 0usize;
        while start < total {
            let end = (start + chunk).min(total);
            let model_ref = model;
            let list_ref = &list;
            let rows_ref = rows;
            let done_ref = &done;
            let prog = progress;
            handles.push(s.spawn(move || {
                let mut reader = RowReader::new(list_ref.clone());
                let mut accs: Vec<ColAcc> = list_ref.iter().map(|_| ColAcc::new(cap)).collect();
                for i in start..end {
                    let row = match rows_ref {
                        Some(l) => l[i] as usize,
                        None => i,
                    };
                    reader.load(model_ref.file(), row);
                    for (acc, &c) in accs.iter_mut().zip(list_ref.iter()) {
                        acc.push(reader.value(c));
                    }
                }
                done_ref.fetch_add(end - start, AtomicOrdering::Relaxed);
                if let Some(p) = prog {
                    p(done_ref.load(AtomicOrdering::Relaxed) as f64 / total as f64);
                }
                accs
            }));
            start = end;
        }
        for h in handles {
            blocks.push(h.join().unwrap_or_default());
        }
    });

    // 按列合并各块的局部统计
    let mut accs: Vec<ColAcc> = list.iter().map(|_| ColAcc::new(cap)).collect();
    for block in blocks {
        for (a, b) in accs.iter_mut().zip(block.iter()) {
            a.merge(b);
        }
    }

    for (acc, &c) in accs.iter_mut().zip(list.iter()) {
        table.rows.push(acc.finish(model, c, total as u64));
    }
    for (acc, &c) in accs.iter().zip(list.iter()) {
        let name = column_name(model, c);
        // 只在「既含数值又含非数值」的列上提示：整列都是文本（无数值）时不必刷提示，
        // 非数值个数本身已在结果表的「非数值数」列里
        if acc.non_numeric > 0 && acc.numeric > 0 {
            table.notes.push(format!(
                "{}：{} 个非数值未参与计算（数值判定与筛选一致）",
                name, acc.non_numeric
            ));
        }
        if acc.numeric > acc.values.len() as u64 {
            table.notes.push(format!(
                "{}：数值 {} 个，超过精确中位数的保留上限 {} 个，中位数未计算（可先筛选缩小范围，或减少一次统计的列数）",
                name, acc.numeric, cap
            ));
        }
    }
    table
        .notes
        .push("标准差为总体标准差（分母 n）；如需样本标准差，用该值乘以 sqrt(n/(n-1)) 即可。".to_string());
    if let Some(p) = progress {
        p(1.0);
    }
    Ok(table)
}

/// 「频次分布 / Top-N」的指标表头（每列若干行）
const FREQUENCY_HEADERS: [&str; 5] = ["列名", "排名", "取值", "出现次数", "占比(%)"];

/// 频次分布可容纳的不同取值总数预算（所有统计列共享，超出报错）
const FREQ_TOTAL_BUDGET: usize = 3_000_000;
/// 单列可容纳的不同取值个数上限
const FREQ_MAX_PER_COLUMN: usize = 1_000_000;
/// 单列可容纳的不同取值个数下限（列很多时也不至于小到不可用）
const FREQ_MIN_PER_COLUMN: usize = 200_000;
/// 引擎允许的最大 Top-N（界面输入也已限制，这里再兜底一次）
pub const MAX_FREQ_TOP_N: usize = 1000;

/// 取值频次分布 / Top-N：对每个选定列统计各取值的出现次数与占比，按次数降序取前 `top_n`。
///
/// 语义与取舍：
/// - 作用范围是传入的行集合（界面传当前筛选结果；未筛选时为全部数据）；
/// - **按原值统计**（不 trim、区分大小写，与去重判重一致）；`trim` 后为空的值统一归入
///   `(空)` 一类（空值也是数据质量的重要信息，因此参与排名与占比）；
/// - 排序：出现次数降序，次数相同按取值升序（结果稳定、可复现）；
/// - 超出 `top_n` 的取值不逐个列出，而是追加一行 `(其余 N 个取值)` 汇总其次数与占比，
///   这样各行的占比之和始终是整体的 100%；
/// - 占比分母是**统计范围行数**（含空值行）；
/// - 内存护栏：不同取值总数超过预算（[`FREQ_TOTAL_BUDGET`]，按列数均分、每列上限
///   [`FREQ_MAX_PER_COLUMN`]）时**报错**并提示缩小范围 —— Top-N 依赖完整计数，
///   丢弃部分取值会得出错误的"最高频"结论，因此宁可拒绝也不给错的结果。
pub fn value_frequency(
    model: &DataModel,
    rows: Option<&[u32]>,
    cols: &[usize],
    top_n: usize,
    progress: Option<&(dyn Fn(f64) + Sync)>,
) -> Result<StatsTable, String> {
    let column_count = model.column_count();
    // 丢弃越界列（界面可能传入陈旧的列索引）、去重、保持用户选择顺序
    let mut list: Vec<usize> = Vec::new();
    for &c in cols {
        if c < column_count && !list.contains(&c) {
            list.push(c);
        }
    }
    if list.is_empty() {
        return Err("请至少选择一个有效的列（列可能已删除或隐藏）".to_string());
    }
    let top_n = top_n.clamp(1, MAX_FREQ_TOP_N);
    let cap = (FREQ_TOTAL_BUDGET / list.len()).clamp(FREQ_MIN_PER_COLUMN, FREQ_MAX_PER_COLUMN);
    value_frequency_with_cap(model, rows, &list, top_n, cap, progress)
}

/// [`value_frequency`] 的内部实现（`cap` 显式传入，便于测试用极小上限驱动超限路径）
fn value_frequency_with_cap(
    model: &DataModel,
    rows: Option<&[u32]>,
    list: &[usize],
    top_n: usize,
    cap: usize,
    progress: Option<&(dyn Fn(f64) + Sync)>,
) -> Result<StatsTable, String> {
    let total = rows.map(|v| v.len()).unwrap_or_else(|| model.row_count());
    let mut table = StatsTable {
        kind: StatsKind::ValueFrequency,
        headers: FREQUENCY_HEADERS.iter().map(|s| s.to_string()).collect(),
        rows: Vec::new(),
        input_rows: total,
        group_columns: 0,
        aggs: list.len(),
        notes: Vec::new(),
    };
    if total == 0 {
        for &c in list {
            table.rows.push(vec![
                column_name(model, c),
                String::new(),
                "(无数据)".to_string(),
                "0".to_string(),
                "0.00".to_string(),
            ]);
        }
        if let Some(p) = progress {
            p(1.0);
        }
        return Ok(table);
    }

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(MAX_STATS_THREADS)
        .min(total);
    let chunk = (total / threads).max(1);
    let done = AtomicUsize::new(0);
    let mut blocks: Vec<FreqBlock> = Vec::new();
    std::thread::scope(|s| {
        let mut handles = Vec::new();
        let mut start = 0usize;
        while start < total {
            let end = (start + chunk).min(total);
            let model_ref = model;
            let list_ref = list;
            let rows_ref = rows;
            let done_ref = &done;
            let prog = progress;
            handles.push(s.spawn(move || {
                let mut reader = RowReader::new(list_ref.to_vec());
                let mut block = FreqBlock::new(list_ref.len());
                for i in start..end {
                    let row = match rows_ref {
                        Some(l) => l[i] as usize,
                        None => i,
                    };
                    reader.load(model_ref.file(), row);
                    for (idx, &c) in list_ref.iter().enumerate() {
                        block.push(idx, reader.value(c), cap);
                    }
                }
                done_ref.fetch_add(end - start, AtomicOrdering::Relaxed);
                if let Some(p) = prog {
                    p(done_ref.load(AtomicOrdering::Relaxed) as f64 / total as f64);
                }
                block
            }));
            start = end;
        }
        for h in handles {
            blocks.push(h.join().unwrap_or_default());
        }
    });

    // 按列合并各块的频次表（内存有界：每列不超过 cap 个不同取值）
    let mut maps: Vec<HashMap<String, u64>> = list.iter().map(|_| HashMap::new()).collect();
    let mut empties: Vec<u64> = vec![0; list.len()];
    let mut overflow: Vec<bool> = vec![false; list.len()];
    for mut block in blocks {
        for (i, m) in block.maps.drain(..).enumerate() {
            empties[i] += block.empties[i];
            overflow[i] |= block.overflow[i];
            let dst = &mut maps[i];
            for (k, v) in m {
                match dst.get_mut(k.as_str()) {
                    Some(count) => *count += v,
                    None => {
                        if dst.len() >= cap {
                            overflow[i] = true; // 已达上限：内存有界，最终报错
                        } else {
                            dst.insert(k, v);
                        }
                    }
                }
            }
        }
    }
    if let Some(i) = overflow.iter().position(|&x| x) {
        return Err(format!(
            "列「{}」的不同取值超过上限 {}（统计范围 {} 行），已停止统计：请先用筛选缩小范围，\
             或减少一次统计的列数，再重新执行",
            column_name(model, list[i]),
            cap,
            total
        ));
    }

    // 每列取 Top-N（超出部分汇总为「(其余 N 个取值)」行）
    for (i, &c) in list.iter().enumerate() {
        let name = column_name(model, c);
        let mut items: Vec<(String, u64)> = maps[i].drain().collect();
        if empties[i] > 0 {
            items.push(("(空)".to_string(), empties[i]));
        }
        if items.is_empty() {
            table.rows.push(vec![
                name.clone(),
                String::new(),
                "(无数据)".to_string(),
                "0".to_string(),
                "0.00".to_string(),
            ]);
            continue;
        }
        // 次数降序；次数相同按取值升序（结果稳定可复现）
        items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let distinct = items.len();
        let shown = top_n.min(distinct);
        let pct = |n: u64| fmt_pct(n as f64 * 100.0 / total as f64);
        for (rank, (value, count)) in items.iter().take(shown).enumerate() {
            table.rows.push(vec![
                name.clone(),
                (rank + 1).to_string(),
                value.clone(),
                count.to_string(),
                pct(*count),
            ]);
        }
        if distinct > shown {
            let rest_count: u64 = items[shown..].iter().map(|(_, c)| *c).sum();
            table.rows.push(vec![
                name.clone(),
                String::new(),
                format!("(其余 {} 个取值)", distinct - shown),
                rest_count.to_string(),
                pct(rest_count),
            ]);
        }
        // 合计行：让「总量对得上」一眼可见（出现次数 == 统计范围行数、占比 100%），
        // 也顺带核对上面各行没有漏算
        let total_count: u64 = items.iter().map(|(_, c)| *c).sum();
        table.rows.push(vec![
            name.clone(),
            String::new(),
            "(合计)".to_string(),
            total_count.to_string(),
            pct(total_count),
        ]);
        if empties[i] > 0 {
            table.notes.push(format!(
                "{}：{} 行为空值（已作为「(空)」参与频次与占比）",
                name, empties[i]
            ));
        }
        table.notes.push(if distinct > shown {
            format!(
                "{}：共 {} 个不同取值，已列出前 {} 项，其余 {} 个未逐项列出（汇总为一行）",
                name,
                distinct,
                shown,
                distinct - shown
            )
        } else {
            format!("{}：共 {} 个不同取值（已全部列出）", name, distinct)
        });
    }
    table.notes.push(
        "占比 = 出现次数 ÷ 统计范围行数（含空值行），按 4 位有效数字显示（去掉尾随零）；\
         每列末行的「(合计)」出现次数应等于统计范围行数、占比 100%，可据此核对总量。"
            .to_string(),
    );
    table.notes.push(
        "按原值统计（区分大小写与首尾空格，与去重一致）；逐行占比因舍入相加可能与 100% 略有出入。"
            .to_string(),
    );
    if let Some(p) = progress {
        p(1.0);
    }
    Ok(table)
}

/// 重复情况统计可容纳的不同键个数上限（超出报错，提示先筛选缩小范围）
const MAX_DUP_KEYS: usize = 2_000_000;
/// 重复情况明细表允许的最大 Top-N
pub const MAX_DUP_TOP_N: usize = 1000;

/// 「重复情况统计」的结果：统计数字 + 明细表 + 可直接落回视图的行号
#[derive(Debug, Clone)]
pub struct DuplicateReport {
    /// 明细表（每个重复键一行，按重复次数降序；可直接导出）
    pub table: StatsTable,
    /// 统计范围行数
    pub input_rows: usize,
    /// 不同键个数（= 去重后的行数）
    pub distinct_keys: usize,
    /// 出现次数 > 1 的键个数
    pub duplicate_keys: usize,
    /// 重复行数 = 统计范围行数 − 不同键个数
    pub duplicate_rows: usize,
    /// **重复键涉及的全部行号**（0 起始，含各键首次出现的那行）——「只看重复的行」用。
    ///
    /// 顺序是**按重复键分组**：组之间按「重复次数降序、键升序」（与明细表一致），
    /// 组内按行号升序 —— 这样落回预览区后，同一个键的各行**紧挨着**，
    /// 一眼就能看出谁和谁重复，便于逐条核对（而不是被其它键的行隔开）。
    /// 注意与 [`DuplicateReport::duplicate_rows`] 的区别：这里含首次出现行。
    pub dup_rows: Vec<u32>,
    /// 每个键首次出现的行号（升序，0 起始）——「只看唯一行」用（与去重结果一致）
    pub unique_rows: Vec<u32>,
}

impl DuplicateReport {
    /// 重复行占比（%）
    pub fn duplicate_rate(&self) -> f64 {
        if self.input_rows == 0 {
            0.0
        } else {
            self.duplicate_rows as f64 * 100.0 / self.input_rows as f64
        }
    }

    /// 一句话摘要（结果窗与日志用）
    pub fn summary(&self) -> String {
        format!(
            "统计范围 {} 行 → 不同键 {} 个，其中重复键 {} 个；重复行 {} 行（占 {}%）",
            self.input_rows,
            self.distinct_keys,
            self.duplicate_keys,
            self.duplicate_rows,
            fmt_pct(self.duplicate_rate())
        )
    }
}

/// 重复情况统计：找出哪些键出现多次、各重复几次、重复行占比。
///
/// 与 [`super::dedup::unique_rows`] 的关系：**判重口径完全一致**（同一套键构建规则、
/// 同样保留首次出现），但这里给出的是「重复的分布」而非去重结果 ——
/// `unique_rows` 与去重的输出逐行相同（有交叉测试守护），另附 `dup_rows` 供「只看重复的行」。
///
/// 单线程（与去重同理）：键表需要全局唯一、首次出现行号依赖全局顺序，并行合并虽可行但收益有限。
/// `columns` 为空表示按全部字段判重；越界列被忽略（与去重一致）。
pub fn duplicate_stats(
    model: &DataModel,
    rows: Option<&[u32]>,
    columns: &[usize],
    top_n: usize,
    progress: Option<&(dyn Fn(f64) + Sync)>,
) -> Result<DuplicateReport, String> {
    duplicate_stats_with_max_keys(model, rows, columns, top_n, MAX_DUP_KEYS, progress)
}

/// [`duplicate_stats`] 的内部实现（`max_keys` 显式传入，便于测试用小上限驱动超限路径）
fn duplicate_stats_with_max_keys(
    model: &DataModel,
    rows: Option<&[u32]>,
    columns: &[usize],
    top_n: usize,
    max_keys: usize,
    progress: Option<&(dyn Fn(f64) + Sync)>,
) -> Result<DuplicateReport, String> {
    let column_count = model.column_count();
    // 空表示「全部字段」；同时丢弃越界列（与去重一致的语义）
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
    if cols.is_empty() {
        return Err("没有可用于判重的列（表头为空？）".to_string());
    }
    let top_n = top_n.clamp(1, MAX_DUP_TOP_N);

    let total = rows.map(|v| v.len()).unwrap_or_else(|| model.row_count());
    // 表头：判重字段各一列 + 出现次数 / 重复行数 / 占行数(%) / 首次出现行号
    let mut headers: Vec<String> = cols.iter().map(|&c| column_name(model, c)).collect();
    headers.push("出现次数".to_string());
    headers.push("重复行数".to_string());
    headers.push("占行数(%)".to_string());
    headers.push("首次出现行号".to_string());

    let mut table = StatsTable {
        kind: StatsKind::DuplicateCheck,
        headers,
        rows: Vec::new(),
        input_rows: total,
        group_columns: cols.len(),
        aggs: 0,
        notes: Vec::new(),
    };

    // 键 → 出现次数 / 首次出现行号 / 后续出现行号
    struct KeyStat {
        count: u64,
        first: u32,
        rest: Vec<u32>,
    }
    let mut map: HashMap<String, KeyStat> = HashMap::with_capacity(1024);
    let mut reader = RowReader::new(cols.clone());
    let mut key = String::with_capacity(256);
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
        match map.get_mut(key.as_str()) {
            Some(st) => {
                st.count += 1;
                st.rest.push(row as u32);
            }
            None => {
                if map.len() >= max_keys {
                    return Err(format!(
                        "判重键的不同取值超过上限 {}（统计范围 {} 行）：请先用筛选缩小范围，\
                         或减少判重字段，再重新执行",
                        max_keys, total
                    ));
                }
                map.insert(
                    key.clone(),
                    KeyStat {
                        count: 1,
                        first: row as u32,
                        rest: Vec::new(),
                    },
                );
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

    // 汇总：唯一行 = 各键首次出现；重复组 = (键, 次数, 首次行号, 该键全部行号)
    let mut unique_rows: Vec<u32> = Vec::with_capacity(map.len());
    let mut detail: Vec<(String, u64, u32, Vec<u32>)> = Vec::new();
    for (k, st) in map.drain() {
        unique_rows.push(st.first);
        if st.count > 1 {
            // 该键的全部行：首次出现 + 后续出现（`rest` 按扫描顺序天然升序）
            let mut rows_of_key = Vec::with_capacity(st.rest.len() + 1);
            rows_of_key.push(st.first);
            rows_of_key.extend_from_slice(&st.rest);
            detail.push((k, st.count, st.first, rows_of_key));
        }
    }
    unique_rows.sort_unstable();

    // 明细排序：重复次数降序，次数相同按键升序（稳定可复现）
    detail.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let duplicate_keys = detail.len();
    // 「只看重复的行」的行号序列：**按重复键分组**展开（同组相邻、组内行号升序），
    // 组顺序与明细表一致 —— 预览区里同一个键的各行因此紧挨着，便于一眼核对
    let mut dup_rows: Vec<u32> = Vec::with_capacity(
        detail.iter().map(|(_, _, _, r)| r.len()).sum::<usize>(),
    );
    for (_, _, _, rows_of_key) in &detail {
        dup_rows.extend_from_slice(rows_of_key);
    }
    let shown = top_n.min(duplicate_keys);
    let pct = |n: u64| fmt_pct(n as f64 * 100.0 / total.max(1) as f64);
    for (k, count, first, _) in detail.iter().take(shown) {
        let mut row: Vec<String> = Vec::with_capacity(cols.len() + 4);
        for part in k.splitn(cols.len(), '\u{1F}') {
            row.push(part.to_string());
        }
        while row.len() < cols.len() {
            row.push(String::new());
        }
        row.push(count.to_string());
        row.push((count - 1).to_string());
        row.push(pct(*count));
        row.push((*first as u64 + 1).to_string()); // 行号从 1 开始（与表格显示一致）
        table.rows.push(row);
    }
    if duplicate_keys > shown {
        let rest_keys = duplicate_keys - shown;
        let rest_rows: u64 = detail[shown..].iter().map(|(_, c, _, _)| *c).sum();
        let mut row: Vec<String> = Vec::with_capacity(cols.len() + 4);
        row.push(format!("(其余 {} 个重复键)", rest_keys));
        while row.len() < cols.len() {
            row.push(String::new());
        }
        row.push(rest_rows.to_string());
        row.push((rest_rows - rest_keys as u64).to_string());
        row.push(pct(rest_rows));
        row.push(String::new());
        table.rows.push(row);
    }
    if duplicate_keys == 0 {
        table.notes.push(
            "统计范围内没有重复键（每个键只出现一次）：重复行 0，可切换到「只看唯一行」查看全部。"
                .to_string(),
        );
    } else if duplicate_keys > shown {
        table.notes.push(format!(
            "共 {} 个重复键，明细已列出前 {} 个（按重复次数降序），其余 {} 个见「(其余 …)」行",
            duplicate_keys, shown, duplicate_keys - shown
        ));
    }
    table.notes.push(
        "「重复行数」= 出现次数 − 1（即多出来的行）；「占行数(%)」= 该键行数 ÷ 统计范围行数，\
         按 4 位有效数字显示；行号从 1 开始（与表格显示一致）。"
            .to_string(),
    );
    table.notes.push(
        "「只看重复的行」会筛出重复键涉及的全部行（含首次出现的那行），并**按重复键分组排列**\
         （同一个键的各行相邻、组内按行号升序，组顺序与本表一致），行号列显示的是原始行号。"
            .to_string(),
    );
    table.notes.push(
        "判重口径与「提取唯一行（去重）」完全一致（按原值组合，保留首次出现的行）。".to_string(),
    );

    let distinct_keys = unique_rows.len();
    Ok(DuplicateReport {
        table,
        input_rows: total,
        distinct_keys,
        duplicate_keys,
        // 重复行数 = 多出来的那部分（统计范围行数 − 不同键个数）；
        // 注意与 `dup_rows` 区分：后者是「重复键涉及的全部行」（含首次出现），
        // 用于「只看重复的行」——看的时候需要连首次出现一起看才能比对
        duplicate_rows: total.saturating_sub(distinct_keys),
        dup_rows,
        unique_rows,
    })
}

/// 交叉统计可容纳的不同 (A,B) 组合数上限（超出报错，提示先筛选缩小范围）
const MAX_CROSS_PAIRS: usize = 1_000_000;
/// 交叉统计明细里每个 A 最多列出几个 B 取值作为示例
const CROSS_SAMPLE_MAX: usize = 5;
/// 交叉统计明细允许的最大 Top-N
pub const MAX_CROSS_TOP_N: usize = 1000;

/// 「交叉/关联统计」的结果：关系判定 + 明细表
#[derive(Debug, Clone)]
pub struct CrossReport {
    /// 明细表（每个 A 一行：行数 / 对应 B 的不同值数 / B 取值示例）
    pub table: StatsTable,
    /// A 字段名
    pub a_name: String,
    /// B 字段名
    pub b_name: String,
    /// 统计范围行数
    pub input_rows: usize,
    /// A 的不同取值个数
    pub a_values: usize,
    /// B 的不同取值个数
    pub b_values: usize,
    /// 不同 (A,B) 组合数
    pub pairs: usize,
    /// 单个 A 最多对应多少个不同 B
    pub max_b_per_a: usize,
    /// 单个 B 最多对应多少个不同 A
    pub max_a_per_b: usize,
    /// 关系判定（一对一 / 一对多 / 多对一 / 多对多）
    pub relation: &'static str,
}

impl CrossReport {
    /// 一句话摘要（结果窗与日志用）
    pub fn summary(&self) -> String {
        format!(
            "统计范围 {} 行 → 「{}」{} 个取值、「{}」{} 个取值、组合 {} 对；关系：{}",
            self.input_rows,
            self.a_name,
            self.a_values,
            self.b_name,
            self.b_values,
            self.pairs,
            self.relation
        )
    }
}

/// 交叉/关联统计：看两个字段之间的对应关系（例：「同一卡号对应多少个账号」）。
///
/// 输出：
/// - 关系判定：一对一 / 一对多（一个 A 对应多个 B）/ 多对一 / 多对多，
///   同时给出两个方向的「最大对应数」（单个 A 最多对应几个 B、单个 B 最多对应几个 A）；
/// - 明细：每个 A 一行 —— 行数、对应 B 的不同值数、B 取值示例（最多
///   [`CROSS_SAMPLE_MAX`] 个），按「对应 B 的不同值数」降序（最"发散"的排前面）。
///
/// 口径说明：按**原值**比较（与去重一致），空值作为一种正常取值参与（示例里显示为 `(空)`）。
/// 内存护栏：不同 (A,B) 组合数 ≤ [`MAX_CROSS_PAIRS`]，A 的不同取值 ≤ [`MAX_GROUPS`]。
/// 单线程（需要同时维护双向映射，与去重/重复统计同理）。
pub fn cross_stats(
    model: &DataModel,
    rows: Option<&[u32]>,
    a_col: usize,
    b_col: usize,
    top_n: usize,
    progress: Option<&(dyn Fn(f64) + Sync)>,
) -> Result<CrossReport, String> {
    let column_count = model.column_count();
    if a_col >= column_count || b_col >= column_count {
        return Err("字段无效（列可能已删除或隐藏）".to_string());
    }
    if a_col == b_col {
        return Err("两个字段不能相同（同一个字段的交叉没有意义）".to_string());
    }
    let top_n = top_n.clamp(1, MAX_CROSS_TOP_N);
    let a_name = column_name(model, a_col);
    let b_name = column_name(model, b_col);
    let total = rows.map(|v| v.len()).unwrap_or_else(|| model.row_count());

    // A 侧：保序（首次出现）+ 行数 + 该 A 的 B 取值集合；B 侧：B → A 取值集合
    let mut a_index: HashMap<String, usize> = HashMap::new();
    let mut a_keys: Vec<String> = Vec::new();
    let mut a_rows: Vec<u64> = Vec::new();
    let mut a_bsets: Vec<HashSet<String>> = Vec::new();
    let mut b_map: HashMap<String, HashSet<String>> = HashMap::new();
    let mut pairs: usize = 0;

    let mut reader = RowReader::new(vec![a_col, b_col]);
    for i in 0..total {
        let row = match rows {
            Some(list) => list[i] as usize,
            None => i,
        };
        reader.load(model.file(), row);
        let av = reader.value(a_col);
        let bv = reader.value(b_col);
        let aidx = match a_index.get(av) {
            Some(&i) => i,
            None => {
                if a_keys.len() >= MAX_GROUPS {
                    return Err(format!(
                        "「{}」的不同取值超过上限 {}：请先用筛选缩小范围",
                        a_name, MAX_GROUPS
                    ));
                }
                let i = a_keys.len();
                a_index.insert(av.to_string(), i);
                a_keys.push(av.to_string());
                a_rows.push(0);
                a_bsets.push(HashSet::new());
                i
            }
        };
        a_rows[aidx] += 1;
        if !a_bsets[aidx].contains(bv) {
            a_bsets[aidx].insert(bv.to_string());
            pairs += 1;
            if pairs > MAX_CROSS_PAIRS {
                return Err(format!(
                    "「{}×{}」的不同组合数超过上限 {}：请先用筛选缩小范围",
                    a_name, b_name, MAX_CROSS_PAIRS
                ));
            }
        }
        match b_map.get_mut(bv) {
            Some(set) => {
                if !set.contains(av) {
                    set.insert(av.to_string());
                }
            }
            None => {
                let mut set = HashSet::new();
                set.insert(av.to_string());
                b_map.insert(bv.to_string(), set);
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

    // 关系判定：两个方向是否存在"一对多"
    let a_values = a_keys.len();
    let b_values = b_map.len();
    let max_b_per_a = a_bsets.iter().map(|s| s.len()).max().unwrap_or(0);
    let max_a_per_b = b_map.values().map(|s| s.len()).max().unwrap_or(0);
    let relation = match (max_b_per_a > 1, max_a_per_b > 1) {
        (false, false) => "一对一（每个取值互相唯一对应）",
        (true, false) => "一对多（一个取值对应多个对方取值）",
        (false, true) => "多对一（多个取值对应同一个对方取值）",
        (true, true) => "多对多（两侧都存在一对多）",
    };

    // 明细：按「对应 B 的不同值数」降序 → 行数降序 → A 值升序
    let mut order: Vec<usize> = (0..a_values).collect();
    order.sort_by(|&x, &y| {
        a_bsets[y]
            .len()
            .cmp(&a_bsets[x].len())
            .then_with(|| a_rows[y].cmp(&a_rows[x]))
            .then_with(|| a_keys[x].cmp(&a_keys[y]))
    });

    let headers: Vec<String> = vec![
        a_name.clone(),
        "行数".to_string(),
        format!("对应「{}」的不同值数", b_name),
        format!("「{}」取值示例", b_name),
    ];
    let mut out_rows: Vec<Vec<String>> = Vec::with_capacity(order.len() + 1);
    let shown = top_n.min(order.len());
    for &aidx in order.iter().take(shown) {
        let mut sample: Vec<&String> = a_bsets[aidx].iter().collect();
        sample.sort();
        let sample_text = if sample.len() <= CROSS_SAMPLE_MAX {
            sample
                .iter()
                .map(|s| display_value(s))
                .collect::<Vec<_>>()
                .join("、")
        } else {
            format!(
                "{}…（共 {} 个）",
                sample[..CROSS_SAMPLE_MAX]
                    .iter()
                    .map(|s| display_value(s))
                    .collect::<Vec<_>>()
                    .join("、"),
                sample.len()
            )
        };
        out_rows.push(vec![
            display_value(&a_keys[aidx]),
            a_rows[aidx].to_string(),
            a_bsets[aidx].len().to_string(),
            sample_text,
        ]);
    }
    if order.len() > shown {
        let rest: usize = order.len() - shown;
        out_rows.push(vec![
            format!("(其余 {} 个取值)", rest),
            String::new(),
            String::new(),
            String::new(),
        ]);
    }

    let mut table = StatsTable {
        kind: StatsKind::CrossTab,
        headers,
        rows: out_rows,
        input_rows: total,
        group_columns: 0,
        aggs: a_values,
        notes: Vec::new(),
    };
    table.notes.push(format!(
        "「{}」{} 个取值、「{}」{} 个取值、不同组合 {} 对；关系判定：{}",
        a_name, a_values, b_name, b_values, pairs, relation
    ));
    table.notes.push(format!(
        "单个「{}」最多对应 {} 个「{}」；单个「{}」最多对应 {} 个「{}」",
        a_name, max_b_per_a, b_name, b_name, max_a_per_b, a_name
    ));
    table.notes.push(
        "按原值比较（与去重一致），空值作为正常取值参与（示例里显示为「(空)」）；\
         「取值示例」最多列 5 个，超出只显示前几个与总数"
            .to_string(),
    );

    Ok(CrossReport {
        table,
        a_name,
        b_name,
        input_rows: total,
        a_values,
        b_values,
        pairs,
        max_b_per_a,
        max_a_per_b,
        relation,
    })
}

/// 展示用的取值文本（空值显示为 `(空)`，与频次分布的约定一致）
fn display_value(s: &str) -> String {
    if s.trim().is_empty() {
        "(空)".to_string()
    } else {
        s.to_string()
    }
}

/// 透视表界面可选的聚合方式（「去重计数」不在其中：[`pivot_table`] 会拒绝）
pub const PIVOT_KINDS: [AggKind; 5] = [
    AggKind::Sum,
    AggKind::Avg,
    AggKind::Max,
    AggKind::Min,
    AggKind::Count,
];

/// 透视表的列字段取值个数上限（超出报错：列太多既看不清也意义有限）
const MAX_PIVOT_COLUMNS: usize = 200;
/// 透视表的单元格总数上限（行组合 × 出现过的列取值；每个单元格一个聚合器，约 60MB）
const MAX_PIVOT_CELLS: usize = 500_000;

/// 透视表（行字段 × 列字段 × 值聚合）。
///
/// 语义：
/// - 行 = `row_cols`（1~2 个字段）的取值组合，列 = `col_col` 的各个取值，
///   单元格 = `value_col` 在 `kind`（求和/均值/最小/最大/计数）下的聚合值；
/// - 末行「(合计)」是各列的**整体聚合**（不是把各行相加）——所以均值、计数这类
///   非可加聚合在合计行依然正确；每行末尾也是该行的整体聚合；
/// - 列顺序按**列字段取值升序**（稳定、可预测；日期字符串升序即时间顺序）；
///   空值的列名显示为 `(空)`；行顺序为行键首次出现顺序（与分组汇总一致）；
/// - 没有数据的单元格留空（与"聚合结果为 0"区分开）。
///
/// 护栏：行组合数 ≤ [`MAX_GROUPS`]、列字段取值 ≤ [`MAX_PIVOT_COLUMNS`]、
/// 单元格总数 ≤ [`MAX_PIVOT_CELLS`]（超出均报错并提示改用分组汇总或先筛选）；
/// 不支持「去重计数」（每个单元格一个集合，内存不可控，可用分组汇总替代）。
pub fn pivot_table(
    model: &DataModel,
    rows: Option<&[u32]>,
    row_cols: &[usize],
    col_col: usize,
    value_col: usize,
    kind: AggKind,
    progress: Option<&(dyn Fn(f64) + Sync)>,
) -> Result<StatsTable, String> {
    let column_count = model.column_count();
    // 行字段：丢弃越界列，保持用户顺序
    let mut rcols: Vec<usize> = Vec::new();
    for &c in row_cols {
        if c < column_count && !rcols.contains(&c) {
            rcols.push(c);
        }
    }
    if rcols.is_empty() {
        return Err("请至少选择一个有效的行字段（列可能已删除或隐藏）".to_string());
    }
    if rcols.len() > MAX_GROUP_COLUMNS {
        return Err(format!("行字段最多 {} 个", MAX_GROUP_COLUMNS));
    }
    if col_col >= column_count {
        return Err("列字段无效（列可能已删除或隐藏）".to_string());
    }
    if value_col >= column_count {
        return Err("值列无效（列可能已删除或隐藏）".to_string());
    }
    if rcols.contains(&col_col) {
        return Err("列字段不能与行字段相同".to_string());
    }
    if kind == AggKind::DistinctCount {
        return Err(
            "透视表不支持「去重计数」（每个单元格都要维护一个值集合，内存不可控）；\
             如需此口径请用「字段分组汇总」按两个字段分组"
                .to_string(),
        );
    }

    let total = rows.map(|v| v.len()).unwrap_or_else(|| model.row_count());
    let mut row_index: HashMap<String, usize> = HashMap::new();
    let mut row_keys: Vec<String> = Vec::new();
    let mut row_aggs: Vec<RowPivot> = Vec::new();
    let mut col_index: HashMap<String, usize> = HashMap::new();
    let mut col_values: Vec<String> = Vec::new();
    let mut cells_total: usize = 0;
    let mut grand = Acc::new(kind);

    // 只读这三类列（RowReader 整行只解析一次）
    let mut need: Vec<usize> = rcols.clone();
    need.push(col_col);
    need.push(value_col);
    let mut reader = RowReader::new(need);
    let mut key = String::with_capacity(128);
    for i in 0..total {
        let row = match rows {
            Some(list) => list[i] as usize,
            None => i,
        };
        reader.load(model.file(), row);
        key.clear();
        for (k, &c) in rcols.iter().enumerate() {
            if k > 0 {
                key.push('\u{1F}');
            }
            key.push_str(reader.value(c));
        }
        let ridx = match row_index.get(key.as_str()) {
            Some(&i) => i,
            None => {
                if row_aggs.len() >= MAX_GROUPS {
                    return Err(format!(
                        "透视表的行组合数超过上限 {}：请减少行字段、或先用筛选缩小范围",
                        MAX_GROUPS
                    ));
                }
                let i = row_aggs.len();
                row_index.insert(key.clone(), i);
                row_keys.push(key.clone());
                row_aggs.push(RowPivot::new(kind));
                i
            }
        };
        let cv = reader.value(col_col);
        if !col_index.contains_key(cv) {
            if col_values.len() >= MAX_PIVOT_COLUMNS {
                return Err(format!(
                    "列字段的不同取值超过上限 {}：请改用取值较少的列字段，\
                     或先筛选缩小范围（也可用「字段分组汇总」按两个字段分组）",
                    MAX_PIVOT_COLUMNS
                ));
            }
            col_index.insert(cv.to_string(), col_values.len());
            col_values.push(cv.to_string());
        }
        let value = reader.value(value_col);
        // 单元格（借用分开：先更新 cells，再更新行合计，避免同时可变借用同一结构）
        let cell_missing = {
            let ra = &mut row_aggs[ridx];
            ra.rows += 1;
            match ra.cells.get_mut(cv) {
                Some(acc) => {
                    acc.push(kind, value);
                    false
                }
                None => true,
            }
        };
        if cell_missing {
            if cells_total >= MAX_PIVOT_CELLS {
                return Err(format!(
                    "透视表的单元格数超过上限 {}：请减少行字段、换一个列字段，或先筛选缩小范围",
                    MAX_PIVOT_CELLS
                ));
            }
            cells_total += 1;
            let mut acc = Acc::new(kind);
            acc.push(kind, value);
            row_aggs[ridx].cells.insert(cv.to_string(), acc);
        }
        row_aggs[ridx].total.push(kind, value);
        grand.push(kind, value);
        if let Some(p) = progress {
            if i % 50_000 == 0 {
                p(i as f64 / total.max(1) as f64);
            }
        }
    }
    if let Some(p) = progress {
        p(1.0);
    }

    // 列顺序：取值升序（空值排在最前，作为「(空)」列）
    let mut col_sorted = col_values.clone();
    col_sorted.sort_unstable();

    // 表头：行字段名… + 各列取值 + 合计
    let mut headers: Vec<String> = rcols.iter().map(|&c| column_name(model, c)).collect();
    for cv in &col_sorted {
        headers.push(if cv.is_empty() {
            "(空)".to_string()
        } else {
            cv.clone()
        });
    }
    headers.push("(合计)".to_string());

    let mut out_rows: Vec<Vec<String>> = Vec::with_capacity(row_aggs.len() + 1);
    for (key, ra) in row_keys.iter().zip(row_aggs.iter()) {
        let mut row: Vec<String> = Vec::with_capacity(headers.len());
        for part in key.splitn(rcols.len(), '\u{1F}') {
            row.push(part.to_string());
        }
        while row.len() < rcols.len() {
            row.push(String::new());
        }
        for cv in &col_sorted {
            row.push(match ra.cells.get(cv) {
                Some(acc) => acc.finish(kind),
                None => String::new(), // 没有数据的单元格留空
            });
        }
        row.push(ra.total.finish(kind));
        out_rows.push(row);
    }
    // 末行合计：逐列的整体聚合（各自的累加器，不是把行相加）
    if total > 0 {
        let mut row: Vec<String> = vec!["(合计)".to_string()];
        while row.len() < rcols.len() {
            row.push(String::new());
        }
        for cv in &col_sorted {
            let mut acc = Acc::new(kind);
            for ra in row_aggs.iter() {
                if let Some(cell) = ra.cells.get(cv) {
                    acc.merge(cell);
                }
            }
            row.push(acc.finish(kind));
        }
        row.push(grand.finish(kind));
        out_rows.push(row);
    }

    let row_name = rcols
        .iter()
        .map(|&c| column_name(model, c))
        .collect::<Vec<_>>()
        .join("、");
    let mut table = StatsTable {
        kind: StatsKind::Pivot,
        headers,
        rows: out_rows,
        input_rows: total,
        group_columns: col_sorted.len(),
        aggs: 0,
        notes: Vec::new(),
    };
    table.notes.push(format!(
        "行字段：{}；列字段：{}；值：{}({})",
        row_name,
        column_name(model, col_col),
        column_name(model, value_col),
        kind.label()
    ));
    table.notes.push(format!(
        "列字段共 {} 个取值（按取值升序排列，空值为「(空)」列）；没有数据的单元格留空",
        col_sorted.len()
    ));
    table.notes.push(
        "每行末尾的「(合计)」是该行的整体聚合，末行是各列的整体聚合 —— 均值/计数等\
         非可加指标因此在合计处依然正确（不是把各行数值相加）"
            .to_string(),
    );
    Ok(table)
}

/// 透视表的一行：单元格 + 行合计
struct RowPivot {
    /// 列值 → 单元格聚合器（借用查询时避免重复分配）
    cells: HashMap<String, Acc>,
    /// 该行的整体聚合（行末「(合计)」列）
    total: Acc,
    /// 该行参与统计的行数
    rows: u64,
}

impl RowPivot {
    fn new(kind: AggKind) -> Self {
        Self {
            cells: HashMap::new(),
            total: Acc::new(kind),
            rows: 0,
        }
    }
}

/// 「空值与数据质量统计」的指标表头（每列一行）
const QUALITY_HEADERS: [&str; 10] = [
    "列名",
    "行数",
    "空值数",
    "纯空白数",
    "空值率(%)",
    "含控制字符数",
    "最短长度",
    "最长长度",
    "平均长度",
    "超长值数(>255)",
];

/// 视为「超长值」的字符数阈值（Excel 单元格/多数系统字段的经验上限）
pub const LONG_VALUE_LEN: u32 = 255;

/// 空值与数据质量统计：逐列给出
/// 空值数、纯空白数、空值率、含控制字符数、最短/最长/平均长度、超长值数。
///
/// 口径（都写进 `notes`）：
/// - **空值数** = 长度为 0 的值；**纯空白数** = `trim` 后为空但长度 > 0（全是空格/制表符等）；
///   **空值率** = (空值数 + 纯空白数) ÷ 行数 —— 业务上"只有空白"与"空"通常等价，故合并计率；
/// - **含控制字符数** = 值里出现 Unicode 控制字符（`char::is_control()`，含制表符 `\t`、
///   换行、以及 `0x7F`）的行数 —— 银行 CSV 常见的尾随制表符会在这里显形；
/// - **长度**按**字符数**统计（中文算 1 个字符，不是字节数）；最短/最长/平均只统计非空值；
/// - **超长值数** = 字符数 > [`LONG_VALUE_LEN`] 的值的个数。
///
/// 一次并行扫描算完所有列（每列 O(1) 状态，无需保留原始值）。
pub fn describe_quality(
    model: &DataModel,
    rows: Option<&[u32]>,
    cols: &[usize],
    progress: Option<&(dyn Fn(f64) + Sync)>,
) -> Result<StatsTable, String> {
    let column_count = model.column_count();
    let mut list: Vec<usize> = Vec::new();
    for &c in cols {
        if c < column_count && !list.contains(&c) {
            list.push(c);
        }
    }
    if list.is_empty() {
        return Err("请至少选择一个有效的列（列可能已删除或隐藏）".to_string());
    }

    let total = rows.map(|v| v.len()).unwrap_or_else(|| model.row_count());
    let mut table = StatsTable {
        kind: StatsKind::DataQuality,
        headers: QUALITY_HEADERS.iter().map(|s| s.to_string()).collect(),
        rows: Vec::new(),
        input_rows: total,
        group_columns: 0,
        aggs: list.len(),
        notes: Vec::new(),
    };
    if total == 0 {
        for &c in &list {
            let acc = QualityAcc::new();
            table.rows.push(acc.finish(model, c, 0));
        }
        if let Some(p) = progress {
            p(1.0);
        }
        return Ok(table);
    }

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(MAX_STATS_THREADS)
        .min(total);
    let chunk = (total / threads).max(1);
    let done = AtomicUsize::new(0);
    let mut blocks: Vec<Vec<QualityAcc>> = Vec::new();
    std::thread::scope(|s| {
        let mut handles = Vec::new();
        let mut start = 0usize;
        while start < total {
            let end = (start + chunk).min(total);
            let model_ref = model;
            let list_ref = &list;
            let rows_ref = rows;
            let done_ref = &done;
            let prog = progress;
            handles.push(s.spawn(move || {
                let mut reader = RowReader::new(list_ref.clone());
                let mut accs: Vec<QualityAcc> = list_ref.iter().map(|_| QualityAcc::new()).collect();
                for i in start..end {
                    let row = match rows_ref {
                        Some(l) => l[i] as usize,
                        None => i,
                    };
                    reader.load(model_ref.file(), row);
                    for (acc, &c) in accs.iter_mut().zip(list_ref.iter()) {
                        acc.push(reader.value(c));
                    }
                }
                done_ref.fetch_add(end - start, AtomicOrdering::Relaxed);
                if let Some(p) = prog {
                    p(done_ref.load(AtomicOrdering::Relaxed) as f64 / total as f64);
                }
                accs
            }));
            start = end;
        }
        for h in handles {
            blocks.push(h.join().unwrap_or_default());
        }
    });

    let mut accs: Vec<QualityAcc> = list.iter().map(|_| QualityAcc::new()).collect();
    for block in blocks {
        for (a, b) in accs.iter_mut().zip(block.iter()) {
            a.merge(b);
        }
    }
    let mut bad_empty = 0usize;
    let mut bad_control = 0usize;
    let mut bad_long = 0usize;
    for acc in accs.iter() {
        if acc.empty + acc.blank > 0 {
            bad_empty += 1;
        }
        if acc.control > 0 {
            bad_control += 1;
        }
        if acc.long_count > 0 {
            bad_long += 1;
        }
    }
    for (acc, &c) in accs.iter_mut().zip(list.iter()) {
        table.rows.push(acc.finish(model, c, total as u64));
    }
    table.notes.push(format!(
        "共统计 {} 列：其中 {} 列存在空值或纯空白、{} 列含控制字符、{} 列有超长值（>{} 字符）",
        list.len(),
        bad_empty,
        bad_control,
        bad_long,
        LONG_VALUE_LEN
    ));
    table.notes.push(
        "空值率 = (空值数 + 纯空白数) ÷ 行数（业务上「只有空白」通常等同于空），按 4 位有效数字显示。"
            .to_string(),
    );
    table.notes.push(
        "含控制字符 = 值里出现制表符/换行/其它 Unicode 控制字符的行数；长度按字符数统计（中文算 1 个）。"
            .to_string(),
    );
    if let Some(p) = progress {
        p(1.0);
    }
    Ok(table)
}

/// 单列的数据质量累加器（O(1) 状态，不保留原始值）
#[derive(Debug, Clone)]
struct QualityAcc {
    /// 统计范围行数
    rows: u64,
    /// 空值数（长度为 0）
    empty: u64,
    /// 纯空白数（trim 后为空但长度 > 0）
    blank: u64,
    /// 含控制字符的行数
    control: u64,
    /// 非空值个数（长度统计的分母）
    non_empty: u64,
    /// 最短长度（非空值；`u32::MAX` 表示尚无样本）
    min_len: u32,
    /// 最长长度（非空值）
    max_len: u32,
    /// 长度合计（非空值）
    sum_len: u64,
    /// 超长值个数（字符数 > [`LONG_VALUE_LEN`]）
    long_count: u64,
}

impl QualityAcc {
    fn new() -> Self {
        Self {
            rows: 0,
            empty: 0,
            blank: 0,
            control: 0,
            non_empty: 0,
            min_len: u32::MAX,
            max_len: 0,
            sum_len: 0,
            long_count: 0,
        }
    }

    fn push(&mut self, value: &str) {
        self.rows += 1;
        if value.is_empty() {
            self.empty += 1;
            return;
        }
        // 一次遍历同时得到字符数与是否含控制字符
        let mut len: u32 = 0;
        let mut has_control = false;
        for ch in value.chars() {
            len += 1;
            if !has_control && ch.is_control() {
                has_control = true;
            }
        }
        if has_control {
            self.control += 1;
        }
        if value.trim().is_empty() {
            self.blank += 1;
        }
        self.non_empty += 1;
        self.min_len = self.min_len.min(len);
        self.max_len = self.max_len.max(len);
        self.sum_len += len as u64;
        if len > LONG_VALUE_LEN {
            self.long_count += 1;
        }
    }

    fn merge(&mut self, other: &QualityAcc) {
        self.rows += other.rows;
        self.empty += other.empty;
        self.blank += other.blank;
        self.control += other.control;
        self.non_empty += other.non_empty;
        self.min_len = self.min_len.min(other.min_len);
        self.max_len = self.max_len.max(other.max_len);
        self.sum_len += other.sum_len;
        self.long_count += other.long_count;
    }

    fn finish(&self, model: &DataModel, col: usize, rows: u64) -> Vec<String> {
        let has_value = self.non_empty > 0;
        let empty_rate = if rows == 0 {
            0.0
        } else {
            (self.empty + self.blank) as f64 * 100.0 / rows as f64
        };
        vec![
            column_name(model, col),
            rows.to_string(),
            self.empty.to_string(),
            self.blank.to_string(),
            fmt_pct(empty_rate),
            self.control.to_string(),
            if has_value {
                self.min_len.to_string()
            } else {
                String::new()
            },
            if has_value {
                self.max_len.to_string()
            } else {
                String::new()
            },
            if has_value {
                fmt_float(self.sum_len as f64 / self.non_empty as f64)
            } else {
                String::new()
            },
            self.long_count.to_string(),
        ]
    }
}

/// 一个行块内、所有统计列的频次表
#[derive(Debug, Default)]
struct FreqBlock {
    /// 每列：取值 → 出现次数
    maps: Vec<HashMap<String, u64>>,
    /// 每列的空值（trim 后为空）行数
    empties: Vec<u64>,
    /// 每列是否因超过上限而停止收集新取值
    overflow: Vec<bool>,
}

impl FreqBlock {
    fn new(cols: usize) -> Self {
        Self {
            maps: (0..cols).map(|_| HashMap::new()).collect(),
            empties: vec![0; cols],
            overflow: vec![false; cols],
        }
    }

    /// 累加一行中某一列的取值；超过 `cap` 时只置超限标记（内存有界，最终由调用方报错）
    fn push(&mut self, idx: usize, value: &str, cap: usize) {
        let t = value.trim();
        if t.is_empty() {
            self.empties[idx] += 1;
            return;
        }
        let map = &mut self.maps[idx];
        match map.get_mut(value) {
            // 命中已有取值：借用查询，不分配
            Some(count) => *count += 1,
            None => {
                if map.len() >= cap {
                    self.overflow[idx] = true;
                } else {
                    map.insert(value.to_string(), 1);
                }
            }
        }
    }
}

/// 单列的描述统计累加器
#[derive(Debug, Clone)]
struct ColAcc {    /// 参与统计的行数（含空值）
    rows: u64,
    /// 非空值个数
    count: u64,
    /// 空值个数（trim 后为空）
    empty: u64,
    /// 非空但非数值的个数
    non_numeric: u64,
    /// 数值个数（均值分母、中位数基数）
    numeric: u64,
    /// 定点十进制求和
    dec: DecSum,
    /// f64 求和（定点不可用时的回退）
    float_sum: f64,
    /// Welford 在线均值
    mean: f64,
    /// Welford 二阶中心矩累积量（Σ(x-mean)²）
    m2: f64,
    min: Option<Num>,
    max: Option<Num>,
    /// 保留的原始数值（精确中位数用；最多 `cap` 个）
    values: Vec<f64>,
    /// 单列保留上限
    cap: usize,
}

impl ColAcc {
    fn new(cap: usize) -> Self {
        Self {
            rows: 0,
            count: 0,
            empty: 0,
            non_numeric: 0,
            numeric: 0,
            dec: DecSum::default(),
            float_sum: 0.0,
            mean: 0.0,
            m2: 0.0,
            min: None,
            max: None,
            values: Vec::new(),
            cap,
        }
    }

    /// 累加一个字段值（原始文本）
    fn push(&mut self, value: &str) {
        self.rows += 1;
        let t = value.trim();
        if t.is_empty() {
            self.empty += 1;
            return;
        }
        self.count += 1;
        match try_num(t) {
            Some(Num::Int(i)) => {
                self.numeric += 1;
                self.dec.add(i, 0);
                let f = i as f64;
                self.float_sum += f;
                self.welford(f);
                self.push_num(Num::Int(i));
                self.keep(f);
            }
            Some(Num::Float(f)) if f.is_finite() => {
                self.numeric += 1;
                self.dec.add_text(t);
                self.float_sum += f;
                self.welford(f);
                self.push_num(Num::Float(f));
                self.keep(f);
            }
            // NaN / ±inf 与解析失败一样，都不算数值
            _ => self.non_numeric += 1,
        }
    }

    /// Welford 在线更新（调用前 `numeric` 已 +1）
    fn welford(&mut self, v: f64) {
        let n = self.numeric as f64;
        let delta = v - self.mean;
        self.mean += delta / n;
        self.m2 += delta * (v - self.mean);
    }

    fn push_num(&mut self, n: Num) {
        self.min = Some(match self.min {
            Some(m) if cmp_num(&m, &n) != Ordering::Greater => m,
            _ => n,
        });
        self.max = Some(match self.max {
            Some(m) if cmp_num(&m, &n) != Ordering::Less => m,
            _ => n,
        });
    }

    /// 保留原始数值（超出上限就自然停止，`values.len() != numeric` 即「中位数不可精确」）
    fn keep(&mut self, v: f64) {
        if self.values.len() < self.cap {
            self.values.push(v);
        }
    }

    /// 合并另一个块的同列结果
    fn merge(&mut self, other: &ColAcc) {
        self.rows += other.rows;
        self.count += other.count;
        self.empty += other.empty;
        self.non_numeric += other.non_numeric;
        self.dec.merge(&other.dec);
        self.float_sum += other.float_sum;
        // Welford 并行合并（Chan 公式的稳定形式）：
        //   mean = meanA + δ·n₂/n（而不是 (n₁·meanA + n₂·meanB)/n —— 后者即使两边均值
        //   相同，也会因 n₁·meanA 的舍入让均值漂移，进而让 M2 凭空增长）
        //   M2   = M2A + M2B + δ²·n₁·n₂/n
        if other.numeric > 0 {
            let n1 = self.numeric as f64;
            let n2 = other.numeric as f64;
            if n1 == 0.0 {
                self.mean = other.mean;
                self.m2 = other.m2;
            } else {
                let n = n1 + n2;
                let delta = other.mean - self.mean;
                self.mean += delta * (n2 / n);
                self.m2 += other.m2 + delta * delta * n1 * n2 / n;
            }
        }
        if let Some(m) = other.min {
            self.min = Some(match self.min {
                Some(x) if cmp_num(&x, &m) != Ordering::Greater => x,
                _ => m,
            });
        }
        if let Some(m) = other.max {
            self.max = Some(match self.max {
                Some(x) if cmp_num(&x, &m) != Ordering::Less => x,
                _ => m,
            });
        }
        // 中位数保留值：到合并前仍完整（values.len() == numeric）才继续拼接
        if self.values.len() as u64 == self.numeric {
            let room = self.cap.saturating_sub(self.values.len());
            let take = other.values.len().min(room);
            self.values.extend_from_slice(&other.values[..take]);
        }
        // 数值个数最后累加（上面的完整性判断用的是合并前的值）
        self.numeric += other.numeric;
    }

    /// 中位数文本（`—` = 数值过多未保留全量）
    fn median_text(&mut self) -> String {
        if self.numeric == 0 {
            return String::new();
        }
        if self.values.len() as u64 != self.numeric {
            return "—".to_string();
        }
        self.values
            .sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
        let n = self.values.len();
        let median = if n % 2 == 1 {
            self.values[n / 2]
        } else {
            (self.values[n / 2 - 1] + self.values[n / 2]) / 2.0
        };
        fmt_float(median)
    }

    /// 产出一行结果（每列一行）
    fn finish(&mut self, model: &DataModel, col: usize, rows: u64) -> Vec<String> {
        let has_num = self.numeric > 0;
        let mean = if has_num {
            let total = self.dec.exact_value().unwrap_or(self.float_sum);
            fmt_float(total / self.numeric as f64)
        } else {
            String::new()
        };
        let std_dev = if has_num {
            // 总体标准差（分母 n）；m2 理论上非负，防御性 max(0)
            fmt_float((self.m2 / self.numeric as f64).max(0.0).sqrt())
        } else {
            String::new()
        };
        let median = self.median_text();
        vec![
            column_name(model, col),
            rows.to_string(),
            self.count.to_string(),
            self.empty.to_string(),
            self.non_numeric.to_string(),
            if has_num {
                self.dec.text(self.float_sum)
            } else {
                String::new()
            },
            mean,
            median,
            if has_num {
                self.min.map(fmt_num).unwrap_or_default()
            } else {
                String::new()
            },
            if has_num {
                self.max.map(fmt_num).unwrap_or_default()
            } else {
                String::new()
            },
            std_dev,
        ]
    }
}

/// 新建一组聚合累加器（仅「去重计数」才创建集合，避免不必要的内存）
fn new_accs(specs: &[AggSpec]) -> Vec<Acc> {
    specs.iter().map(|s| Acc::new(s.kind)).collect()
}

/// 定点十进制累加器：把各数值按小数位数放大成 `i128` 整数相加，
/// 从而避免 f64 的累积误差（`0.01` 这类十进制小数在 f64 里并不精确，
/// 金额列成百上千次相加后会显形为 `3613134567.580001`）。
///
/// 只要遇到一个**无法**精确写成十进制字面量的值（科学计数法、超过
/// [`DEC_MAX_SCALE`] 位小数、超过 [`DEC_MAX_DIGITS`] 位数字、或 i128 溢出），
/// 就整体作废（`exact = false`），由调用方改用 f64 累加值兜底。
#[derive(Debug, Clone)]
struct DecSum {
    /// 各值按 [`DecSum::scale`] 放大后的累加和
    sum: i128,
    /// 当前小数位数（组内各值小数位数的最大值）
    scale: u32,
    /// 是否仍可用定点精确表示
    exact: bool,
}

impl Default for DecSum {
    fn default() -> Self {
        Self {
            sum: 0,
            scale: 0,
            exact: true,
        }
    }
}

impl DecSum {
    /// 累加一个已经解析好的定点值（`v` 为 `sc` 位小数的缩放整数）
    fn add(&mut self, v: i128, sc: u32) {
        if !self.exact {
            return; // 已回退 f64，不必再算
        }
        if sc > DEC_MAX_SCALE {
            self.exact = false;
            return;
        }
        // 先把两个操作数对齐到较大的小数位数
        let target = self.scale.max(sc);
        let scaled_v = match pow10_i128(target - sc).and_then(|f| v.checked_mul(f)) {
            Some(x) => x,
            None => {
                self.exact = false;
                return;
            }
        };
        if target > self.scale {
            let f = pow10_i128(target - self.scale).expect("scale 受 DEC_MAX_SCALE 限制");
            match self.sum.checked_mul(f) {
                Some(s) => self.sum = s,
                None => {
                    self.exact = false;
                    return;
                }
            }
            self.scale = target;
        }
        match self.sum.checked_add(scaled_v) {
            Some(s) => self.sum = s,
            None => self.exact = false,
        }
    }

    /// 累加一个数值文本（解析失败 → 回退 f64）
    fn add_text(&mut self, t: &str) {
        match parse_dec(t) {
            Some((v, sc)) => self.add(v, sc),
            None => self.exact = false,
        }
    }

    /// 合并另一个累加器（两边小数位数可能不同，先对齐再相加）
    fn merge(&mut self, other: &DecSum) {
        self.exact &= other.exact;
        if !self.exact {
            return;
        }
        let target = self.scale.max(other.scale);
        let a = pow10_i128(target - self.scale).and_then(|f| self.sum.checked_mul(f));
        let b = pow10_i128(target - other.scale).and_then(|f| other.sum.checked_mul(f));
        match a.and_then(|a| b.map(|b| (a, b))).and_then(|(a, b)| a.checked_add(b)) {
            Some(s) => {
                self.sum = s;
                self.scale = target;
            }
            None => self.exact = false,
        }
    }

    /// 精确值（不可精确时返回 `None`）
    fn exact_value(&self) -> Option<f64> {
        if self.exact {
            Some(self.sum as f64 / pow10_f64(self.scale))
        } else {
            None
        }
    }

    /// 输出文本：精确时用十进制（去掉尾随零），否则用 f64 兜底值
    fn text(&self, fallback: f64) -> String {
        if self.exact {
            fmt_dec(self.sum, self.scale)
        } else {
            fmt_float(fallback)
        }
    }
}

/// 单个分组内、单个聚合列的累加器
#[derive(Debug, Clone, Default)]
struct Acc {
    /// 组内参与本聚合的行数（含空值）= 组行数
    total: u64,
    /// 非空值个数（trim 后非空）
    non_empty: u64,
    /// 空值个数（trim 后为空）
    empty: u64,
    /// 非空但不是数值的个数（求和/均值/最大/最小忽略这些值）
    non_numeric: u64,
    /// 数值个数（可解析为数值的非空值）—— 均值的分母
    numeric: u64,
    /// 定点十进制求和
    dec: DecSum,
    /// f64 求和（始终累积；定点不可用时的回退）
    float_sum: f64,
    min: Option<Num>,
    max: Option<Num>,
    /// 去重计数用的集合（仅当聚合方式为去重计数时存在）
    distinct: Option<HashSet<String>>,
    /// 去重计数是否因超过 [`MAX_DISTINCT_VALUES`] 而变成「≥N」
    distinct_capped: bool,
}

impl Acc {
    fn new(kind: AggKind) -> Self {
        Self {
            distinct: if kind == AggKind::DistinctCount {
                Some(HashSet::with_capacity(64))
            } else {
                None
            },
            ..Default::default()
        }
    }

    /// 累加一个字段值（`value` 为本行该列的原始文本）
    fn push(&mut self, kind: AggKind, value: &str) {
        self.total += 1;
        let t = value.trim();
        if t.is_empty() {
            self.empty += 1;
            return;
        }
        self.non_empty += 1;
        match kind {
            AggKind::Count | AggKind::EmptyCount => {}
            AggKind::DistinctCount => {
                if let Some(set) = self.distinct.as_mut() {
                    // 未达上限时只有一次哈希（insert）；达上限后仅检查是否为新值
                    if set.len() >= MAX_DISTINCT_VALUES {
                        if !set.contains(t) {
                            self.distinct_capped = true;
                        }
                        return;
                    }
                    set.insert(t.to_string());
                }
            }
            AggKind::Sum | AggKind::Avg | AggKind::Min | AggKind::Max => match try_num(t) {
                Some(Num::Int(i)) => {
                    self.numeric += 1;
                    self.dec.add(i, 0);
                    self.float_sum += i as f64;
                    self.push_num(Num::Int(i));
                }
                // 能写成十进制字面量就用定点精确累加，否则整体回退 f64
                Some(Num::Float(f)) if f.is_finite() => {
                    self.numeric += 1;
                    self.dec.add_text(t);
                    self.float_sum += f;
                    self.push_num(Num::Float(f));
                }
                // NaN / ±inf 与解析失败一样，都不算用户意义上的数值（与 model::classify 一致）
                _ => self.non_numeric += 1,
            },
        }
    }

    fn push_num(&mut self, n: Num) {
        self.min = Some(match self.min {
            Some(m) if cmp_num(&m, &n) != Ordering::Greater => m,
            _ => n,
        });
        self.max = Some(match self.max {
            Some(m) if cmp_num(&m, &n) != Ordering::Less => m,
            _ => n,
        });
    }

    /// 合并另一个累加器（并行分块的局部结果汇总）
    fn merge(&mut self, other: &Acc) {
        self.total += other.total;
        self.non_empty += other.non_empty;
        self.empty += other.empty;
        self.non_numeric += other.non_numeric;
        self.numeric += other.numeric;
        self.float_sum += other.float_sum;
        self.dec.merge(&other.dec);
        if let Some(m) = other.min {
            self.min = Some(match self.min {
                Some(x) if cmp_num(&x, &m) != Ordering::Greater => x,
                _ => m,
            });
        }
        if let Some(m) = other.max {
            self.max = Some(match self.max {
                Some(x) if cmp_num(&x, &m) != Ordering::Less => x,
                _ => m,
            });
        }
        if let (Some(dst), Some(src)) = (self.distinct.as_mut(), other.distinct.as_ref()) {
            for v in src {
                // 未达上限时只有一次哈希（insert）；达上限后只判断是否为新值
                if dst.len() >= MAX_DISTINCT_VALUES {
                    if !dst.contains(v) {
                        self.distinct_capped = true;
                        break;
                    }
                    continue;
                }
                dst.insert(v.clone());
            }
        }
        self.distinct_capped |= other.distinct_capped;
    }

    /// 产出该聚合列的最终文本值
    fn finish(&self, kind: AggKind) -> String {
        match kind {
            AggKind::Count => self.non_empty.to_string(),
            AggKind::EmptyCount => self.empty.to_string(),
            AggKind::DistinctCount => {
                let n = self.distinct.as_ref().map(|s| s.len()).unwrap_or(0);
                if self.distinct_capped {
                    format!(">={}", n)
                } else {
                    n.to_string()
                }
            }
            AggKind::Sum => self.dec.text(self.float_sum),
            AggKind::Avg => {
                if self.numeric == 0 {
                    String::new()
                } else {
                    // 均值 = 数值和 ÷ 数值个数（非数值不计入分母）；分子优先用定点精确和
                    let total = self.dec.exact_value().unwrap_or(self.float_sum);
                    fmt_float(total / self.numeric as f64)
                }
            }
            AggKind::Min => self.min.map(fmt_num).unwrap_or_default(),
            AggKind::Max => self.max.map(fmt_num).unwrap_or_default(),
        }
    }
}

/// 10 的 `n` 次幂（`n` 受 [`DEC_MAX_SCALE`] 限制，不会溢出 i128）
fn pow10_i128(n: u32) -> Option<i128> {
    10i128.checked_pow(n)
}

fn pow10_f64(n: u32) -> f64 {
    10f64.powi(n as i32)
}

/// 占比输出：保留 **4 位有效数字**并去掉尾随零（如 `12.35` / `0.0475` / `0.00475`）。
///
/// 固定 2 位小数会把低频取值全部显示成 `0.00`（例如 115 万行里出现 55 次的取值，
/// 真实占比 0.00475%），相邻名次看不出差别；按有效数字输出既能看清量级又能区分。
fn fmt_pct(v: f64) -> String {
    if !v.is_finite() {
        return v.to_string();
    }
    if v == 0.0 {
        return "0".to_string();
    }
    let exp = v.abs().log10().floor() as i32; // v ≥ 1 时为非负
    let decimals = (3 - exp).clamp(0, 12) as usize; // 4 位有效数字
    let mut s = format!("{:.*}", decimals, v);
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

/// 解析十进制字面量（可选正负号 + 数字 + 最多一个小数点），
/// 返回 `(按小数位数放大后的整数, 小数位数)`。
///
/// 仅接受能**精确**写成定点的字面量：科学计数法（`1e5`）、超过 [`DEC_MAX_SCALE`]
/// 位小数、以及超过 [`DEC_MAX_DIGITS`] 位数字的输入都返回 `None`（调用方回退 f64）。
/// 这也是为什么不用 `f64` 直接累加——它能表示 `3613134567.58` 吗？不能，只能表示
/// 一个极接近的二进制小数，成百上千次相加后误差就会显形（曾出现 `…580001`）。
fn parse_dec(s: &str) -> Option<(i128, u32)> {
    let t = s.trim();
    let (neg, body) = match t.as_bytes().first()? {
        b'-' => (true, &t[1..]),
        b'+' => (false, &t[1..]),
        _ => (false, t),
    };
    let (int_part, frac_part) = match body.split_once('.') {
        Some((a, b)) => (a, b),
        None => (body, ""),
    };
    if (int_part.is_empty() && frac_part.is_empty())
        || !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let scale = frac_part.len() as u32;
    if scale > DEC_MAX_SCALE || int_part.len() + frac_part.len() > DEC_MAX_DIGITS {
        return None;
    }
    let mut v: i128 = 0;
    for b in int_part.bytes().chain(frac_part.bytes()) {
        v = v * 10 + (b - b'0') as i128;
    }
    Some((if neg { -v } else { v }, scale))
}

/// 定点十进制输出：`(sum, scale)` → 十进制字符串（去掉尾随零与空的整数部分补零）
fn fmt_dec(sum: i128, scale: u32) -> String {
    if scale == 0 {
        return sum.to_string();
    }
    let scale = scale as usize;
    let digits = sum.unsigned_abs().to_string();
    let (int_part, frac_part) = if digits.len() <= scale {
        ("0".to_string(), format!("{:0>width$}", digits, width = scale))
    } else {
        let (a, b) = digits.split_at(digits.len() - scale);
        (a.to_string(), b.to_string())
    };
    let frac_part = frac_part.trim_end_matches('0');
    let negative = sum < 0 && !(int_part == "0" && frac_part.is_empty());
    let mut out = String::with_capacity(int_part.len() + frac_part.len() + 2);
    if negative {
        out.push('-');
    }
    out.push_str(&int_part);
    if !frac_part.is_empty() {
        out.push('.');
        out.push_str(frac_part);
    }
    out
}

fn fmt_num(n: Num) -> String {
    match n {
        Num::Int(i) => i.to_string(),
        Num::Float(f) => fmt_float(f),
    }
}

/// 浮点输出：最多 6 位小数并去掉尾随零（避免 `0.30000000000000004` 这类噪声）；
/// 极大/极小的数退回最短往返表示，避免定宽格式化丢位或变成 0。
fn fmt_float(v: f64) -> String {
    if !v.is_finite() {
        return v.to_string();
    }
    if v != 0.0 && (v.abs() >= 1e15 || v.abs() < 1e-6) {
        return format!("{}", v);
    }
    let mut s = format!("{:.6}", v);
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    if s == "-0" {
        "0".to_string()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(data: &[u8], suffix: &str) -> String {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "csv_stats_test_{}_{}{}",
            std::process::id(),
            id,
            suffix
        ));
        std::fs::write(&p, data).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn spec(column: usize, kind: AggKind) -> AggSpec {
        AggSpec { column, kind }
    }

    #[test]
    fn group_by_single_column_sum_count_avg() {
        // 按城市分组：行数 / 金额求和 / 金额均值
        let p = write_temp(
            "城市,金额\n北京,100\n上海,50\n北京,300.5\n上海,\n北京,\n".as_bytes(),
            "_single.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let t = group_by(
            &m,
            None,
            &[0],
            &[
                spec(1, AggKind::Sum),
                spec(1, AggKind::Avg),
                spec(1, AggKind::Count),
                spec(1, AggKind::EmptyCount),
                spec(1, AggKind::Min),
                spec(1, AggKind::Max),
            ],
            None,
        )
        .unwrap();
        assert_eq!(t.headers[0], "城市");
        assert_eq!(t.headers[1], "行数");
        assert_eq!(t.headers[2], "金额(求和)");
        assert_eq!(t.group_count(), 2);
        assert_eq!(t.input_rows, 5);
        // 北京：3 行，100+300.5=400.5，均值 200.25，非空 2，空 1，最小 100，最大 300.5
        assert_eq!(
            t.rows[0],
            vec!["北京", "3", "400.5", "200.25", "2", "1", "100", "300.5"]
        );
        // 上海：2 行，50（另一为空），均值 50，非空 1，空 1
        assert_eq!(
            t.rows[1],
            vec!["上海", "2", "50", "50", "1", "1", "50", "50"]
        );
        assert!(t.notes.is_empty(), "没有非数值时不应有提示：{:?}", t.notes);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn group_by_multi_columns_keeps_first_seen_order() {
        // 多字段分组：key 组合唯一才算一组；输出顺序 = 首次出现顺序
        let p = write_temp(
            "城市,部门,金额\n北京,A,1\n上海,B,2\n北京,B,4\n北京,A,8\n上海,B,16\n".as_bytes(),
            "_multi.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let t = group_by(&m, None, &[0, 1], &[spec(2, AggKind::Sum)], None).unwrap();
        assert_eq!(t.headers, vec!["城市", "部门", "行数", "金额(求和)"]);
        assert_eq!(t.group_count(), 3);
        assert_eq!(t.rows[0], vec!["北京", "A", "2", "9"]);
        assert_eq!(t.rows[1], vec!["上海", "B", "2", "18"]);
        assert_eq!(t.rows[2], vec!["北京", "B", "1", "4"]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn group_by_i128_exact_sum() {
        // 大金额求和：i128 精确（i64 会溢出）
        let p = write_temp(
            "组,金额\nA,9000000000000000000\nA,9000000000000000000\n".as_bytes(),
            "_i128.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let t = group_by(&m, None, &[0], &[spec(1, AggKind::Sum)], None).unwrap();
        assert_eq!(t.rows[0][2], "18000000000000000000");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn sum_has_no_f64_accumulation_noise() {
        // 回归（用户实测）：金额列含两位小数时，求和必须是精确的十进制结果，
        // 不能出现 `3613134567.580001` 这类 f64 累积误差。
        // 400 × 1000000000.01 = 400000000004（精确）
        let mut data = String::from("组,金额\n");
        for _ in 0..400 {
            data.push_str("A,1000000000.01\n");
        }
        let p = write_temp(data.as_bytes(), "_noise.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let t = group_by(&m, None, &[0], &[spec(1, AggKind::Sum)], None).unwrap();
        assert_eq!(t.rows[0][2], "400000000004");
        // 对照：朴素的 f64 累加会跑偏（400000000004.002197…）——这正是修复前的问题
        let naive: f64 = (0..400).map(|_| 1_000_000_000.01f64).sum();
        assert_ne!(
            fmt_float(naive),
            "400000000004",
            "该用例应能暴露 f64 累加误差，否则本测试失去意义"
        );
        // 均值同样以定点精确和作分子
        let t2 = group_by(&m, None, &[0], &[spec(1, AggKind::Avg)], None).unwrap();
        assert_eq!(t2.rows[0][2], "1000000000.01");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn sum_aligns_mixed_decimal_scales() {
        // 混合小数位（1 位 / 2 位 / 整数）要对齐后精确相加
        let p = write_temp(
            "组,金额\nA,1\nA,2.5\nA,3.25\nA,-0.75\n".as_bytes(),
            "_scale.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let t = group_by(&m, None, &[0], &[spec(1, AggKind::Sum)], None).unwrap();
        assert_eq!(t.rows[0][2], "6"); // 1 + 2.5 + 3.25 - 0.75 = 6
        // 尾随零会被去掉：1.50 + 2.50 = 4（而不是 "4.00"）
        let p2 = write_temp("组,金额\nA,1.50\nA,2.50\n".as_bytes(), "_zero.csv");
        let m2 = DataModel::open(&p2, 100).unwrap();
        let t2 = group_by(&m2, None, &[0], &[spec(1, AggKind::Sum)], None).unwrap();
        assert_eq!(t2.rows[0][2], "4");
        std::fs::remove_file(&p).ok();
        std::fs::remove_file(&p2).ok();
    }

    #[test]
    fn sum_merges_blocks_with_different_scales() {
        // 并行分块时同一组的不同行块可能小数位数不同：合并必须对齐 scale
        let mut data = String::from("组,金额\n");
        for _ in 0..6000 {
            data.push_str("A,1.1\n");
        }
        for _ in 0..6000 {
            data.push_str("A,2.25\n");
        }
        let p = write_temp(data.as_bytes(), "_blockscale.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let t = group_by(&m, None, &[0], &[spec(1, AggKind::Sum)], None).unwrap();
        // 6000 × 1.1 + 6000 × 2.25 = 6600 + 13500 = 20100
        assert_eq!(t.rows[0][2], "20100");
        assert_eq!(t.rows[0][1], "12000");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn sum_falls_back_for_non_decimal_values() {
        // 科学计数法 / 超过 9 位小数 → 无法定点表示 → 整体回退 f64（结果仍然可用）
        let p = write_temp("组,金额\nA,1e3\nA,0.0000000001\n".as_bytes(), "_fallback.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let t = group_by(&m, None, &[0], &[spec(1, AggKind::Sum)], None).unwrap();
        let v: f64 = t.rows[0][2].parse().unwrap();
        assert!((v - 1000.0000000001).abs() < 1e-6, "回退值异常: {}", t.rows[0][2]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn parse_dec_accepts_and_rejects_expected_forms() {
        assert_eq!(parse_dec("0"), Some((0, 0)));
        assert_eq!(parse_dec("-12"), Some((-12, 0)));
        assert_eq!(parse_dec("+7"), Some((7, 0)));
        assert_eq!(parse_dec("1.50"), Some((150, 2)));
        assert_eq!(parse_dec(".5"), Some((5, 1)));
        assert_eq!(parse_dec("5."), Some((5, 0)));
        assert_eq!(parse_dec("  100.25  "), Some((10025, 2)));
        assert_eq!(parse_dec("1e5"), None);
        assert_eq!(parse_dec("1.2.3"), None);
        assert_eq!(parse_dec("abc"), None);
        assert_eq!(parse_dec(""), None);
        assert_eq!(parse_dec("."), None);
        assert_eq!(parse_dec("1.0000000001"), None); // 10 位小数 → 回退
    }

    // ===== 重复情况统计（duplicate_stats）=====

    #[test]
    fn duplicate_stats_finds_repeated_keys() {
        // A 2 次、B 1 次、C 3 次 → 重复键 2 个（C、A），重复行 3（多出来的那 3 行）
        let p = write_temp(
            "卡号\nA\nA\nB\nC\nC\nC\n".as_bytes(),
            "_dup1.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let r = duplicate_stats(&m, None, &[], 100, None).unwrap();
        assert_eq!(r.input_rows, 6);
        assert_eq!(r.distinct_keys, 3);
        assert_eq!(r.duplicate_keys, 2);
        assert_eq!(r.duplicate_rows, 3);
        assert!((r.duplicate_rate() - 50.0).abs() < 1e-9);
        // 明细按次数降序：C(3) 在前，A(2) 在后；首次出现行号从 1 开始
        assert_eq!(r.table.rows[0], vec!["C", "3", "2", "50", "4"]);
        assert_eq!(r.table.rows[1], vec!["A", "2", "1", "33.33", "1"]);
        // 可筛选的行号按**重复键分组**排列：C 组（3 次，行 3/4/5）在前，A 组（2 次，行 0/1）在后
        // —— 这样落回预览区时同一个键的各行相邻，便于一眼核对
        assert_eq!(r.dup_rows, vec![3, 4, 5, 0, 1]);
        assert_eq!(r.unique_rows, vec![0, 2, 3]);
        assert!(r.summary().contains("重复行 3 行（占 50%）"), "{}", r.summary());
        // （dup_rows 是「重复键涉及的全部行」：A 的 2 行 + C 的 3 行 = 5 行，含首次出现）
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn duplicate_stats_matches_dedup_engine() {
        // 判重口径必须与「提取唯一行（去重）」逐行一致（交叉验证两套实现）
        let p = write_temp(
            "A,B\nx,1\ny,2\nx,1\nz,3\nx,1\ny,2\n".as_bytes(),
            "_dup2.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let r = duplicate_stats(&m, None, &[], 100, None).unwrap();
        let (dedup_unique, report) = crate::core::dedup::unique_rows(&m, None, &[], None);
        assert_eq!(r.unique_rows, dedup_unique);
        assert_eq!(r.distinct_keys, report.unique_rows);
        assert_eq!(r.duplicate_rows, report.duplicates);
        assert_eq!(r.input_rows, report.input_rows);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn duplicate_stats_by_selected_columns_and_row_subset() {
        let p = write_temp(
            "ID,城市\n1,北京\n2,北京\n3,上海\n4,上海\n".as_bytes(),
            "_dup3.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        // 只按城市判重：北京 2 行、上海 2 行 → 2 个重复键、2 行重复
        let r = duplicate_stats(&m, None, &[1], 100, None).unwrap();
        assert_eq!(r.table.group_columns, 1);
        assert_eq!(r.duplicate_keys, 2);
        assert_eq!(r.duplicate_rows, 2);
        // 两组次数相同（各 2 次）→ 组间按键升序：上海(U+4E0A) < 北京(U+5317)
        assert_eq!(r.dup_rows, vec![2, 3, 0, 1]);
        assert_eq!(r.unique_rows, vec![0, 2]);
        // 判重字段同时作为表头第一列（列名 = 字段名）
        assert_eq!(r.table.headers[0], "城市");
        // 行子集：只看第 0、3 行（北京 + 上海）→ 无重复
        let r2 = duplicate_stats(&m, Some(&[0, 3]), &[1], 100, None).unwrap();
        assert_eq!(r2.input_rows, 2);
        assert_eq!(r2.duplicate_keys, 0);
        assert!(r2.dup_rows.is_empty());
        assert!(r2.table.notes.iter().any(|n| n.contains("没有重复键")));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn duplicate_stats_keeps_same_key_rows_adjacent() {
        // 「只看重复的行」要求同一键的各行相邻：组间按次数降序、键升序，组内按行号升序
        // 数据：A 在行 0、4；B 在行 1、5；C(唯一) 行 2；D 在行 3、6、7
        let p = write_temp(
            "K\nA\nB\nC\nD\nA\nB\nD\nD\n".as_bytes(),
            "_dup_adj.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let r = duplicate_stats(&m, None, &[], 100, None).unwrap();
        // D(3 次) 组在前 → 行 3、6、7；A、B 各 2 次 → 按键升序 A 组（0、4）后 B 组（1、5）
        assert_eq!(r.dup_rows, vec![3, 6, 7, 0, 4, 1, 5]);
        // 同一键的行必须相邻：按「组号」序列检查它单调不减（即不会出现 A、B、A 这种交错）
        let group_of = |row: u32| -> usize {
            match row {
                3 | 6 | 7 => 0,
                0 | 4 => 1,
                _ => 2,
            }
        };
        let groups: Vec<usize> = r.dup_rows.iter().map(|&x| group_of(x)).collect();
        assert_eq!(groups, vec![0, 0, 0, 1, 1, 2, 2], "同一键的行必须相邻");
        // 唯一行仍按行号升序（与去重一致）
        assert_eq!(r.unique_rows, vec![0, 1, 2, 3]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn duplicate_stats_top_n_truncates_with_rest_row() {
        // 4 个键各 2 次 → Top-2 只列 2 个，其余汇总为一行
        let p = write_temp(
            "K\na\na\nb\nb\nc\nc\nd\nd\n".as_bytes(),
            "_dup4.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let r = duplicate_stats(&m, None, &[], 2, None).unwrap();
        assert_eq!(r.duplicate_keys, 4);
        assert_eq!(r.table.rows.len(), 3); // 2 项 + 其余
        assert_eq!(r.table.rows[0], vec!["a", "2", "1", "25", "1"]);
        assert_eq!(r.table.rows[1], vec!["b", "2", "1", "25", "3"]);
        assert_eq!(r.table.rows[2], vec!["(其余 2 个重复键)", "4", "2", "50", ""]);
        assert!(r.table.notes.iter().any(|n| n.contains("共 4 个重复键")));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn duplicate_stats_chinese_quoted_and_key_ambiguity() {
        // 中文与含逗号的引号字段；键用 \x1F 连接避免 ("ab","c") 与 ("a","bc") 混淆
        let p = write_temp(
            "城市,备注\n北京,\"a,b\"\n北京,\"a,b\"\nab,c\na,bc\nab,c\n".as_bytes(),
            "_dup5.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let r = duplicate_stats(&m, None, &[], 100, None).unwrap();
        assert_eq!(r.distinct_keys, 3, "拼接歧义会导致误判为重复");
        assert_eq!(r.duplicate_keys, 2);
        // 明细里能看到中文字段与含逗号的值
        assert!(r.table.rows.iter().any(|row| row[0] == "北京" && row[1] == "a,b"));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn duplicate_stats_rejects_too_many_keys() {
        let p = write_temp("K\n1\n2\n3\n".as_bytes(), "_dup6.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let err = duplicate_stats_with_max_keys(&m, None, &[], 100, 2, None).unwrap_err();
        assert!(err.contains("超过上限 2"), "{err}");
        // 上限内正常
        let ok = duplicate_stats_with_max_keys(&m, None, &[], 100, 3, None).unwrap();
        assert_eq!(ok.distinct_keys, 3);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn duplicate_stats_empty_range_and_kind_metadata() {
        let p = write_temp("K\n1\n1\n".as_bytes(), "_dup7.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let r = duplicate_stats(&m, Some(&[]), &[], 100, None).unwrap();
        assert_eq!(r.input_rows, 0);
        assert_eq!(r.distinct_keys, 0);
        assert_eq!(r.duplicate_keys, 0);
        assert_eq!(r.duplicate_rate(), 0.0);
        assert!(r.table.rows.is_empty());
        assert_eq!(r.table.kind, StatsKind::DuplicateCheck);
        assert_eq!(r.table.kind.file_stem(), "重复情况");
        assert_eq!(r.table.kind.title(), "重复情况结果");
        assert_eq!(r.table.kind.label(), "重复情况");
        std::fs::remove_file(&p).ok();
    }

    // ===== 空值与数据质量统计（describe_quality）=====

    #[test]
    fn quality_counts_empty_blank_and_control_chars() {
        // 值：10 / 空 / 三个空格 / "abc\t" / abc
        // → 空值 1、纯空白 1、含控制字符 1（尾随制表符）、最短 2、最长 4、平均 3
        let p = write_temp("备注\n10\n\n   \nabc\t\nabc\n".as_bytes(), "_q1.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let t = describe_quality(&m, None, &[0], None).unwrap();
        assert_eq!(t.kind, StatsKind::DataQuality);
        let r = &t.rows[0];
        assert_eq!(t.headers[4], "空值率(%)");
        assert_eq!(r[0], "备注");
        assert_eq!(r[1], "5", "行数");
        assert_eq!(r[2], "1", "空值数");
        assert_eq!(r[3], "1", "纯空白数");
        assert_eq!(r[4], "40", "空值率 = (1+1)/5");
        assert_eq!(r[5], "1", "含控制字符数（尾随制表符）");
        assert_eq!(r[6], "2", "最短长度（10）");
        assert_eq!(r[7], "4", "最长长度（abc\\t 是 4 个字符）");
        assert_eq!(r[8], "3", "平均长度 (2+3+4+3)/4");
        assert_eq!(r[9], "0", "无超长值");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn quality_lengths_are_char_counts() {
        // 中文按字符数（不是字节数）；最短/最长/平均只算非空值
        let p = write_temp("城市\n北京\n上海市\n深圳市\n".as_bytes(), "_q2.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let t = describe_quality(&m, None, &[0], None).unwrap();
        let r = &t.rows[0];
        assert_eq!(r[2], "0", "空值数");
        assert_eq!(r[3], "0", "纯空白数");
        assert_eq!(r[4], "0", "空值率");
        assert_eq!(r[5], "0", "含控制字符数");
        assert_eq!(r[6], "2", "最短长度（北京）");
        assert_eq!(r[7], "3", "最长长度（上海市/深圳市）");
        assert_eq!(r[8], "2.666667", "平均长度（字符数）");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn quality_flags_overlong_values() {
        // 超长值（> 255 字符）单独计数
        let mut data = String::from("内容\n");
        data.push_str(&"x".repeat(300));
        data.push('\n');
        data.push_str(&"y".repeat(255));
        data.push('\n');
        data.push_str("短\n");
        let p = write_temp(data.as_bytes(), "_q3.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let t = describe_quality(&m, None, &[0], None).unwrap();
        let r = &t.rows[0];
        assert_eq!(r[7], "300", "最长长度");
        assert_eq!(r[9], "1", "超长值数（只有 300 字符那个 > 255）");
        assert!(
            t.notes.iter().any(|n| n.contains("1 列有超长值")),
            "{:?}",
            t.notes
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn quality_multi_column_subset_and_empty_range() {
        let p = write_temp("A,B,C\n1,x,\n2,,\n3,,z\n".as_bytes(), "_q4.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let t = describe_quality(&m, None, &[0, 2], None).unwrap();
        assert_eq!(t.aggs, 2);
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[0][0], "A");
        assert_eq!(t.rows[0][2], "0", "A 列无空值");
        assert_eq!(t.rows[1][0], "C");
        assert_eq!(t.rows[1][2], "2", "C 列两个空值");
        assert_eq!(t.rows[1][4], "66.67", "C 列空值率 2/3");
        // 行子集：只看第 0 行（A=1，C=空）
        let t2 = describe_quality(&m, Some(&[0]), &[0, 2], None).unwrap();
        assert_eq!(t2.input_rows, 1);
        assert_eq!(t2.rows[1][2], "1");
        // 空范围：每列一行，指标为 0（长度列留空）
        let t3 = describe_quality(&m, Some(&[]), &[0, 2], None).unwrap();
        assert_eq!(t3.rows[0], vec!["A", "0", "0", "0", "0", "0", "", "", "", "0"]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn quality_rejects_invalid_columns() {
        let p = write_temp("A\n1\n".as_bytes(), "_q5.csv");
        let m = DataModel::open(&p, 100).unwrap();
        assert!(describe_quality(&m, None, &[], None).is_err());
        assert!(describe_quality(&m, None, &[99], None).is_err());
        let t = describe_quality(&m, None, &[0, 42], None).unwrap();
        assert_eq!(t.rows.len(), 1, "越界列被忽略");
        assert_eq!(t.kind.file_stem(), "数据质量");
        assert_eq!(t.kind.title(), "数据质量结果");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn quality_parallel_merge_matches_expected() {
        // 多分块（12000 行）：空值/空白/控制字符/长度的合并结果正确
        let mut data = String::from("值\n");
        for i in 0..12000u32 {
            match i % 4 {
                0 => data.push('\n'),                        // 空值
                1 => data.push_str("  \n"),                  // 纯空白
                2 => data.push_str("abc\t\n"),               // 含控制字符（3 字符 + 制表符）
                _ => data.push_str("北京\n"),                // 2 字符
            }
        }
        let p = write_temp(data.as_bytes(), "_q6.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let t = describe_quality(&m, None, &[0], None).unwrap();
        let r = &t.rows[0];
        assert_eq!(r[1], "12000");
        assert_eq!(r[2], "3000", "空值数");
        assert_eq!(r[3], "3000", "纯空白数");
        assert_eq!(r[4], "50", "空值率");
        assert_eq!(r[5], "3000", "含控制字符数");
        assert_eq!(r[6], "2", "最短长度（北京）");
        assert_eq!(r[7], "4", "最长长度（abc\\t）");
        std::fs::remove_file(&p).ok();
    }

    // ===== 透视表（pivot_table）=====

    #[test]
    fn pivot_table_sums_by_row_and_column() {
        // 行=城市、列=月份、值=金额求和
        let p = write_temp(
            "城市,月份,金额\n北京,1,10\n北京,2,20\n上海,1,5\n北京,1,30\n上海,3,7\n".as_bytes(),
            "_pv1.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let t = pivot_table(&m, None, &[0], 1, 2, AggKind::Sum, None).unwrap();
        assert_eq!(t.kind, StatsKind::Pivot);
        assert_eq!(t.headers, vec!["城市", "1", "2", "3", "(合计)"]);
        // 行序 = 行键首次出现顺序：北京、上海
        // 列序 = 取值升序：1、2、3；无数据的单元格留空
        assert_eq!(t.rows[0], vec!["北京", "40", "20", "", "60"]);
        assert_eq!(t.rows[1], vec!["上海", "5", "", "7", "12"]);
        // 末行 = 各列的整体聚合
        assert_eq!(t.rows[2], vec!["(合计)", "45", "20", "7", "72"]);
        assert_eq!(t.group_columns, 3, "列字段取值个数");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn pivot_table_average_total_is_overall_not_sum_of_rows() {
        // 均值：合计行必须是整体均值（不是把各行均值相加）
        let p = write_temp(
            "组,列,值\nA,x,10\nA,x,30\nB,x,0\n".as_bytes(),
            "_pv2.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let t = pivot_table(&m, None, &[0], 1, 2, AggKind::Avg, None).unwrap();
        // A 的均值 = 20、B 的均值 = 0；x 列整体均值 = (10+30+0)/3 = 13.333333
        assert_eq!(t.rows[0], vec!["A", "20", "20"]);
        assert_eq!(t.rows[1], vec!["B", "0", "0"]);
        assert_eq!(t.rows[2], vec!["(合计)", "13.333333", "13.333333"]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn pivot_table_multi_row_fields_and_empty_column() {
        // 两个行字段（组合键）+ 列字段里的空值显示为「(空)」列
        let p = write_temp(
            "省,市,渠道,金额\n江苏,南京,线上,10\n江苏,南京,,20\n江苏,苏州,线上,5\n".as_bytes(),
            "_pv3.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let t = pivot_table(&m, None, &[0, 1], 2, 3, AggKind::Sum, None).unwrap();
        assert_eq!(t.headers, vec!["省", "市", "(空)", "线上", "(合计)"]);
        assert_eq!(t.rows[0], vec!["江苏", "南京", "20", "10", "30"]);
        assert_eq!(t.rows[1], vec!["江苏", "苏州", "", "5", "5"]);
        assert_eq!(t.rows[2], vec!["(合计)", "", "20", "15", "35"]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn pivot_table_counts_and_row_subset() {
        // 计数（非空）+ 统计范围取行子集
        let p = write_temp(
            "组,列,值\nA,x,1\nA,x,\nA,y,2\nB,y,3\n".as_bytes(),
            "_pv4.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let t = pivot_table(&m, None, &[0], 1, 2, AggKind::Count, None).unwrap();
        // A×x 的非空计数 = 1（另一行为空）、A×y = 1、B×y = 1
        assert_eq!(t.rows[0], vec!["A", "1", "1", "2"]);
        assert_eq!(t.rows[1], vec!["B", "", "1", "1"]);
        assert_eq!(t.rows[2], vec!["(合计)", "1", "2", "3"]);
        // 行子集：只看第 0、1 行（都属 A×x）
        let t2 = pivot_table(&m, Some(&[0, 1]), &[0], 1, 2, AggKind::Count, None).unwrap();
        assert_eq!(t2.input_rows, 2);
        assert_eq!(t2.rows[0], vec!["A", "1", "1"]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn pivot_table_rejects_invalid_inputs() {
        let p = write_temp("A,B,C\nx,y,1\n".as_bytes(), "_pv5.csv");
        let m = DataModel::open(&p, 100).unwrap();
        assert!(pivot_table(&m, None, &[], 1, 2, AggKind::Sum, None).is_err(), "无行字段");
        assert!(pivot_table(&m, None, &[9], 1, 2, AggKind::Sum, None).is_err(), "行字段越界");
        assert!(pivot_table(&m, None, &[0], 9, 2, AggKind::Sum, None).is_err(), "列字段越界");
        assert!(pivot_table(&m, None, &[0], 1, 9, AggKind::Sum, None).is_err(), "值列越界");
        assert!(
            pivot_table(&m, None, &[0], 0, 2, AggKind::Sum, None).is_err(),
            "列字段与行字段相同"
        );
        let err = pivot_table(&m, None, &[0], 1, 2, AggKind::DistinctCount, None).unwrap_err();
        assert!(err.contains("去重计数"), "{err}");
        // 空范围：只有表头（无行、无合计行）
        let t = pivot_table(&m, Some(&[]), &[0], 1, 2, AggKind::Sum, None).unwrap();
        assert!(t.rows.is_empty());
        assert_eq!(t.kind.file_stem(), "透视表");
        assert_eq!(t.kind.title(), "透视表结果");
        std::fs::remove_file(&p).ok();
    }

    // ===== 交叉/关联统计（cross_stats）=====

    #[test]
    fn cross_stats_one_to_many_relation() {
        // 卡号 → 账号：一个卡号对应多个账号（一对多）
        let p = write_temp(
            "卡号,账号\nK1,A1\nK1,A2\nK2,A1\nK2,A3\n".as_bytes(),
            "_cx1.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let r = cross_stats(&m, None, 0, 1, 100, None).unwrap();
        assert_eq!(r.a_values, 2);
        assert_eq!(r.b_values, 3);
        assert_eq!(r.pairs, 4);
        assert_eq!(r.max_b_per_a, 2);
        assert_eq!(r.max_a_per_b, 2);
        assert!(r.relation.starts_with("多对多"), "{}", r.relation);
        assert_eq!(r.table.kind, StatsKind::CrossTab);
        assert_eq!(
            r.table.headers,
            vec!["卡号", "行数", "对应「账号」的不同值数", "「账号」取值示例"]
        );
        // 明细按「对应 B 的不同值数」降序，再按行数、取值升序 → K1 与 K2 都是 2 个 B、2 行 → 按取值升序 K1 在前
        assert_eq!(r.table.rows[0], vec!["K1", "2", "2", "A1、A2"]);
        assert_eq!(r.table.rows[1], vec!["K2", "2", "2", "A1、A3"]);
        assert!(r.summary().contains("组合 4 对"), "{}", r.summary());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn cross_stats_one_to_one_and_many_to_one() {
        // 一对一：ID ↔ 卡号 一一对应
        let p = write_temp("ID,卡号\n1,K1\n2,K2\n3,K3\n".as_bytes(), "_cx2.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let r = cross_stats(&m, None, 0, 1, 100, None).unwrap();
        assert!(r.relation.starts_with("一对一"), "{}", r.relation);
        assert_eq!(r.pairs, 3);
        assert_eq!(r.max_b_per_a, 1);
        assert_eq!(r.max_a_per_b, 1);

        // 多对一：多个账号指向同一个卡号（B→A 一侧一对多）
        let p2 = write_temp("账号,卡号\nA1,K1\nA2,K1\n".as_bytes(), "_cx3.csv");
        let m2 = DataModel::open(&p2, 100).unwrap();
        let r2 = cross_stats(&m2, None, 0, 1, 100, None).unwrap();
        assert!(r2.relation.starts_with("多对一"), "{}", r2.relation);
        assert_eq!(r2.max_b_per_a, 1);
        assert_eq!(r2.max_a_per_b, 2);
        std::fs::remove_file(&p).ok();
        std::fs::remove_file(&p2).ok();
    }

    #[test]
    fn cross_stats_samples_top_n_and_notes() {
        // 取值示例超过 5 个时只列前 5 个并给出总数；明细 Top-N 截断
        let mut data = String::from("A,B\n");
        for j in 0..8 {
            data.push_str(&format!("a1,b{}\n", j));
        }
        data.push_str("a2,b0\n");
        let p = write_temp(data.as_bytes(), "_cx4.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let r = cross_stats(&m, None, 0, 1, 100, None).unwrap();
        let sample = &r.table.rows[0][3];
        assert!(sample.starts_with("b0、b1、b2、b3、b4"), "{sample}");
        assert!(sample.contains("共 8 个"), "{sample}");
        assert!(r.table.notes.iter().any(|n| n.contains("最多对应 8 个")));

        // Top-1：只列第一个（a1），其余汇总为一行
        let r2 = cross_stats(&m, None, 0, 1, 1, None).unwrap();
        assert_eq!(r2.table.rows.len(), 2);
        assert_eq!(r2.table.rows[0][0], "a1");
        assert_eq!(r2.table.rows[1][0], "(其余 1 个取值)");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn cross_stats_handles_empty_and_invalid() {
        let p = write_temp("A,B\nx,\nx,y\n".as_bytes(), "_cx5.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let r = cross_stats(&m, None, 0, 1, 100, None).unwrap();
        // 空值作为一种取值参与，示例里显示为「(空)」
        assert_eq!(r.table.rows[0][0], "x");
        assert_eq!(r.table.rows[0][2], "2");
        assert!(r.table.rows[0][3].contains("(空)"), "{:?}", r.table.rows[0]);
        // 非法输入
        assert!(cross_stats(&m, None, 0, 0, 100, None).is_err(), "同字段");
        assert!(cross_stats(&m, None, 9, 1, 100, None).is_err(), "越界");
        // 空范围：无明细，关系为"一对一"（两侧最大对应数都是 0）
        let r2 = cross_stats(&m, Some(&[]), 0, 1, 100, None).unwrap();
        assert_eq!(r2.input_rows, 0);
        assert!(r2.table.rows.is_empty());
        assert_eq!(r2.relation, "一对一（每个取值互相唯一对应）");
        assert_eq!(r2.table.kind.file_stem(), "交叉统计");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn fmt_pct_keeps_four_significant_digits() {
        // 固定 2 位小数会把低频占比全压成 0.00（用户实测反馈）：改按 4 位有效数字输出
        assert_eq!(fmt_pct(100.0), "100");
        assert_eq!(fmt_pct(50.0), "50");
        assert_eq!(fmt_pct(33.333333), "33.33");
        assert_eq!(fmt_pct(37.5), "37.5");
        assert_eq!(fmt_pct(0.0475), "0.0475");
        assert_eq!(fmt_pct(55.0 * 100.0 / 1158418.0), "0.004748");
        assert_eq!(fmt_pct(95.0 * 100.0 / 1158418.0), "0.008201");
        assert_eq!(fmt_pct(0.0), "0");
        // 极小占比仍能看出量级（不会退化成 0）
        assert_eq!(fmt_pct(1.0 * 100.0 / 1e9), "0.0000001");
    }

    #[test]
    fn fmt_dec_formats_expected() {
        assert_eq!(fmt_dec(0, 0), "0");
        assert_eq!(fmt_dec(42, 0), "42");
        assert_eq!(fmt_dec(40000000000004, 2), "400000000000.04");
        assert_eq!(fmt_dec(150, 2), "1.5");
        assert_eq!(fmt_dec(5, 1), "0.5");
        assert_eq!(fmt_dec(-5, 1), "-0.5");
        assert_eq!(fmt_dec(0, 2), "0");
        assert_eq!(fmt_dec(-0, 2), "0");
        assert_eq!(fmt_dec(1234567, 6), "1.234567");
    }

    #[test]
    fn group_by_merges_across_parallel_blocks() {
        // 跨行块的局部聚合必须正确合并（5000 行足以触发多块），且顺序为首次出现顺序
        let mut data = String::from("组,值\n");
        for i in 0..5000u32 {
            // 组 i%7 首次出现在行 i 处；值 = i（保证求和可预期）
            data.push_str(&format!("g{},{}\n", i % 7, i));
        }
        let p = write_temp(data.as_bytes(), "_parallel.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let t = group_by(&m, None, &[0], &[spec(1, AggKind::Sum)], None).unwrap();
        assert_eq!(t.group_count(), 7);
        let mut expect = vec![0i64; 7];
        let mut counts = vec![0i64; 7];
        for i in 0..5000u32 {
            expect[(i % 7) as usize] += i as i64;
            counts[(i % 7) as usize] += 1;
        }
        for g in 0..7usize {
            assert_eq!(t.rows[g][0], format!("g{}", g), "首次出现顺序应稳定");
            assert_eq!(t.rows[g][1], counts[g].to_string());
            assert_eq!(t.rows[g][2], expect[g].to_string());
        }
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn group_by_respects_row_subset() {
        // 只统计传入的行子集（当前筛选结果）
        let p = write_temp("城市,金额\n北京,1\n上海,2\n北京,4\n".as_bytes(), "_subset.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let t = group_by(&m, Some(&[0, 2]), &[0], &[spec(1, AggKind::Sum)], None).unwrap();
        assert_eq!(t.input_rows, 2);
        assert_eq!(t.group_count(), 1);
        assert_eq!(t.rows[0], vec!["北京", "2", "5"]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn group_by_distinct_count_and_empty_input() {
        // 去重计数：组内不同值个数（空值不计入）
        let p = write_temp(
            "卡号,城市\nA,北京\nB,北京\nA,北京\n,北京\nC,上海\n".as_bytes(),
            "_distinct.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let t = group_by(&m, None, &[1], &[spec(0, AggKind::DistinctCount)], None).unwrap();
        assert_eq!(t.rows[0], vec!["北京", "4", "2"]);
        assert_eq!(t.rows[1], vec!["上海", "1", "1"]);

        // 空输入：只有表头、没有分组
        let t2 = group_by(&m, Some(&[]), &[1], &[spec(0, AggKind::Sum)], None).unwrap();
        assert_eq!(t2.group_count(), 0);
        assert_eq!(t2.input_rows, 0);
        assert_eq!(t2.headers.len(), 3); // 城市 + 行数 + 卡号(求和)
        assert!(t2.notes.is_empty(), "空输入不应有提示");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn group_by_counts_non_numeric_separately() {
        // 非数值（如"未知"）不计入求和/均值/极值，而是单独计数并在结果中标注
        let p = write_temp(
            "城市,金额\n北京,10\n北京,abc\n北京,20\n北京,3.5\n".as_bytes(),
            "_nonnum.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let t = group_by(
            &m,
            None,
            &[0],
            &[
                spec(1, AggKind::Sum),
                spec(1, AggKind::Count),
                spec(1, AggKind::Max),
            ],
            None,
        )
        .unwrap();
        // 求和 33.5（跳过 abc）、计数 4（非空有 4 个）、最大 20
        assert_eq!(t.rows[0], vec!["北京", "4", "33.5", "4", "20"]);
        // 提示逐聚合列给出：求和列与最大列各有 1 个非数值，计数列不受影响
        assert_eq!(t.notes.len(), 2, "{:?}", t.notes);
        assert!(t.notes.iter().any(|n| n.contains("金额(求和)：1 个非数值")));
        assert!(t.notes.iter().any(|n| n.contains("金额(最大)：1 个非数值")));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn avg_denominator_excludes_non_numeric() {
        // 均值 = 数值和 ÷ 数值个数；非数值（abc/未知）不计入分母，也不计入分子
        let p = write_temp(
            "组,金额\nA,10\nA,20\nA,abc\nA,未知\n".as_bytes(),
            "_avgdenom.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let t = group_by(
            &m,
            None,
            &[0],
            &[spec(1, AggKind::Avg), spec(1, AggKind::Sum), spec(1, AggKind::Count)],
            None,
        )
        .unwrap();
        // 行数 4、均值 (10+20)/2 = 15、求和 30、非空计数 4
        assert_eq!(t.rows[0], vec!["A", "4", "15", "30", "4"]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn group_by_treats_nan_and_inf_as_non_numeric() {
        // NaN / ±inf 不是用户意义上的数值（与 model::classify 一致）：不计入求和/极值
        let p = write_temp(
            "组,金额\nA,10\nA,NaN\nA,inf\nA,-inf\nA,20\n".as_bytes(),
            "_nan.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let t = group_by(
            &m,
            None,
            &[0],
            &[
                spec(1, AggKind::Sum),
                spec(1, AggKind::Max),
                spec(1, AggKind::Min),
                spec(1, AggKind::Avg),
            ],
            None,
        )
        .unwrap();
        // 只统计 10 与 20：求和 30、最大 20、最小 10、均值 15
        assert_eq!(t.rows[0], vec!["A", "5", "30", "20", "10", "15"]);
        assert_eq!(t.notes.len(), 4, "四个聚合列都应有非数值提示：{:?}", t.notes);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn group_by_ignores_out_of_range_columns() {
        // 越界列（界面可能传入陈旧索引）：分组列被忽略，全越界则报错；聚合列被忽略
        let p = write_temp("城市,金额\n北京,1\n".as_bytes(), "_range.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let t = group_by(&m, None, &[0, 99], &[spec(1, AggKind::Sum), spec(88, AggKind::Sum)], None)
            .unwrap();
        assert_eq!(t.headers, vec!["城市", "行数", "金额(求和)"]);
        assert!(group_by(&m, None, &[99], &[spec(1, AggKind::Sum)], None).is_err());
        assert!(group_by(&m, None, &[0], &[spec(99, AggKind::Sum)], None).is_err());
        // 分组字段超过上限
        assert!(group_by(&m, None, &[0, 1, 0, 1], &[spec(1, AggKind::Sum)], None).is_ok());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn group_by_key_join_ambiguity() {
        // key 用 \x1F 连接：("ab","c") 与 ("a","bc") 不应被并成一组
        let p = write_temp("A,B,值\nab,c,1\na,bc,2\n".as_bytes(), "_join.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let t = group_by(&m, None, &[0, 1], &[spec(2, AggKind::Sum)], None).unwrap();
        assert_eq!(t.group_count(), 2);
        assert_eq!(t.rows[0], vec!["ab", "c", "1", "1"]);
        assert_eq!(t.rows[1], vec!["a", "bc", "1", "2"]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn group_by_chinese_and_quoted_fields() {
        // 中文、含逗号的引号字段、含制表符的字段都能正确分组
        let p = write_temp(
            "城市,备注,金额\n北京,\"a,b\",1\n北京\t,\"a,b\",2\n上海,c,3\n".as_bytes(),
            "_cn.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let t = group_by(&m, None, &[0, 1], &[spec(2, AggKind::Sum)], None).unwrap();
        assert_eq!(t.group_count(), 3);
        assert_eq!(t.rows[0], vec!["北京", "a,b", "1", "1"]);
        assert_eq!(t.rows[1], vec!["北京\t", "a,b", "1", "2"]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn group_by_too_many_groups_errors() {
        // 分组数超过上限时拒绝（提示改用其他字段），避免生成海量结果表
        let mut data = String::from("ID\n");
        for i in 0..(MAX_GROUPS + 5) {
            data.push_str(&format!("{}\n", i));
        }
        let p = write_temp(data.as_bytes(), "_toomany.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let r = group_by(&m, None, &[0], &[spec(0, AggKind::Count)], None);
        assert!(r.is_err(), "分组数超限应报错");
        assert!(r.unwrap_err().contains(&MAX_GROUPS.to_string()));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn agg_kind_labels_roundtrip() {
        for k in AggKind::ALL {
            assert_eq!(AggKind::from_label(k.label()), Some(k));
        }
        assert_eq!(AggKind::from_label("不存在的聚合"), None);
        assert!(AggKind::Sum.is_numeric());
        assert!(!AggKind::DistinctCount.is_numeric());
        assert!(AggKind::Max.is_numeric());
        assert!(!AggKind::Count.is_numeric());
    }

    #[test]
    fn fmt_float_trims_noise() {
        assert_eq!(fmt_float(400.5), "400.5");
        assert_eq!(fmt_float(300.0), "300");
        assert_eq!(fmt_float(1.0 / 3.0), "0.333333");
        assert_eq!(fmt_float(0.0), "0");
        assert_eq!(fmt_float(-0.0), "0");
        assert_eq!(fmt_float(1e20), "100000000000000000000");
    }

    // ===== 数值列统计（describe_columns）=====

    fn describe(m: &DataModel, cols: &[usize]) -> StatsTable {
        describe_columns(m, None, cols, None).unwrap()
    }

    #[test]
    fn column_summary_basic_metrics() {
        // 1,2,3,4 → 计数 4、空 0、非数值 0、和 10、均值 2.5、中位数 2.5、最小 1、最大 4
        let p = write_temp("金额\n1\n2\n3\n4\n".as_bytes(), "_cs1.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let t = describe(&m, &[0]);
        assert_eq!(t.kind, StatsKind::ColumnSummary);
        assert_eq!(t.headers[0], "列名");
        assert_eq!(t.headers.len(), 11);
        assert_eq!(t.group_count(), 1);
        assert_eq!(t.input_rows, 4);
        assert_eq!(
            t.rows[0],
            vec!["金额", "4", "4", "0", "0", "10", "2.5", "2.5", "1", "4", "1.118034"]
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn column_summary_odd_median_and_zeros() {
        // 奇数个数值 → 中位数取中间那个；全 0 → 标准差 0
        let p = write_temp("金额\n5\n1\n3\n".as_bytes(), "_cs2.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let t = describe(&m, &[0]);
        assert_eq!(t.rows[0][7], "3", "奇数个数的中位数");
        assert_eq!(t.rows[0][5], "9");

        let p2 = write_temp("金额\n0\n0\n0\n".as_bytes(), "_cs3.csv");
        let m2 = DataModel::open(&p2, 100).unwrap();
        let t2 = describe(&m2, &[0]);
        assert_eq!(t2.rows[0][10], "0", "全 0 的标准差");
        assert_eq!(t2.rows[0][7], "0", "全 0 的中位数");
        std::fs::remove_file(&p).ok();
        std::fs::remove_file(&p2).ok();
    }

    #[test]
    fn column_summary_counts_empty_and_non_numeric() {
        // 空值、非数值、NaN/inf 分别计数，且不参与求和/均值/中位数/极值
        let p = write_temp(
            "金额\n10\n\n20\nabc\nNaN\ninf\n30\n".as_bytes(),
            "_cs4.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let t = describe(&m, &[0]);
        let r = &t.rows[0];
        assert_eq!(r[1], "7", "行数（含空值）");
        assert_eq!(r[2], "6", "计数（非空）");
        assert_eq!(r[3], "1", "空值数");
        assert_eq!(r[4], "3", "非数值数（abc / NaN / inf）");
        // 只有 10/20/30 参与数值统计
        assert_eq!(r[5], "60", "求和");
        assert_eq!(r[6], "20", "均值（10+20+30)/3）");
        assert_eq!(r[7], "20", "中位数");
        assert_eq!(r[8], "10", "最小");
        assert_eq!(r[9], "30", "最大");
        assert!(
            t.notes.iter().any(|n| n.contains("3 个非数值")),
            "{:?}",
            t.notes
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn column_summary_exact_sum_and_std_dev() {
        // 求和用定点精确（不出现 f64 尾差）；标准差用 Welford（总体，分母 n）
        let mut data = String::from("金额\n");
        for _ in 0..400 {
            data.push_str("1000000000.01\n");
        }
        let p = write_temp(data.as_bytes(), "_cs5.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let t = describe(&m, &[0]);
        assert_eq!(t.rows[0][5], "400000000004", "精确求和");
        assert_eq!(t.rows[0][10], "0", "全同值标准差为 0");

        // 2,4,4,4,5,5,7,9 → 均值 5、总体标准差 2
        let p2 = write_temp("金额\n2\n4\n4\n4\n5\n5\n7\n9\n".as_bytes(), "_cs6.csv");
        let m2 = DataModel::open(&p2, 100).unwrap();
        let t2 = describe(&m2, &[0]);
        assert_eq!(t2.rows[0][6], "5", "均值");
        assert_eq!(t2.rows[0][10], "2", "总体标准差");
        assert_eq!(t2.rows[0][7], "4.5", "中位数");
        std::fs::remove_file(&p).ok();
        std::fs::remove_file(&p2).ok();
    }

    #[test]
    fn column_summary_multi_column_single_pass_and_order() {
        // 多列：按用户选择顺序逐行输出，且各列统计互不影响
        let p = write_temp(
            "A,B,文本\n1,10,x\n2,20,y\n3,30,z\n".as_bytes(),
            "_cs7.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let t = describe(&m, &[1, 0]);
        assert_eq!(t.group_count(), 2);
        assert_eq!(t.rows[0][0], "B");
        assert_eq!(t.rows[0][5], "60");
        assert_eq!(t.rows[1][0], "A");
        assert_eq!(t.rows[1][5], "6");
        // 文本列：既无求和也无均值/中位数/极值/标准差
        let t2 = describe(&m, &[2]);
        assert_eq!(t2.rows[0][2], "3", "计数（非空）");
        assert_eq!(t2.rows[0][4], "3", "非数值数");
        assert_eq!(t2.rows[0][5], "", "文本列无求和");
        assert_eq!(t2.rows[0][7], "", "文本列无中位数");
        assert_eq!(t2.rows[0][10], "", "文本列无标准差");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn column_summary_respects_row_subset_and_empty_range() {
        // 统计范围 = 传入的行子集（当前筛选结果）
        let p = write_temp("金额\n1\n2\n100\n200\n".as_bytes(), "_cs8.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let t = describe_columns(&m, Some(&[0, 1]), &[0], None).unwrap();
        assert_eq!(t.input_rows, 2);
        assert_eq!(t.rows[0][5], "3");
        assert_eq!(t.rows[0][8], "1");

        // 空范围：仍给出每个被选列一行（计数全 0）
        let t2 = describe_columns(&m, Some(&[]), &[0], None).unwrap();
        assert_eq!(t2.group_count(), 1);
        assert_eq!(t2.input_rows, 0);
        assert_eq!(t2.rows[0], vec!["金额", "0", "0", "0", "0", "", "", "", "", "", ""]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn column_summary_median_unavailable_when_too_many_values() {
        // 数值个数超过「精确中位数」保留上限 → 中位数输出 `—` 并在 notes 说明（不给近似值）
        let p = write_temp("金额\n1\n2\n".as_bytes(), "_cs9.csv");
        let m = DataModel::open(&p, 100).unwrap();
        // 用极小的 cap 直接驱动这条路径（等价于数值超过默认上限）
        let mut acc = ColAcc::new(1);
        acc.push("1");
        acc.push("2");
        assert_eq!(acc.numeric, 2);
        assert_eq!(acc.values.len(), 1);
        let row = acc.finish(&m, 0, 2);
        assert_eq!(row[7], "—");
        // 合并也要保持「不完整」标记：把另一块并入后仍不可精确
        let mut other = ColAcc::new(1);
        other.push("3");
        acc.merge(&other);
        assert_eq!(acc.numeric, 3);
        assert_ne!(acc.values.len() as u64, acc.numeric);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn column_summary_parallel_merge_matches_single_pass() {
        // 12000 行（多分块）与手工计算一致；跨块 Welford/min/max/计数合并正确
        let mut data = String::from("值\n");
        let mut vals: Vec<f64> = Vec::new();
        for i in 0..12000u32 {
            let v = (i % 97) as f64 / 4.0;
            vals.push(v);
            data.push_str(&format!("{}\n", v));
        }
        let p = write_temp(data.as_bytes(), "_cs10.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let t = describe(&m, &[0]);
        let n = vals.len() as f64;
        let sum: f64 = vals.iter().sum();
        let mean = sum / n;
        let var = vals.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / n;
        let got = |i: usize| -> f64 { t.rows[0][i].parse().unwrap() };
        assert_eq!(t.rows[0][1], "12000");
        assert!((got(5) - sum).abs() < 1e-6, "求和 {} vs {}", got(5), sum);
        assert!((got(6) - mean).abs() < 1e-6, "均值 {} vs {}", got(6), mean);
        assert!(
            (got(10) - var.sqrt()).abs() < 1e-6,
            "标准差 {} vs {}",
            got(10),
            var.sqrt()
        );
        let mut sorted = vals.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(
            got(7),
            (sorted[5999] + sorted[6000]) / 2.0,
            "中位数（偶数个取中间两个的平均）"
        );
        assert_eq!(got(8), sorted[0], "最小");
        assert_eq!(got(9), sorted[11999], "最大");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn column_summary_rejects_invalid_columns() {
        let p = write_temp("A\n1\n".as_bytes(), "_cs11.csv");
        let m = DataModel::open(&p, 100).unwrap();
        assert!(describe_columns(&m, None, &[99], None).is_err());
        assert!(describe_columns(&m, None, &[], None).is_err());
        // 越界列被忽略，其余列正常统计
        let t = describe(&m, &[0, 42]);
        assert_eq!(t.group_count(), 1);
        assert_eq!(t.rows[0][0], "A");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn stats_table_summary_text() {
        let p = write_temp("金额\n1\n2\n".as_bytes(), "_cs12.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let t = describe(&m, &[0]);
        assert_eq!(t.summary(), "统计范围 2 行 → 1 列");
        assert_eq!(t.row_unit(), "列");
        assert!(t.notes.iter().any(|n| n.contains("总体标准差")));
        let g = group_by(&m, None, &[0], &[spec(0, AggKind::Count)], None).unwrap();
        // 数据只有 1、2 两个不同取值 → 2 组
        assert_eq!(g.summary(), "统计范围 2 行 → 2 组（分组字段 1 个，聚合列 1 个）");
        assert_eq!(g.row_unit(), "组");
        std::fs::remove_file(&p).ok();
    }

    // ===== 取值频次分布 / Top-N（value_frequency）=====

    fn freq(m: &DataModel, cols: &[usize], top_n: usize) -> StatsTable {
        value_frequency(m, None, cols, top_n, None).unwrap()
    }

    #[test]
    fn frequency_ranks_by_count_desc_then_value() {
        // c 3 次、a 2 次、b 1 次 → 排名按次数降序，次数相同按取值升序
        let p = write_temp("城市\nc\na\nb\nc\na\nc\n".as_bytes(), "_fq1.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let t = freq(&m, &[0], 10);
        assert_eq!(t.kind, StatsKind::ValueFrequency);
        assert_eq!(t.headers, vec!["列名", "排名", "取值", "出现次数", "占比(%)"]);
        assert_eq!(t.input_rows, 6);
        assert_eq!(t.rows[0], vec!["城市", "1", "c", "3", "50"]);
        assert_eq!(t.rows[1], vec!["城市", "2", "a", "2", "33.33"]);
        assert_eq!(t.rows[2], vec!["城市", "3", "b", "1", "16.67"]);
        // 末行为合计：次数 == 统计范围行数、占比 100%
        assert_eq!(t.rows[3], vec!["城市", "", "(合计)", "6", "100"]);
        assert_eq!(t.rows.len(), 4);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn frequency_top_n_truncates_with_rest_row() {
        // Top-2：只列前 2 名，剩余取值汇总为「(其余 N 个取值)」一行，占比之和 = 100%
        let p = write_temp(
            "卡号\nA\nA\nA\nB\nB\nC\nD\nD\n".as_bytes(),
            "_fq2.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let t = freq(&m, &[0], 2);
        assert_eq!(t.rows.len(), 4); // 2 名 + 其余 + 合计
        assert_eq!(t.rows[0], vec!["卡号", "1", "A", "3", "37.5"]);
        // B 与 D 都是 2 次 → 按取值升序，B 在前
        assert_eq!(t.rows[1], vec!["卡号", "2", "B", "2", "25"]);
        assert_eq!(t.rows[2], vec!["卡号", "", "(其余 2 个取值)", "3", "37.5"]);
        // 合计行：出现次数 == 统计范围行数（占比 100%）
        assert_eq!(t.rows[3], vec!["卡号", "", "(合计)", "8", "100"]);
        // 去掉合计行后，各「出现次数」之和同样是统计范围行数
        let sum: u64 = t
            .rows
            .iter()
            .filter(|r| r[2] != "(合计)")
            .map(|r| r[3].parse::<u64>().unwrap())
            .sum();
        assert_eq!(sum, 8);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn frequency_counts_empty_as_its_own_value() {
        // 空/全空白行归入「(空)」一类，并参与排名与占比
        let p = write_temp(
            "备注\n\nx\n   \nx\n\n".as_bytes(),
            "_fq3.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let t = freq(&m, &[0], 10);
        assert_eq!(t.input_rows, 5);
        assert_eq!(t.rows[0], vec!["备注", "1", "(空)", "3", "60"]);
        assert_eq!(t.rows[1], vec!["备注", "2", "x", "2", "40"]);
        assert_eq!(t.rows[2], vec!["备注", "", "(合计)", "5", "100"]);
        assert!(t.notes.iter().any(|n| n.contains("3 行为空值")), "{:?}", t.notes);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn frequency_multi_column_and_row_subset() {
        // 多列：每列各自一组 Top-N，输出带「列名」列；统计范围可传行子集
        let p = write_temp(
            "城市,渠道\n北京,网银\n上海,网银\n北京,手机\n深圳,柜面\n".as_bytes(),
            "_fq4.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let t = freq(&m, &[0, 1], 10);
        assert_eq!(t.aggs, 2);
        assert_eq!(t.row_unit(), "项");
        // 城市：北京 2/4 = 50%；渠道：网银 2 次居首，其余各 1 次按取值升序（手机 < 柜面）
        assert_eq!(t.rows[0], vec!["城市", "1", "北京", "2", "50"]);
        assert_eq!(t.rows[1], vec!["城市", "2", "上海", "1", "25"]);
        assert_eq!(t.rows[2], vec!["城市", "3", "深圳", "1", "25"]);
        assert_eq!(t.rows[3], vec!["城市", "", "(合计)", "4", "100"]);
        assert_eq!(t.rows[4], vec!["渠道", "1", "网银", "2", "50"]);
        assert_eq!(t.rows[5], vec!["渠道", "2", "手机", "1", "25"]);
        assert_eq!(t.rows[6], vec!["渠道", "3", "柜面", "1", "25"]);
        assert_eq!(t.rows[7], vec!["渠道", "", "(合计)", "4", "100"]);
        // 行子集：只看第 0、2 行（都是北京）
        let t2 = value_frequency(&m, Some(&[0, 2]), &[0], 10, None).unwrap();
        assert_eq!(t2.input_rows, 2);
        assert_eq!(t2.rows[0], vec!["城市", "1", "北京", "2", "100"]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn frequency_chinese_quoted_and_whitespace_values() {
        // 中文、含逗号的引号字段、含制表符的值都按原值区分（与去重判重一致）
        let p = write_temp(
            "备注,\"城市\"\n\"a,b\",北京\na,北京 \n\"a,b\",北京\n".as_bytes(),
            "_fq5.csv",
        );
        let m = DataModel::open(&p, 100).unwrap();
        let t = freq(&m, &[1], 10);
        // "北京" 与 "北京 " 是两类（不 trim）
        assert_eq!(t.rows[0][2], "北京");
        assert_eq!(t.rows[0][3], "2");
        assert_eq!(t.rows[1][2], "北京 ");
        assert_eq!(t.rows[1][3], "1");
        let t2 = freq(&m, &[0], 10);
        assert_eq!(t2.rows[0], vec!["备注", "1", "a,b", "2", "66.67"]);
        assert_eq!(t2.rows.last().unwrap(), &vec!["备注", "", "(合计)", "3", "100"]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn frequency_parallel_merge_matches_expected() {
        // 12000 行、7 个取值 → 跨块合并后计数正确（并行分块路径）
        let mut data = String::from("组\n");
        for i in 0..12000u32 {
            data.push_str(&format!("g{}\n", i % 7));
        }
        let p = write_temp(data.as_bytes(), "_fq6.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let t = freq(&m, &[0], 10);
        assert_eq!(t.rows.len(), 8); // 7 个取值 + 合计
        for (i, r) in t.rows.iter().take(7).enumerate() {
            assert_eq!(r[2], format!("g{}", i), "并列时按取值升序");
            let expect = (0..12000).filter(|x| x % 7 == i as u32).count();
            assert_eq!(r[3], expect.to_string());
        }
        assert_eq!(t.rows[7], vec!["组", "", "(合计)", "12000", "100"]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn frequency_rejects_invalid_columns_and_empty_range() {
        let p = write_temp("A\n1\n".as_bytes(), "_fq7.csv");
        let m = DataModel::open(&p, 100).unwrap();
        assert!(value_frequency(&m, None, &[], 10, None).is_err());
        assert!(value_frequency(&m, None, &[99], 10, None).is_err());
        // 越界列被忽略，其余列正常
        let t = freq(&m, &[0, 42], 10);
        assert_eq!(t.aggs, 1);
        // 空范围：每列一行「(无数据)」
        let t2 = value_frequency(&m, Some(&[]), &[0], 10, None).unwrap();
        assert_eq!(t2.rows, vec![vec!["A", "", "(无数据)", "0", "0.00"]]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn frequency_reports_error_when_too_many_distinct_values() {
        // 不同取值超过上限时**报错**（Top-N 依赖完整计数，不能悄悄丢弃取值）
        let p = write_temp("卡号\nA\nB\nC\n".as_bytes(), "_fq8.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let err = value_frequency_with_cap(&m, None, &[0], 10, 2, None).unwrap_err();
        assert!(err.contains("卡号"), "{err}");
        assert!(err.contains("超过上限 2"), "{err}");
        // 上限内正常返回
        let ok = value_frequency_with_cap(&m, None, &[0], 10, 3, None).unwrap();
        assert_eq!(ok.rows.len(), 4); // 3 个取值 + 合计行

        // 块级：超过 cap 时只保留 cap 个不同取值（内存有界），已有取值继续计数
        let mut block = FreqBlock::new(1);
        for v in ["a", "b", "c"] {
            block.push(0, v, 2);
        }
        assert!(block.overflow[0], "超过 cap 应置超限标记");
        assert_eq!(block.maps[0].len(), 2, "内存有界：只保留 cap 个不同取值");
        assert_eq!(block.maps[0].get("a"), Some(&1));
        block.push(0, "a", 2);
        assert_eq!(block.maps[0].get("a"), Some(&2));
        // 空值单独计数、不占取值配额
        block.push(0, "   ", 2);
        assert_eq!(block.empties[0], 1);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn frequency_notes_and_summary_text() {
        let p = write_temp("城市\n北京\n上海\n".as_bytes(), "_fq9.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let t = freq(&m, &[0], 20);
        // 2 个取值 + 合计行 = 3 项
        assert_eq!(t.summary(), "统计范围 2 行 → 1 列的取值频次（共 3 项）");
        assert!(t.notes.iter().any(|n| n.contains("2 个不同取值")));
        assert!(t.notes.iter().any(|n| n.contains("4 位有效数字")));
        assert!(t.notes.iter().any(|n| n.contains("核对总量")));
        assert_eq!(t.kind.file_stem(), "频次分布");
        assert_eq!(t.kind.title(), "频次分布结果");
        std::fs::remove_file(&p).ok();
    }
}

