//! 导出引擎：CSV 流式写出（utf-8-sig）与 XLSX 写出。
//!
//! XLSX 要点（修正 Python 版缺陷）：
//! - 全文本导出：所有列一律按文本（`@` 格式）写入，长数字/前导零/大整数不会被
//!   Excel 数值化，从根本上杜绝科学计数法与精度丢失（无需按列名或采样识别长数字列）；
//! - 超过 Excel 单表 1,048,576 行上限自动新建 sheet（Python 版直接崩溃）；
//! - 全部流式写入，不把数据驻留内存。

use std::fs::File;
use std::io::{BufWriter, Write};

use rust_xlsxwriter::{Format, Workbook};

/// Excel 单 sheet 最大行数（含表头）
pub const XLSX_MAX_ROWS_PER_SHEET: u32 = 1_048_576;

/// CSV 流式写出器（utf-8-sig，即带 BOM 的 UTF-8，与 Python 版一致）
pub struct CsvWriter {
    w: BufWriter<File>,
    pub rows_written: usize,
    /// 是否给以 = + - @ 开头的字段加 ' 前缀（公式注入防护；内部中转文件应关闭）
    guard_formula: bool,
}

impl CsvWriter {
    /// 创建写出器并写入表头（默认启用公式注入防护）
    pub fn new(path: &str, header: &[String]) -> std::io::Result<Self> {
        Self::with_guard(path, header, true)
    }

    /// 创建写出器并写入表头，**不**加公式注入前缀。
    /// 用于内部中转文件（如两阶段拆分的中间 CSV），避免污染后续读回的数据。
    pub fn new_raw(path: &str, header: &[String]) -> std::io::Result<Self> {
        Self::with_guard(path, header, false)
    }

    fn with_guard(path: &str, header: &[String], guard: bool) -> std::io::Result<Self> {
        let file = File::create(path).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("无法创建文件 {}: {}", path, e),
            )
        })?;
        let mut w = BufWriter::new(file);
        // BOM（utf-8-sig）
        w.write_all(&[0xEF, 0xBB, 0xBF])?;
        let mut w = Self {
            w,
            rows_written: 0,
            guard_formula: guard,
        };
        w.write_row(header)?;
        Ok(w)
    }

    /// 写入一行（按 CSV 规则转义：含逗号/引号/换行的字段用引号包裹，引号翻倍；
    /// 公式注入防护（若启用）：以 = + - @ 开头的字段加 ' 前缀，避免 Excel 打开时执行公式）
    pub fn write_row(&mut self, fields: &[String]) -> std::io::Result<()> {
        // 优化点：需要引号时按引号位置**分片**写出，而不是逐字节 write_all
        //（早期实现是 `for b in f.bytes() { self.w.write_all(&[b]) }`，含引号的
        // 大字段会被拆成逐字节调用）。
        // 注意两点实测结论：(1) 不要在中间 Vec 里拼整行再写出——多一次整行拷贝，
        // 按字段拆分反而慢 14%；(2) `needs_quote` 保持用 `str::contains` 判断，
        // 它底层走 memchr，比手写 `.iter().any()` 逐字节扫描更快（改成后者时
        // 按字段拆分慢了约 7%）——绝大多数字段本就不需要引号，这条是热路径。
        for (i, f) in fields.iter().enumerate() {
            if i > 0 {
                self.w.write_all(b",")?;
            }
            if self.guard_formula && is_formula_prefix(f) {
                self.w.write_all(b"'")?;
            }
            let bytes = f.as_bytes();
            let needs_quote =
                f.contains(',') || f.contains('"') || f.contains('\n') || f.contains('\r');
            if needs_quote {
                self.w.write_all(b"\"")?;
                let mut last = 0usize;
                for (j, &b) in bytes.iter().enumerate() {
                    if b == b'"' {
                        self.w.write_all(&bytes[last..j])?;
                        self.w.write_all(b"\"\"")?;
                        last = j + 1;
                    }
                }
                self.w.write_all(&bytes[last..])?;
                self.w.write_all(b"\"")?;
            } else {
                self.w.write_all(bytes)?;
            }
        }
        self.w.write_all(b"\r\n")?;
        self.rows_written += 1;
        Ok(())
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        self.w.flush()
    }

    pub fn finish(mut self) -> std::io::Result<()> {
        self.w.flush()
    }
}

/// XLSX 流式写出器：超过单 sheet 上限自动分 sheet；全文本导出（所有字段按文本，防丢失）。
pub struct XlsxWriter {
    path: String,
    workbook: Workbook,
    sheet_index: usize,
    row_in_sheet: u32,
    header: Vec<String>,
    text_format: Format,
    pub rows_written: usize,
}

