//! 多文件合并引擎：表头校验、共有字段交集、流式合并为单个 CSV。
//!
//! 流程（GUI）：一次性选多个 CSV（可追加选择）→ 先逐一读表头校验一致性
//! （**读取数据前**就完成校验）→ 表头不一致时按用户选择只保留"相同字段名"的字段
//! （交集）→ 流式写出合并结果，再交给 `DataModel` 当普通单文件打开，
//! 从而复用筛选 / 拆分 / 列操作 / 导出的全部能力。
//!
//! 字段按**表头名**映射而非位置：即使各文件列顺序不同也能正确合并，
//! 目标文件中缺少的字段写空值。

use std::collections::HashSet;

use super::export::CsvWriter;
use super::reader::CsvFile;

/// 单个文件的表头信息（供对话框展示与校验）
#[derive(Debug, Clone)]
pub struct FileHeader {
    pub path: String,
    pub header: Vec<String>,
}

/// 表头一致性判定结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderCheck {
    /// 所有文件表头完全一致（字段名与顺序都相同）
    Identical,
    /// 字段名集合相同，仅顺序不同（可按名称重排合并，无需丢弃字段）
    Reordered,
    /// 存在字段名差异（须由用户选择"只保留相同字段名"才能合并）
    Differ,
}

impl HeaderCheck {
    pub fn label(&self) -> &'static str {
        match self {
            HeaderCheck::Identical => "完全一致",
            HeaderCheck::Reordered => "字段相同但顺序不同",
            HeaderCheck::Differ => "存在字段名差异",
        }
    }
}

/// 合并模式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMode {
    /// 要求表头一致（顺序不同也可以，按名称重排）
    RequireIdentical,
    /// 只保留所有文件共有的字段名（交集，按首个文件的顺序）
    KeepCommonFields,
}

impl MergeMode {
    pub fn label(&self) -> &'static str {
        match self {
            MergeMode::RequireIdentical => "按原字段全并",
            MergeMode::KeepCommonFields => "只保留相同字段名",
        }
    }
}

/// 合并结果报告
#[derive(Debug, Clone)]
pub struct MergeReport {
    pub files: usize,
    pub rows: usize,
    pub columns: usize,
    pub out_path: String,
    pub mode: MergeMode,
}

/// 读取单个文件的**表头**（只读文件开头，不构建行索引）。
///
/// 表头校验必须在大文件上也是秒级的：`CsvFile::open` 会为整个文件建行索引
/// （6.5GB 约 4 秒），选十几个文件校验就要一分钟，因此这里只读开头一段，
/// 按检测到的编码解码后取第一行解析。
pub fn read_file_header(path: &str) -> Result<FileHeader, String> {
    use std::io::Read;

    use super::csv::parse_record_str;
    use super::encoding::{detect_encoding, CsvEncoding};

    let mut f = std::fs::File::open(path).map_err(|e| format!("无法打开文件 {}：{}", path, e))?;
    let size = f.metadata().map(|m| m.len()).unwrap_or(0) as usize;
    let mut buf = vec![0u8; size.min(1 << 16)];
    let n = f.read(&mut buf).map_err(|e| format!("读取文件失败：{}", e))?;
    buf.truncate(n);
    if buf.is_empty() {
        return Err(format!("文件为空：{}", path));
    }

    let enc = detect_encoding(&buf);
    let text: String = match enc {
        CsvEncoding::Utf16Le => encoding_rs::UTF_16LE.decode(&buf).0.into_owned(),
        CsvEncoding::Utf16Be => encoding_rs::UTF_16BE.decode(&buf).0.into_owned(),
        CsvEncoding::Gb18030 => encoding_rs::GB18030.decode(&buf).0.into_owned(),
        _ => {
            let body = if buf.starts_with(&[0xEF, 0xBB, 0xBF]) {
                &buf[3..]
            } else {
                &buf[..]
            };
            String::from_utf8_lossy(body).into_owned()
        }
    };

    // 表头 = 第一行（与 reader.rs 一致：列名去首尾空白）
    let first_line = text.lines().next().unwrap_or("");
    let header: Vec<String> = parse_record_str(first_line)
        .iter()
        .map(|s| s.trim().to_string())
        .collect();
    if header.is_empty() {
        return Err(format!("无法解析表头：{}", path));
    }
    Ok(FileHeader {
        path: path.to_string(),
        header,
    })
}

