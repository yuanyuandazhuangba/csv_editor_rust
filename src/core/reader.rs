//! CSV 文件读取器：mmap + 行偏移索引 + 惰性字段访问。
//!
//! 架构（内存占用 O(行数)≈8B/行，6.5GB/900 万行文件峰值内存 < 500MB）：
//! - 打开文件 → mmap（不载入内容，仅映射虚拟地址空间）；
//! - 检测编码（严格验证优先，见 encoding.rs）；
//! - 单遍扫描构建行偏移索引（引号感知，见 csv.rs::build_line_index）；
//! - 表头行解析为列名；
//! - 字段访问惰性：按 (行, 列) 取原始字节，仅当需要时解码为 UTF-8。
//!
//! UTF-16 说明：UTF-16 无法在字节级安全切分（分隔符为 0x00 2C），
//! 打开时整体转码为内存 UTF-8 再走统一路径（UTF-16 大文件场景极少，
//! 且转码后行索引一致）。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use memmap2::Mmap;

use super::csv::{build_line_index, parse_record};
use super::encoding::{detect_encoding, CsvEncoding};

/// 类型推断结果（供列元数据使用，见 model.rs）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeKind {
    /// 文本（含混合/超长）
    Text,
    /// 整数
    Int,
    /// 浮点
    Float,
    /// 全空（或几乎全空）
    Empty,
}

impl TypeKind {
    pub fn label(&self) -> &'static str {
        match self {
            TypeKind::Text => "文本",
            TypeKind::Int => "整数",
            TypeKind::Float => "浮点",
            TypeKind::Empty => "空",
        }
    }
}

/// 列元数据
#[derive(Debug, Clone)]
pub struct ColumnMeta {
    pub name: String,
    pub type_kind: TypeKind,
}

/// CSV 文件读取器
pub struct CsvFile {
    /// 数据视图：mmap（UTF-8/GB18030 字节级安全解析）或整体转码后的 UTF-8（UTF-16）
    data: MmapOrOwned,
    /// BOM 偏移（UTF-8 BOM 为 3，其余为 0）：行索引基于 data[data_offset..] 构建
    data_offset: usize,
    /// 原始文件编码
    pub encoding: CsvEncoding,
    /// 行起始偏移索引（末尾哨兵 = 数据长度）
    line_starts: Vec<u64>,
    /// 表头（UTF-8）
    header: Vec<String>,
    /// 列元数据
    cols: Vec<ColumnMeta>,
}

/// 数据视图：借用 mmap 或持有的 UTF-8 缓冲
enum MmapOrOwned {
    Mapped(Mmap),
    Owned(Vec<u8>),
}

impl std::ops::Deref for MmapOrOwned {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            MmapOrOwned::Mapped(m) => m,
            MmapOrOwned::Owned(v) => v,
        }
    }
}

impl CsvFile {
    /// 打开 CSV 文件：映射、编码检测、行索引构建、表头读取。
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let path = path.as_ref();
        let file = File::open(path).map_err(|e| format!("无法打开文件 {}: {}", path.display(), e))?;
        let file_size = file.metadata().map(|m| m.len()).unwrap_or(0) as usize;
        // 先映射以读取头部采样（编码检测只需前 64KB，不触发整文件页加载）
        let mmap = unsafe { Mmap::map(&file) }.map_err(|e| format!("内存映射失败: {}", e))?;

        let sample_len = file_size.min(65536);
        let encoding = detect_encoding(&mmap[..sample_len]);

        // UTF-8 BOM：行索引与访问基于 BOM 之后的数据
        let data_offset = if encoding == CsvEncoding::Utf8Bom { 3 } else { 0 };

        // UTF-16 需整体转码（字节级无法安全切分）；大数据量 UTF-16 场景极少
        let data = match encoding {
            CsvEncoding::Utf16Le | CsvEncoding::Utf16Be => {
                let enc = match encoding {
                    CsvEncoding::Utf16Le => encoding_rs::UTF_16LE,
                    _ => encoding_rs::UTF_16BE,
                };
                let (cow, _, _) = enc.decode(&mmap);
                MmapOrOwned::Owned(cow.into_owned().into_bytes())
            }
            _ => MmapOrOwned::Mapped(mmap),
        };

