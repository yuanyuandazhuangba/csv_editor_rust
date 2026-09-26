//! 列操作引擎：拆分字段/合并字段/复制列/插入列/删除列/隐藏列/行哈希。
//!
//! 设计（修正 Python 版缺陷）：
//! - Python 版每次操作 `df.copy(deep=True)` 深拷贝整表 + 20 步历史，
//!   对 6.5GB 数据单次拷贝即 OOM → 本引擎以**操作描述（派生列）**记录，
//!   零复制零物化；字段在访问时按需计算；
//! - undo/redo 为命令式栈：undo 弹出操作并反向执行，redo 重放，同样零复制；
//! - 删除/隐藏为视图层操作（不物理重写 900 万行数据）。

use std::collections::HashSet;

use super::hash::hash_bytes;
use super::model::DataModel;

/// 哈希算法
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgo {
    Md5,
    Sha1,
    Sha256,
    Sha512,
}

impl HashAlgo {
    pub fn label(&self) -> &'static str {
        match self {
            HashAlgo::Md5 => "md5",
            HashAlgo::Sha1 => "sha1",
            HashAlgo::Sha256 => "sha256",
            HashAlgo::Sha512 => "sha512",
        }
    }

    pub fn from_label(s: &str) -> Option<HashAlgo> {
        match s.to_lowercase().as_str() {
            "md5" => Some(HashAlgo::Md5),
            "sha1" => Some(HashAlgo::Sha1),
            "sha256" => Some(HashAlgo::Sha256),
            "sha512" => Some(HashAlgo::Sha512),
            _ => None,
        }
    }
}

/// 派生列计算方式（新列的惰性数据来源）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DerivedColumn {
    /// 拆分源列的第 part 段（按分隔符，n 段时最多拆 n-1 次）
    SplitField {
        source: usize,
        part: usize,
        separator: String,
    },
    /// 多列拼接
    Merge {
        sources: Vec<usize>,
        separator: String,
    },
    /// 复制列
    Copy { source: usize },
    /// 常量列
    Constant { value: String },
    /// 行哈希
    Hash {
        sources: Vec<usize>,
        algorithm: HashAlgo,
    },
}

/// 视图列引用：原始列或派生列
#[derive(Debug, Clone)]
pub enum ColumnRef {
    Base(usize),
    Derived(DerivedColumn),
}

/// 列操作（记录用于 undo/redo）
#[derive(Debug, Clone)]
pub enum ColAction {
    /// 添加列（position: Some(i) 表示插入到视图列 i 之前；None = 末尾）
    AddColumn {
        name: String,
        derived: DerivedColumn,
        position: Option<usize>,
    },
    /// 删除列（保存被删列的引用以便 undo 恢复）
    RemoveColumn { index: usize, name: String, col_ref: ColumnRef },
    /// 隐藏/显示切换
    ToggleHidden { index: usize },
}

/// 列操作引擎
#[derive(Clone)]
pub struct ColumnOps {
    /// 视图列名（原始列 + 派生列，删除列后移除）
    header: Vec<String>,
    /// 视图列引用
    refs: Vec<ColumnRef>,
    /// 隐藏的视图列索引
    hidden: HashSet<usize>,
    /// undo 栈
    undo_stack: Vec<ColAction>,
    /// redo 栈
    redo_stack: Vec<ColAction>,
}

