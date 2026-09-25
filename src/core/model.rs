//! 数据模型：列元数据、全文本类型、行访问器。
//!
//! 全文本模式：所有列一律视为文本（TypeKind::Text），避免数值类型影响导出
//! （长数字/前导零/大整数精度不丢失，保证数据完整）。
//! `classify` 保留用于诊断/测试，但不再参与导出类型判断。

use super::reader::{CsvFile, ColumnMeta, TypeKind};

/// 默认类型推断采样行数
pub const DEFAULT_SAMPLE_ROWS: usize = 1000;

/// 数据模型：持有 CSV 文件与列元数据
pub struct DataModel {
    file: CsvFile,
    cols: Vec<ColumnMeta>,
    /// 实际采样行数（文件行数不足时减少）
    sampled: usize,
}

impl DataModel {
    /// 打开 CSV 文件并推断列类型
    pub fn open(path: &str, sample_rows: usize) -> Result<Self, String> {
        let file = CsvFile::open(path)?;
        let cols = infer_types(&file, sample_rows);
        let sampled = sample_rows.min(file.row_count());
        Ok(Self { file, cols, sampled })
    }

    pub fn file(&self) -> &CsvFile {
        &self.file
    }

    pub fn columns(&self) -> &[ColumnMeta] {
        &self.cols
    }

    pub fn column_count(&self) -> usize {
        self.cols.len()
    }

    pub fn row_count(&self) -> usize {
        self.file.row_count()
    }

    pub fn header(&self) -> &[String] {
        self.file.header()
    }

    pub fn sampled_rows(&self) -> usize {
        self.sampled
    }

    pub fn encoding_label(&self) -> &'static str {
        self.file.encoding.label()
    }

    /// 访问字段（UTF-8 文本）
    pub fn field(&self, row: usize, col: usize) -> String {
        self.file.field(row, col)
    }

    /// 访问整行
    pub fn row(&self, row: usize) -> Vec<String> {
        self.file.row(row)
    }

    /// 访问整行（复用 `out` 容量，避免每行分配；用于大数据逐行遍历）
    pub fn row_into(&self, row: usize, out: &mut Vec<String>) {
        self.file.row_into(row, out);
    }
}

/// 对 CSV 文件执行列类型推断（采样）。
/// 全文本模式：所有列一律视为文本，避免数值类型影响导出（长数字/前导零/大整数精度不丢失）。
pub fn infer_types(_file: &CsvFile, _sample_rows: usize) -> Vec<ColumnMeta> {
    let header = _file.header();
    (0.._file.column_count())
        .map(|col| ColumnMeta {
            name: header.get(col).cloned().unwrap_or_default(),
            type_kind: TypeKind::Text,
        })
        .collect()
}

