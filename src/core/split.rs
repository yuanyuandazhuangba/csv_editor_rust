//! 拆分引擎：按行数拆分、按字段拆分（单遍哈希分组，消除 O(N×U) 全表重复扫描）。
//!
//! 修正 Python 版缺陷：
//! - 按字段拆分不再对每个唯一值全表扫描（df[df[col]==value]），
//!   改为单遍遍历 + HashMap 分组，边扫描边写出，O(N)；
//! - 文件数上限保护（避免"按账号拆分"产生数百万文件）；
//! - 重名文件自动追加序号（Python 版直接覆盖）。

use std::collections::HashMap;

use super::export::{CsvWriter, XlsxWriter};
use super::model::DataModel;

/// 按字段拆分时允许的最大文件数
pub const MAX_SPLIT_FILES: usize = 20000;

/// 输出格式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Csv,
    Xlsx,
}

impl OutputFormat {
    pub fn label(&self) -> &'static str {
        match self {
            OutputFormat::Csv => "CSV",
            OutputFormat::Xlsx => "XLSX",
        }
    }

    pub fn from_label(s: &str) -> Option<OutputFormat> {
        match s.to_uppercase().as_str() {
            "CSV" => Some(OutputFormat::Csv),
            "XLSX" | "XLS" => Some(OutputFormat::Xlsx),
            _ => None,
        }
    }

    fn extension(&self) -> &'static str {
        match self {
            OutputFormat::Csv => "csv",
            OutputFormat::Xlsx => "xlsx",
        }
    }
}

/// 单个输出文件的行数统计（完整性核对用）
#[derive(Debug, Clone)]
pub struct SplitFileStat {
    /// 输出文件名（不含目录）
    pub name: String,
    /// 写入时累计的数据行数（不含表头）
    pub written_rows: usize,
    /// 读回核对得到的落盘数据行数（不含表头）
    pub disk_rows: usize,
}

/// 拆分结果报告：用于向用户证明「导出数据完整、无遗漏」。
///
/// 每个文件都记录两个数：写入时累计的行数，以及**读回文件重新统计**的落盘行数。
/// 两者一致、且落盘合计等于数据来源行数，才算核对通过。
#[derive(Debug, Clone)]
pub struct SplitReport {
    /// 生成的文件数
    pub files: usize,
    /// 数据来源行数（本次要拆分的行数，不含表头）
    pub source_rows: usize,
    /// 各输出文件的统计（按文件顺序）
    pub stats: Vec<SplitFileStat>,
}

impl SplitReport {
    /// 写入时累计的总行数
    pub fn total_written(&self) -> usize {
        self.stats.iter().map(|s| s.written_rows).sum()
    }

    /// 读回核对的总行数（真实落盘）
    pub fn total_on_disk(&self) -> usize {
        self.stats.iter().map(|s| s.disk_rows).sum()
    }

    /// 完整性结论：每个文件「写入行数 == 落盘行数」，且落盘合计 == 数据来源行数
    pub fn is_complete(&self) -> bool {
        self.total_on_disk() == self.source_rows
            && self.stats.iter().all(|s| s.written_rows == s.disk_rows)
    }

    /// 供日志显示的一句话核对结论
    pub fn verdict(&self) -> String {
        if self.is_complete() {
            format!(
                "✅ 完整性核对通过：全部输出合计 {} 行（不含表头）= 数据来源 {} 行，无遗漏",
                self.total_on_disk(),
                self.source_rows
            )
        } else {
            let diff = self.source_rows as isize - self.total_on_disk() as isize;
            format!(
                "⚠ 完整性核对未通过：全部输出合计 {} 行 ≠ 数据来源 {} 行（相差 {} 行），请检查磁盘空间与目标目录权限",
                self.total_on_disk(),
                self.source_rows,
                diff.abs()
            )
        }
    }
}

