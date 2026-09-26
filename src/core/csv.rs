//! CSV 解析状态机（字节级，与编码无关）。
//!
//! 设计要点：
//! - 分隔符/引号/换行均为 ASCII 字节；GBK/GB18030 等多字节编码的后续字节
//!   取值范围（0x40..=0xFE）不含 ASCII 控制字符，因此可在字节级安全解析，
//!   无需先转码。UTF-16 文件则需先整体转码为 UTF-8（见 reader.rs）。
//! - 行索引构建为单遍线性扫描（引号感知：引号内换行不计入行边界），
//!   时间复杂度 O(N)，900 万行约 6.5GB 数据耗时数秒。

/// 扫描 `data` 构建行偏移索引。
///
/// 返回 `Vec<u64>`：`lines[i]` 是第 i 行（含表头，0 起始）的起始字节偏移，
/// 末尾追加 `data.len()` 作为哨兵，因此行数 = `lines.len() - 1`。
/// 行边界规则：
/// - `\n` 换行；
/// - `\r\n` 与单独 `\r` 均视为换行（兼容旧 Mac 格式）；
/// - 双引号包裹的字段内换行/CR/LF 不计入行边界。
pub fn build_line_index(data: &[u8]) -> Vec<u64> {
    let mut lines: Vec<u64> = Vec::with_capacity(data.len() / 60 + 16);
    lines.push(0);
    let mut in_quotes = false;
    let mut i = 0usize;
    let n = data.len();
    while i < n {
        let b = data[i];
        if in_quotes {
            if b == b'"' {
                if i + 1 < n && data[i + 1] == b'"' {
                    i += 2; // 转义引号 ""
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
                    lines.push((i + 1) as u64);
                    i += 1;
                }
                b'\r' => {
                    if i + 1 < n && data[i + 1] == b'\n' {
                        i += 2;
                    } else {
                        i += 1;
                    }
                    lines.push(i as u64);
                }
                _ => {
                    i += 1;
                }
            }
        }
    }
    lines.push(n as u64);
    // 末尾换行符后无内容时不应产生额外空行：去掉重复的哨兵
    if lines.len() >= 2 && lines[lines.len() - 1] == lines[lines.len() - 2] {
        lines.pop();
    }
    lines
}

/// 解析一行（不含行尾换行符）为字段列表，返回各字段的字节内容。
///
/// 规则（RFC 4180 + 常见 CSV 实践）：
/// - 逗号分隔；
/// - 字段首字符为 `"` 时按引号字段解析，内部 `""` 转义为单个 `"`，
///   引号内可含逗号、换行、`\r`；
/// - 字段内首尾空白不做裁剪（与 Python pandas 默认一致，裁剪由上层策略决定）；
/// - 宽松处理：引号出现在字段中间时按普通字符处理（部分工具导出习惯）。
pub fn parse_record(line: &[u8]) -> Vec<Vec<u8>> {
    let mut fields: Vec<Vec<u8>> = Vec::with_capacity(16);
    let mut cur: Vec<u8> = Vec::with_capacity(32);
    let mut in_quotes = false;
    let mut i = 0usize;
    let n = line.len();
    while i < n {
        let b = line[i];
        if in_quotes {
            if b == b'"' {
                if i + 1 < n && line[i + 1] == b'"' {
                    cur.push(b'"');
                    i += 2;
                } else {
                    in_quotes = false;
                    i += 1;
                }
            } else {
                cur.push(b);
                i += 1;
            }
        } else if b == b'"' && cur.is_empty() {
            // 字段起始引号
            in_quotes = true;
            i += 1;
        } else if b == b',' {
            fields.push(std::mem::take(&mut cur));
            i += 1;
        } else {
            cur.push(b);
            i += 1;
        }
    }
    fields.push(cur);
    fields
}

/// 解析一行并把各字段内容依次写入复用缓冲 `scratch`，同时把每个字段的**结束偏移**
/// 记入 `ends`，返回字段数（≥1）。两个缓冲都会先被清空。
///
/// 与 [`parse_record`] 语义完全一致（引号字段、`""` 转义、字段中间引号按普通字符），
/// 区别是不分配 `Vec<Vec<u8>>`，供高频逐行路径（筛选/拆分/导出）复用缓冲使用。
/// 第 `i` 个字段的内容为 `scratch[ends[i-1]..ends[i]]`（`i == 0` 时起点为 0）。
pub fn parse_record_into(line: &[u8], scratch: &mut Vec<u8>, ends: &mut Vec<u32>) -> usize {
    scratch.clear();
    ends.clear();
    let n = line.len();
    let mut i = 0usize;
    // 当前字段在 scratch 中的起点：用于判断「字段起始引号」（等价于旧实现的 cur.is_empty()）
    let mut field_start = 0usize;
    let mut in_quotes = false;
    while i < n {
        let b = line[i];
        if in_quotes {
            if b == b'"' {
                if i + 1 < n && line[i + 1] == b'"' {
                    scratch.push(b'"');
                    i += 2;
                } else {
                    in_quotes = false;
                    i += 1;
                }
            } else {
                scratch.push(b);
                i += 1;
            }
        } else if b == b'"' && scratch.len() == field_start {
            in_quotes = true;
            i += 1;
        } else if b == b',' {
            ends.push(scratch.len() as u32);
            field_start = scratch.len();
            i += 1;
        } else {
            scratch.push(b);
            i += 1;
        }
    }
    ends.push(scratch.len() as u32);
    ends.len()
}