/// 字段值类型分类：
/// - 空 → None（由调用方处理）
/// - 整数（含正负，≤18 位）→ Int
/// - 浮点（含小数/指数）→ Float
/// - 其他（含 >18 位长数字）→ Text
pub fn classify(value: &str) -> TypeKind {
    let v = value.trim();
    if v.is_empty() {
        return TypeKind::Empty;
    }
    // 特殊非数值文本（f64 可解析但非用户意义上的数值）
    let lower = v.to_ascii_lowercase();
    if matches!(lower.as_str(), "nan" | "inf" | "+inf" | "-inf" | "infinity" | "+infinity" | "-infinity") {
        return TypeKind::Text;
    }
    let bytes = v.as_bytes();
    let is_numeric = !bytes.is_empty()
        && bytes
            .iter()
            .all(|&b| b.is_ascii_digit() || b == b'-' || b == b'+');
    if is_numeric {
        let digits: usize = bytes
            .iter()
            .filter(|&&b| b.is_ascii_digit())
            .count();
        // 单独的正负号（"-"、"+"）不是数字
        if digits == 0 {
            return TypeKind::Text;
        }
        // 位数 > 18 视为长数字：i64 可能溢出，且避免 Excel 精度问题 → 文本
        if digits > 18 {
            return TypeKind::Text;
        }
        return TypeKind::Int;
    }
    // 浮点：f64 可解析（小数、指数）
    if v.parse::<f64>().is_ok() {
        return TypeKind::Float;
    }
    TypeKind::Text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(data: &[u8], suffix: &str) -> String {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("csv_model_test_{}{}", std::process::id(), suffix));
        std::fs::write(&path, data).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn classify_values() {
        assert_eq!(classify(""), TypeKind::Empty);
        assert_eq!(classify("   "), TypeKind::Empty);
        assert_eq!(classify("123"), TypeKind::Int);
        assert_eq!(classify("-45"), TypeKind::Int);
        assert_eq!(classify("+7"), TypeKind::Int);
        assert_eq!(classify("3.14"), TypeKind::Float);
        assert_eq!(classify("-0.5"), TypeKind::Float);
        assert_eq!(classify("1e10"), TypeKind::Float);
        assert_eq!(classify("15270188000168848"), TypeKind::Int); // 17 位
        // 19 位卡号 → 长数字 → Text
        assert_eq!(classify("6222020200112345678"), TypeKind::Text);
        assert_eq!(classify("abc"), TypeKind::Text);
        assert_eq!(classify("北京"), TypeKind::Text);
        assert_eq!(classify("12abc"), TypeKind::Text);
        // review 修复：单独正负号与特殊浮点文本 → Text
        assert_eq!(classify("-"), TypeKind::Text);
        assert_eq!(classify("+"), TypeKind::Text);
        assert_eq!(classify("NaN"), TypeKind::Text);
        assert_eq!(classify("inf"), TypeKind::Text);
        assert_eq!(classify("Infinity"), TypeKind::Text);
    }

    #[test]
    fn infer_int_float_text_columns() {
        let p = write_temp(
            b"ID,SCORE,NAME,BIGID\n1,3.5,alice,6222020200112345678\n2,4.0,bob,6222020200112345679\n",
            "_types.csv",
        );
        let f = CsvFile::open(&p).unwrap();
        let cols = infer_types(&f, 100);
        assert_eq!(cols[0].type_kind, TypeKind::Text);
        assert_eq!(cols[1].type_kind, TypeKind::Text);
        assert_eq!(cols[2].type_kind, TypeKind::Text);
        assert_eq!(cols[3].type_kind, TypeKind::Text); // 长数字
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn infer_empty_column() {
        let p = write_temp(b"A,B,C\n1,,x\n2,,y\n", "_emptycol.csv");
        let f = CsvFile::open(&p).unwrap();
        let cols = infer_types(&f, 100);
        assert_eq!(cols[1].type_kind, TypeKind::Text);
        assert_eq!(cols[0].type_kind, TypeKind::Text);
        assert_eq!(cols[2].type_kind, TypeKind::Text);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn infer_mixed_becomes_text() {
        let p = write_temp(b"A\n1\ntwo\n3.5\n", "_mixed.csv");
        let f = CsvFile::open(&p).unwrap();
        let cols = infer_types(&f, 100);
        assert_eq!(cols[0].type_kind, TypeKind::Text);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn data_model_open() {
        let p = write_temp(b"ID,NAME\n1,alice\n2,bob\n", "_model.csv");
        let m = DataModel::open(&p, 100).unwrap();
        assert_eq!(m.row_count(), 2);
        assert_eq!(m.column_count(), 2);
        assert_eq!(m.field(1, 1), "bob");
        assert_eq!(m.columns()[0].type_kind, TypeKind::Text);
        assert_eq!(m.encoding_label(), "UTF-8");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn sample_rows_capped() {
        let p = write_temp(b"ID\n1\n2\n3\n", "_sample.csv");
        let m = DataModel::open(&p, 100).unwrap();
        assert_eq!(m.sampled_rows(), 3);
        std::fs::remove_file(&p).ok();
    }
}