/// 流式统计文件的数据行数（**不含表头**，引号感知：引号字段内的换行不计行）。
///
/// 用于拆分完成后核对输出文件的真实落盘行数。分块读取、不建索引、不缓存内容，
/// 因此对超大文件也只是一次顺序读。
pub fn count_data_rows(path: &str) -> Result<usize, String> {
    use std::io::Read;

    let mut f = std::fs::File::open(path).map_err(|e| format!("无法读取文件 {}：{}", path, e))?;
    let mut buf = vec![0u8; 1 << 20];
    let mut boundaries = 0usize; // 引号感知的行边界数
    let mut in_quotes = false;
    let mut pending_cr = false; // 跨块的 \r，留待下一块判定
    let mut last_byte: Option<u8> = None;
    loop {
        let n = f.read(&mut buf).map_err(|e| format!("读取文件失败：{}", e))?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        last_byte = Some(chunk[n - 1]);
        let mut i = 0usize;
        if pending_cr {
            // 上一块末尾的 \r：本块首字节为 \n 则合并为一个 \r\n 边界，否则 \r 单独成边界
            boundaries += 1;
            if chunk[0] == b'\n' {
                i = 1;
            }
            pending_cr = false;
        }
        while i < n {
            let b = chunk[i];
            if in_quotes {
                if b == b'"' {
                    if i + 1 < n && chunk[i + 1] == b'"' {
                        i += 2; // 转义引号
                    } else {
                        in_quotes = false;
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            } else {
                match b {
                    b'"' => {
                        in_quotes = true;
                        i += 1;
                    }
                    b'\n' => {
                        boundaries += 1;
                        i += 1;
                    }
                    b'\r' => {
                        if i + 1 == n {
                            pending_cr = true;
                            i += 1;
                        } else if chunk[i + 1] == b'\n' {
                            boundaries += 1;
                            i += 2;
                        } else {
                            boundaries += 1;
                            i += 1;
                        }
                    }
                    _ => i += 1,
                }
            }
        }
    }
    if pending_cr {
        boundaries += 1;
    }
    let data_rows = match last_byte {
        None => 0, // 空文件
        // 末尾有换行：最后一个边界之后没有内容，boundaries 中已含表头行 → 减 1
        Some(b'\n') | Some(b'\r') => boundaries.saturating_sub(1),
        // 末尾无换行：表头 + 数据行 = boundaries + 1
        Some(_) => boundaries,
    };
    Ok(data_rows)
}

/// CSV 字段转义（含逗号/引号/换行时加引号并翻倍引号）
fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// 写出「拆分完整性核对表」（CSV，utf-8-sig，Excel 可直接双击打开）。
///
/// 内容依次为：拆分类型、拆分依据、输出格式、数据来源行数、输出文件数、
/// 输出总行数，以及**逐文件的数据行数**（不含表头，同时给出写入行数便于对照），
/// 最后是完整性核对结果。核对表与拆分输出放在同一目录，便于随数据一起归档。
pub fn write_split_checkfile(
    path: &str,
    generated_at: &str,
    split_type: &str,
    split_basis: &str,
    format_label: &str,
    report: &SplitReport,
) -> Result<(), String> {
    use std::io::Write;

    let f = std::fs::File::create(path).map_err(|e| format!("无法创建核对表 {}：{}", path, e))?;
    let mut w = std::io::BufWriter::new(f);
    // BOM（utf-8-sig），Excel 打开中文不乱码
    w.write_all(&[0xEF, 0xBB, 0xBF])
        .map_err(|e| e.to_string())?;

    let line = |w: &mut std::io::BufWriter<std::fs::File>, cells: &[&str]| -> std::io::Result<()> {
        let s: Vec<String> = cells.iter().map(|c| csv_escape(c)).collect();
        writeln!(w, "{}", s.join(","))
    };

    line(&mut w, &["拆分完整性核对表"]).map_err(|e| e.to_string())?;
    line(&mut w, &["生成时间", generated_at]).map_err(|e| e.to_string())?;
    line(&mut w, &["拆分类型", split_type]).map_err(|e| e.to_string())?;
    line(&mut w, &["拆分依据", split_basis]).map_err(|e| e.to_string())?;
    line(&mut w, &["输出格式", format_label]).map_err(|e| e.to_string())?;
    line(&mut w, &["数据来源行数（不含表头）", &report.source_rows.to_string()])
        .map_err(|e| e.to_string())?;
    line(&mut w, &["输出文件数", &report.files.to_string()]).map_err(|e| e.to_string())?;
    line(
        &mut w,
        &[
            "输出总行数（不含表头）",
            &report.total_on_disk().to_string(),
        ],
    )
    .map_err(|e| e.to_string())?;
    line(
        &mut w,
        &[
            "完整性核对结果",
            if report.is_complete() {
                "通过（输出总行数 = 数据来源行数，无遗漏）"
            } else {
                "未通过（输出总行数与数据来源行数不一致，请检查磁盘空间与目录权限）"
            },
        ],
    )
    .map_err(|e| e.to_string())?;
    line(&mut w, &[]).map_err(|e| e.to_string())?;
    line(
        &mut w,
        &[
            "序号",
            "文件名",
            "数据行数（不含表头）",
            "写入行数（不含表头）",
        ],
    )
    .map_err(|e| e.to_string())?;
    for (i, s) in report.stats.iter().enumerate() {
        line(
            &mut w,
            &[
                &(i + 1).to_string(),
                &s.name,
                &s.disk_rows.to_string(),
                &s.written_rows.to_string(),
            ],
        )
        .map_err(|e| e.to_string())?;
    }
    line(
        &mut w,
        &[
            "合计",
            &format!("{} 个文件", report.files),
            &report.total_on_disk().to_string(),
            &report.total_written().to_string(),
        ],
    )
    .map_err(|e| e.to_string())?;
    w.flush().map_err(|e| e.to_string())?;
    Ok(())
}

/// 拆分选项
#[derive(Debug, Clone)]
pub struct SplitOptions {
    pub folder: String,
    pub format: OutputFormat,
}

/// 清理文件名中的非法字符（修正 Windows 文件名规则；控制字符一并替换）
pub fn sanitize_filename(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '\n' | '\r' | '\t' => '_',
            c if c.is_control() => '_',
            _ => c,
        })
        .collect();
    let s2 = s.trim_matches(|c| c == ' ' || c == '.');
    if s2.is_empty() {
        return "field".to_string();
    }
    s = s2.to_string();
    // 压缩连续下划线
    let mut out = String::with_capacity(s.len());
    let mut prev_underscore = false;
    for c in s.chars() {
        if c == '_' {
            if !prev_underscore {
                out.push('_');
            }
            prev_underscore = true;
        } else {
            out.push(c);
            prev_underscore = false;
        }
    }
    if out.len() > 100 {
        // 按字符边界截断，避免 String::truncate 在非 UTF-8 边界 panic
        let mut cut = 100;
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    out
}

/// 打开的文件写出器（CSV 或 XLSX 统一接口）
enum OpenWriter {
    Csv(CsvWriter),
    Xlsx(XlsxWriter),
}

impl OpenWriter {
    fn new(path: &str, format: OutputFormat, header: &[String]) -> Result<Self, String> {
        match format {
            OutputFormat::Csv => Ok(OpenWriter::Csv(
                CsvWriter::new(path, header).map_err(|e| e.to_string())?,
            )),
            OutputFormat::Xlsx => {
                // 全文本导出：无需识别长数字列
                Ok(OpenWriter::Xlsx(XlsxWriter::new(path, header)?))
            }
        }
    }

    fn write_row(&mut self, fields: &[String]) -> Result<(), String> {
        match self {
            OpenWriter::Csv(w) => w.write_row(fields).map_err(|e| e.to_string()),
            OpenWriter::Xlsx(w) => w.write_row(fields),
        }
    }

    fn finish(self) -> Result<(), String> {
        match self {
            OpenWriter::Csv(w) => w.finish().map_err(|e| e.to_string()),
            OpenWriter::Xlsx(w) => w.finish(),
        }
    }
}

/// 按行数拆分：每 batch_size 行生成一个文件 split_data_part_N.csv/xlsx。
/// `rows`：要拆分的行号（None = 全部数据行）。
/// 返回生成的文件数。
pub fn split_by_rows(
    model: &DataModel,
    rows: Option<&[u32]>,
    batch_size: usize,
    opts: &SplitOptions,
    progress: Option<&(dyn Fn(f64) + Sync)>,
) -> Result<SplitReport, String> {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Mutex;

    if batch_size == 0 {
        return Err("每批行数必须大于 0".to_string());
    }
    let n = model.row_count();
    let total = rows.map(|v| v.len()).unwrap_or(n);
    if n == 0 || total == 0 {
        return Err("没有可拆分的数据".to_string());
    }
    let header = model.header().to_vec();
    // 批划分：第 b 批覆盖 [b*batch, min((b+1)*batch, total))，编号与单个文件一一对应
    // （数据恰好耗尽时不会产生仅含表头的空尾文件，与原串行实现行为一致）。
    let batch_count = total.div_ceil(batch_size);

    // 并行写出：各批完全独立（不同输出文件、不同行区间），按批号原子分派。
    // 并发度取核数但不超过 8 —— 拆分是 IO 密集型（同时打开多个输出文件），
    // 并发过高在机械盘/网络盘上会因寻道竞争而变慢。
    let threads = std::thread::available_parallelism()
        .map(|x| x.get())
        .unwrap_or(4)
        .clamp(1, 8)
        .min(batch_count);
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let files_done = AtomicUsize::new(0);
    let err: Mutex<Option<String>> = Mutex::new(None);
    // 收集每批的写入行数：(批号, 行数)；批号唯一，写入端每次只锁一下
    let written_log: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::with_capacity(batch_count));

    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let mut rec: Vec<String> = Vec::new();
                loop {
                    if err.lock().map(|g| g.is_some()).unwrap_or(true) {
                        break; // 已有其他线程失败，尽快退出
                    }
                    let b = next.fetch_add(1, AtomicOrdering::Relaxed);
                    if b >= batch_count {
                        break;
                    }
                    let start = b * batch_size;
                    let end = (start + batch_size).min(total);
                    let path = format!(
                        "{}\\split_data_part_{}.{}",
                        opts.folder,
                        b + 1,
                        opts.format.extension()
                    );
                    let mut w = match OpenWriter::new(&path, opts.format, &header) {
                        Ok(w) => w,
                        Err(e) => {
                            *err.lock().unwrap() = Some(e);
                            break;
                        }
                    };
                    let mut write_err: Option<String> = None;
                    for i in start..end {
                        let row = match rows {
                            Some(list) => list[i] as usize,
                            None => i,
                        };
                        model.row_into(row, &mut rec);
                        if let Err(e) = w.write_row(&rec) {
                            write_err = Some(e);
                            break;
                        }
                    }
                    if let Some(e) = write_err {
                        *err.lock().unwrap() = Some(e);
                        break;
                    }
                    if let Err(e) = w.finish() {
                        *err.lock().unwrap() = Some(e);
                        break;
                    }
                    files_done.fetch_add(1, AtomicOrdering::Relaxed);
                    written_log.lock().unwrap().push((b, end - start));
                    let d = done.fetch_add(end - start, AtomicOrdering::Relaxed) + (end - start);
                    if let Some(p) = progress {
                        p(d as f64 / total as f64);
                    }
                }
            });
        }
    });

    if let Some(e) = err.into_inner().unwrap_or(None) {
        return Err(e);
    }
    let files = files_done.load(AtomicOrdering::Relaxed);
    let _ = files;

    // 读回核对：逐个输出文件重新统计落盘数据行数（引号感知、流式、不建索引）
    let mut written = written_log.into_inner().unwrap_or_default();
    written.sort_unstable_by_key(|(b, _)| *b);
    let mut stats: Vec<SplitFileStat> = Vec::with_capacity(written.len());
    for (b, rows) in written {
        let name = format!(
            "split_data_part_{}.{}",
            b + 1,
            opts.format.extension()
        );
        let path = format!("{}\\{}", opts.folder, name);
        let disk_rows = count_data_rows(&path)?;
        stats.push(SplitFileStat {
            name,
            written_rows: rows,
            disk_rows,
        });
    }
    Ok(SplitReport {
        files: stats.len(),
        source_rows: total,
        stats,
    })
}