        // 行索引构建：分块顺序读文件（跨块维护引号/CR 状态），不触碰 mmap 页，
        // 避免把 6.5GB 文件整体拉入工作集（实测峰值内存可降低两个数量级）。
        let line_starts = if matches!(encoding, CsvEncoding::Utf16Le | CsvEncoding::Utf16Be) {
            build_line_index(&data[data_offset..])
        } else {
            let mut f = file;
            build_index_streaming(&mut f, data_offset as u64, 1 << 24)
                .map_err(|e| format!("读取文件失败: {}", e))?
        };
        if line_starts.len() < 2 {
            return Err("文件为空或没有可解析的行".to_string());
        }
        // 表头 = 第一行（去行尾换行）；从 mmap 惰性读取（仅触发首行所在页）
        let body: &[u8] = &data[data_offset..];
        let header_line = &body[line_starts[0] as usize..line_starts[1] as usize];
        let header_line = trim_line_ending(header_line);
        let header_raw = parse_record(header_line);
        if header_raw.is_empty() {
            return Err("无法解析表头".to_string());
        }
        // 表头列名清理：去除首尾空白/制表符（数据字段不做处理）
        let header: Vec<String> = header_raw
            .iter()
            .map(|f| decode_field(f, encoding).trim().to_string())
            .collect();

        // 初始列元数据：类型推断在 model.rs 中完善（此处填充 Text 占位，
        // 由 `infer_types` 方法根据采样覆盖）。
        let cols = header
            .iter()
            .map(|name| ColumnMeta {
                name: name.clone(),
                type_kind: TypeKind::Text,
            })
            .collect();

        Ok(Self {
            data,
            data_offset,
            encoding,
            line_starts,
            header,
            cols,
        })
    }

    /// 精确行数（不含表头）。
    pub fn row_count(&self) -> usize {
        self.line_starts.len().saturating_sub(2)
    }

    /// 表头列数
    pub fn column_count(&self) -> usize {
        self.header.len()
    }

    pub fn header(&self) -> &[String] {
        &self.header
    }

    pub fn columns(&self) -> &[ColumnMeta] {
        &self.cols
    }

    pub fn column_names(&self) -> Vec<&str> {
        self.header.iter().map(|s| s.as_str()).collect()
    }

    /// 取第 `row` 行（0 起始，不含表头）的原始字节（含行尾换行符）。
    pub fn row_bytes(&self, row: usize) -> &[u8] {
        if row + 1 >= self.line_starts.len() {
            return &[];
        }
        let start = self.data_offset + self.line_starts[row + 1] as usize;
        let end = self.data_offset + self.line_starts[row + 2] as usize;
        &self.data[start..end]
    }

    /// 取 (row, col) 字段的原始字节（无引号/转义，不裁剪空白）。
    pub fn field_bytes(&self, row: usize, col: usize) -> Option<Vec<u8>> {
        let raw = trim_line_ending(self.row_bytes(row));
        parse_record(raw).get(col).cloned()
    }

    /// 取 (row, col) 字段解码后的 UTF-8 文本。
    pub fn field(&self, row: usize, col: usize) -> String {
        self.field_bytes(row, col)
            .map(|f| decode_field(&f, self.encoding).into_owned())
            .unwrap_or_default()
    }

    /// 取整行字段（UTF-8 文本）。
    pub fn row(&self, row: usize) -> Vec<String> {
        let raw = self.row_bytes(row);
        let raw = trim_line_ending(raw);
        parse_record(raw)
            .iter()
            .map(|f| decode_field(f, self.encoding).into_owned())
            .collect()
    }

    /// 取整行字段（UTF-8 文本），复用 `out` 的容量避免每行字符串分配。
    /// 用于大数据量逐行遍历（如拆分），显著降低分配开销。
    pub fn row_into(&self, row: usize, out: &mut Vec<String>) {
        out.clear();
        let raw = self.row_bytes(row);
        let raw = trim_line_ending(raw);
        for f in parse_record(raw) {
            out.push(decode_field(&f, self.encoding).into_owned());
        }
    }

    /// 行数统计（并行近似，用于进度条）：分块统计换行符数量，不含引号语义。
    /// 精确行数以 `row_count()` 为准。用 thread::scope 借用数据，零拷贝。
    pub fn parallel_estimate_line_count(&self) -> usize {
        let data = &self.data[self.data_offset..];
        if data.is_empty() {
            return 0;
        }
        let thread_count = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(16);
        let chunk = (data.len() / thread_count).max(1 << 20); // 每块至少 1MB
        std::thread::scope(|s| {
            let mut handles = Vec::new();
            let mut offset = 0usize;
            while offset < data.len() {
                let end = (offset + chunk).min(data.len());
                let slice = &data[offset..end];
                handles.push(s.spawn(move || {
                    slice.iter().filter(|&&b| b == b'\n').count()
                }));
                offset = end;
            }
            handles.into_iter().map(|h| h.join().unwrap_or(0)).sum()
        })
    }
}