/// 解析一行文本形式的记录（便捷封装）。
pub fn parse_record_str(line: &str) -> Vec<String> {
    parse_record(line.as_bytes())
        .into_iter()
        .map(|f| String::from_utf8_lossy(&f).into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_fields() {
        let rec = parse_record("ID,交易卡号,交易账号".as_bytes());
        assert_eq!(rec.len(), 3);
        assert_eq!(&rec[0], b"ID");
        assert_eq!(&rec[1], "交易卡号".as_bytes());
        assert_eq!(&rec[2], "交易账号".as_bytes());
    }

    #[test]
    fn quoted_field_with_comma() {
        let rec = parse_record(b"a,\"b,c\",d");
        assert_eq!(rec.len(), 3);
        assert_eq!(&rec[1], b"b,c");
    }

    #[test]
    fn escaped_quotes() {
        let rec = parse_record(b"\"he said \"\"hi\"\"\",x");
        assert_eq!(rec.len(), 2);
        assert_eq!(&rec[0], b"he said \"hi\"");
    }

    #[test]
    fn empty_fields_and_trailing_comma() {
        let rec = parse_record(b"a,,c,");
        assert_eq!(rec.len(), 4);
        assert_eq!(&rec[1], b"");
        assert_eq!(&rec[3], b"");
    }

    #[test]
    fn quote_in_middle_is_literal() {
        // 宽松：字段中间出现的引号按普通字符
        let rec = parse_record(b"ab\"cd,e");
        assert_eq!(rec.len(), 2);
        assert_eq!(&rec[0], b"ab\"cd");
    }

    #[test]
    fn line_index_lf() {
        let data = b"a,b\nc,d\ne,f";
        let lines = build_line_index(data);
        assert_eq!(lines, vec![0, 4, 8, 11]);
        // 行内容
        assert_eq!(&data[lines[0] as usize..lines[1] as usize], b"a,b\n");
    }

    #[test]
    fn line_index_crlf_and_cr() {
        // "a,b\r\nc,d\re,f\n" 长度 13，共 3 行
        let data = b"a,b\r\nc,d\re,f\n";
        let lines = build_line_index(data);
        assert_eq!(lines, vec![0, 5, 9, 13]);
    }

    #[test]
    fn line_index_quoted_newline_not_counted() {
        // 引号字段内含换行：不应产生行边界。
        // 数据 "a,\"line1\nline2\",b\nc,d\n" 长度 22，共 2 个逻辑行
        let data = b"a,\"line1\nline2\",b\nc,d\n";
        let lines = build_line_index(data);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], 0);
        assert_eq!(lines[1], 18); // "a,\"line1\nline2\",b\n" 的长度
        assert_eq!(lines[2], 22);
    }

    #[test]
    fn line_index_empty_lines() {
        let data = b"a\n\nb\n";
        let lines = build_line_index(data);
        assert_eq!(lines, vec![0, 2, 3, 5]);
    }

    #[test]
    fn parse_record_handles_quoted_crlf_fields() {
        // 行索引把引号内 CRLF 当作字段内容
        let data = b"a,\"x\r\ny\",b";
        let lines = build_line_index(data);
        assert_eq!(lines.len(), 2); // 引号内 CRLF 不切行
        let line = &data[lines[0] as usize..lines[1] as usize];
        let rec = parse_record(line);
        assert_eq!(rec.len(), 3);
        assert_eq!(&rec[1], b"x\r\ny");
    }

    #[test]
    fn parse_record_into_matches_parse_record() {
        // 零分配版本必须与 parse_record 逐字段一致（含引号、转义、空字段、多行字段等边界）
        let cases: Vec<&[u8]> = vec![
            b"a,b,c",
            b"a,\"b,c\",d",
            b"\"he said \"\"hi\"\"\",x",
            b"a,,c,",
            b"ab\"cd,e",
            b"\"a\"\"b\",c",
            b"",
            b",,",
            b"\"\"",
            b"\"multi\nline\",x",
            b"\"\"\"\",\"\"",
            "中文字段,另一个".as_bytes(),
        ];
        let mut scratch: Vec<u8> = Vec::new();
        let mut ends: Vec<u32> = Vec::new();
        for line in cases {
            let expect = parse_record(line);
            let count = parse_record_into(line, &mut scratch, &mut ends);
            let shown = String::from_utf8_lossy(line);
            assert_eq!(count, expect.len(), "字段数不一致: {shown:?}");
            let mut prev = 0usize;
            for (i, field) in expect.iter().enumerate() {
                let end = ends[i] as usize;
                assert_eq!(
                    &scratch[prev..end],
                    &field[..],
                    "第 {i} 个字段不一致: {shown:?}"
                );
                prev = end;
            }
            assert_eq!(prev, scratch.len(), "字段边界未覆盖全部缓冲: {shown:?}");
        }
    }

    #[test]
    fn parse_record_into_reuses_buffers() {
        // 连续调用会清空并复用缓冲（不残留上一次的数据）
        let mut scratch: Vec<u8> = Vec::new();
        let mut ends: Vec<u32> = Vec::new();
        assert_eq!(parse_record_into(b"aaaa,bbbb", &mut scratch, &mut ends), 2);
        let n = parse_record_into(b"x", &mut scratch, &mut ends);
        assert_eq!(n, 1);
        assert_eq!(&scratch[..], b"x");
        assert_eq!(ends, vec![1]);
    }

    #[test]
    fn parse_record_str_utf8() {
        let rec = parse_record_str("姓名,城市");
        assert_eq!(rec[0], "姓名");
        assert_eq!(rec[1], "城市");
        let rec2 = parse_record_str("张三,北京");
        assert_eq!(rec2[1], "北京");
    }
}