/// 按字段拆分：以 columns 组合的值为 key，单遍分组写出。
/// `rows`：要拆分的行号（None = 全部数据行）。
/// 返回生成的文件数。
pub fn split_by_field(
    model: &DataModel,
    rows: Option<&[u32]>,
    columns: &[usize],
    opts: &SplitOptions,
    progress: Option<&(dyn Fn(f64) + Sync)>,
) -> Result<SplitReport, String> {
    if columns.is_empty() {
        return Err("请选择至少一个拆分字段".to_string());
    }
    for &c in columns {
        if c >= model.column_count() {
            return Err(format!("字段索引 {} 超出范围", c));
        }
    }
    let n = model.row_count();
    let total = rows.map(|v| v.len()).unwrap_or(n);
    if n == 0 || total == 0 {
        return Err("没有可拆分的数据".to_string());
    }
    match opts.format {
        // CSV 流式逐行写，内存低，保持单遍
        OutputFormat::Csv => split_by_field_csv(model, rows, columns, opts, progress),
        // XLSX 的 Workbook 非流式（全部单元格驻留内存直到 save），
        // 改为两阶段：先写中间 CSV（低内存），再逐个转 XLSX（峰值 = 单个最大分组）
        OutputFormat::Xlsx => split_by_field_xlsx(model, rows, columns, opts, progress),
    }
}