/// 逐行读取指定列文本的复用读取器（零分配）。
///
/// 供筛选这类「对每行反复取字段」的高频路径使用：
/// - **整行只解析一次**（`parse_record_into` 写入复用缓冲），同一行的多个条件共享结果；
/// - 所需列的文本写入复用 `String`（`String::clear` 保留容量），不产生每行分配。
pub struct RowReader {
    /// 解析后的字段内容（连续存放）
    scratch: Vec<u8>,
    /// 各字段的结束偏移
    ends: Vec<u32>,
    /// 需要读取的列（升序去重）与 `values` 一一对应
    need_cols: Vec<usize>,
    /// 与 `need_cols` 对应的字段文本（复用容量）
    values: Vec<String>,
    /// 已解析的行号（None = 尚无数据）
    parsed: Option<usize>,
}

impl RowReader {
    /// `need_cols` 为本次筛选用到的所有列（内部会排序去重）
    pub fn new(mut need_cols: Vec<usize>) -> Self {
        need_cols.sort_unstable();
        need_cols.dedup();
        let n = need_cols.len();
        Self {
            scratch: Vec::with_capacity(1024),
            ends: Vec::with_capacity(64),
            need_cols,
            values: (0..n).map(|_| String::with_capacity(64)).collect(),
            parsed: None,
        }
    }

    /// 载入某一行的所需列（该行已载入时直接返回）
    pub fn load(&mut self, file: &CsvFile, row: usize) {
        if self.parsed == Some(row) {
            return;
        }
        let raw = trim_line_ending(file.row_bytes(row));
        let count = super::csv::parse_record_into(raw, &mut self.scratch, &mut self.ends);
        for i in 0..self.need_cols.len() {
            let col = self.need_cols[i];
            let value = &mut self.values[i];
            value.clear();
            if col < count {
                let start = if col == 0 {
                    0usize
                } else {
                    self.ends[col - 1] as usize
                };
                let end = self.ends[col] as usize;
                decode_into(&self.scratch[start..end], file.encoding, value);
            }
        }
        self.parsed = Some(row);
    }

    /// 取某列的文本（列不在 `need_cols` 中时返回空串）
    pub fn value(&self, col: usize) -> &str {
        match self.need_cols.binary_search(&col) {
            Ok(i) => self.values[i].as_str(),
            Err(_) => "",
        }
    }
}

/// 把字段字节解码为 UTF-8 文本并追加到 `out`（复用 `out` 容量，不分配）
fn decode_into(bytes: &[u8], encoding: CsvEncoding, out: &mut String) {
    match encoding {
        CsvEncoding::Gb18030 => {
            let (cow, _, _) = encoding_rs::GB18030.decode(bytes);
            out.push_str(&cow);
        }
        // UTF-8 / UTF-8 BOM / UTF-16（打开时已整体转码为 UTF-8）可直接借用
        _ => out.push_str(&String::from_utf8_lossy(bytes)),
    }
}

/// 去掉行尾换行符（\r\n、\n、\r）。
fn trim_line_ending(mut line: &[u8]) -> &[u8] {
    while line.last() == Some(&b'\n') || line.last() == Some(&b'\r') {
        line = &line[..line.len() - 1];
    }
    line
}