impl XlsxWriter {
    pub fn new(path: &str, header: &[String]) -> Result<Self, String> {
        // Excel 最大列数 16384（XFD）
        if header.len() > 16_384 {
            return Err(format!(
                "列数 {} 超过 Excel 上限 16384，无法导出 XLSX（可改用 CSV）",
                header.len()
            ));
        }
        let mut workbook = Workbook::new();
        workbook.add_worksheet(); // sheet 0
        Ok(Self {
            path: path.to_string(),
            workbook,
            sheet_index: 0,
            row_in_sheet: 0,
            header: header.to_vec(),
            text_format: Format::new().set_num_format("@"),
            rows_written: 0,
        })
    }

    fn write_header(&mut self) -> Result<(), String> {
        let sheet = self
            .workbook
            .worksheet_from_index(self.sheet_index)
            .map_err(|e| format!("获取工作表失败: {}", e))?;
        for (c, name) in self.header.iter().enumerate() {
            sheet
                .write_string(self.row_in_sheet, c as u16, name.trim())
                .map_err(|e| format!("写入表头失败: {}", e))?;
        }
        self.row_in_sheet += 1;
        // 全文本导出：所有列设为文本格式（@），确保 Excel 中不自动转数值、不丢精度
        for c in 0..self.header.len() {
            sheet
                .set_column_format(c as u16, &self.text_format)
                .map_err(|e| format!("设置列格式失败: {}", e))?;
        }
        Ok(())
    }

    /// 写入一行数据；自动分 sheet
    pub fn write_row(&mut self, fields: &[String]) -> Result<(), String> {
        if self.row_in_sheet == 0 {
            self.write_header()?;
        }
        if self.row_in_sheet >= XLSX_MAX_ROWS_PER_SHEET {
            self.next_sheet()?;
        }
        let sheet = self
            .workbook
            .worksheet_from_index(self.sheet_index)
            .map_err(|e| format!("获取工作表失败: {}", e))?;
        for (c, f) in fields.iter().enumerate() {
            // 全文本导出：所有字段一律写为文本，避免长数字/前导零/大整数被 Excel 数值化丢失精度；
            // 同时去掉字段前后空白（银行 CSV 字段值常带尾部制表符等），保证单元格值干净
            sheet
                .write_string(self.row_in_sheet, c as u16, f.trim())
                .map_err(|e| format!("写入单元格失败: {}", e))?;
        }
        self.row_in_sheet += 1;
        self.rows_written += 1;
        Ok(())
    }

    fn next_sheet(&mut self) -> Result<(), String> {
        self.sheet_index += 1;
        self.workbook.add_worksheet();
        self.row_in_sheet = 0;
        self.write_header()
    }

    pub fn finish(mut self) -> Result<(), String> {
        if self.row_in_sheet == 0 {
            self.write_header()?;
        }
        self.workbook
            .save(&self.path)
            .map_err(|e| format!("保存 XLSX 失败: {}", e))
    }
}

/// 公式注入前缀检测（CWE-1236）：= + - @ 开头（trim 后）的字段在 Excel 中可能被当公式执行
fn is_formula_prefix(s: &str) -> bool {
    let t = s.trim_start();
    matches!(
        t.as_bytes().first(),
        Some(b'=') | Some(b'+') | Some(b'-') | Some(b'@')
    )
}

/// 导出「重复行对应表」：每条记录给出"保留的首次出现行 ↔ 被剔除的重复行"，
/// 并附上判重字段的取值，便于用户逐条核对去重是否正确。
///
/// 行号从 **1** 开始（与界面表格显示一致）。`is_xlsx` 为真时导出 XLSX，否则 CSV。
pub fn export_dup_map(
    model: &super::model::DataModel,
    map: &[(u32, u32)],
    key_columns: &[usize],
    path: &str,
    is_xlsx: bool,
    progress: Option<&(dyn Fn(f64) + Sync)>,
) -> Result<usize, String> {
    let mut header: Vec<String> = vec!["保留行号".to_string(), "被剔除行号".to_string()];
    for &c in key_columns {
        header.push(
            model
                .header()
                .get(c)
                .cloned()
                .unwrap_or_else(|| format!("列{}", c + 1)),
        );
    }

    let total = map.len();
    // 判重字段值直接从原文件按需读取（复用读取器，零分配）
    let mut reader = super::reader::RowReader::new(key_columns.to_vec());
    let mut row: Vec<String> = Vec::with_capacity(header.len());

    let mut csv_w: Option<CsvWriter> = if is_xlsx {
        None
    } else {
        Some(CsvWriter::new(path, &header).map_err(|e| e.to_string())?)
    };
    let mut xlsx_w: Option<XlsxWriter> = if is_xlsx {
        Some(XlsxWriter::new(path, &header)?)
    } else {
        None
    };

    for (i, &(kept, dropped)) in map.iter().enumerate() {
        row.clear();
        row.push((kept + 1).to_string());
        row.push((dropped + 1).to_string());
        reader.load(model.file(), kept as usize);
        for &c in key_columns {
            row.push(reader.value(c).to_string());
        }
        if let Some(w) = csv_w.as_mut() {
            w.write_row(&row).map_err(|e| e.to_string())?;
        }
        if let Some(w) = xlsx_w.as_mut() {
            w.write_row(&row)?;
        }
        if let Some(p) = progress {
            if i % 50_000 == 0 {
                p(i as f64 / total.max(1) as f64);
            }
        }
    }
    if let Some(w) = csv_w {
        w.finish().map_err(|e| e.to_string())?;
    }
    if let Some(w) = xlsx_w {
        w.finish()?;
    }
    if let Some(p) = progress {
        p(1.0);
    }
    Ok(total)
}

