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
) -> Result<usize, String> {
    if batch_size == 0 {
        return Err("每批行数必须大于 0".to_string());
    }
    let n = model.row_count();
    let total = rows.map(|v| v.len()).unwrap_or(n);
    if n == 0 || total == 0 {
        return Err("没有可拆分的数据".to_string());
    }
    let header = model.header().to_vec();
    let iter: Box<dyn Iterator<Item = usize>> = match rows {
        Some(list) => Box::new(list.iter().map(|&r| r as usize)),
        None => Box::new(0..n),
    };
    // peek 预检：数据恰好耗尽时不再打开新文件（避免 CSV 模式残留仅表头的空尾文件）
    let mut iter = iter.peekable();
    let mut file_count = 0usize;
    let mut done = 0usize;
    let mut rec: Vec<String> = Vec::new();
    while iter.peek().is_some() {
        let path = format!(
            "{}\\split_data_part_{}.{}",
            opts.folder,
            file_count + 1,
            opts.format.extension()
        );
        let mut w = OpenWriter::new(&path, opts.format, &header)?;
        let mut written = 0usize;
        for _ in 0..batch_size {
            match iter.next() {
                Some(row) => {
                    model.row_into(row, &mut rec);
                    w.write_row(&rec)?;
                    written += 1;
                }
                None => break,
            }
        }
        w.finish()?;
        file_count += 1;
        done += written;
        if let Some(p) = progress {
            p(done as f64 / total as f64);
        }
    }
    Ok(file_count)
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
) -> Result<usize, String> {
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
) -> Result<usize, String> {
    let n = model.row_count();
    let total = rows.map(|v| v.len()).unwrap_or(n);
    let header = model.header().to_vec();

    // key(字段值组合) -> 文件序号
    let mut key_index: HashMap<String, usize> = HashMap::with_capacity(1024);
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
    for row in iter {
        model.row_into(row, &mut rec);
        let key = make_key(&rec, columns);
        let idx = match key_index.get(&key) {
            Some(&i) => i,
            None => {
                if writers.len() >= MAX_SPLIT_FILES {
                    return Err(format!(
                        "该字段唯一值超过 {} 个，不适合按字段拆分（可考虑按行数拆分）",
                        MAX_SPLIT_FILES
                    ));
                }
                let idx = writers.len();
                let mut base = sanitize_filename(&key);
                if key.trim().is_empty() || base == "nan" {
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
                key_index.insert(key, idx);
                idx
            }
        };
        if let Some(w) = writers[idx].as_mut() {
            w.write_row(&rec)?;
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

    let count = writers.len();
    for w in writers.into_iter().flatten() {
        w.finish()?;
    }
    Ok(count)
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
) -> Result<usize, String> {
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
    for row in iter {
        model.row_into(row, &mut rec);
        let key = make_key(&rec, columns);
        let idx = match key_index.get(&key) {
            Some(&i) => i,
            None => {
                if csv_writers.len() >= MAX_SPLIT_FILES {
                    return Err(format!(
                        "该字段唯一值超过 {} 个，不适合按字段拆分（可考虑按行数拆分）",
                        MAX_SPLIT_FILES
                    ));
                }
                let idx = csv_writers.len();
                let mut base = sanitize_filename(&key);
                if key.trim().is_empty() || base == "nan" {
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
                temp_paths.push(temp_path.clone());
                temp_guard.0.push(temp_path);
                key_index.insert(key, idx);
                idx
            }
        };
        if let Some(w) = csv_writers[idx].as_mut() {
            w.write_row(&rec).map_err(|e| e.to_string())?;
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

    // ---- 第二遍：逐个中间 CSV → XLSX（内存 = 单组行数），转换后删除中间文件 ----
    let mut done_groups = 0usize;
    let mut fields: Vec<String> = Vec::new();
    for i in 0..group_count {
        let src = &temp_paths[i];
        let out_path = format!("{}\\{}.xlsx", opts.folder, final_names[i]);
        let file = crate::core::reader::CsvFile::open(src).map_err(|e| e.to_string())?;
        let mut w = XlsxWriter::new(&out_path, &header)?;
        for r in 0..file.row_count() {
            file.row_into(r, &mut fields);
            w.write_row(&fields)?;
        }
        w.finish()?;
        drop(file); // 先释放 mmap 文件句柄，Windows 才能删除该中间 CSV
        let _ = std::fs::remove_file(src);
        done_groups += 1;
        if let Some(p) = progress {
            p(0.5 + (done_groups as f64 / group_count.max(1) as f64) * 0.5);
        }
    }
    Ok(group_count)
}

/// 生成分组 key：多字段用 \x1F 连接（字段值中几乎不会出现）
fn make_key(rec: &[String], columns: &[usize]) -> String {
    if columns.len() == 1 {
        // 字段数不一致的行（列数少于所选索引）→ 视为空值
        return rec.get(columns[0]).map(|s| s.trim()).unwrap_or("").to_string();
    }
    let mut parts = Vec::with_capacity(columns.len());
    for &c in columns {
        parts.push(rec.get(c).map(|s| s.trim()).unwrap_or(""));
    }
    parts.join("\u{1F}")
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
        let count = split_by_rows(&m, None, 2, &opts, None).unwrap();
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
        let count = split_by_field(&m, None, &[0], &opts, None).unwrap();
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
        let count = split_by_field(&m, None, &[0, 1], &opts, None).unwrap();
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
        let count = split_by_field(&m, None, &[0], &opts, None).unwrap();
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
        let count = split_by_field(&m, None, &[0], &opts, None).unwrap();
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
        let count = split_by_field(&m, None, &[0], &opts, None).unwrap();
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
        let count = split_by_field(&m, None, &[0], &opts, None).unwrap();
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
        let count = split_by_field(&m, None, &[0], &opts, None).unwrap();
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
        let count = split_by_rows(&m, Some(&[1, 3]), 1, &opts, None).unwrap();
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
        let count = split_by_rows(&m, None, 2, &opts, None).unwrap();
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
        let count = split_by_field(&m, Some(&[0, 2]), &[0], &opts, None).unwrap();
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
        let count = split_by_field(&m, None, &[1], &opts, None).unwrap();
        assert!(count >= 1);
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
