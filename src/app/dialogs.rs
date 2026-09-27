//! 对话框状态（egui 即时模式：以布尔标志控制对话框显示）。

use csv_editor_rust::core::db::{self, DbConfig, DbKind, TableInfo};
use csv_editor_rust::core::merge::{FileHeader, HeaderCheck};
use csv_editor_rust::core::stats::AggKind;

/// 数据库连接窗口与「库/表浏览」窗口的全部界面状态
pub struct DbState {
    /// 是否显示「连接数据库」窗口
    pub connect_window: bool,
    /// 是否显示「数据库表浏览」窗口（连接成功后打开）
    pub browse_window: bool,
    // —— 连接参数 ——
    /// 数据库类型（`DbKind::ALL` 的下标）
    pub kind_idx: usize,
    pub host: String,
    pub port: String,
    pub database: String,
    pub user: String,
    pub password: String,
    /// DSN 名（通用 ODBC 模式下可选）
    pub dsn: String,
    /// ODBC 驱动名（留空 = 按类型自动挑选）
    pub driver: String,
    /// 通用 ODBC：使用 DSN 而非直连参数
    pub use_dsn: bool,
    /// 手工编辑连接串（勾选后不再按上面的字段自动重算）
    pub manual_conn_str: bool,
    pub conn_str: String,
    /// 保存此连接时使用的名称
    pub conn_name: String,
    /// 是否把密码一起保存到本机配置文件（明文）
    pub save_password: bool,
    pub show_password: bool,
    /// 连接/测试结果提示
    pub status: String,
    /// 是否有数据库连接任务在执行（防止重复发起）
    pub connecting: bool,
    /// 本次连接仅为「测试连接」：成功后不打开浏览窗口、不保留连接
    pub test_only: bool,
    // —— 本机 ODBC 环境 ——
    pub drivers: Vec<String>,
    pub dsns: Vec<String>,
    pub odbc_loaded: bool,
    // —— 已保存的连接 ——
    pub saved: Vec<DbConfig>,
    pub saved_loaded: bool,
    // —— 浏览状态 ——
    /// 当前选中的库/模式（通用 ODBC 下恒为空）
    pub scope: String,
    pub scopes: Vec<String>,
    pub tables: Vec<TableInfo>,
    /// 表名筛选关键字
    pub table_filter: String,
    pub selected_table: Option<usize>,
    pub columns: Vec<String>,
    /// 与 columns 一一对应的勾选状态（默认全选）
    pub col_checked: Vec<bool>,
    /// 预览数据（表头 + 行）与说明
    pub preview: Option<(Vec<String>, Vec<Vec<String>>)>,
    pub preview_note: String,
    /// 载入编辑器时的行数上限（文本，0 = 全部）
    pub max_rows_text: String,
}

impl Default for DbState {
    fn default() -> Self {
        let kind = DbKind::ALL[0];
        Self {
            connect_window: false,
            browse_window: false,
            kind_idx: 0,
            host: "127.0.0.1".to_string(),
            port: kind.default_port().to_string(),
            database: String::new(),
            user: String::new(),
            password: String::new(),
            dsn: String::new(),
            driver: String::new(),
            use_dsn: false,
            manual_conn_str: false,
            conn_str: String::new(),
            conn_name: String::new(),
            save_password: false,
            show_password: false,
            status: String::new(),
            connecting: false,
            test_only: false,
            drivers: Vec::new(),
            dsns: Vec::new(),
            odbc_loaded: false,
            saved: Vec::new(),
            saved_loaded: false,
            scope: String::new(),
            scopes: Vec::new(),
            tables: Vec::new(),
            table_filter: String::new(),
            selected_table: None,
            columns: Vec::new(),
            col_checked: Vec::new(),
            preview: None,
            preview_note: String::new(),
            max_rows_text: db::DEFAULT_MAX_ROWS.to_string(),
        }
    }
}

impl DbState {
    /// 当前数据库类型
    pub fn kind(&self) -> DbKind {
        DbKind::ALL[self.kind_idx.min(DbKind::ALL.len() - 1)]
    }

    /// 由界面输入构造一份连接配置
    pub fn config(&self) -> DbConfig {
        let kind = self.kind();
        DbConfig {
            name: self.conn_name.trim().to_string(),
            kind,
            host: self.host.trim().to_string(),
            port: self.port.trim().to_string(),
            database: self.database.trim().to_string(),
            user: self.user.clone(),
            password: self.password.clone(),
            save_password: self.save_password,
            driver: self.driver.trim().to_string(),
            dsn: if kind == DbKind::Generic && self.use_dsn {
                self.dsn.trim().to_string()
            } else {
                String::new()
            },
            conn_str: if self.manual_conn_str {
                self.conn_str.trim().to_string()
            } else {
                String::new()
            },
        }
    }