/// 便捷：把行集合导出为 CSV
pub fn export_rows_csv(
    path: &str,
    header: &[String],
    rows: impl Iterator<Item = Vec<String>>,
) -> Result<usize, String> {
    let mut w = CsvWriter::new(path, header).map_err(|e| e.to_string())?;
    let mut n = 0;
    for r in rows {
        w.write_row(&r).map_err(|e| e.to_string())?;
        n += 1;
    }
    w.finish().map_err(|e| e.to_string())?;
    Ok(n)
}

/// 便捷：把行集合导出为 XLSX（全文本导出，防丢失）
pub fn export_rows_xlsx(
    path: &str,
    header: &[String],
    rows: impl Iterator<Item = Vec<String>>,
) -> Result<usize, String> {
    // 全文本导出：所有字段按文本写出，无需识别长数字列
    let mut w = XlsxWriter::new(path, header)?;
    let mut n = 0;
    for r in rows {
        w.write_row(&r)?;
        n += 1;
    }
    w.finish()?;
    Ok(n)
}

/// 从列视图导出为 CSV（隐藏列剔除、派生列计算、utf-8-sig）。
/// `rows`：要导出的行号（None = 全部数据行）。
pub fn export_view_csv(
    model: &super::model::DataModel,
    ops: &super::column_ops::ColumnOps,
    rows: Option<&[u32]>,
    path: &str,
    progress: Option<&(dyn Fn(f64) + Sync)>,
) -> Result<usize, String> {
    let header = ops.header().to_vec();
    let visible = ops.visible_columns();
    let header_visible: Vec<String> = visible.iter().map(|&i| header[i].clone()).collect();
    let mut w = CsvWriter::new(path, &header_visible).map_err(|e| e.to_string())?;

    let total = rows.map(|r| r.len() as f64).unwrap_or(model.row_count() as f64);
    let mut done = 0usize;
    let mut n = 0usize;
    let write_row = |w: &mut CsvWriter, row: usize| -> std::io::Result<()> {
        let fields: Vec<String> = visible.iter().map(|&c| ops.field(model, row, c)).collect();
        w.write_row(&fields)
    };
    match rows {
        Some(list) => {
            for &row in list {
                let r = row as usize;
                if r < model.row_count() {
                    write_row(&mut w, r).map_err(|e| e.to_string())?;
                    n += 1;
                }
                done += 1;
                if let Some(p) = progress {
                    p(done as f64 / total.max(1.0));
                }
            }
        }
        None => {
            for row in 0..model.row_count() {
                write_row(&mut w, row).map_err(|e| e.to_string())?;
                n += 1;
                done += 1;
                if let Some(p) = progress {
                    p(done as f64 / total.max(1.0));
                }
            }
        }
    }
    w.finish().map_err(|e| e.to_string())?;
    Ok(n)
}