/// 批量读取表头（顺序与入参一致）
pub fn read_file_headers(paths: &[String]) -> Result<Vec<FileHeader>, String> {
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        out.push(read_file_header(p)?);
    }
    Ok(out)
}

/// 表头字段名集合（用于"字段相同但顺序不同"的判定）
fn name_set(header: &[String]) -> HashSet<&str> {
    header.iter().map(|s| s.as_str()).collect()
}

/// 判定一组表头是否一致 / 仅顺序不同 / 有差异
pub fn check_headers(headers: &[Vec<String>]) -> HeaderCheck {
    let Some(first) = headers.first() else {
        return HeaderCheck::Identical;
    };
    if headers.iter().all(|h| h == first) {
        return HeaderCheck::Identical;
    }
    let set0 = name_set(first);
    if headers.iter().all(|h| name_set(h) == set0) {
        return HeaderCheck::Reordered;
    }
    HeaderCheck::Differ
}

/// 所有文件共有的字段名（按首个文件的顺序，去重）
pub fn common_fields(headers: &[Vec<String>]) -> Vec<String> {
    let Some(first) = headers.first() else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for name in first {
        if !seen.insert(name.as_str()) {
            continue; // 首文件内重复的列名只取一次
        }
        if headers
            .iter()
            .all(|h| h.iter().any(|x| x == name))
        {
            out.push(name.clone());
        }
    }
    out
}

/// 合并后的目标列（按模式决定）
pub fn target_columns(headers: &[Vec<String>], mode: MergeMode) -> Vec<String> {
    match mode {
        MergeMode::RequireIdentical => headers.first().cloned().unwrap_or_default(),
        MergeMode::KeepCommonFields => common_fields(headers),
    }
}

/// 各文件表头的差异摘要（供提示文案使用）：返回仅出现在部分文件里的字段名
pub fn exclusive_fields(headers: &[Vec<String>]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for h in headers {
        for name in h {
            if seen.contains(name.as_str()) {
                continue;
            }
            let count = headers.iter().filter(|x| x.iter().any(|y| y == name)).count();
            if count < headers.len() {
                seen.insert(name.as_str());
                out.push(name.clone());
            }
        }
    }
    out
}