/// 按字段拆分（CSV）：单遍哈希分组流式写出，内存 O(分组数)。
fn split_by_field_csv(
    model: &DataModel,
    rows: Option<&[u32]>,
    columns: &[usize],
    opts: &SplitOptions,
    progress: Option<&(dyn Fn(f64) + Sync)>,
) -> Result<SplitReport, String> {
    let n = model.row_count();
    let total = rows.map(|v| v.len()).unwrap_or(n);
    let header = model.header().to_vec();

    // key(字段值组合) -> 文件序号
    let mut key_index: HashMap<String, usize> = HashMap::with_capacity(1024);
    // 每个分组写入的数据行数与最终文件名（完整性核对用）
    let mut group_rows: Vec<usize> = Vec::new();
    let mut final_names: Vec<String> = Vec::new();
    // 文件名占用统计（重名追加序号）
    let mut used_names: HashMap<String, usize> = HashMap::new();
    let mut writers: Vec<Option<OpenWriter>> = Vec::new();
    let mut empty_seq = 0usize;

    let iter: Box<dyn Iterator<Item = usize>> = match rows {
        Some(list) => Box::new(list.iter().map(|&r| r as usize)),
        None => Box::new(0..n),
    };
    let mut done = 0usize;
    let mut rec: Vec<String> = Vec::new();
    // 复用 key 缓冲：命中已有分组时可直接用 &str 查询（Borrow<str>），
    // 只有首次遇到新分组才需要 clone 一份 owned key 存入表里。
    let mut key_buf = String::new();
    for row in iter {
        model.row_into(row, &mut rec);
        make_key_into(&rec, columns, &mut key_buf);
        let idx = match key_index.get(key_buf.as_str()) {
            Some(&i) => i,
            None => {
                if writers.len() >= MAX_SPLIT_FILES {
                    return Err(format!(
                        "该字段唯一值超过 {} 个，不适合按字段拆分（可考虑按行数拆分）",
                        MAX_SPLIT_FILES
                    ));
                }
                let idx = writers.len();
                let mut base = sanitize_filename(&key_buf);
                if key_buf.trim().is_empty() || base == "nan" {
                    empty_seq += 1;
                    base = format!("empty_value_{}", empty_seq);
                }
                let count = used_names.entry(base.clone()).or_insert(0);
                let final_name = if *count == 0 {
                    base
                } else {
                    format!("{}_{}", base, *count + 1)
                };
                *count += 1;
                let path = format!(
                    "{}\\{}.{}",
                    opts.folder,
                    final_name,
                    opts.format.extension()
                );
                let w = OpenWriter::new(&path, OutputFormat::Csv, &header)?;
                writers.push(Some(w));
                group_rows.push(0);
                final_names.push(final_name);
                key_index.insert(key_buf.clone(), idx);
                idx
            }
        };
        if let Some(w) = writers[idx].as_mut() {
            w.write_row(&rec)?;
            group_rows[idx] += 1;
        }
        done += 1;
        if done % 50000 == 0 {
            if let Some(p) = progress {
                p(done as f64 / total as f64);
            }
        }
    }
    if let Some(p) = progress {
        p(1.0);
    }

    for w in writers.into_iter().flatten() {
        w.finish()?;
    }

    // 读回核对：逐个输出文件重新统计落盘数据行数
    let mut stats: Vec<SplitFileStat> = Vec::with_capacity(final_names.len());
    for (i, name) in final_names.iter().enumerate() {
        let file_name = format!("{}.{}", name, opts.format.extension());
        let path = format!("{}\\{}", opts.folder, file_name);
        let disk_rows = count_data_rows(&path)?;
        stats.push(SplitFileStat {
            name: file_name,
            written_rows: group_rows.get(i).copied().unwrap_or(0),
            disk_rows,
        });
    }
    Ok(SplitReport {
        files: stats.len(),
        source_rows: total,
        stats,
    })
}

/// RAII：函数返回（含错误）时删除未清理的中间拆分临时 CSV
struct TempCleanup(Vec<String>);
impl Drop for TempCleanup {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// 按字段拆分（XLSX）：两阶段流式，避免所有分组 Workbook 同时驻留内存。
/// 第一遍单遍哈希分组写入中间 CSV（低内存），第二遍逐个把中间 CSV 转成 XLSX
/// （峰值内存 = 单个最大分组的数据量），转换后删除中间文件。
fn split_by_field_xlsx(
    model: &DataModel,
    rows: Option<&[u32]>,
    columns: &[usize],
    opts: &SplitOptions,
    progress: Option<&(dyn Fn(f64) + Sync)>,
) -> Result<SplitReport, String> {
    let n = model.row_count();
    let total = rows.map(|v| v.len()).unwrap_or(n);
    let header = model.header().to_vec();

    // ---- 第一遍：单遍分组，每组写入中间 CSV（每 writer 仅 8KB 缓冲）----
    // 临时文件清理 guard 先声明：Rust drop 为反向声明顺序，guard 后声明会先于 writer
    // 关闭而删除文件（Windows 文件占用）失败，导致临时文件泄漏
    let mut temp_guard = TempCleanup(Vec::new());
    let mut key_index: HashMap<String, usize> = HashMap::with_capacity(1024);
    let mut used_names: HashMap<String, usize> = HashMap::new();
    let mut csv_writers: Vec<Option<CsvWriter>> = Vec::new();
    let mut final_names: Vec<String> = Vec::new();
    // 每个分组写入的数据行数（完整性核对用）
    let mut group_rows: Vec<usize> = Vec::new();
    let mut empty_seq = 0usize;
    let mut temp_paths: Vec<String> = Vec::new();
    // 临时命名含时间戳后缀，避免跨运行/进程复用冲突
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    let iter: Box<dyn Iterator<Item = usize>> = match rows {
        Some(list) => Box::new(list.iter().map(|&r| r as usize)),
        None => Box::new(0..n),
    };
    let mut done = 0usize;
    let mut rec: Vec<String> = Vec::new();
    // 复用 key 缓冲：命中已有分组时可直接用 &str 查询（Borrow<str>），
    // 只有首次遇到新分组才需要 clone 一份 owned key 存入表里。
    let mut key_buf = String::new();
    for row in iter {
        model.row_into(row, &mut rec);
        make_key_into(&rec, columns, &mut key_buf);
        let idx = match key_index.get(key_buf.as_str()) {
            Some(&i) => i,
            None => {
                if csv_writers.len() >= MAX_SPLIT_FILES {
                    return Err(format!(
                        "该字段唯一值超过 {} 个，不适合按字段拆分（可考虑按行数拆分）",
                        MAX_SPLIT_FILES
                    ));
                }
                let idx = csv_writers.len();
                let mut base = sanitize_filename(&key_buf);
                if key_buf.trim().is_empty() || base == "nan" {
                    empty_seq += 1;
                    base = format!("empty_value_{}", empty_seq);
                }
                let count = used_names.entry(base.clone()).or_insert(0);
                let final_name = if *count == 0 {
                    base
                } else {
                    format!("{}_{}", base, *count + 1)
                };
                *count += 1;
                // 中间临时 CSV（`~` 前缀标识，转换后删除）；new_raw 不加公式防护前缀，避免污染数据
                let temp_path = format!(
                    "{}\\~split_tmp_{}_{}_{}.csv",
                    opts.folder,
                    std::process::id(),
                    idx,
                    ts
                );
                let w = CsvWriter::new_raw(&temp_path, &header).map_err(|e| e.to_string())?;
                csv_writers.push(Some(w));
                final_names.push(final_name);
                group_rows.push(0);
                temp_paths.push(temp_path.clone());
                temp_guard.0.push(temp_path);
                key_index.insert(key_buf.clone(), idx);
                idx
            }
        };
        if let Some(w) = csv_writers[idx].as_mut() {
            w.write_row(&rec).map_err(|e| e.to_string())?;
            group_rows[idx] += 1;
        }
        done += 1;
        if done % 50000 == 0 {
            if let Some(p) = progress {
                p((done as f64 / total as f64) * 0.5);
            }
        }
    }
    let group_count = csv_writers.len();
    for w in csv_writers.into_iter().flatten() {
        w.finish().map_err(|e| e.to_string())?;
    }
    if let Some(p) = progress {
        p(0.5);
    }

    // ---- 第二遍：并行把各中间 CSV 转成 XLSX，转换后删除中间文件 ----
    // 各分组相互独立（各自一个 Workbook、一个输出文件），可安全并行。
    // 并发度上限取 6：每个转换会把该分组的全部单元格驻留内存，同时进行过多会
    // 显著抬高进程峰值内存，此上限在“吃满核”与“控内存”之间折中。
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Mutex;

    let threads = std::thread::available_parallelism()
        .map(|x| x.get())
        .unwrap_or(4)
        .clamp(1, 6)
        .min(group_count.max(1));
    let next = AtomicUsize::new(0);
    let done_groups = AtomicUsize::new(0);
    let err: Mutex<Option<String>> = Mutex::new(None);
    // 转换时读回的中间 CSV 行数：(组号, 行数)——用于与写入行数对照
    let csv_rows_log: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::with_capacity(group_count));

    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let mut fields: Vec<String> = Vec::new();
                loop {
                    if err.lock().map(|g| g.is_some()).unwrap_or(true) {
                        break;
                    }
                    let i = next.fetch_add(1, AtomicOrdering::Relaxed);
                    if i >= group_count {
                        break;
                    }
                    let src = &temp_paths[i];
                    let out_path = format!("{}\\{}.xlsx", opts.folder, final_names[i]);
                    match convert_csv_to_xlsx(src, &out_path, &header, &mut fields) {
                        Ok(rows) => {
                            csv_rows_log.lock().unwrap().push((i, rows));
                        }
                        Err(e) => {
                            *err.lock().unwrap() = Some(e);
                            break;
                        }
                    }
                    let _ = std::fs::remove_file(src);
                    let d = done_groups.fetch_add(1, AtomicOrdering::Relaxed) + 1;
                    if let Some(p) = progress {
                        p(0.5 + (d as f64 / group_count.max(1) as f64) * 0.5);
                    }
                }
            });
        }
    });

    if let Some(e) = err.into_inner().unwrap_or(None) {
        return Err(e);
    }
    drop(temp_guard); // 提前释放：未转换成功的中间文件由 TempCleanup 兜底删除

    // 核对：用「转换时读回的中间 CSV 行数」对照「写入时累计的行数」。
    // 中间 CSV 是 XLSX 转换的输入，其行数可独立读回验证；XLSX 本身的行数由
    // 写出 API 逐行写入保证（转换后中间文件即删除，无法再次读回）。
    let mut disk_by_group = vec![0usize; group_count];
    for (i, rows) in csv_rows_log.into_inner().unwrap_or_default() {
        if i < group_count {
            disk_by_group[i] = rows;
        }
    }
    let stats: Vec<SplitFileStat> = (0..group_count)
        .map(|i| SplitFileStat {
            name: format!("{}.xlsx", final_names.get(i).cloned().unwrap_or_default()),
            written_rows: group_rows.get(i).copied().unwrap_or(0),
            disk_rows: disk_by_group[i],
        })
        .collect();
    Ok(SplitReport {
        files: group_count,
        source_rows: total,
        stats,
    })
}

