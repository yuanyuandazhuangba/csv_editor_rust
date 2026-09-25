//! 行哈希引擎：md5/sha1/sha256/sha512，批量并行计算。

use super::column_ops::HashAlgo;
use super::model::DataModel;

/// 计算字节哈希（hex 小写）
pub fn hash_bytes(data: &[u8], algo: HashAlgo) -> String {
    use md5::Md5;
    use sha1::Sha1;
    use sha2::{Digest, Sha256, Sha512};
    let digest: Vec<u8> = match algo {
        HashAlgo::Md5 => Md5::digest(data).to_vec(),
        HashAlgo::Sha1 => Sha1::digest(data).to_vec(),
        HashAlgo::Sha256 => Sha256::digest(data).to_vec(),
        HashAlgo::Sha512 => Sha512::digest(data).to_vec(),
    };
    digest.iter().map(|b| format!("{:02x}", b)).collect()
}

/// 计算单行的哈希（按所选列字段拼接后哈希）
pub fn compute_row_hash(
    model: &DataModel,
    row: usize,
    columns: &[usize],
    algo: HashAlgo,
) -> String {
    let mut acc = String::new();
    for &c in columns {
        acc.push_str(&model.field(row, c));
    }
    hash_bytes(acc.as_bytes(), algo)
}

/// 批量计算行哈希（并行分块），返回与行号对齐的哈希数组。
pub fn compute_row_hashes(
    model: &DataModel,
    columns: &[usize],
    algo: HashAlgo,
    progress: Option<&(dyn Fn(f64) + Sync)>,
) -> Vec<String> {
    let n = model.row_count();
    if n == 0 {
        return Vec::new();
    }
    let threads = std::thread::available_parallelism()
        .map(|x| x.get())
        .unwrap_or(4)
        .min(16);
    let chunk = (n / threads).max(1);
    let mut parts: Vec<(usize, Vec<String>)> = Vec::with_capacity(threads + 1);
    std::thread::scope(|s| {
        let mut handles = Vec::new();
        let mut start = 0usize;
        while start < n {
            let end = (start + chunk).min(n);
            let m = model;
            let cols = columns;
            let a = algo;
            handles.push(s.spawn(move || {
                let mut out = Vec::with_capacity(end - start);
                for row in start..end {
                    out.push(compute_row_hash(m, row, cols, a));
                }
                (start, out)
            }));
            start = end;
        }
        for h in handles {
            parts.push(h.join().unwrap_or_default());
        }
    });
    parts.sort_by_key(|(start, _)| *start);
    let mut out = Vec::with_capacity(n);
    for (i, (start, v)) in parts.into_iter().enumerate() {
        out.extend(v);
        if let Some(p) = progress {
            let done = (i + 1) * chunk;
            p((done as f64).min(n as f64) / n as f64);
        }
        let _ = start;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(data: &[u8], suffix: &str) -> String {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("csv_hash_test_{}_{}", std::process::id(), suffix));
        std::fs::write(&path, data).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn hash_bytes_known_vectors() {
        // RFC 1321 测试向量
        assert_eq!(
            hash_bytes(b"", HashAlgo::Md5),
            "d41d8cd98f00b204e9800998ecf8427e"
        );
        assert_eq!(
            hash_bytes(b"abc", HashAlgo::Md5),
            "900150983cd24fb0d6963f7d28e17f72"
        );
        // sha1("abc") = a9993e364706816aba3e25717850c26c9cd0d89d
        assert_eq!(
            hash_bytes(b"abc", HashAlgo::Sha1),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        // sha256("abc")
        assert_eq!(
            hash_bytes(b"abc", HashAlgo::Sha256),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // sha512("abc")
        assert_eq!(
            hash_bytes(b"abc", HashAlgo::Sha512),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
             2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
    }

    #[test]
    fn batch_hashes_match_serial() {
        let p = write_temp(b"ID,NAME\n1,alice\n2,bob\n3,carol\n", "_b.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let cols = vec![0, 1];
        let batch = compute_row_hashes(&m, &cols, HashAlgo::Sha256, None);
        assert_eq!(batch.len(), 3);
        for (row, h) in batch.iter().enumerate() {
            assert_eq!(*h, compute_row_hash(&m, row, &cols, HashAlgo::Sha256));
        }
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn empty_columns_hashes_empty_string() {
        let p = write_temp(b"ID\n1\n2\n", "_e.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let h = compute_row_hash(&m, 0, &[], HashAlgo::Md5);
        assert_eq!(h, hash_bytes(b"", HashAlgo::Md5));
        std::fs::remove_file(&p).ok();
    }
}
