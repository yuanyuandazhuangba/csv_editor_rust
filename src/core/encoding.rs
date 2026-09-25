//! 编码检测与转码模块。
//!
//! 设计要点（修正 Python 1.3 版缺陷）：
//! - Python 版"chardet 猜测 + GB18030 优先"对 UTF-8 文件可能产生静默乱码
//!   （GB18030 几乎可解码任意字节序列，"能解码"≠"编码正确"）。
//! - 本模块改为"严格验证优先"：先 BOM、再严格 UTF-8 验证、再 UTF-16 启发式，
//!   最后 GB18030 兜底。UTF-8 是自校验编码，严格验证可靠，GBK/GB2312 文件
//!   必然通过不了 UTF-8 验证而正确落到 GB18030。
//! - 统一内部表示：所有数据解码为 UTF-8 字符串（Rust String）。

use encoding_rs::{CoderResult, Decoder, GB18030, UTF_16BE, UTF_16LE, UTF_8};

/// 检测到的 CSV 文件编码
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CsvEncoding {
    /// UTF-8 无 BOM（含纯 ASCII）
    Utf8,
    /// UTF-8 带 BOM（EF BB BF）
    Utf8Bom,
    /// UTF-16 LE（FF FE）
    Utf16Le,
    /// UTF-16 BE（FE FF）
    Utf16Be,
    /// GB18030（超集，覆盖 GBK / GB2312，即常见"ANSI"中文文件）
    Gb18030,
}

impl CsvEncoding {
    /// 人类可读标签（用于 UI/日志）
    pub fn label(&self) -> &'static str {
        match self {
            CsvEncoding::Utf8 => "UTF-8",
            CsvEncoding::Utf8Bom => "UTF-8 (BOM)",
            CsvEncoding::Utf16Le => "UTF-16 LE",
            CsvEncoding::Utf16Be => "UTF-16 BE",
            CsvEncoding::Gb18030 => "GB18030",
        }
    }

    /// 创建对应编码的流式解码器
    fn new_decoder(&self) -> Decoder {
        match self {
            CsvEncoding::Utf8 => UTF_8.new_decoder_without_bom_handling(),
            CsvEncoding::Utf8Bom => UTF_8.new_decoder(),
            CsvEncoding::Utf16Le => UTF_16LE.new_decoder(),
            CsvEncoding::Utf16Be => UTF_16BE.new_decoder(),
            CsvEncoding::Gb18030 => GB18030.new_decoder(),
        }
    }
}

/// 严格验证 `data` 是否为合法 UTF-8（无 BOM 处理）。
/// UTF-8 是自校验编码：非法字节序列立即报错，因此该判定是可靠的。
fn is_valid_utf8(data: &[u8]) -> bool {
    let mut dec = UTF_8.new_decoder_without_bom_handling();
    let mut out = String::with_capacity(data.len().min(65536) + 16);
    let (res, _, had_errors) = dec.decode_to_string(data, &mut out, true);
    if had_errors {
        return false;
    }
    matches!(res, CoderResult::InputEmpty)
}

/// UTF-16 无 BOM 启发式：统计偶数/奇数位置出现 0x00 的比例。
fn looks_like_utf16(data: &[u8]) -> Option<CsvEncoding> {
    let n = data.len().min(1024);
    if n < 32 {
        return None;
    }
    let (mut even_null, mut odd_null) = (0usize, 0usize);
    for (i, &b) in data[..n].iter().enumerate() {
        if b == 0 {
            if i % 2 == 0 {
                even_null += 1;
            } else {
                odd_null += 1;
            }
        }
    }
    let half = n / 2;
    // 若偶数位有大量 0x00 而奇数位几乎没有 → UTF-16LE（ASCII 文本场景）
    if even_null as f64 / half as f64 > 0.3 && (odd_null as f64 / half as f64) < 0.05 {
        return Some(CsvEncoding::Utf16Le);
    }
    if odd_null as f64 / half as f64 > 0.3 && (even_null as f64 / half as f64) < 0.05 {
        return Some(CsvEncoding::Utf16Be);
    }
    None
}

/// 检测文件编码。
///
/// 优先级：BOM → 严格 UTF-8 → UTF-16 启发式 → GB18030 兜底。
/// 传入数据建议取文件开头一段（如前 64KB，足以覆盖表头与多字节首字符）。
pub fn detect_encoding(data: &[u8]) -> CsvEncoding {
    if data.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return CsvEncoding::Utf8Bom;
    }
    if data.starts_with(&[0xFF, 0xFE]) {
        return CsvEncoding::Utf16Le;
    }
    if data.starts_with(&[0xFE, 0xFF]) {
        return CsvEncoding::Utf16Be;
    }
    // 严格 UTF-8 验证（关键：UTF-8 文件绝不落到 GB18030）
    if is_valid_utf8(data) {
        return CsvEncoding::Utf8;
    }
    if let Some(enc) = looks_like_utf16(data) {
        return enc;
    }
    // GBK / GB2312 / 其他 ANSI 中文 → GB18030 超集兜底
    CsvEncoding::Gb18030
}

/// 流式转码器：将源编码字节流逐块解码为 UTF-8 字符串。
///
/// 用法：`transcoder.push(&bytes, /* last = */ false)` 连续喂入分块数据，
/// 最后一块传 `last = true` 刷新解码器状态。
pub struct Transcoder {
    dec: Decoder,
    buf: String,
}

impl Transcoder {
    pub fn new(enc: CsvEncoding) -> Self {
        Self {
            dec: enc.new_decoder(),
            buf: String::new(),
        }
    }