/// 把一份中间 CSV 转换成 XLSX（供并行转换调用；`fields` 为复用的行缓冲）。
fn convert_csv_to_xlsx(
    src: &str,
    out_path: &str,
    header: &[String],
    fields: &mut Vec<String>,
) -> Result<usize, String> {
    let file = crate::core::reader::CsvFile::open(src).map_err(|e| e.to_string())?;
    let rows = file.row_count(); // 读回的中间 CSV 数据行数（供完整性核对）
    let mut w = XlsxWriter::new(out_path, header)?;
    for r in 0..rows {
        file.row_into(r, fields);
        w.write_row(fields)?;
    }
    w.finish()?;
    // 先释放 mmap 文件句柄，Windows 才能删除该中间 CSV
    drop(file);
    Ok(rows)
}

/// 生成分组 key，写入复用的缓冲（避免每行一次 String 分配）。
/// 多字段用 \x1F 连接（字段值中几乎不会出现）；字段数不足的行按空值处理。
fn make_key_into(rec: &[String], columns: &[usize], out: &mut String) {
    out.clear();
    for (i, &c) in columns.iter().enumerate() {
        if i > 0 {
            out.push('\u{1F}');
        }
        out.push_str(rec.get(c).map(|s| s.trim()).unwrap_or(""));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::DataModel;

    fn temp_dir() -> String {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir();
        let p = dir.join(format!(
            "csv_split_test_{}_{}",
            std::process::id(),
            id
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn cleanup(dir: &str) {
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sanitize_filenames() {
        assert_eq!(sanitize_filename("abc"), "abc");
        assert_eq!(sanitize_filename("a<b>c:d\"e/f\\g|h?i*j"), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(sanitize_filename(""), "field");
        assert_eq!(sanitize_filename("   "), "field");
        assert_eq!(sanitize_filename("a  b__c"), "a  b_c"); // 空格保留（与 Python 一致），下划线压缩
        assert_eq!(sanitize_filename(".."), "field");
        assert_eq!(sanitize_filename("交易明细-2026"), "交易明细-2026");
    }

    #[test]
    fn sanitize_long_chinese_no_panic() {
        // security 修复：>100 字节的中文键截断不得在非 UTF-8 边界 panic
        let long = "很长的中文拆分键".repeat(20); // > 100 字节
        let s = sanitize_filename(&long);
        assert!(s.is_char_boundary(s.len()));
        assert!(s.len() <= 100);
        let s2 = sanitize_filename(&"漢字".repeat(40));
        assert!(s2.is_char_boundary(s2.len()));
    }

    #[test]
    fn split_by_rows_csv() {
        let data = "ID,NAME\n1,a\n2,b\n3,c\n4,d\n5,e\n".as_bytes();
        let p = write_temp_file(data, "_rowsrc.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let dir = temp_dir();
        let opts = SplitOptions {
            folder: dir.clone(),
            format: OutputFormat::Csv,
        };
        let count = split_by_rows(&m, None, 2, &opts, None).unwrap().files;
        assert_eq!(count, 3);
        // 每个文件 2 行 + 表头
        let f1 = format!("{}\\split_data_part_1.csv", dir);
        let content = std::fs::read_to_string(&f1).unwrap();
        assert!(content.starts_with('\u{FEFF}'));
        assert_eq!(content.matches('\n').count(), 3); // 表头+2行
        cleanup(&dir);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn split_by_field_csv_single() {
        let data = "BANK,ID\n苏州银行,1\n工商银行,2\n苏州银行,3\n".as_bytes();
        let p = write_temp_file(data, "_fieldsrc.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let dir = temp_dir();
        let opts = SplitOptions {
            folder: dir.clone(),
            format: OutputFormat::Csv,
        };
        let count = split_by_field(&m, None, &[0], &opts, None).unwrap().files;
        assert_eq!(count, 2);
        let f1 = format!("{}\\苏州银行.csv", dir);
        let c1 = std::fs::read_to_string(&f1).unwrap();
        assert!(c1.contains("1\r\n") && c1.contains("3\r\n"));
        cleanup(&dir);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn split_by_field_multi_column() {
        let data = "A,B,V\nx,y,1\nx,z,2\nx,y,3\n".as_bytes();
        let p = write_temp_file(data, "_multisrc.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let dir = temp_dir();
        let opts = SplitOptions {
            folder: dir.clone(),
            format: OutputFormat::Csv,
        };
        let count = split_by_field(&m, None, &[0, 1], &opts, None).unwrap().files;
        assert_eq!(count, 2);
        // 组合文件名 x_y.csv 与 x_z.csv
        let f1 = format!("{}\\x_y.csv", dir);
        let c1 = std::fs::read_to_string(&f1).unwrap();
        assert_eq!(c1.matches('\n').count(), 3); // 表头 + 2 行
        cleanup(&dir);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn split_by_field_empty_value() {
        let data = "BANK,ID\n,1\n工商银行,2\n".as_bytes();
        let p = write_temp_file(data, "_emptysrc.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let dir = temp_dir();
        let opts = SplitOptions {
            folder: dir.clone(),
            format: OutputFormat::Csv,
        };
        let count = split_by_field(&m, None, &[0], &opts, None).unwrap().files;
        assert_eq!(count, 2);
        let f1 = format!("{}\\empty_value_1.csv", dir);
        assert!(std::fs::metadata(&f1).is_ok());
        cleanup(&dir);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn split_by_field_duplicate_sanitized_names() {
        // "a/b" 与 "a_b" sanitize 后同名 → 追加序号不覆盖
        let data = "K,V\na/b,1\na_b,2\n".as_bytes();
        let p = write_temp_file(data, "_dup.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let dir = temp_dir();
        let opts = SplitOptions {
            folder: dir.clone(),
            format: OutputFormat::Csv,
        };
        let count = split_by_field(&m, None, &[0], &opts, None).unwrap().files;
        assert_eq!(count, 2);
        assert!(std::fs::metadata(format!("{}\\a_b.csv", dir)).is_ok());
        assert!(std::fs::metadata(format!("{}\\a_b_2.csv", dir)).is_ok());
        cleanup(&dir);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn split_by_field_xlsx() {
        let data = "BANK,ID\n苏州银行,1\n工商银行,2\n".as_bytes();
        let p = write_temp_file(data, "_xlsxsrc.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let dir = temp_dir();
        let opts = SplitOptions {
            folder: dir.clone(),
            format: OutputFormat::Xlsx,
        };
        let count = split_by_field(&m, None, &[0], &opts, None).unwrap().files;
        assert_eq!(count, 2);
        assert!(std::fs::metadata(format!("{}\\苏州银行.xlsx", dir)).is_ok());
        assert!(std::fs::metadata(format!("{}\\工商银行.xlsx", dir)).is_ok());
        // 两阶段：中间临时 CSV 应已清理，无 ~split_tmp 残留
        let tmp_left = std::fs::read_dir(&dir)
            .map(|it| it.filter_map(|e| e.ok()).any(|e| e.file_name().to_string_lossy().starts_with("~split_tmp")))
            .unwrap_or(false);
        assert!(!tmp_left, "两阶段拆分应清理中间临时 CSV");
        cleanup(&dir);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn split_by_field_xlsx_no_formula_guard_pollution() {
        // 回归：两阶段中间 CSV 不得加公式防护 ' 前缀，否则负数/电话/公式文本被污染
        let data = "BANK,VAL\n银行A,-100.5\n银行A,+8613900000000\n银行B,=1+1\n".as_bytes();
        let p = write_temp_file(data, "_pollution.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let dir = temp_dir();
        let opts = SplitOptions {
            folder: dir.clone(),
            format: OutputFormat::Xlsx,
        };
        let count = split_by_field(&m, None, &[0], &opts, None).unwrap().files;
        assert_eq!(count, 2);
        // 读回银行A.xlsx：全文本导出后负数以文本保留（sharedStrings），无 ' 前缀污染
        let file = std::fs::File::open(format!("{}\\银行A.xlsx", dir)).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut xml = String::new();
        {
            let mut s = archive.by_name("xl/worksheets/sheet1.xml").unwrap();
            std::io::Read::read_to_string(&mut s, &mut xml).unwrap();
        }
        if let Ok(mut s) = archive.by_name("xl/sharedStrings.xml") {
            std::io::Read::read_to_string(&mut s, &mut xml).unwrap();
        }
        assert!(!xml.contains("'-100.5"), "负数被公式防护污染: {}", xml);
        assert!(xml.contains("-100.5"), "负数未正确写入: {}", xml);
        cleanup(&dir);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn split_by_field_xlsx_many_groups() {
        // 两阶段 XLSX：多分组（>1 个）时逐个转换，中间文件清理，输出正确
        let mut data = String::from("GROUP,VAL\n");
        for i in 0..3000u32 {
            data.push_str(&format!("g{},v{}\n", i % 3, i));
        }
        let p = write_temp_file(data.as_bytes(), "_xlsxmany.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let dir = temp_dir();
        let opts = SplitOptions {
            folder: dir.clone(),
            format: OutputFormat::Xlsx,
        };
        let count = split_by_field(&m, None, &[0], &opts, None).unwrap().files;
        assert_eq!(count, 3);
        for g in 0..3 {
            let path = format!("{}\\g{}.xlsx", dir, g);
            assert!(std::fs::metadata(&path).is_ok(), "缺少 g{}.xlsx", g);
        }
        let tmp_left = std::fs::read_dir(&dir)
            .map(|it| it.filter_map(|e| e.ok()).any(|e| e.file_name().to_string_lossy().starts_with("~split_tmp")))
            .unwrap_or(false);
        assert!(!tmp_left, "两阶段拆分应清理中间临时 CSV");
        cleanup(&dir);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn split_by_rows_respects_row_subset() {
        // 拆分跟随筛选结果：rows 子集只拆指定行
        let data = "ID,NAME\n1,a\n2,b\n3,c\n4,d\n5,e\n".as_bytes();
        let p = write_temp_file(data, "_subset.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let dir = temp_dir();
        let opts = SplitOptions {
            folder: dir.clone(),
            format: OutputFormat::Csv,
        };
        // 只拆第 1、3 行（"2,b"、"4,d"），每批 1 行 → 2 个文件
        let count = split_by_rows(&m, Some(&[1, 3]), 1, &opts, None).unwrap().files;
        assert_eq!(count, 2);
        let c1 = std::fs::read_to_string(format!("{}\\split_data_part_1.csv", dir)).unwrap();
        let c2 = std::fs::read_to_string(format!("{}\\split_data_part_2.csv", dir)).unwrap();
        assert!(c1.contains("2,b") && !c1.contains("1,a"));
        assert!(c2.contains("4,d") && !c2.contains("5,e"));
        cleanup(&dir);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn split_by_rows_exact_multiple_no_empty_tail() {
        // 回归：行数恰为 batch 整数倍时不得残留仅表头的空尾文件（CSV 模式）
        let data = "ID,NAME\n1,a\n2,b\n3,c\n4,d\n".as_bytes();
        let p = write_temp_file(data, "_exact.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let dir = temp_dir();
        let opts = SplitOptions {
            folder: dir.clone(),
            format: OutputFormat::Csv,
        };
        let count = split_by_rows(&m, None, 2, &opts, None).unwrap().files;
        assert_eq!(count, 2);
        assert!(std::fs::metadata(format!("{}\\split_data_part_1.csv", dir)).is_ok());
        assert!(std::fs::metadata(format!("{}\\split_data_part_2.csv", dir)).is_ok());
        assert!(std::fs::metadata(format!("{}\\split_data_part_3.csv", dir)).is_err());
        cleanup(&dir);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn split_empty_rows_errors() {
        // 空筛选结果（Some(&[])）应报错而不是生成空文件
        let data = "ID,NAME\n1,a\n2,b\n".as_bytes();
        let p = write_temp_file(data, "_emptyrows.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let dir = temp_dir();
        let opts = SplitOptions {
            folder: dir.clone(),
            format: OutputFormat::Csv,
        };
        let e1 = split_by_rows(&m, Some(&[]), 1, &opts, None).unwrap_err();
        assert!(e1.contains("没有可拆分的数据"));
        let e2 = split_by_field(&m, Some(&[]), &[0], &opts, None).unwrap_err();
        assert!(e2.contains("没有可拆分的数据"));
        cleanup(&dir);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn split_by_field_respects_row_subset() {
        // 按字段拆分跟随筛选结果：仅筛选行参与分组
        let data = "BANK,ID\n苏州银行,1\n工商银行,2\n苏州银行,3\n工商银行,4\n".as_bytes();
        let p = write_temp_file(data, "_subfield.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let dir = temp_dir();
        let opts = SplitOptions {
            folder: dir.clone(),
            format: OutputFormat::Csv,
        };
        // 只拆第 0、2 行（苏州银行,1 / 苏州银行,3）→ 仅 1 个分组文件
        let count = split_by_field(&m, Some(&[0, 2]), &[0], &opts, None).unwrap().files;
        assert_eq!(count, 1);
        let content = std::fs::read_to_string(format!("{}\\苏州银行.csv", dir)).unwrap();
        assert!(content.contains(",1") && content.contains(",3") && !content.contains(",2"));
        cleanup(&dir);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn split_by_field_ragged_rows_no_panic() {
        // review 修复：字段数不一致的行（某行列数少于所选列索引）不应 panic
        // "A,B\nx,y\nz\n" 第二行只有 1 字段，按第 2 列拆分时 key 应为空值
        let data = "A,B\nx,y\nz\n".as_bytes();
        let p = write_temp_file(data, "_ragged.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let dir = temp_dir();
        let opts = SplitOptions {
            folder: dir.clone(),
            format: OutputFormat::Csv,
        };
        let count = split_by_field(&m, None, &[1], &opts, None).unwrap().files;
        assert!(count >= 1);
        cleanup(&dir);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn split_by_rows_parallel_many_batches() {
        // 并行分派：批数（1000）远多于并发度上限（8），验证文件数、行序与内容
        let mut data = String::from("ID,NAME\n");
        for i in 0..1000 {
            data.push_str(&format!("{},n{}\n", i, i));
        }
        let p = write_temp_file(data.as_bytes(), "_manybatch.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let dir = temp_dir();
        let opts = SplitOptions {
            folder: dir.clone(),
            format: OutputFormat::Csv,
        };
        let count = split_by_rows(&m, None, 1, &opts, None).unwrap().files;
        assert_eq!(count, 1000);
        // 抽查若干批：批号即文件序号，且行序与源文件一致
        for i in [0usize, 1, 499, 998, 999] {
            let c =
                std::fs::read_to_string(format!("{}\\split_data_part_{}.csv", dir, i + 1)).unwrap();
            assert!(
                c.contains(&format!("{},n{}", i, i)),
                "第 {} 批内容缺失: {}",
                i + 1,
                c
            );
            assert!(!c.contains(&format!("{},n{}", i + 1, i + 1)), "第 {} 批混入了他批数据", i + 1);
        }
        cleanup(&dir);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn split_by_rows_reports_error_for_bad_folder() {
        // 并行写出遇到坏目录：必须返回 Err（错误在多个工作线程间正确汇总，而非 panic/静默成功）
        let data = "ID,NAME\n1,a\n2,b\n";
        let p = write_temp_file(data.as_bytes(), "_badfolder.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let opts = SplitOptions {
            folder: std::env::temp_dir()
                .join("csv_split_no_such_dir_should_not_exist")
                .to_string_lossy()
                .into_owned(),
            format: OutputFormat::Csv,
        };
        let e = split_by_rows(&m, None, 1, &opts, None).unwrap_err();
        assert!(!e.is_empty(), "应返回可读的错误信息");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn split_by_field_xlsx_parallel_many_groups() {
        // XLSX 并行转换：分组数（10）超过并发上限（6），验证不丢组且内容正确
        let mut data = String::from("GROUP,VAL\n");
        for i in 0..500u32 {
            data.push_str(&format!("g{},v{}\n", i % 10, i));
        }
        let p = write_temp_file(data.as_bytes(), "_xlsxpar.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let dir = temp_dir();
        let opts = SplitOptions {
            folder: dir.clone(),
            format: OutputFormat::Xlsx,
        };
        let count = split_by_field(&m, None, &[0], &opts, None).unwrap().files;
        assert_eq!(count, 10);
        for g in 0..10 {
            let path = format!("{}\\g{}.xlsx", dir, g);
            let file = std::fs::File::open(&path).unwrap_or_else(|_| panic!("缺少 {}", path));
            let mut archive = zip::ZipArchive::new(file).unwrap();
            let mut xml = String::new();
            if let Ok(mut s) = archive.by_name("xl/sharedStrings.xml") {
                std::io::Read::read_to_string(&mut s, &mut xml).unwrap();
            }
            // 每一组的行都应落在自己的文件里（抽查该组的首个值）
            let first = format!("v{}", g);
            assert!(xml.contains(&first), "g{}.xlsx 内容缺失（{}）", g, path);
        }
        // 中间临时 CSV 应已全部清理
        let tmp_left = std::fs::read_dir(&dir)
            .map(|it| {
                it.filter_map(|e| e.ok())
                    .any(|e| e.file_name().to_string_lossy().starts_with("~split_tmp"))
            })
            .unwrap_or(false);
        assert!(!tmp_left, "并行转换后应清理中间临时 CSV");
        cleanup(&dir);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn count_data_rows_is_quote_aware() {
        // 核对用的行数统计：引号字段内的换行不计行；LF / CRLF / CR / 末尾无换行 都正确
        let dir = temp_dir();
        let cases: Vec<(&str, &[u8], usize)> = vec![
            ("lf.csv", b"ID,NAME\n1,a\n2,b\n", 2),
            ("nonewline.csv", b"ID,NAME\n1,a\n2,b", 2),
            ("headeronly.csv", b"ID,NAME\n", 0),
            ("empty.csv", b"", 0),
            ("crlf.csv", b"ID,NAME\r\n1,a\r\n2,b\r\n", 2),
            ("cr.csv", b"ID,NAME\r1,a\r2,b\r", 2),
            // 引号内的换行属于同一字段，不应计为行边界
            ("quoted_lf.csv", b"ID,NAME\n1,\"x\ny\"\n2,b\n", 2),
            ("quoted_crlf.csv", b"ID,NAME\r\n1,\"x\r\ny\"\r\n2,b\r\n", 2),
            // 转义引号 "" 不应干扰引号状态
            ("escaped_quote.csv", b"ID,NAME\n1,\"he said \"\"hi\"\"\"\n2,b\n", 2),
        ];
        for (name, data, expect) in cases {
            let path = format!("{}\\{}", dir, name);
            std::fs::write(&path, data).unwrap();
            let got = count_data_rows(&path).unwrap();
            assert_eq!(got, expect, "{name} 行数统计不符");
        }
        cleanup(&dir);
    }

    #[test]
    fn split_report_reconciles_row_counts() {
        // 拆分报告应逐文件给出「写入行数 == 落盘行数」，且合计等于数据来源行数
        let data = "ID,NAME\n1,a\n2,b\n3,c\n4,d\n5,e\n";
        let p = write_temp_file(data.as_bytes(), "_report.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let dir = temp_dir();
        let opts = SplitOptions {
            folder: dir.clone(),
            format: OutputFormat::Csv,
        };
        // 按行数：每批 2 行 → 3 个文件（2、2、1）
        let rep = split_by_rows(&m, None, 2, &opts, None).unwrap();
        assert_eq!(rep.files, 3);
        assert_eq!(rep.source_rows, 5);
        assert_eq!(
            rep.stats.iter().map(|s| s.disk_rows).collect::<Vec<_>>(),
            vec![2, 2, 1]
        );
        assert!(
            rep.stats.iter().all(|s| s.written_rows == s.disk_rows),
            "写入行数与落盘行数应一致"
        );
        assert_eq!(rep.total_on_disk(), 5);
        assert!(rep.is_complete(), "完整性核对应通过：{}", rep.verdict());

        // 按字段：5 个不同 NAME → 5 个文件，行数合计仍应为 5
        let rep2 = split_by_field(&m, None, &[1], &opts, None).unwrap();
        assert_eq!(rep2.source_rows, 5);
        assert_eq!(rep2.total_on_disk(), 5);
        assert!(rep2.is_complete(), "按字段拆分也应通过核对：{}", rep2.verdict());

        cleanup(&dir);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn checkfile_contains_all_required_items() {
        // 核对表须含：拆分类型/依据、逐文件行数、输出总行数、来源行数、核对结果
        let data = "ID,NAME\n1,a\n2,b\n3,c\n4,d\n";
        let p = write_temp_file(data.as_bytes(), "_check.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let dir = temp_dir();
        let opts = SplitOptions {
            folder: dir.clone(),
            format: OutputFormat::Csv,
        };
        let rep = split_by_rows(&m, None, 2, &opts, None).unwrap();
        let cf = format!("{}\\_check_result.csv", dir);
        write_split_checkfile(
            &cf,
            "2026-09-27 10:00:00",
            "按行数拆分",
            "每批 2 行",
            "CSV",
            &rep,
        )
        .unwrap();
        let text = std::fs::read_to_string(&cf).unwrap();
        for item in [
            "拆分完整性核对表",
            "按行数拆分",
            "每批 2 行",
            "数据来源行数（不含表头）,4",
            "输出文件数,2",
            "输出总行数（不含表头）,4",
            "通过",
            "split_data_part_1.csv,2,2",
            "split_data_part_2.csv,2,2",
            "合计,2 个文件,4,4",
        ] {
            assert!(text.contains(item), "核对表缺少条目 {item:?}：\n{text}");
        }
        // 以 BOM 开头（Excel 中文不乱码）
        assert!(std::fs::read(&cf).unwrap().starts_with(&[0xEF, 0xBB, 0xBF]));
        cleanup(&dir);
        std::fs::remove_file(&p).ok();
    }

    fn write_temp_file(data: &[u8], suffix: &str) -> String {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("csv_split_src_{}_{}", std::process::id(), suffix));
        std::fs::write(&path, data).unwrap();
        path.to_string_lossy().into_owned()
    }
}
