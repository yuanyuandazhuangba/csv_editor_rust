//! 对话框状态（egui 即时模式：以布尔标志控制对话框显示）。

use csv_editor_rust::core::merge::{FileHeader, HeaderCheck};
use csv_editor_rust::core::stats::AggKind;

/// 各操作对话框的显示标志与输入状态
#[derive(Default)]
pub struct DialogStates {
    // 列操作
    pub split_field: bool,
    pub merge_field: bool,
    pub generate_hash: bool,
    pub copy_column: bool,
    pub insert_column: bool,
    pub delete_column: bool,
    pub hide_column: bool,
    pub show_column: bool,
    // 关于
    pub about: bool,
    // 帮助 / 捐助
    pub help_guide: bool,
    pub donate: bool,
    // 多文件合并
    pub merge_window: bool,
    /// 已选文件路径（可追加选择）
    pub merge_files: Vec<String>,
    /// 表头校验信息（与 merge_files 一一对应；空 = 尚未读取）
    pub merge_headers: Vec<FileHeader>,
    /// 表头一致性判定（None = 尚未校验）
    pub merge_check: Option<HeaderCheck>,
    /// 差异字段名（表头不一致时用于提示）
    pub merge_diff: Vec<String>,
    /// 是否正在后台读取表头
    pub merge_scanning: bool,
    /// 表头不一致时：只保留相同字段名（交集）
    pub merge_keep_common: bool,
    // 数据分析：提取唯一行（去重）
    pub dedup_window: bool,
    /// 去重完成后的结果提示窗（询问是否导出「重复行对应表」）
    pub dedup_done_window: bool,
    /// 判重字段（视图列索引；空 = 全部字段）
    pub dedup_cols: Vec<usize>,
    // 数据分析：字段分组汇总
    pub stats_window: bool,
    /// 分组汇总结果窗
    pub stats_result_window: bool,
    /// 下一帧强制把结果窗从「最小化」状态展开（新结果、或用户主动要求查看时置位）
    pub stats_result_expand_pending: bool,
    /// 分组字段（视图列索引，1~3 个）
    pub stats_group_cols: Vec<usize>,
    /// 聚合规格：视图列索引 + 聚合方式（同一列可添加多个聚合）
    pub stats_aggs: Vec<(usize, AggKind)>,
    /// 「添加聚合」时选中的列（视图列索引）
    pub stats_new_col: usize,
    /// 「添加聚合」时选中的聚合方式（`AggKind::ALL` 的下标）
    pub stats_new_kind: usize,
    // 筛选/拆分/文件信息 弹出窗口
    pub filter_window: bool,
    pub split_window: bool,
    pub file_info: bool,
    // 导出
    pub export_data: bool,
    // 拆分参数
    pub split_method: usize, // 0 行数 1 字段
    pub split_batch: String,
    pub split_cols: Vec<usize>,
    pub split_format: usize, // 0 CSV 1 XLSX
    // 列操作对话框输入
    pub split_col: usize,
    pub split_sep: String,
    pub split_num: usize,
    pub split_prefix: String,
    pub merge_cols: Vec<usize>,
    pub merge_name: String,
    pub merge_sep: String,
    pub hash_cols: Vec<usize>,
    pub hash_algo: usize,
    pub hash_name: String,
    pub copy_col: usize,
    pub copy_name: String,
    pub insert_name: String,
    pub insert_pos: usize, // 0 开头 1 末尾 2 指定
    pub insert_ref: usize,
    pub insert_default: String,
    pub delete_cols: Vec<usize>,
    pub hide_cols: Vec<usize>,
    pub show_cols: Vec<usize>,
}