    /// 用一份已保存的连接配置回填界面
    pub fn apply_config(&mut self, cfg: &DbConfig) {
        self.kind_idx = DbKind::ALL
            .iter()
            .position(|k| *k == cfg.kind)
            .unwrap_or(DbKind::ALL.len() - 1);
        self.host = cfg.host.clone();
        self.port = cfg.port.clone();
        self.database = cfg.database.clone();
        self.user = cfg.user.clone();
        self.password = cfg.password.clone();
        self.dsn = cfg.dsn.clone();
        self.driver = cfg.driver.clone();
        self.use_dsn = !cfg.dsn.trim().is_empty();
        self.manual_conn_str = !cfg.conn_str.trim().is_empty();
        self.conn_str = cfg.conn_str.clone();
        self.conn_name = cfg.name.clone();
        self.save_password = cfg.save_password;
        self.status.clear();
        self.conn_str = cfg.conn_str.clone();
    }

    /// 载入编辑器时使用的行数上限（0 = 全部）
    pub fn max_rows(&self) -> usize {
        self.max_rows_text
            .trim()
            .parse::<usize>()
            .unwrap_or(db::DEFAULT_MAX_ROWS)
    }

    /// 勾选出的字段（未勾选任何项时返回空 = 全部列）
    pub fn checked_columns(&self) -> Vec<String> {
        let picked: Vec<String> = self
            .columns
            .iter()
            .zip(self.col_checked.iter())
            .filter(|(_, c)| **c)
            .map(|(n, _)| n.clone())
            .collect();
        if picked.len() == self.columns.len() {
            Vec::new() // 全选等价于不指定列（SELECT *）
        } else {
            picked
        }
    }
}

/// 各操作对话框的显示标志与输入状态
#[derive(Default)]
pub struct DialogStates {
    // 数据库连接与库表浏览
    pub db: DbState,
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
    // 数据分析：数值列统计
    pub col_stats_window: bool,
    /// 要统计的列（视图列索引）
    pub col_stats_cols: Vec<usize>,
    // 数据分析：取值频次分布 / Top-N
    pub freq_window: bool,
    /// 要统计的列（视图列索引）
    pub freq_cols: Vec<usize>,
    /// 每列显示前 N 项（0 = 尚未初始化，界面按 20 处理）
    pub freq_top_n: usize,
    /// 「按频次筛选」的最小出现次数（0 = 尚未初始化，界面按 2 处理）
    pub freq_min_count: usize,
    // 数据分析：重复情况统计
    pub dup_window: bool,
    /// 判重字段（视图列索引；空 = 全部字段）
    pub dup_cols: Vec<usize>,
    /// 明细列出前 N 个重复键（0 = 尚未初始化，界面按 100 处理）
    pub dup_top_n: usize,
    // 数据分析：各统计功能的规模上限（可在参数窗里按硬件调整，并写入 egui memory 记忆）
    pub stats_limits: csv_editor_rust::core::stats::StatsLimits,
    /// 是否已从 egui memory 恢复过上限设置（首次打开参数窗时恢复一次）
    pub stats_limits_loaded: bool,
    // 数据分析：透视表
    pub pivot_window: bool,
    /// 行字段（视图列索引，1~2 个）
    pub pivot_row_cols: Vec<usize>,
    /// 列字段（视图列索引）
    pub pivot_col: usize,
    /// 指标列表：视图列索引 + 聚合方式（`stats::PIVOT_KINDS` 的下标），可添加多项
    pub pivot_values: Vec<(usize, usize)>,
    /// 「添加指标」时选中的值列（视图列索引）与聚合方式下标
    pub pivot_new_col: usize,
    pub pivot_new_kind: usize,
    /// 规模上限（0 = 尚未初始化，界面按 `stats::PIVOT_DEFAULT_*` 处理）
    pub pivot_max_rows: usize,
    pub pivot_max_cols: usize,
    pub pivot_max_cells: usize,
    // 数据分析：交叉/关联统计
    pub cross_window: bool,
    /// A 字段（视图列索引）
    pub cross_a: usize,
    /// B 字段（视图列索引）
    pub cross_b: usize,
    /// 明细前 N 项（0 = 尚未初始化，界面按 100 处理）
    pub cross_top_n: usize,
    // 数据分析：空值与数据质量统计
    pub quality_window: bool,
    /// 要检查的列（视图列索引）
    pub quality_cols: Vec<usize>,
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