    /// 解码一块数据，返回追加到内部缓冲后产出的 UTF-8 文本（所有权借用）。
    /// 每次调用会清空内部输出缓冲，因此调用方应立即消费返回值。
    pub fn push(&mut self, data: &[u8], last: bool) -> &str {
        self.buf.clear();
        // decode_to_string 按 String 容量写入，容量不足时输出截断（OutputFull）。
        // 预留在 2× 输入以上：GB18030/UTF-16 → UTF-8 输出长度 ≤ 2× 输入。
        self.buf.reserve(data.len().saturating_mul(2) + 32);
        let _ = self.dec.decode_to_string(data, &mut self.buf, last);
        &self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gb18030_bytes(s: &str) -> Vec<u8> {
        // 用 encoding_rs 把 UTF-8 字符串编码为 GB18030 字节
        let (bytes, _, _) = encoding_rs::GB18030.encode(s);
        bytes.into_owned()
    }

    #[test]
    fn detect_utf8_chinese() {
        let data = "交易卡号,交易账号\n123,456".as_bytes();
        assert_eq!(detect_encoding(data), CsvEncoding::Utf8);
    }

    #[test]
    fn detect_utf8_bom() {
        let mut data = vec![0xEF, 0xBB, 0xBF];
        data.extend_from_slice("交易卡号".as_bytes());
        assert_eq!(detect_encoding(&data), CsvEncoding::Utf8Bom);
    }

    #[test]
    fn detect_utf8_ascii() {
        let data = b"ID,NAME\n1,alice";
        assert_eq!(detect_encoding(data), CsvEncoding::Utf8);
    }

    #[test]
    fn detect_gb18030_chinese() {
        // GB18030 编码的中文文件：UTF-8 严格验证必然失败 → Gb18030
        let data = gb18030_bytes("交易卡号,交易账号\n15270188000168848,\"1\"");
        assert_eq!(detect_encoding(&data), CsvEncoding::Gb18030);
    }

    #[test]
    fn detect_gbk_chinese() {
        // GBK 编码（GB18030 超集覆盖）
        let (bytes, _, _) = encoding_rs::GBK.encode("姓名,年龄\n张三,30");
        assert_eq!(detect_encoding(&bytes), CsvEncoding::Gb18030);
    }

    #[test]
    fn detect_utf16_le_bom() {
        let mut data = vec![0xFF, 0xFE];
        data.extend_from_slice(&[0x2E, 0x4E, 0xE4, 0x8B]); // "交" "易" 的 UTF-16LE 字节
        assert_eq!(detect_encoding(&data), CsvEncoding::Utf16Le);
    }

    #[test]
    fn detect_utf16_be_bom() {
        let mut data = vec![0xFE, 0xFF];
        data.extend_from_slice(&[0x4E, 0x2E, 0x8B, 0xE4]);
        assert_eq!(detect_encoding(&data), CsvEncoding::Utf16Be);
    }

    #[test]
    fn utf8_file_not_misdetected_as_gb18030() {
        // 回归：Python 版曾把 UTF-8 文件误判为 GB18030 产生乱码。
        // 用真实 jymxb.csv 风格表头验证 UTF-8 文件一定被识别为 Utf8。
        let header = "ID,交易卡号,交易账号,交易方户名,交易方证件号码,交易时间,交易金额,交易余额,收付标志,交易对手账卡号,现金标志,对手户名,对手身份证号,对手开户银行,摘要说明,交易币种,交易网点名称,交易发生地,交易是否成功,传票号,IP地址,MAC地址,对手交易余额,交易流水号,日志号,凭证种类,凭证号,交易柜员号,备注,查询反馈结果原因,数据来源_压缩包,数据来源_文件路径,数据来源_行号,数据来源_银行名称,数据来源_银行分类,行MD5_值,无效标签_新增,重复标签_新增,冲正标记_新增,交易卡号_新增,交易账号_新增,交易方户名_新增,交易方证件号码_新增,账户类型_新增,账号开户银行_新增,账户状态_新增,交易日期_新增,交易时点_新增,冲正组指纹_新增,交易方标记_新增,对手方标记_新增,案件名_新增,批次号_新增,批次名_新增,清洗日期_新增".as_bytes();
        assert_eq!(detect_encoding(header), CsvEncoding::Utf8);
    }

    #[test]
    fn transcode_gb18030_to_utf8() {
        let bytes = gb18030_bytes("姓名,城市\n张三,北京");
        let mut tr = Transcoder::new(CsvEncoding::Gb18030);
        let out = tr.push(&bytes, true);
        assert_eq!(out, "姓名,城市\n张三,北京");
    }

    #[test]
    fn transcode_chunked_matches_whole() {
        // 分块喂入与整块喂入结果一致（覆盖跨块多字节字符）
        let bytes = gb18030_bytes("交易卡号,交易账号\n15270188000168848,\"测试\"");
        let mut tr = Transcoder::new(CsvEncoding::Gb18030);
        let mut acc = String::new();
        for chunk in bytes.chunks(7) {
            acc.push_str(tr.push(chunk, false));
        }
        acc.push_str(tr.push(&[], true));
        let (whole, _, _) = encoding_rs::GB18030.decode(&bytes);
        assert_eq!(acc, whole);
    }

    #[test]
    fn malformed_utf8_falls_back() {
        // 含非法 UTF-8 序列（如 0xE0 单独出现）→ 不是 UTF-8
        let data = [b'A', b'B', 0xE0, 0x81, b'C'];
        assert!(!is_valid_utf8(&data));
        // 检测结果应落为 GB18030 而非 UTF-8
        assert_eq!(detect_encoding(&data), CsvEncoding::Gb18030);
    }
}