/// 流式合并多个 CSV 到 `out_path`。
///
/// - 先打开所有文件并读表头完成校验（数据行尚未读取）；
/// - `MergeMode::RequireIdentical` 下若表头有字段差异则报错，调用方应改为
///   `KeepCommonFields`（GUI 由用户选择）；
/// - 输出为 utf-8-sig CSV，**不加**公式注入防护前缀（`new_raw`），
///   否则负数/加号开头的字段会被污染（该文件随后会被当作数据打开）。
pub fn merge_to_file(
    paths: &[String],
    mode: MergeMode,
    out_path: &str,
    progress: Option<&(dyn Fn(f64) + Sync)>,
) -> Result<MergeReport, String> {
    if paths.is_empty() {
        return Err("请先选择要合并的 CSV 文件".to_string());
    }
    // 1) 打开并读表头（读取数据前完成校验）
    let mut files: Vec<CsvFile> = Vec::with_capacity(paths.len());
    let mut headers: Vec<Vec<String>> = Vec::with_capacity(paths.len());
    for p in paths {
        let f = CsvFile::open(p).map_err(|e| format!("打开文件失败：{}（{}）", p, e))?;
        headers.push(f.header().to_vec());
        files.push(f);
    }
    // 2) 表头校验
    let check = check_headers(&headers);
    if check == HeaderCheck::Differ && mode == MergeMode::RequireIdentical {
        let diff = exclusive_fields(&headers);
        return Err(format!(
            "所选文件的表头不一致（差异字段：{}），请改用「只保留相同字段名」后合并",
            if diff.is_empty() {
                "未知".to_string()
            } else {
                diff.join("、")
            }
        ));
    }
    let target = target_columns(&headers, mode);
    if target.is_empty() {
        return Err("所选文件没有共同字段，无法合并".to_string());
    }

    // 3) 流式写出（字段按名称映射，缺列写空）
    let total_rows: usize = files.iter().map(|f| f.row_count()).sum();
    let mut w = CsvWriter::new_raw(out_path, &target).map_err(|e| e.to_string())?;
    let mut rec: Vec<String> = Vec::new();
    let mut row_out: Vec<String> = Vec::with_capacity(target.len());
    let mut written = 0usize;
    for file in &files {
        let map: Vec<Option<usize>> = target
            .iter()
            .map(|name| file.header().iter().position(|h| h == name))
            .collect();
        for r in 0..file.row_count() {
            file.row_into(r, &mut rec);
            row_out.clear();
            for m in &map {
                row_out.push(m.and_then(|i| rec.get(i)).cloned().unwrap_or_default());
            }
            w.write_row(&row_out).map_err(|e| e.to_string())?;
            written += 1;
            if written % 50_000 == 0 {
                if let Some(p) = progress {
                    p(written as f64 / total_rows.max(1) as f64);
                }
            }
        }
    }
    w.finish().map_err(|e| e.to_string())?;
    if let Some(p) = progress {
        p(1.0);
    }
    Ok(MergeReport {
        files: files.len(),
        rows: written,
        columns: target.len(),
        out_path: out_path.to_string(),
        mode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("csv_merge_test_{}_{}", std::process::id(), id));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write(dir: &std::path::Path, name: &str, content: &str) -> String {
        let p = dir.join(name);
        std::fs::write(&p, content.as_bytes()).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn h(cols: &[&str]) -> Vec<String> {
        cols.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn header_check_identical_reordered_differ() {
        let a = h(&["A", "B", "C"]);
        assert_eq!(check_headers(&[a.clone(), a.clone()]), HeaderCheck::Identical);
        assert_eq!(
            check_headers(&[a.clone(), h(&["A", "B", "C"]), h(&["C", "A", "B"])]),
            HeaderCheck::Reordered
        );
        assert_eq!(
            check_headers(&[a, h(&["A", "B", "D"])]),
            HeaderCheck::Differ
        );
        // 空输入视为一致
        assert_eq!(check_headers(&[]), HeaderCheck::Identical);
    }

    #[test]
    fn common_fields_and_exclusive() {
        let headers = vec![
            h(&["A", "B", "C"]),
            h(&["A", "C", "D"]),
            h(&["A", "B", "C"]),
        ];
        assert_eq!(common_fields(&headers), h(&["A", "C"]));
        let diff = exclusive_fields(&headers);
        assert!(diff.contains(&"B".to_string()));
        assert!(diff.contains(&"D".to_string()));
        assert!(!diff.contains(&"A".to_string()));
    }

    #[test]
    fn merge_identical_headers() {
        let dir = temp_dir();
        let p1 = write(&dir, "a.csv", "ID,NAME\n1,alice\n2,bob\n");
        let p2 = write(&dir, "b.csv", "ID,NAME\n3,carol\n");
        let out = dir.join("merged.csv").to_string_lossy().into_owned();
        let rep = merge_to_file(
            &[p1.clone(), p2.clone()],
            MergeMode::RequireIdentical,
            &out,
            None,
        )
        .unwrap();
        assert_eq!(rep.files, 2);
        assert_eq!(rep.rows, 3);
        assert_eq!(rep.columns, 2);
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.starts_with('\u{FEFF}'));
        assert!(text.contains("ID,NAME"));
        assert!(text.contains("1,alice") && text.contains("2,bob") && text.contains("3,carol"));
        // 行序：先 a 后 b
        let alice = text.find("alice").unwrap();
        let carol = text.find("carol").unwrap();
        assert!(alice < carol);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_reordered_headers_maps_by_name() {
        let dir = temp_dir();
        // 列顺序不同：B 文件是 NAME,ID
        let p1 = write(&dir, "a.csv", "ID,NAME\n1,alice\n");
        let p2 = write(&dir, "b.csv", "NAME,ID\nbob,2\n");
        let out = dir.join("m.csv").to_string_lossy().into_owned();
        let rep = merge_to_file(&[p1, p2], MergeMode::RequireIdentical, &out, None).unwrap();
        assert_eq!(rep.rows, 2);
        let text = std::fs::read_to_string(&out).unwrap();
        // 结果按首个文件的列序 ID,NAME 输出，bob 行应是 2,bob
        assert!(text.contains("2,bob"), "按名称重排失败：{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_keep_common_fields() {
        let dir = temp_dir();
        let p1 = write(&dir, "a.csv", "A,B,C\n1,2,3\n4,5,6\n");
        let p2 = write(&dir, "b.csv", "A,C,D\n7,8,9\n");
        let out = dir.join("c.csv").to_string_lossy().into_owned();
        // 严格模式应报错并指出差异字段
        let err = merge_to_file(&[p1.clone(), p2.clone()], MergeMode::RequireIdentical, &out, None)
            .unwrap_err();
        assert!(err.contains("表头不一致"), "错误信息应提示表头不一致：{err}");
        assert!(err.contains('B') && err.contains('D'), "应列出差异字段：{err}");
        // 交集模式：只保留 A、C
        let rep = merge_to_file(&[p1, p2], MergeMode::KeepCommonFields, &out, None).unwrap();
        assert_eq!(rep.columns, 2);
        assert_eq!(rep.rows, 3);
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.contains("A,C"), "目标表头应为 A,C：{text}");
        assert!(!text.contains(",B,"), "不应包含差异字段 B");
        // 行内容：a 的 (1,3)、(4,6)，b 的 (7,8)
        assert!(text.contains("1,3") && text.contains("4,6") && text.contains("7,8"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_output_not_polluted_by_formula_guard() {
        // 负数/加号开头的字段不得被加 ' 前缀（合并输出是数据，不是 Excel 防护目标）
        let dir = temp_dir();
        let p1 = write(&dir, "a.csv", "BANK,VAL\n银行A,-100.5\n银行A,+8613900000000\n");
        let out = dir.join("d.csv").to_string_lossy().into_owned();
        merge_to_file(&[p1], MergeMode::RequireIdentical, &out, None).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.contains("-100.5"), "负数被污染：{text}");
        assert!(text.contains("+8613900000000"), "加号字段被污染：{text}");
        assert!(!text.contains("'-100.5"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_empty_selection_errors() {
        let dir = temp_dir();
        let out = dir.join("x.csv").to_string_lossy().into_owned();
        assert!(merge_to_file(&[], MergeMode::RequireIdentical, &out, None).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_file_header_fast_covers_encodings() {
        let dir = temp_dir();
        // UTF-8 带 BOM
        let p = write(&dir, "a.csv", "\u{FEFF}ID,NAME\n1,a\n2,b\n");
        assert_eq!(read_file_header(&p).unwrap().header, h(&["ID", "NAME"]));
        // 列名首尾空白/制表符应被裁剪（与 reader.rs 行为一致）
        let p2 = write(&dir, "b.csv", " 交易卡号 \t,交易账号\n1,2\n");
        assert_eq!(
            read_file_header(&p2).unwrap().header,
            h(&["交易卡号", "交易账号"])
        );
        // GB18030（ANSI 中文文件）
        let (gbk, _, _) = encoding_rs::GB18030.encode("姓名,城市\n张三,北京\n");
        let p3 = dir.join("c.csv");
        std::fs::write(&p3, &gbk).unwrap();
        assert_eq!(
            read_file_header(&p3.to_string_lossy()).unwrap().header,
            h(&["姓名", "城市"])
        );
        // UTF-16 LE 带 BOM
        let mut u16b = vec![0xFF, 0xFE];
        for c in "ID,NAME\n1,a\n".encode_utf16() {
            u16b.extend_from_slice(&c.to_le_bytes());
        }
        let p4 = dir.join("d.csv");
        std::fs::write(&p4, &u16b).unwrap();
        assert_eq!(
            read_file_header(&p4.to_string_lossy()).unwrap().header,
            h(&["ID", "NAME"])
        );
        // 空文件应报错
        let p5 = write(&dir, "e.csv", "");
        assert!(read_file_header(&p5).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