impl ColumnOps {
    /// 基于原始表头创建
    pub fn new(header: &[String]) -> Self {
        Self {
            header: header.to_vec(),
            refs: (0..header.len()).map(ColumnRef::Base).collect(),
            hidden: HashSet::new(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        }
    }

    pub fn header(&self) -> &[String] {
        &self.header
    }

    pub fn column_count(&self) -> usize {
        self.header.len()
    }

    pub fn is_hidden(&self, vcol: usize) -> bool {
        self.hidden.contains(&vcol)
    }

    pub fn visible_columns(&self) -> Vec<usize> {
        (0..self.header.len())
            .filter(|i| !self.hidden.contains(i))
            .collect()
    }

    /// 计算视图列 (row, vcol) 的字段值（惰性）
    pub fn field(&self, model: &DataModel, row: usize, vcol: usize) -> String {
        let Some(r) = self.refs.get(vcol) else {
            return String::new();
        };
        match r {
            ColumnRef::Base(i) => model.field(row, *i),
            ColumnRef::Derived(d) => self.compute_derived(model, row, d),
        }
    }

    /// 计算整行视图字段
    pub fn row(&self, model: &DataModel, row: usize) -> Vec<String> {
        self.visible_columns()
            .into_iter()
            .map(|c| self.field(model, row, c))
            .collect()
    }

    /// 视图列 `vcol` 对应的原始数据列索引；派生列返回 None。
    /// 用于把 UI 选择的视图列映射为原始列（按字段拆分、列操作的数据源）。
    pub fn view_to_base(&self, vcol: usize) -> Option<usize> {
        match self.refs.get(vcol) {
            Some(ColumnRef::Base(i)) => Some(*i),
            _ => None,
        }
    }

    fn compute_derived(&self, model: &DataModel, row: usize, d: &DerivedColumn) -> String {
        match d {
            DerivedColumn::SplitField {
                source,
                part,
                separator,
            } => {
                let v = model.field(row, *source);
                let parts: Vec<&str> = if separator.is_empty() {
                    v.split_whitespace().collect()
                } else {
                    v.split(separator).collect()
                };
                parts
                    .get(*part)
                    .copied()
                    .unwrap_or("")
                    .trim()
                    .to_string()
            }
            DerivedColumn::Merge { sources, separator } => {
                let parts: Vec<String> = sources
                    .iter()
                    .map(|&i| model.field(row, i))
                    .collect();
                parts.join(separator)
            }
            DerivedColumn::Copy { source } => model.field(row, *source),
            DerivedColumn::Constant { value } => value.clone(),
            DerivedColumn::Hash { sources, algorithm } => {
                let mut acc = String::new();
                for &i in sources {
                    acc.push_str(&model.field(row, i));
                }
                hash_bytes(acc.as_bytes(), *algorithm)
            }
        }
    }

    // ===== 列操作（自动记录 undo）=====

    /// 添加列（派生列）
    pub fn add_column(
        &mut self,
        name: &str,
        derived: DerivedColumn,
        position: Option<usize>,
    ) -> Result<(), String> {
        let name = unique_name(&self.header, name);
        let pos = position.unwrap_or(self.header.len()).min(self.header.len());
        self.header.insert(pos, name.clone());
        self.refs.insert(pos, ColumnRef::Derived(derived.clone()));
        // 隐藏索引位移
        shift_hidden(&mut self.hidden, pos, 1);
        self.undo_stack.push(ColAction::AddColumn {
            name,
            derived,
            position: Some(pos),
        });
        self.redo_stack.clear();
        Ok(())
    }

    /// 拆分字段：按分隔符把源列拆成 num_cols 个新列（追加末尾）
    pub fn split_field(
        &mut self,
        source: usize,
        separator: &str,
        num_cols: usize,
        prefix: &str,
    ) -> Result<(), String> {
        if source >= self.header.len() {
            return Err("源列不存在".to_string());
        }
        for i in 0..num_cols {
            self.add_column(
                &format!("{}{}", prefix, i + 1),
                DerivedColumn::SplitField {
                    source,
                    part: i,
                    separator: separator.to_string(),
                },
                None,
            )?;
        }
        Ok(())
    }

    /// 合并字段
    pub fn merge_fields(
        &mut self,
        sources: &[usize],
        new_name: &str,
        separator: &str,
    ) -> Result<(), String> {
        if sources.is_empty() {
            return Err("请选择要合并的字段".to_string());
        }
        self.add_column(
            new_name,
            DerivedColumn::Merge {
                sources: sources.to_vec(),
                separator: separator.to_string(),
            },
            None,
        )
    }

    /// 复制列
    pub fn copy_column(&mut self, source: usize, new_name: &str) -> Result<(), String> {
        self.add_column(new_name, DerivedColumn::Copy { source }, None)
    }

    /// 插入常量列
    pub fn insert_column(
        &mut self,
        new_name: &str,
        position: InsertPosition,
        default_value: &str,
    ) -> Result<(), String> {
        let pos = match position {
            InsertPosition::Start => Some(0),
            InsertPosition::End => None,
            InsertPosition::Before(ref_col) => Some(ref_col),
        };
        self.add_column(
            new_name,
            DerivedColumn::Constant {
                value: default_value.to_string(),
            },
            pos,
        )
    }

    /// 删除列（视图层移除）
    pub fn delete_columns(&mut self, indices: &[usize]) -> Result<(), String> {
        let mut idxs: Vec<usize> = indices.to_vec();
        idxs.sort_unstable();
        idxs.dedup();
        // 从大到小删除（索引稳定）
        for &i in idxs.iter().rev() {
            if i >= self.header.len() {
                return Err(format!("列索引 {} 不存在", i));
            }
            let name = self.header.remove(i);
            let col_ref = self.refs.remove(i);
            self.hidden.remove(&i);
            shift_hidden(&mut self.hidden, i, -1);
            self.undo_stack.push(ColAction::RemoveColumn {
                index: i,
                name,
                col_ref,
            });
        }
        self.redo_stack.clear();
        Ok(())
    }

    /// 隐藏列
    pub fn hide_columns(&mut self, indices: &[usize]) -> Result<(), String> {
        for &i in indices {
            if i >= self.header.len() {
                continue;
            }
            if !self.hidden.contains(&i) {
                self.hidden.insert(i);
                self.undo_stack.push(ColAction::ToggleHidden { index: i });
            }
        }
        self.redo_stack.clear();
        Ok(())
    }

    /// 显示列
    pub fn show_columns(&mut self, indices: &[usize]) -> Result<(), String> {
        for &i in indices {
            if i >= self.header.len() {
                continue;
            }
            if self.hidden.contains(&i) {
                self.hidden.remove(&i);
                self.undo_stack.push(ColAction::ToggleHidden { index: i });
            }
        }
        self.redo_stack.clear();
        Ok(())
    }

    /// 生成行哈希列
    pub fn generate_hash(
        &mut self,
        sources: &[usize],
        hash_column: &str,
        algorithm: HashAlgo,
    ) -> Result<(), String> {
        self.add_column(
            hash_column,
            DerivedColumn::Hash {
                sources: sources.to_vec(),
                algorithm,
            },
            None,
        )
    }

    // ===== undo / redo（命令式，零复制）=====

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    pub fn undo(&mut self) -> Result<(), String> {
        let Some(action) = self.undo_stack.pop() else {
            return Err("没有可撤销的操作".to_string());
        };
        self.apply_reverse(&action)?;
        self.redo_stack.push(action);
        Ok(())
    }

    pub fn redo(&mut self) -> Result<(), String> {
        let Some(action) = self.redo_stack.pop() else {
            return Err("没有可恢复的操作".to_string());
        };
        self.apply_forward(&action)?;
        self.undo_stack.push(action);
        Ok(())
    }

    fn apply_reverse(&mut self, action: &ColAction) -> Result<(), String> {
        match action {
            ColAction::AddColumn { position, .. } => {
                let pos = position.unwrap_or(self.header.len().saturating_sub(1));
                if pos < self.header.len() {
                    self.header.remove(pos);
                    self.refs.remove(pos);
                    self.hidden.remove(&pos);
                    shift_hidden(&mut self.hidden, pos, -1);
                }
            }
            ColAction::RemoveColumn {
                index, name, col_ref,
            } => {
                let pos = (*index).min(self.header.len());
                self.header.insert(pos, name.clone());
                self.refs.insert(pos, col_ref.clone());
                shift_hidden(&mut self.hidden, pos, 1);
            }
            ColAction::ToggleHidden { index } => {
                if self.hidden.contains(index) {
                    self.hidden.remove(index);
                } else {
                    self.hidden.insert(*index);
                }
            }
        }
        Ok(())
    }

    fn apply_forward(&mut self, action: &ColAction) -> Result<(), String> {
        match action {
            ColAction::AddColumn {
                name,
                derived,
                position,
            } => {
                let pos = position.unwrap_or(self.header.len()).min(self.header.len());
                self.header.insert(pos, name.clone());
                self.refs
                    .insert(pos, ColumnRef::Derived(derived.clone()));
                shift_hidden(&mut self.hidden, pos, 1);
            }
            ColAction::RemoveColumn { index, .. } => {
                if *index < self.header.len() {
                    self.header.remove(*index);
                    self.refs.remove(*index);
                    self.hidden.remove(index);
                    shift_hidden(&mut self.hidden, *index, -1);
                }
            }
            ColAction::ToggleHidden { index } => {
                if self.hidden.contains(index) {
                    self.hidden.remove(index);
                } else {
                    self.hidden.insert(*index);
                }
            }
        }
        Ok(())
    }
}

/// 插入位置
#[derive(Debug, Clone, Copy)]
pub enum InsertPosition {
    Start,
    End,
    Before(usize),
}

/// 生成唯一列名（重名加 _N 后缀）
fn unique_name(header: &[String], base: &str) -> String {
    if !header.iter().any(|h| h == base) {
        return base.to_string();
    }
    let mut i = 1;
    loop {
        let candidate = format!("{}_{}", base, i);
        if !header.iter().any(|h| h == &candidate) {
            return candidate;
        }
        i += 1;
    }
}

/// 隐藏索引集合位移（插入/删除列时维护）
fn shift_hidden(hidden: &mut HashSet<usize>, from: usize, delta: i32) {
    let shifted: Vec<usize> = hidden
        .iter()
        .filter_map(|&i| {
            if delta >= 0 {
                if i >= from {
                    Some(i + delta as usize)
                } else {
                    Some(i)
                }
            } else if i > from {
                Some(i.saturating_sub((-delta) as usize))
            } else {
                Some(i)
            }
        })
        .collect();
    hidden.clear();
    hidden.extend(shifted);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::DataModel;

    fn write_temp(data: &[u8], suffix: &str) -> String {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("csv_colops_test_{}_{}", std::process::id(), suffix));
        std::fs::write(&path, data).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn model() -> DataModel {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = write_temp(
            b"ID,NAME,AMOUNT\n1,alice,100\n2,bob,200\n",
            &format!("_m{}.csv", id),
        );
        let m = DataModel::open(&p, 100).unwrap();
        std::fs::remove_file(&p).ok();
        m
    }

    #[test]
    fn split_field_column() {
        let m = model();
        let header = m.header().to_vec();
        let mut ops = ColumnOps::new(&header);
        // 拆 NAME：alice → 无空格；用 split 测试 "a b" 风格更直接
        ops.split_field(1, " ", 2, "name_part_").unwrap();
        assert_eq!(ops.column_count(), 5);
        assert_eq!(ops.header()[3], "name_part_1");
        assert_eq!(ops.field(&m, 0, 3), "alice");
        assert_eq!(ops.field(&m, 0, 4), "");
    }

    #[test]
    fn split_field_with_separator() {
        let p = write_temp(b"TS\n2021-12-30 00:00:00\n2022-01-01 12:30:00\n", "_ts.csv");
        let m = DataModel::open(&p, 100).unwrap();
        let mut ops = ColumnOps::new(m.header());
        ops.split_field(0, " ", 2, "ts_").unwrap();
        assert_eq!(ops.field(&m, 0, 1), "2021-12-30");
        assert_eq!(ops.field(&m, 0, 2), "00:00:00");
        assert_eq!(ops.field(&m, 1, 2), "12:30:00");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn merge_fields() {
        let m = model();
        let mut ops = ColumnOps::new(m.header());
        ops.merge_fields(&[1, 2], "merged", "_").unwrap();
        let v = ops.field(&m, 0, 3);
        assert_eq!(v, "alice_100");
    }

    #[test]
    fn copy_and_constant_insert() {
        let m = model();
        let mut ops = ColumnOps::new(m.header());
        ops.copy_column(1, "name2").unwrap();
        assert_eq!(ops.field(&m, 1, 3), "bob");
        ops.insert_column("flag", InsertPosition::Start, "X").unwrap();
        assert_eq!(ops.header()[0], "flag");
        assert_eq!(ops.field(&m, 0, 0), "X");
        // 原列右移
        assert_eq!(ops.field(&m, 0, 1), "1");
    }

    #[test]
    fn delete_columns_view_only() {
        let m = model();
        let mut ops = ColumnOps::new(m.header());
        ops.delete_columns(&[2]).unwrap(); // 删 AMOUNT
        assert_eq!(ops.column_count(), 2);
        assert_eq!(ops.header(), &["ID".to_string(), "NAME".to_string()]);
        // 原始数据不受影响
        assert_eq!(ops.field(&m, 0, 1), "alice");
    }

    #[test]
    fn hide_show_columns() {
        let m = model();
        let mut ops = ColumnOps::new(m.header());
        ops.hide_columns(&[2]).unwrap();
        assert!(ops.is_hidden(2));
        assert_eq!(ops.visible_columns(), vec![0, 1]);
        assert_eq!(ops.row(&m, 0), vec!["1".to_string(), "alice".to_string()]);
        ops.show_columns(&[2]).unwrap();
        assert_eq!(ops.visible_columns(), vec![0, 1, 2]);
    }

    #[test]
    fn delete_derived_hash_columns_all_algorithms() {
        // 回归：4 种算法生成的哈希列都应能正常删除（曾出现「MD5 哈希列删不掉、
        // 报列索引不存在」的报告——那是界面把陈旧的列索引原样提交所致，
        // 核心层对各算法一视同仁；这里把这条语义固定下来）。
        let m = model();
        let base = m.column_count();
        for (i, algo) in [HashAlgo::Md5, HashAlgo::Sha1, HashAlgo::Sha256, HashAlgo::Sha512]
            .into_iter()
            .enumerate()
        {
            let mut ops = ColumnOps::new(m.header());
            let name = format!("hash_{i}");
            ops.generate_hash(&[0], &name, algo).unwrap();
            assert_eq!(ops.column_count(), base + 1, "生成后应多一列");
            // 新列是派生列（无对应原始列）
            assert_eq!(ops.view_to_base(base), None);
            // 删除刚生成的哈希列（位于末尾）
            ops.delete_columns(&[base]).unwrap();
            assert_eq!(ops.column_count(), base, "哈希列应可删除");
            // 删除后原列引用保持正确
            assert_eq!(ops.view_to_base(base - 1), Some(base - 1));
            // 再用刚刚删除的索引删除 → 越界应报错（核心层保持严格校验；
            // 界面侧必须过滤陈旧索引，见 dialog_delete_column）
            let err = ops.delete_columns(&[base]).unwrap_err();
            assert!(err.contains("不存在"), "越界应给出明确错误: {err}");
        }
    }

    #[test]
    fn generate_hash_column() {
        let m = model();
        let mut ops = ColumnOps::new(m.header());
        ops.generate_hash(&[0, 1], "row_hash", HashAlgo::Md5).unwrap();
        let h = ops.field(&m, 0, 3);
        assert_eq!(h.len(), 32); // md5 hex
        // 与直接计算一致
        let expected = hash_bytes(b"1alice", HashAlgo::Md5);
        assert_eq!(h, expected);
    }

    #[test]
    fn undo_redo_zero_copy() {
        let m = model();
        let mut ops = ColumnOps::new(m.header());
        ops.copy_column(1, "dup").unwrap();
        ops.merge_fields(&[0, 1], "id_name", "-").unwrap();
        assert_eq!(ops.column_count(), 5);
        assert!(ops.can_undo());
        // undo 两次
        ops.undo().unwrap();
        assert_eq!(ops.column_count(), 4);
        assert_eq!(ops.header()[3], "dup");
        ops.undo().unwrap();
        assert_eq!(ops.column_count(), 3);
        assert_eq!(ops.header(), m.header());
        // redo 两次
        assert!(ops.can_redo());
        ops.redo().unwrap();
        assert_eq!(ops.column_count(), 4);
        ops.redo().unwrap();
        assert_eq!(ops.column_count(), 5);
        assert_eq!(ops.field(&m, 0, 4), "1-alice");
    }

    #[test]
    fn undo_after_delete_and_hide() {
        let m = model();
        let mut ops = ColumnOps::new(m.header());
        ops.delete_columns(&[1]).unwrap();
        assert_eq!(ops.column_count(), 2);
        ops.undo().unwrap();
        assert_eq!(ops.column_count(), 3);
        assert_eq!(ops.header()[1], "NAME");
        ops.hide_columns(&[0]).unwrap();
        assert!(ops.is_hidden(0));
        ops.undo().unwrap();
        assert!(!ops.is_hidden(0));
    }

    #[test]
    fn unique_names_on_duplicate() {
        let m = model();
        let mut ops = ColumnOps::new(m.header());
        ops.copy_column(1, "NAME").unwrap();
        assert_eq!(ops.header()[3], "NAME_1");
        ops.copy_column(1, "NAME").unwrap();
        assert_eq!(ops.header()[4], "NAME_2");
    }

    #[test]
    fn view_to_base_mapping() {
        // review 修复：视图列 → 原始列映射；派生列返回 None
        let m = model();
        let mut ops = ColumnOps::new(m.header());
        assert_eq!(ops.view_to_base(0), Some(0));
        assert_eq!(ops.view_to_base(2), Some(2));
        ops.copy_column(1, "dup").unwrap(); // 视图索引 3 = 派生列
        assert_eq!(ops.view_to_base(3), None);
        ops.delete_columns(&[1]).unwrap(); // 删除 NAME，原始索引不变
        assert_eq!(ops.view_to_base(0), Some(0)); // ID
        assert_eq!(ops.view_to_base(1), Some(2)); // 原 AMOUNT 原始索引仍为 2
    }
}