/// 分块顺序读文件构建行偏移索引（引号感知），跨块维护引号状态与未决 `\r`。
///
/// 与内存版 `build_line_index` 语义一致，但通过 `File::read` 分块读取，
/// 不触发 mmap 页加载，避免大文件整体进入工作集。
/// `bom_offset`：数据视图起点（BOM 字节数），行偏移为视图内偏移。
fn build_index_streaming(
    file: &mut File,
    bom_offset: u64,
    block_size: usize,
) -> std::io::Result<Vec<u64>> {
    let mut lines: Vec<u64> = Vec::with_capacity(1 << 20);
    lines.push(0);
    file.seek(SeekFrom::Start(bom_offset))?;

    let mut buf = vec![0u8; block_size];
    let mut in_quotes = false;
    let mut base: u64 = 0; // 当前块在数据视图内的起始偏移
    let mut pending_cr = false; // 上一块末尾的 \r 待定
    let mut cr_offset: u64 = 0; // 待定 \r 的视图内偏移

    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            if pending_cr {
                // 文件尾：\r 独立换行
                lines.push(cr_offset + 1);
            }
            break;
        }
        let mut start = 0usize;
        if pending_cr {
            if buf[0] == b'\n' {
                // 上一块 \r + 本块 \n → \r\n 换行，起点在 \n 之后
                lines.push(cr_offset + 2);
                start = 1;
            } else {
                lines.push(cr_offset + 1);
                start = 0;
            }
            pending_cr = false;
        }
        // 块尾若为 \r，留待下一块判定（可能组成 \r\n）
        let mut end = n;
        if buf[n - 1] == b'\r' {
            end = n - 1;
            pending_cr = true;
            cr_offset = base + (n - 1) as u64;
        }
        let chunk = &buf[start..end];
        let mut i = 0usize;
        while i < chunk.len() {
            let b = chunk[i];
            if in_quotes {
                if b == b'"' {
                    if i + 1 < chunk.len() && chunk[i + 1] == b'"' {
                        i += 2;
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
                        lines.push(base + i as u64 + 1);
                        i += 1;
                    }
                    b'\r' => {
                        if i + 1 < chunk.len() && chunk[i + 1] == b'\n' {
                            i += 2;
                        } else {
                            i += 1;
                        }
                        lines.push(base + i as u64);
                    }
                    _ => {
                        i += 1;
                    }
                }
            }
        }
        base += n as u64;
    }

    lines.push(base);
    // 末尾换行后无内容：去掉重复哨兵
    if lines.len() >= 2 && lines[lines.len() - 1] == lines[lines.len() - 2] {
        lines.pop();
    }
    Ok(lines)
}

