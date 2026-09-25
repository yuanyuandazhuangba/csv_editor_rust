//! 对话框状态（egui 即时模式：以布尔标志控制对话框显示）。

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
