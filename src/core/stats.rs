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
}

impl StatsKind {
    /// 功能名（日志与提示口径）
    pub fn label(&self) -> &'static str {
        match self {
            StatsKind::GroupBy => "分组汇总",
            StatsKind::ColumnSummary => "列统计",
        }
    }

    /// 结果窗标题（同时是 egui 窗口 Id，见 `app::dialog_ui::set_window_collapsed`）
    pub fn title(&self) -> &'static str {
        match self {
            StatsKind::GroupBy => "分组汇总结果",
            StatsKind::ColumnSummary => "列统计结果",
        }
    }

    /// 导出默认文件名（不含扩展名）
    pub fn file_stem(&self) -> &'static str {
        match self {
            StatsKind::GroupBy => "分组汇总",
            StatsKind::ColumnSummary => "列统计",
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

/// 单列的描述统计累加器
#[derive(Debug, Clone)]
struct ColAcc {
    /// 参与统计的行数（含空值）
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
}

