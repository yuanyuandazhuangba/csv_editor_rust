//! 统计分析（第一期）：**字段分组汇总**。
//!
//! 语义（与界面说明一致）：
//! - 作用范围是**传入的行集合**（界面传入的是当前筛选结果；未筛选时为全部数据行）；
//! - 按 1~3 个字段的取值组合分组，对指定列做聚合：
//!   行数（每组自动给出）、计数（非空）、求和、均值、最大、最小、去重计数、空值数；
//! - 分组 key 用 `\x1F` 连接（字段值中几乎不会出现），避免「a|b」这类拼接歧义；
//! - **输出顺序 = 分组在行序中首次出现的顺序**（稳定、可复现，与输入行序一致）。
//!
//! 数值语义与筛选一致（复用 `filter.rs` 的 `try_num`/`cmp_num`）：
//! 整数用 `i128` 精确累加（19 位卡号/大金额不丢精度）；**求和与均值的分子用
//! 「定点十进制」累加**（把各值按小数位数放大成 `i128` 整数再相加，见 [`parse_dec`]），
//! 因此金额列不会出现 `3613134567.580001` 这类 f64 累积误差——只有当某个值无法用
//! 十进制字面量精确表示（科学计数法、超过 [`DEC_MAX_SCALE`] 位小数或超长数字）时，
//! 才整体回退到 `f64` 累加。
//! 非空但**无法解析为数值**的值不计入求和/均值/最大/最小，而是单独计数
//! （结果里的 `non_numeric`，供界面提示"该列有 N 个非数值未参与计算"）。
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

/// 分组汇总结果表（可直接交给 `export.rs` 的 CsvWriter/XlsxWriter 导出）
#[derive(Debug, Clone, Default)]
pub struct StatsTable {
    /// 表头：分组字段名… +「行数」+ 每个聚合列（`列名(聚合)`）
    pub headers: Vec<String>,
    /// 数据行：每行一个分组
    pub rows: Vec<Vec<String>>,
    /// 参与统计的输入行数（筛选结果行数或全部数据行数）
    pub input_rows: usize,
    /// 分组字段数
    pub group_columns: usize,
    /// 聚合列数
    pub aggs: usize,
    /// 各聚合列累计的「非空但非数值」个数（求和/均值/最大/最小会忽略它们）
    pub non_numeric: Vec<u64>,
}

impl StatsTable {
    /// 分组数（结果行数）
    pub fn group_count(&self) -> usize {
        self.rows.len()
    }

    /// 是否存在非数值被忽略的聚合列（供界面提示）
    pub fn has_non_numeric(&self) -> bool {
        self.non_numeric.iter().any(|&n| n > 0)
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
        headers,
        rows: Vec::new(),
        input_rows: total,
        group_columns: gcols.len(),
        aggs: specs.len(),
        non_numeric: Vec::new(),
    };
    if total == 0 {
        table.non_numeric = vec![0; specs.len()];
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
    table.non_numeric = (0..specs.len())
        .map(|i| accs.iter().map(|a| a[i].non_numeric).sum())
        .collect();
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

/// 新建一组聚合累加器（仅「去重计数」才创建集合，避免不必要的内存）
fn new_accs(specs: &[AggSpec]) -> Vec<Acc> {
    specs.iter().map(|s| Acc::new(s.kind)).collect()
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
    /// 定点十进制求和：各值按 [`Acc::dec_scale`] 放大后的 `i128` 累加和
    dec_sum: i128,
    /// 定点求和当前的小数位数（组内各值小数位数的最大值）
    dec_scale: u32,
    /// 是否组内所有数值都能用定点十进制精确表示（false → 求和回退 `f64`）
    dec_exact: bool,
    /// f64 求和（始终累积；定点不可用时的回退，也是均值的分子来源之一）
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
            // 默认值派生出来的是 false，这里显式置为「可精确」
            dec_exact: true,
            ..Default::default()
        }
    }

    /// 定点累加一个十进制数值（`v` 为 `sc` 位小数的缩放整数表示）
    fn add_dec(&mut self, v: i128, sc: u32) {
        if !self.dec_exact {
            return; // 已回退 f64，不必再算
        }
        if sc > DEC_MAX_SCALE {
            self.dec_exact = false;
            return;
        }
        // 先把两个操作数对齐到较大的小数位数
        let target = self.dec_scale.max(sc);
        let scaled_v = match pow10_i128(target - sc).and_then(|f| v.checked_mul(f)) {
            Some(x) => x,
            None => {
                self.dec_exact = false;
                return;
            }
        };
        if target > self.dec_scale {
            let f = pow10_i128(target - self.dec_scale).expect("scale 受 DEC_MAX_SCALE 限制");
            match self.dec_sum.checked_mul(f) {
                Some(s) => self.dec_sum = s,
                None => {
                    self.dec_exact = false;
                    return;
                }
            }
            self.dec_scale = target;
        }
        match self.dec_sum.checked_add(scaled_v) {
            Some(s) => self.dec_sum = s,
            None => self.dec_exact = false,
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
                    self.add_dec(i, 0);
                    self.float_sum += i as f64;
                    self.push_num(Num::Int(i));
                }
                Some(Num::Float(f)) => {
                    // 能写成十进制字面量就用定点精确累加，否则整体回退 f64
                    match parse_dec(t) {
                        Some((v, sc)) => self.add_dec(v, sc),
                        None => self.dec_exact = false,
                    }
                    self.float_sum += f;
                    self.push_num(Num::Float(f));
                }
                None => self.non_numeric += 1,
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
        self.float_sum += other.float_sum;
        // 定点求和：两边小数位数可能不同（不同行块的数值小数位数不一样），先对齐再相加
        self.dec_exact &= other.dec_exact;
        if self.dec_exact {
            let target = self.dec_scale.max(other.dec_scale);
            let a = pow10_i128(target - self.dec_scale).and_then(|f| self.dec_sum.checked_mul(f));
            let b = pow10_i128(target - other.dec_scale).and_then(|f| other.dec_sum.checked_mul(f));
            match a.and_then(|a| b.map(|b| (a, b))).and_then(|(a, b)| a.checked_add(b)) {
                Some(s) => {
                    self.dec_sum = s;
                    self.dec_scale = target;
                }
                None => self.dec_exact = false,
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
            AggKind::Sum => self.sum_text(),
            AggKind::Avg => {
                if self.non_empty == 0 {
                    String::new()
                } else {
                    // 均值（分子取定点精确和；不可精确时用 f64 累加值）
                    let total = if self.dec_exact {
                        self.dec_sum as f64 / pow10_f64(self.dec_scale)
                    } else {
                        self.float_sum
                    };
                    fmt_float(total / self.non_empty as f64)
                }
            }
            AggKind::Min => self.min.map(fmt_num).unwrap_or_default(),
            AggKind::Max => self.max.map(fmt_num).unwrap_or_default(),
        }
    }

    /// 求和结果文本：定点精确时输出精确十进制（去掉尾随零），否则回退 f64
    fn sum_text(&self) -> String {
        if self.dec_exact {
            fmt_dec(self.dec_sum, self.dec_scale)
        } else {
            fmt_float(self.float_sum)
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
        assert!(!t.has_non_numeric());
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
        assert_eq!(t2.non_numeric.len(), 1);
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
        assert!(t.has_non_numeric());
        assert_eq!(t.non_numeric, vec![1, 0, 1]);
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
}