/// 按文件编码解码字段字节为 UTF-8（GB18030 等转码；UTF-8 直接借用）。
fn decode_field<'a>(bytes: &'a [u8], encoding: CsvEncoding) -> std::borrow::Cow<'a, str> {
    match encoding {
        CsvEncoding::Utf8 | CsvEncoding::Utf8Bom => String::from_utf8_lossy(bytes),
        CsvEncoding::Utf16Le | CsvEncoding::Utf16Be => String::from_utf8_lossy(bytes),
        CsvEncoding::Gb18030 => {
            let (cow, _, _) = encoding_rs::GB18030.decode(bytes);
            cow
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(data: &[u8], suffix: &str) -> String {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("csv_reader_test_{}{}", std::process::id(), suffix));
        std::fs::write(&path, data).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn remove(path: &str) {
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn open_utf8_csv() {
        let p = write_temp(b"ID,NAME\n1,alice\n2,bob\n", "_u8.csv");
        let f = CsvFile::open(&p).unwrap();
        assert_eq!(f.row_count(), 2);
        assert_eq!(f.column_count(), 2);
        assert_eq!(f.header(), &["ID".to_string(), "NAME".to_string()]);
        assert_eq!(f.field(0, 1), "alice");
        assert_eq!(f.row(1), vec!["2".to_string(), "bob".to_string()]);
        remove(&p);
    }

    #[test]
    fn open_gb18030_csv() {
        let (bytes, _, _) = encoding_rs::GB18030.encode("姓名,城市\n张三,北京\n李四,上海");
        let p = write_temp(&bytes, "_gb.csv");
        let f = CsvFile::open(&p).unwrap();
        assert_eq!(f.encoding, CsvEncoding::Gb18030);
        assert_eq!(f.header(), &["姓名".to_string(), "城市".to_string()]);
        assert_eq!(f.field(0, 1), "北京");
        assert_eq!(f.field(1, 0), "李四");
        remove(&p);
    }

    #[test]
    fn open_utf8_bom_csv() {
        let mut data = vec![0xEF, 0xBB, 0xBF];
        data.extend_from_slice("ID,NAME\n1,alice\n".as_bytes());
        let p = write_temp(&data, "_bom.csv");
        let f = CsvFile::open(&p).unwrap();
        assert_eq!(f.encoding, CsvEncoding::Utf8Bom);
        assert_eq!(f.header(), &["ID".to_string(), "NAME".to_string()]);
        assert_eq!(f.field(0, 1), "alice");
        remove(&p);
    }

    #[test]
    fn open_utf16le_csv() {
        let mut data = vec![0xFF, 0xFE];
        for ch in "ID,NAME\n1,alice\n".encode_utf16() {
            data.extend_from_slice(&ch.to_le_bytes());
        }
        let p = write_temp(&data, "_u16.csv");
        let f = CsvFile::open(&p).unwrap();
        assert_eq!(f.encoding, CsvEncoding::Utf16Le);
        assert_eq!(f.row_count(), 1);
        assert_eq!(f.header(), &["ID".to_string(), "NAME".to_string()]);
        assert_eq!(f.field(0, 1), "alice");
        remove(&p);
    }

    #[test]
    fn quoted_fields_with_commas() {
        let p = write_temp(b"a,b\n\"x,y\",z\n", "_q.csv");
        let f = CsvFile::open(&p).unwrap();
        assert_eq!(f.field(0, 0), "x,y");
        assert_eq!(f.field(0, 1), "z");
        remove(&p);
    }

    #[test]
    fn empty_file_errors() {
        let p = write_temp(b"", "_empty.csv");
        assert!(CsvFile::open(&p).is_err());
        remove(&p);
    }

    #[test]
    fn estimate_line_count_parallel() {
        let p = write_temp(b"ID,NAME\n1,a\n2,b\n3,c\n", "_est.csv");
        let f = CsvFile::open(&p).unwrap();
        assert_eq!(f.parallel_estimate_line_count(), 4);
        remove(&p);
    }

    #[test]
    fn crlf_file() {
        let p = write_temp(b"ID,NAME\r\n1,alice\r\n2,bob\r\n", "_crlf.csv");
        let f = CsvFile::open(&p).unwrap();
        assert_eq!(f.row_count(), 2);
        assert_eq!(f.field(1, 1), "bob");
        remove(&p);
    }

    #[test]
    fn header_names_trimmed() {
        // 表头列名去除首尾空白/制表符（数据字段不处理）
        let p = write_temp(
            "交易卡号\t, 交易账号 ,交易方户名\t\n1,2,3\n".as_bytes(),
            "_hdrtrim.csv",
        );
        let f = CsvFile::open(&p).unwrap();
        assert_eq!(f.header(), &["交易卡号".to_string(), "交易账号".to_string(), "交易方户名".to_string()]);
        // 数据字段保持原样（不 trim）
        let row = f.row(0);
        assert_eq!(row, vec!["1".to_string(), "2".to_string(), "3".to_string()]);
        remove(&p);
    }

    #[test]
    fn streaming_index_matches_memory_for_all_edge_cases() {
        // 覆盖：LF、CRLF、CR、引号内换行、引号转义、末尾无换行、末尾换行
        let cases: Vec<&[u8]> = vec![
            b"a,b\nc,d\ne,f\n",
            b"a,b\r\nc,d\re,f\n",
            b"a,\"line1\nline2\",b\nc,d\n",
            b"a,\"x\r\ny\",b",
            b"\"he said \"\"hi\"\"\",x\ny",
            b"a\n\nb\n",
            b"a,b",
            b"",
        ];
        for data in cases {
            let p = write_temp(data, "_stream.csv");
            let mut file = File::open(&p).unwrap();
            let mem = build_line_index(data);
            // 用极小块（3 字节）强制大量跨块边界
            let stream = build_index_streaming(&mut file, 0, 3).unwrap();
            assert_eq!(
                mem, stream,
                "数据 {:?}: 内存版 {:?} != 流式版 {:?}",
                String::from_utf8_lossy(data),
                mem,
                stream
            );
            remove(&p);
        }
    }

    #[test]
    fn streaming_index_with_bom_offset() {
        let data = b"\xEF\xBB\xBFa,b\nc,d\n";
        let p = write_temp(data, "_bomstream.csv");
        let mut file = File::open(&p).unwrap();
        let stream = build_index_streaming(&mut file, 3, 4).unwrap();
        // 视图内偏移：0=a, 4=\n 后=5? "a,b\n" 视图内 0..4
        assert_eq!(stream, vec![0, 4, 8]);
        remove(&p);
    }
}