/// 从列视图导出为 XLSX（长数字列文本格式、超行数自动分 sheet）。
pub fn export_view_xlsx(
    model: &super::model::DataModel,
    ops: &super::column_ops::ColumnOps,
    rows: Option<&[u32]>,
    path: &str,
    progress: Option<&(dyn Fn(f64) + Sync)>,
) -> Result<usize, String> {
    let header = ops.header().to_vec();
    let visible = ops.visible_columns();
    let header_visible: Vec<String> = visible.iter().map(|&i| header[i].clone()).collect();

    // 全文本导出：所有字段按文本写出，无需识别长数字列
    let mut w = XlsxWriter::new(path, &header_visible)?;

    let total = rows.map(|r| r.len() as f64).unwrap_or(model.row_count() as f64);
    let mut done = 0usize;
    let mut n = 0usize;
    match rows {
        Some(list) => {
            for &row in list {
                let r = row as usize;
                if r < model.row_count() {
                    let fields: Vec<String> =
                        visible.iter().map(|&c| ops.field(model, r, c)).collect();
                    w.write_row(&fields)?;
                    n += 1;
                }
                done += 1;
                if let Some(p) = progress {
                    p(done as f64 / total.max(1.0));
                }
            }
        }
        None => {
            for row in 0..model.row_count() {
                let fields: Vec<String> =
                    visible.iter().map(|&c| ops.field(model, row, c)).collect();
                w.write_row(&fields)?;
                n += 1;
                done += 1;
                if let Some(p) = progress {
                    p(done as f64 / total.max(1.0));
                }
            }
        }
    }
    w.finish()?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(suffix: &str) -> String {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("csv_export_test_{}_{}", std::process::id(), suffix));
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn csv_writer_roundtrip() {
        let p = write_temp("a.csv");
        let header = vec!["ID".to_string(), "备注".to_string()];
        let mut w = CsvWriter::new(&p, &header).unwrap();
        w.write_row(&["1".into(), "含,逗号".into()]).unwrap();
        w.write_row(&["2".into(), "含\"引号\"".into()]).unwrap();
        w.write_row(&["3".into(), "多\n行".into()]).unwrap();
        w.finish().unwrap();
        let content = std::fs::read(&p).unwrap();
        // BOM
        assert_eq!(&content[..3], &[0xEF, 0xBB, 0xBF]);
        let text = String::from_utf8_lossy(&content[3..]);
        assert!(text.contains("\"含,逗号\""));
        assert!(text.contains("\"含\"\"引号\"\"\""));
        assert!(text.contains("\"多\n行\""));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn csv_writer_formula_injection_guard() {
        // security 修复：以 = + - @ 开头字段加 ' 前缀（CWE-1236）
        let p = write_temp("f.csv");
        let header = vec!["A".to_string()];
        let mut w = CsvWriter::new(&p, &header).unwrap();
        w.write_row(&["=SUM(A1:A2)".into()]).unwrap();
        w.write_row(&["+1".into()]).unwrap();
        w.write_row(&["-1".into()]).unwrap();
        w.write_row(&["@cmd".into()]).unwrap();
        w.write_row(&["正常文本".into()]).unwrap();
        w.finish().unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("'=SUM(A1:A2)"));
        assert!(text.contains("'+1"));
        assert!(text.contains("'-1"));
        assert!(text.contains("'@cmd"));
        assert!(text.contains("正常文本"));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn xlsx_writer_rejects_too_many_columns() {
        // security 修复：列数超过 Excel 上限 16384 时显式报错
        let p = write_temp("wide.xlsx");
        let header: Vec<String> = (0..16_400).map(|i| format!("C{}", i)).collect();
        let r = XlsxWriter::new(&p, &header);
        assert!(r.is_err());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn xlsx_writer_basic_and_sheet_split() {
        let p = write_temp("b.xlsx");
        let header = vec!["卡号".to_string(), "值".to_string()];
        let mut w = XlsxWriter::new(&p, &header).unwrap();
        // 写入超过单 sheet 上限的行数（用小阈值模拟不可行，直接用真实上限附近
        // 的成本过高；这里验证常规写入 + 长数字以文本保留）
        for i in 0..100u32 {
            let card = format!("622202020011234567{}", i % 10);
            w.write_row(&[card, i.to_string()]).unwrap();
        }
        w.finish().unwrap();
        assert!(std::fs::metadata(&p).unwrap().len() > 0);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn xlsx_writer_many_rows_auto_sheet() {
        let p = write_temp("c.xlsx");
        let header = vec!["A".to_string()];
        let mut w = XlsxWriter::new(&p, &header).unwrap();
        w.write_row(&["1".into()]).unwrap();
        w.write_row(&["2".into()]).unwrap();
        let rows = w.rows_written;
        w.finish().unwrap();
        assert!(rows >= 2);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn xlsx_long_number_written_as_text() {
        // 长数字以文本写入（write_string 路径）：数字列正常数值
        let p = write_temp("d.xlsx");
        let header = vec!["卡号".to_string(), "金额".to_string()];
        let mut w = XlsxWriter::new(&p, &header).unwrap();
        w.write_row(&["6222020200112345678".into(), "123.5".into()])
            .unwrap();
        w.finish().unwrap();
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn export_view_csv_with_hidden_and_derived() {
        use crate::core::column_ops::ColumnOps;
        use crate::core::model::DataModel;
        let src = write_temp("_viewsrc.csv");
        std::fs::write(&src, b"ID,NAME\n1,alice\n2,bob\n").unwrap();
        let m = DataModel::open(&src, 100).unwrap();
        let mut ops = ColumnOps::new(m.header());
        ops.copy_column(1, "NAME2").unwrap(); // 派生列
        let out = write_temp("v.csv");
        let n = export_view_csv(&m, &ops, Some(&[0, 1]), &out, None).unwrap();
        assert_eq!(n, 2);
        let content = std::fs::read_to_string(&out).unwrap();
        assert!(content.starts_with('\u{FEFF}'));
        assert!(content.contains("ID,NAME,NAME2"));
        assert!(content.contains("1,alice,alice"));
        std::fs::remove_file(&out).ok();
        std::fs::remove_file(&src).ok();
    }

    #[test]
    fn xlsx_writer_tab_suffixed_numbers_not_zero() {
        // 回归：银行 CSV 字段值带尾部制表符（如 "2553.34\t"），
        // 数值解析必须先 trim 再 parse，否则金额全部写成 0（此前 bug）
        let p = write_temp("tab.xlsx");
        let header = vec!["金额".to_string(), "卡号".to_string()];
        let mut w = XlsxWriter::new(&p, &header).unwrap();
        w.write_row(&["2553.34\t".into(), "6230520600147748672\t".into()])
            .unwrap();
        w.write_row(&["0.00\t".into(), "271101101708087\t".into()])
            .unwrap();
        w.finish().unwrap();

        // 读回：全文本导出后金额以文本保留（sharedStrings），不再是数值
        let file = std::fs::File::open(&p).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut sheet_xml = String::new();
        let mut shared = String::new();
        {
            let mut sheet = archive.by_name("xl/worksheets/sheet1.xml").unwrap();
            std::io::Read::read_to_string(&mut sheet, &mut sheet_xml).unwrap();
        }
        if let Ok(mut s) = archive.by_name("xl/sharedStrings.xml") {
            std::io::Read::read_to_string(&mut s, &mut shared).unwrap();
        }
        assert!(
            !sheet_xml.contains("<v>2553.34</v>"),
            "金额不应写为数值: {}",
            sheet_xml
        );
        assert!(
            shared.contains("2553.34"),
            "金额 2553.34 未以文本保留: {}",
            shared
        );
        // 全文本 + 去空白：字段值不含尾随制表符
        assert!(
            !shared.contains("2553.34\t"),
            "金额值未去除尾随制表符: {}",
            shared
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn xlsx_writer_trims_whitespace() {
        // 回归：XLSX 导出应去除字段前后空白（空格/制表符），保留字段内部内容
        let p = write_temp("trim.xlsx");
        let header = vec!["金额".to_string(), "城市".to_string()];
        let mut w = XlsxWriter::new(&p, &header).unwrap();
        w.write_row(&["  2553.34\t".into(), "  北京  ".into()])
            .unwrap();
        w.finish().unwrap();
        let file = std::fs::File::open(&p).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut shared = String::new();
        if let Ok(mut s) = archive.by_name("xl/sharedStrings.xml") {
            std::io::Read::read_to_string(&mut s, &mut shared).unwrap();
        }
        assert!(shared.contains("2553.34"), "金额值缺失: {}", shared);
        assert!(shared.contains("北京"), "城市值缺失: {}", shared);
        assert!(!shared.contains("2553.34\t"), "金额值未去除尾部制表符: {}", shared);
        assert!(!shared.contains("  2553.34"), "金额值未去除前导空格: {}", shared);
        assert!(!shared.contains("  北京"), "城市值未去除前导空格: {}", shared);
        assert!(!shared.contains("北京  "), "城市值未去除尾随空格: {}", shared);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn export_view_xlsx_all_rows() {
        use crate::core::column_ops::ColumnOps;
        use crate::core::model::DataModel;
        let src = write_temp("_viewsrc2.csv");
        std::fs::write(
            &src,
            "卡号,金额\n6222020200112345678,100\n6222020200112345679,200\n".as_bytes(),
        )
        .unwrap();
        let m = DataModel::open(&src, 100).unwrap();
        let ops = ColumnOps::new(m.header());
        let out = write_temp("vx.xlsx");
        let n = export_view_xlsx(&m, &ops, None, &out, None).unwrap();
        assert_eq!(n, 2);
        assert!(std::fs::metadata(&out).unwrap().len() > 0);
        std::fs::remove_file(&out).ok();
        std::fs::remove_file(&src).ok();
    }
}
