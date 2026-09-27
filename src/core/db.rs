//! 数据库连接与只读取数（Windows 自带 ODBC：`odbc32.dll`，纯 FFI，零第三方依赖）。
//!
//! 设计要点：
//! - **只读**：连接时设置 `SQL_ATTR_ACCESS_MODE = SQL_MODE_READ_ONLY`，只发出
//!   `SELECT` 与 ODBC 元数据调用（`SQLTables`/`SQLColumns`），不执行任何 DDL/DML，
//!   不使用事务；读取过程只把数据写进本进程内存或本机临时 CSV，数据库原数据不被改动。
//! - **不落第三方 DLL**：`odbc32.dll` 是 Windows 系统自带（Win7 起即有），
//!   保持「单文件、不随附外部 DLL」的发布特性；各数据库的 ODBC 驱动由用户自行安装
//!   （数据库客户端/驱动包自带，属常见前置条件）。
//! - **取数即 CSV**：从数据库读出的表/视图数据写成 UTF-8 临时 CSV，再走既有的
//!   `DataModel` 加载流程，因此筛选/拆分/统计/导出等全部功能可直接复用。
//! - 表名/列名一律用驱动给出的标识符引用字符包裹并转义，SQL 不拼接用户输入。

use std::os::raw::{c_int, c_short, c_void};
use std::path::PathBuf;

use super::export::CsvWriter;

/// 从数据库载入编辑器时默认的行数上限（0 = 不限制，全部读取）
pub const DEFAULT_MAX_ROWS: usize = 100_000;

// ==================== ODBC 常量（sql.h / sqlext.h）====================

type Handle = *mut c_void;
/// SQLSMALLINT
type SmallInt = c_short;
/// SQLUSMALLINT
type USmallInt = u16;
/// SQLINTEGER
type Integer = c_int;
/// SQLLEN
type Len = isize;
/// SQLULEN
type ULen = usize;

const SQL_HANDLE_ENV: SmallInt = 1;
const SQL_HANDLE_DBC: SmallInt = 2;
const SQL_HANDLE_STMT: SmallInt = 3;
const SQL_NULL_HANDLE: Handle = std::ptr::null_mut();
const SQL_SUCCESS: SmallInt = 0;
const SQL_SUCCESS_WITH_INFO: SmallInt = 1;
const SQL_NO_DATA: SmallInt = 100;
const SQL_ERROR: SmallInt = -1;
const SQL_INVALID_HANDLE: SmallInt = -2;
const SQL_NTS: SmallInt = -3;
const SQL_NULL_DATA: Len = -1;
const SQL_ATTR_ODBC_VERSION: Integer = 200;
const SQL_OV_ODBC3: ULen = 3;
/// 连接属性：访问模式（连接前设置，驱动据此拒绝写操作）
const SQL_ATTR_ACCESS_MODE: Integer = 101;
const SQL_MODE_READ_ONLY: ULen = 1;
/// 连接属性：登录超时 / 连接超时（秒）
const SQL_ATTR_LOGIN_TIMEOUT: Integer = 103;
const SQL_ATTR_CONNECTION_TIMEOUT: Integer = 113;
/// 语句属性：最大行数（驱动侧兜底）
///
/// 注意：**不要**使用 ODBC 的 `SQL_ATTR_QUERY_TIMEOUT`。实测 MySQL Connector/ODBC
/// 会把它当成**连接级**限制：一旦为某个元数据查询设过 5 秒，之后同一连接上的
/// 长查询（全表取数）会在 5 秒整被服务端中断
/// （`Query execution was interrupted, maximum statement execution time exceeded`）。
/// 需要限制单条语句时长时，改用数据库自己的语句级写法（MySQL 用优化器提示
/// `/*+ MAX_EXECUTION_TIME(ms) */`，只作用于该语句）。
const SQL_ATTR_MAX_ROWS: Integer = 1;
/// SQL_C_WCHAR：结果按 UTF-16 取回（驱动负责字符集转换，中文不乱码）
const SQL_C_WCHAR: SmallInt = -8;
/// SQL_C_CHAR：结果按连接字符集（MySQL 为 UTF-8）取回，省一轮编码转换
const SQL_C_CHAR: SmallInt = 1;
/// 批量取数（数组绑定）语句属性
const SQL_ATTR_ROW_STATUS_PTR: Integer = 25;
const SQL_ATTR_ROWS_FETCHED_PTR: Integer = 26;
const SQL_ATTR_ROW_ARRAY_SIZE: Integer = 27;

/// 取数进度回调：(已读行数, 预期总行数)。`None` = 总行数未知，
/// 界面应显示为「已读取 N 行」的不确定进度，而不是虚假的百分比。
pub type ProgressFn<'a> = &'a (dyn Fn(u64, Option<u64>) + Sync);

/// 进度分母：优先用「表的实际行数」，用户设置了行数上限时取两者较小值；
/// 都没有（不限行数且统计不出总数）则为 `None`（不确定进度）。
fn progress_total(expected_total: Option<u64>, max_rows: usize) -> Option<u64> {
    match (expected_total, max_rows) {
        (Some(t), 0) => Some(t),
        (Some(t), n) => Some(t.min(n as u64)),
        (None, 0) => None,
        (None, n) => Some(n as u64),
    }
}

/// 批量取数的批量行数上限
const BATCH_MAX_ROWS: usize = 200;
/// 批量取数列缓冲的总内存上限（超出则自动减小批量行数）
const BATCH_BUF_LIMIT: usize = 32 * 1024 * 1024;
/// 单列按字符计的缓冲上限：超过它的列不参与数组绑定（回退逐格取数，避免分配巨量缓冲）
const BATCH_MAX_COL_CHARS: usize = 16_384;
/// 列缓冲最小字节数
const BATCH_MIN_COL_BYTES: usize = 64;

/// 批量取数不可用时返回的错误前缀（调用方据此回退到逐格 `SQLGetData` 路径）
const BATCH_FALLBACK: &str = "\u{1}batch-fallback: ";

fn batch_fallback(reason: &str) -> String {
    format!("{}{}", BATCH_FALLBACK, reason)
}

/// 关闭并释放语句句柄（错误路径与正常结束共用）
fn free_stmt(stmt: Handle) {
    unsafe {
        SQLFreeStmt(stmt, SQL_CLOSE);
        SQLFreeHandle(SQL_HANDLE_STMT, stmt);
    }
}

/// 结果是否属于「批量取数不可用，请回退逐格路径」
pub fn is_batch_fallback(err: &str) -> bool {
    err.starts_with(BATCH_FALLBACK)
}
const SQL_DRIVER_NOPROMPT: USmallInt = 0;
const SQL_FETCH_NEXT: SmallInt = 1;
const SQL_FETCH_FIRST: SmallInt = 2;
const SQL_CLOSE: USmallInt = 0;
const SQL_DBMS_NAME: USmallInt = 17;
const SQL_DATABASE_NAME: USmallInt = 16;
const SQL_DBMS_VER: USmallInt = 18;
const SQL_IDENTIFIER_QUOTE_CHAR: USmallInt = 29;

#[link(name = "odbc32")]
extern "system" {
    fn SQLAllocHandle(handle_type: SmallInt, input: Handle, output: *mut Handle) -> SmallInt;
    fn SQLFreeHandle(handle_type: SmallInt, h: Handle) -> SmallInt;
    fn SQLSetEnvAttr(env: Handle, attr: Integer, value: *mut c_void, len: Integer) -> SmallInt;
    fn SQLSetConnectAttrW(dbc: Handle, attr: Integer, value: *mut c_void, len: Integer) -> SmallInt;
    fn SQLSetStmtAttrW(stmt: Handle, attr: Integer, value: *mut c_void, len: Integer) -> SmallInt;
    fn SQLDriverConnectW(
        dbc: Handle,
        hwnd: Handle,
        in_str: *const u16,
        in_len: SmallInt,
        out_str: *mut u16,
        out_len: SmallInt,
        out_len_ptr: *mut SmallInt,
        completion: USmallInt,
    ) -> SmallInt;
    fn SQLDisconnect(dbc: Handle) -> SmallInt;
    fn SQLDriversW(
        env: Handle,
        direction: SmallInt,
        desc: *mut u16,
        desc_len: SmallInt,
        desc_len_ptr: *mut SmallInt,
        attrs: *mut u16,
        attrs_len: SmallInt,
        attrs_len_ptr: *mut SmallInt,
    ) -> SmallInt;
    fn SQLDataSourcesW(
        env: Handle,
        direction: SmallInt,
        server: *mut u16,
        server_len: SmallInt,
        server_len_ptr: *mut SmallInt,
        desc: *mut u16,
        desc_len: SmallInt,
        desc_len_ptr: *mut SmallInt,
    ) -> SmallInt;
    fn SQLTablesW(
        stmt: Handle,
        cat: *const u16,
        cat_len: SmallInt,
        schema: *const u16,
        schema_len: SmallInt,
        table: *const u16,
        table_len: SmallInt,
        types: *const u16,
        types_len: SmallInt,
    ) -> SmallInt;
    fn SQLColumnsW(
        stmt: Handle,
        cat: *const u16,
        cat_len: SmallInt,
        schema: *const u16,
        schema_len: SmallInt,
        table: *const u16,
        table_len: SmallInt,
        column: *const u16,
        column_len: SmallInt,
    ) -> SmallInt;
    fn SQLExecDirectW(stmt: Handle, sql: *const u16, len: Integer) -> SmallInt;
    fn SQLNumResultCols(stmt: Handle, n: *mut SmallInt) -> SmallInt;
    fn SQLDescribeColW(
        stmt: Handle,
        col: USmallInt,
        name: *mut u16,
        buf_len: SmallInt,
        name_len: *mut SmallInt,
        data_type: *mut SmallInt,
        col_size: *mut ULen,
        decimal_digits: *mut SmallInt,
        nullable: *mut SmallInt,
    ) -> SmallInt;
    fn SQLFetch(stmt: Handle) -> SmallInt;
    fn SQLBindCol(
        stmt: Handle,
        col: USmallInt,
        target_type: SmallInt,
        buf: *mut c_void,
        buf_len: Len,
        ind: *mut Len,
    ) -> SmallInt;
    fn SQLGetData(
        stmt: Handle,
        col: USmallInt,
        target_type: SmallInt,
        buf: *mut c_void,
        buf_len: Len,
        ind: *mut Len,
    ) -> SmallInt;
    fn SQLFreeStmt(stmt: Handle, option: USmallInt) -> SmallInt;
    fn SQLGetInfoW(
        dbc: Handle,
        info_type: USmallInt,
        buf: *mut c_void,
        buf_len: SmallInt,
        len_ptr: *mut SmallInt,
    ) -> SmallInt;
    fn SQLGetDiagRecW(
        handle_type: SmallInt,
        h: Handle,
        rec: SmallInt,
        state: *mut u16,
        native: *mut Integer,
        msg: *mut u16,
        msg_len: SmallInt,
        msg_len_ptr: *mut SmallInt,
    ) -> SmallInt;
}

// ==================== 小工具 ====================

fn ok(rc: SmallInt) -> bool {
    rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO
}

/// `&str` → 以 NUL 结尾的 UTF-16（ODBC 宽字符 API 使用）
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// UTF-16 缓冲 → String（`len` 为字符数；len<0 时按 NUL 截断）
fn from_wide(buf: &[u16], len: i32) -> String {
    let n = if len < 0 {
        buf.iter().position(|&c| c == 0).unwrap_or(buf.len())
    } else {
        (len as usize).min(buf.len())
    };
    String::from_utf16_lossy(&buf[..n])
}

/// 读取 ODBC 诊断信息（错误/警告文本），拼成可读消息
fn diag(handle_type: SmallInt, h: Handle) -> String {
    let mut parts: Vec<String> = Vec::new();
    for rec in 1..=8i16 {
        let mut state = [0u16; 6];
        let mut native: Integer = 0;
        let mut msg = vec![0u16; 1024];
        let mut msg_len: SmallInt = 0;
        let rc = unsafe {
            SQLGetDiagRecW(
                handle_type,
                h,
                rec,
                state.as_mut_ptr(),
                &mut native,
                msg.as_mut_ptr(),
                msg.len() as SmallInt,
                &mut msg_len,
            )
        };
        if rc == SQL_NO_DATA || rc == SQL_ERROR || rc == SQL_INVALID_HANDLE {
            break;
        }
        let st = String::from_utf16_lossy(&state);
        let st = st.trim_end_matches('\0');
        let text = from_wide(&msg, msg_len as i32);
        parts.push(format!("[{}] {}", st, text.trim()));
    }
    parts.join("；")
}

/// 设置整数型连接属性（长度参数按 ODBC 3.x 约定为 0）
fn set_conn_attr(dbc: Handle, attr: Integer, value: ULen) -> bool {
    ok(unsafe { SQLSetConnectAttrW(dbc, attr, value as *mut c_void, 0) })
}

// ==================== 数据库类型 ====================

/// 支持的数据库类型（决定连接串写法、默认端口与 SQL 方言细节）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DbKind {
    MySql,
    PostgreSql,
    SqlServer,
    Oracle,
    /// 通用 ODBC：用户自行选择 DSN 或驱动并填写连接串
    Generic,
}

impl DbKind {
    pub const ALL: [DbKind; 5] = [
        DbKind::MySql,
        DbKind::PostgreSql,
        DbKind::SqlServer,
        DbKind::Oracle,
        DbKind::Generic,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DbKind::MySql => "MySQL / MariaDB",
            DbKind::PostgreSql => "PostgreSQL",
            DbKind::SqlServer => "SQL Server",
            DbKind::Oracle => "Oracle",
            DbKind::Generic => "通用 ODBC（DSN / 其它库）",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            DbKind::MySql => "mysql",
            DbKind::PostgreSql => "postgresql",
            DbKind::SqlServer => "sqlserver",
            DbKind::Oracle => "oracle",
            DbKind::Generic => "odbc",
        }
    }

    pub fn from_key(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "mysql" | "mariadb" => DbKind::MySql,
            "postgresql" | "postgres" | "pgsql" => DbKind::PostgreSql,
            "sqlserver" | "mssql" => DbKind::SqlServer,
            "oracle" => DbKind::Oracle,
            _ => DbKind::Generic,
        }
    }

    pub fn default_port(self) -> &'static str {
        match self {
            DbKind::MySql => "3306",
            DbKind::PostgreSql => "5432",
            DbKind::SqlServer => "1433",
            DbKind::Oracle => "1521",
            DbKind::Generic => "",
        }
    }

    /// 「数据库/模式」这一层在各库里的实际含义（界面提示用）
    pub fn scope_label(self) -> &'static str {
        match self {
            DbKind::MySql => "数据库",
            DbKind::PostgreSql => "模式（schema）",
            DbKind::SqlServer => "数据库",
            DbKind::Oracle => "用户（schema）",
            DbKind::Generic => "目录 / 数据库",
        }
    }
}

// ==================== 连接配置 ====================

/// 一条数据库连接配置
#[derive(Clone, Debug)]
pub struct DbConfig {
    /// 连接名称（保存/复用时显示）
    pub name: String,
    pub kind: DbKind,
    pub host: String,
    pub port: String,
    pub database: String,
    pub user: String,
    pub password: String,
    /// 是否把密码写入本机配置文件（明文，默认否）
    pub save_password: bool,
    /// ODBC 驱动名（留空则按类型自动挑选已安装驱动）
    pub driver: String,
    /// DSN 名（与 driver 二选一，DSN 优先）
    pub dsn: String,
    /// 手工连接串：非空时直接使用（覆盖上面的字段拼装）
    pub conn_str: String,
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: DbKind::MySql,
            host: "127.0.0.1".to_string(),
            port: DbKind::MySql.default_port().to_string(),
            database: String::new(),
            user: String::new(),
            password: String::new(),
            save_password: false,
            driver: String::new(),
            dsn: String::new(),
            conn_str: String::new(),
        }
    }
}

impl DbConfig {
    /// 配置显示名（未命名时用「主机/库」兜底）
    pub fn display_name(&self) -> String {
        if !self.name.trim().is_empty() {
            return self.name.trim().to_string();
        }
        let mut s = self.kind.label().to_string();
        if !self.host.trim().is_empty() {
            s.push(' ');
            s.push_str(self.host.trim());
        }
        if !self.port.trim().is_empty() && self.kind != DbKind::Generic {
            s.push(':');
            s.push_str(self.port.trim());
        }
        if !self.database.trim().is_empty() {
            s.push('/');
            s.push_str(self.database.trim());
        }
        if !self.dsn.trim().is_empty() {
            s.push_str(" DSN=");
            s.push_str(self.dsn.trim());
        }
        s
    }

    /// 数据库客户端实际使用的连接串（不含密码时为脱敏版本，供日志/界面显示）
    pub fn redacted_conn_str(&self, drivers: &[String]) -> String {
        let cs = self.build_conn_str(drivers);
        redact_password(&cs)
    }

    /// 拼装 ODBC 连接串。`drivers` 为本机已安装驱动名列表（用于自动挑选驱动）。
    pub fn build_conn_str(&self, drivers: &[String]) -> String {
        if !self.conn_str.trim().is_empty() {
            // 手工连接串：自动补齐密码占位（用户未写 PWD 时追加）
            let mut s = self.conn_str.trim().to_string();
            if !self.password.is_empty() && !contains_password_key(&s) {
                s = format!("{};PWD={};", s.trim_end_matches(';'), self.password);
            }
            return s;
        }
        let mut parts: Vec<String> = Vec::new();
        // DSN 模式
        if !self.dsn.trim().is_empty() {
            parts.push(format!("DSN={}", self.dsn.trim()));
            if !self.user.is_empty() {
                parts.push(format!("UID={}", self.user));
            }
            if !self.password.is_empty() {
                parts.push(format!("PWD={}", self.password));
            }
            if !self.database.trim().is_empty() {
                parts.push(format!("DBQ={}", self.database.trim()));
            }
            return parts.join(";") + ";";
        }

        let driver = self.effective_driver(drivers);
        if !driver.is_empty() {
            parts.push(format!("DRIVER={{{}}}", driver));
        }
        match self.kind {
            DbKind::MySql => {
                if !self.host.trim().is_empty() {
                    parts.push(format!("SERVER={}", self.host.trim()));
                }
                if !self.port.trim().is_empty() {
                    parts.push(format!("PORT={}", self.port.trim()));
                }
                if !self.database.trim().is_empty() {
                    parts.push(format!("DATABASE={}", self.database.trim()));
                }
                if !self.user.is_empty() {
                    parts.push(format!("UID={}", self.user));
                }
                if !self.password.is_empty() {
                    parts.push(format!("PWD={}", self.password));
                }
                // 驱动返回多字节/长字段时按实际长度回传，避免大文本被截断
                parts.push("OPTION=3".to_string());
                parts.push("CHARSET=utf8mb4".to_string());
            }
            DbKind::PostgreSql => {
                if !self.host.trim().is_empty() {
                    parts.push(format!("SERVER={}", self.host.trim()));
                }
                if !self.port.trim().is_empty() {
                    parts.push(format!("PORT={}", self.port.trim()));
                }
                if !self.database.trim().is_empty() {
                    parts.push(format!("DATABASE={}", self.database.trim()));
                }
                if !self.user.is_empty() {
                    parts.push(format!("UID={}", self.user));
                }
                if !self.password.is_empty() {
                    parts.push(format!("PWD={}", self.password));
                }
                // psqlODBC：只读连接（驱动侧再上一道保险）
                parts.push("ReadOnly=1".to_string());
                parts.push("ConnSettings=SET default_transaction_read_only=on;".to_string());
            }
            DbKind::SqlServer => {
                let server = if self.port.trim().is_empty() {
                    self.host.trim().to_string()
                } else {
                    format!("{},{}", self.host.trim(), self.port.trim())
                };
                if !server.is_empty() {
                    parts.push(format!("SERVER={}", server));
                }
                if !self.database.trim().is_empty() {
                    parts.push(format!("DATABASE={}", self.database.trim()));
                }
                if !self.user.is_empty() {
                    parts.push(format!("UID={}", self.user));
                }
                if !self.password.is_empty() {
                    parts.push(format!("PWD={}", self.password));
                }
                // ODBC Driver 18+ 默认强制加密，本机自签/内网实例常无法验签；显式放宽
                if driver.contains("18") {
                    parts.push("Encrypt=Optional".to_string());
                    parts.push("TrustServerCertificate=Yes".to_string());
                } else if driver.contains("17") {
                    parts.push("TrustServerCertificate=Yes".to_string());
                }
                parts.push("ApplicationIntent=ReadOnly".to_string());
            }
            DbKind::Oracle => {
                // Oracle：数据库用 DBQ（主机:端口/服务名）
                let host = self.host.trim();
                let db = self.database.trim();
                if !host.is_empty() || !db.is_empty() {
                    let dbq = if db.is_empty() {
                        host.to_string()
                    } else if self.port.trim().is_empty() {
                        format!("{}/{}", host, db)
                    } else {
                        format!("{}:{}/{}", host, self.port.trim(), db)
                    };
                    parts.push(format!("DBQ={}", dbq));
                }
                if !self.user.is_empty() {
                    parts.push(format!("UID={}", self.user));
                }
                if !self.password.is_empty() {
                    parts.push(format!("PWD={}", self.password));
                }
            }
            DbKind::Generic => {
                if !self.host.trim().is_empty() {
                    parts.push(format!("SERVER={}", self.host.trim()));
                }
                if !self.database.trim().is_empty() {
                    parts.push(format!("DATABASE={}", self.database.trim()));
                }
                if !self.user.is_empty() {
                    parts.push(format!("UID={}", self.user));
                }
                if !self.password.is_empty() {
                    parts.push(format!("PWD={}", self.password));
                }
            }
        }
        parts.join(";") + ";"
    }

    /// 实际使用的 ODBC 驱动名：显式填写优先，否则从已安装驱动中按类型挑选
    pub fn effective_driver(&self, drivers: &[String]) -> String {
        if !self.driver.trim().is_empty() {
            return self.driver.trim().to_string();
        }
        pick_driver(self.kind, drivers).unwrap_or_default()
    }
}

/// 脱敏：把连接串里的 PWD/Password 值换成 `***`
fn redact_password(cs: &str) -> String {
    cs.split(';')
        .map(|kv| {
            let upper = kv.trim().to_ascii_uppercase();
            if upper.starts_with("PWD=") || upper.starts_with("PASSWORD=") {
                format!("{}=***", kv.trim().split('=').next().unwrap_or("PWD"))
            } else {
                kv.trim().to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn contains_password_key(cs: &str) -> bool {
    cs.split(';').any(|kv| {
        let u = kv.trim().to_ascii_uppercase();
        u.starts_with("PWD=") || u.starts_with("PASSWORD=")
    })
}

/// 会话级只读语句：连接建立后执行，让**数据库自己**拒绝任何写操作。
///
/// 背景：ODBC 的 `SQL_ATTR_ACCESS_MODE = SQL_MODE_READ_ONLY` 只是「告知驱动」，
/// 实测 MySQL Connector/ODBC 并不会把它翻译成会话只读（`@@session.transaction_read_only`
/// 仍为 0）。这里再在数据库侧上一道保险：语句执行失败时静默忽略（老版本/无权限不影响连接）。
/// SQL Server 没有等价的会话级只读（连接串里的 `ApplicationIntent=ReadOnly` 仅在只读副本生效），
/// Oracle 的 `SET TRANSACTION READ ONLY` 是事务级设置（会长时间持有事务快照），均不使用。
fn read_only_session_sql(kind: DbKind) -> Option<&'static str> {
    match kind {
        // MySQL 5.6.5+ / MariaDB 支持；此后本连接的任何写操作都会报 1792 错误
        DbKind::MySql => Some("SET SESSION TRANSACTION READ ONLY"),
        DbKind::PostgreSql => Some("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY"),
        DbKind::SqlServer | DbKind::Oracle | DbKind::Generic => None,
    }
}

/// 该类型数据库常用的 ODBC 驱动与获取途径（界面提示 / 连接失败时的排错指引）
pub fn driver_hint(kind: DbKind) -> &'static str {
    match kind {
        DbKind::MySql => {
            "请安装「MySQL Connector/ODBC」（MySQL 官网 → Downloads → Connector/ODBC，选 64 位 MSI；\
             或 MariaDB Connector/ODBC），装好后回到本窗口在「ODBC 驱动」下拉里选择它"
        }
        DbKind::PostgreSql => {
            "请安装「PostgreSQL ODBC 驱动（psqlODBC）」（postgresql.org → Downloads → ODBC driver，选 x64）"
        }
        DbKind::SqlServer => {
            "请安装「Microsoft ODBC Driver 17 / 18 for SQL Server」（微软官网下载 msodbcsql；\
             Windows 自带的「SQL Server」旧驱动无需安装，可直接在下拉里选择）"
        }
        DbKind::Oracle => {
            "请安装 Oracle ODBC 驱动（随 Oracle Instant Client / Oracle Client 提供），\
             或先在「ODBC 数据源管理器」里配置好 DSN 再改用「连接其它数据库（ODBC / DSN）…」"
        }
        DbKind::Generic => {
            "请安装目标数据库对应的 64 位 ODBC 驱动，或在「ODBC 数据源管理器」里配置一个 DSN 后勾选「使用已配置的数据源（DSN）」"
        }
    }
}

/// 连接串里是否已经指定了驱动（DRIVER=）或数据源（DSN=）
fn conn_str_has_driver_or_dsn(cs: &str) -> bool {
    cs.split(';').any(|kv| {
        let u = kv.trim().to_ascii_uppercase();
        u.starts_with("DRIVER=") || u.starts_with("DSN=")
    })
}

/// 按数据库类型从已安装驱动里挑选一个合适的（None = 没找到，需要用户装驱动）
pub fn pick_driver(kind: DbKind, drivers: &[String]) -> Option<String> {
    let find = |pred: &dyn Fn(&str) -> bool| -> Option<String> {
        drivers.iter().find(|d| pred(d)).cloned()
    };
    match kind {
        DbKind::MySql => {
            // 优先 MySQL 的 Unicode 驱动，其次 MariaDB
            find(&|d| {
                let l = d.to_ascii_lowercase();
                l.contains("mysql") && l.contains("unicode")
            })
            .or_else(|| {
                find(&|d| {
                    let l = d.to_ascii_lowercase();
                    l.contains("mariadb")
                })
            })
            .or_else(|| find(&|d| d.to_ascii_lowercase().contains("mysql")))
        }
        DbKind::PostgreSql => find(&|d| d.contains("PostgreSQL Unicode"))
            .or_else(|| find(&|d| d.to_ascii_lowercase().contains("postgresql"))),
        DbKind::SqlServer => {
            for pref in ["ODBC Driver 18 for SQL Server", "ODBC Driver 17 for SQL Server"] {
                if let Some(d) = find(&|d| d == pref) {
                    return Some(d);
                }
            }
            find(&|d| d.contains("ODBC Driver") && d.contains("SQL Server"))
                .or_else(|| find(&|d| d.contains("SQL Server Native Client")))
                .or_else(|| find(&|d| d.trim() == "SQL Server"))
        }
        DbKind::Oracle => find(&|d| d.to_ascii_lowercase().contains("oracle")),
        DbKind::Generic => None,
    }
}

// ==================== 枚举本机 ODBC 驱动 / DSN ====================

fn alloc_env() -> Result<Handle, String> {
    let mut env: Handle = SQL_NULL_HANDLE;
    let rc = unsafe { SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env) };
    if !ok(rc) {
        return Err(format!(
            "无法初始化 ODBC 环境（odbc32.dll）：{}",
            diag(SQL_HANDLE_ENV, SQL_NULL_HANDLE)
        ));
    }
    let rc = unsafe { SQLSetEnvAttr(env, SQL_ATTR_ODBC_VERSION, SQL_OV_ODBC3 as *mut c_void, 0) };
    if !ok(rc) {
        let e = diag(SQL_HANDLE_ENV, env);
        unsafe { SQLFreeHandle(SQL_HANDLE_ENV, env) };
        return Err(format!("ODBC 环境版本设置失败：{}", e));
    }
    Ok(env)
}

/// 本机已安装的 ODBC 驱动名列表（用于自动挑选驱动与界面下拉）
pub fn installed_drivers() -> Vec<String> {
    let env = match alloc_env() {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut dir = SQL_FETCH_FIRST;
    loop {
        let mut desc = vec![0u16; 512];
        let mut attrs = vec![0u16; 1024];
        let mut dlen: SmallInt = 0;
        let mut alen: SmallInt = 0;
        let rc = unsafe {
            SQLDriversW(
                env,
                dir,
                desc.as_mut_ptr(),
                desc.len() as SmallInt,
                &mut dlen,
                attrs.as_mut_ptr(),
                attrs.len() as SmallInt,
                &mut alen,
            )
        };
        if rc == SQL_NO_DATA || rc == SQL_ERROR || rc == SQL_INVALID_HANDLE {
            break;
        }
        let name = from_wide(&desc, dlen as i32);
        if !name.trim().is_empty() {
            out.push(name.trim().to_string());
        }
        dir = SQL_FETCH_NEXT;
    }
    unsafe { SQLFreeHandle(SQL_HANDLE_ENV, env) };
    out.sort();
    out.dedup();
    out
}

/// 本机已配置的 ODBC 数据源（DSN）名列表
pub fn installed_dsns() -> Vec<String> {
    let env = match alloc_env() {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut dir = SQL_FETCH_FIRST;
    loop {
        let mut name = vec![0u16; 512];
        let mut desc = vec![0u16; 512];
        let mut nlen: SmallInt = 0;
        let mut dlen: SmallInt = 0;
        let rc = unsafe {
            SQLDataSourcesW(
                env,
                dir,
                name.as_mut_ptr(),
                name.len() as SmallInt,
                &mut nlen,
                desc.as_mut_ptr(),
                desc.len() as SmallInt,
                &mut dlen,
            )
        };
        if rc == SQL_NO_DATA || rc == SQL_ERROR || rc == SQL_INVALID_HANDLE {
            break;
        }
        let n = from_wide(&name, nlen as i32);
        if !n.trim().is_empty() {
            out.push(n.trim().to_string());
        }
        dir = SQL_FETCH_NEXT;
    }
    unsafe { SQLFreeHandle(SQL_HANDLE_ENV, env) };
    out.sort();
    out.dedup();
    out
}

// ==================== 表元数据 ====================

/// 一个表/视图（含其所属目录与模式）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableInfo {
    pub catalog: String,
    pub schema: String,
    pub name: String,
    /// `TABLE` / `VIEW` / 其它驱动返回的类型
    pub kind: String,
}

impl TableInfo {
    pub fn display(&self) -> String {
        if self.kind.eq_ignore_ascii_case("VIEW") {
            format!("{}（视图）", self.name)
        } else {
            self.name.clone()
        }
    }
}

// ==================== 连接 ====================

/// 一个已建立的 ODBC 连接（只读）。
///
/// 句柄可在不同线程间传递：调用方需保证同一连接不会被并发使用
/// （界面层通过「同时只允许一个数据库任务」保证）。
pub struct DbConn {
    env: Handle,
    dbc: Handle,
    kind: DbKind,
    quote_open: char,
    quote_close: char,
    /// DBMS 名 + 版本（连接后读取）
    server_info: String,
    /// 当前默认库名（通用 ODBC 下用于省略冗余的目录限定）
    current_db: String,
    /// 实际使用的驱动名（连接串回读）
    driver_name: String,
    /// 是否已在数据库侧确认会话只读（MySQL / PostgreSQL）
    session_read_only: bool,
}

unsafe impl Send for DbConn {}
unsafe impl Sync for DbConn {}

impl DbConn {
    /// 建立只读连接
    pub fn connect(cfg: &DbConfig, drivers: &[String]) -> Result<Self, String> {
        let conn_str = cfg.build_conn_str(drivers);
        if conn_str.trim().is_empty() || conn_str.trim() == ";" {
            return Err("连接串为空：请填写服务器地址，或选择 DSN / 手写连接串".to_string());
        }
        // 既没有驱动也没有 DSN 时，ODBC 只会回一句晦涩的「未发现数据源名称…(IM002)」；
        // 这里提前判断并给出真正的原因与解决办法（最常见原因是本机没装该库的 ODBC 驱动）。
        if !conn_str_has_driver_or_dsn(&conn_str) {
            let drv = if cfg.driver.trim().is_empty() {
                "（未指定）".to_string()
            } else {
                cfg.driver.trim().to_string()
            };
            return Err(format!(
                "连接串里既没有 DRIVER 也没有 DSN，ODBC 无法确定使用哪个驱动。\n\
                 当前「ODBC 驱动」：{}；本机已安装 {} 个 ODBC 驱动。\n\
                 解决办法（任选其一）：\n  1) {}；\n  2) 在「ODBC 数据源管理器」里配置一个 DSN，然后改用「连接其它数据库（ODBC / DSN）…」并勾选「使用已配置的数据源（DSN）」；\n  3) 勾选「手动编辑连接串」，在连接串里写上 DRIVER={{驱动名}} 或 DSN={{数据源名}}。",
                drv,
                drivers.len(),
                driver_hint(cfg.kind)
            ));
        }
        let env = alloc_env()?;
        let mut dbc: Handle = SQL_NULL_HANDLE;
        let rc = unsafe { SQLAllocHandle(SQL_HANDLE_DBC, env, &mut dbc) };
        if !ok(rc) {
            let e = diag(SQL_HANDLE_ENV, env);
            unsafe { SQLFreeHandle(SQL_HANDLE_ENV, env) };
            return Err(format!("无法分配连接句柄：{}", e));
        }
        // 只读 + 超时：访问模式在连接建立时传给驱动，从协议层拒绝写
        set_conn_attr(dbc, SQL_ATTR_ACCESS_MODE, SQL_MODE_READ_ONLY);
        set_conn_attr(dbc, SQL_ATTR_LOGIN_TIMEOUT, 15);
        set_conn_attr(dbc, SQL_ATTR_CONNECTION_TIMEOUT, 30);

        let cs = wide(&conn_str);
        let mut out = vec![0u16; 4096];
        let mut out_len: SmallInt = 0;
        let rc = unsafe {
            SQLDriverConnectW(
                dbc,
                SQL_NULL_HANDLE,
                cs.as_ptr(),
                SQL_NTS,
                out.as_mut_ptr(),
                out.len() as SmallInt,
                &mut out_len,
                SQL_DRIVER_NOPROMPT,
            )
        };
        if !ok(rc) {
            let e = diag(SQL_HANDLE_DBC, dbc);
            unsafe {
                SQLFreeHandle(SQL_HANDLE_DBC, dbc);
                SQLFreeHandle(SQL_HANDLE_ENV, env);
            }
            let msg = if e.trim().is_empty() {
                "连接失败（驱动未给出详细信息）".to_string()
            } else {
                e
            };
            return Err(format!("连接失败：{}", msg));
        }

        let quote = get_info_char(dbc, SQL_IDENTIFIER_QUOTE_CHAR).unwrap_or('"');
        let (quote_open, quote_close) = if quote == '[' {
            ('[', ']')
        } else {
            (quote, quote)
        };
        let dbms = get_info_str(dbc, SQL_DBMS_NAME);
        let ver = get_info_str(dbc, SQL_DBMS_VER);
        let current_db = get_info_str(dbc, SQL_DATABASE_NAME);
        let server_info = format!("{} {}", dbms.trim(), ver.trim())
            .trim()
            .to_string();
        // 驱动名：连接串回读里通常带 DRIVER= 项
        let effective = cfg.effective_driver(drivers);
        let driver_name = if !effective.is_empty() {
            effective
        } else {
            let s = from_wide(&out, out_len as i32);
            extract_conn_value(&s, "DRIVER").unwrap_or_default()
        };

        let mut conn = Self {
            env,
            dbc,
            kind: cfg.kind,
            quote_open,
            quote_close,
            server_info,
            current_db,
            driver_name,
            session_read_only: false,
        };
        // 第二道只读保险：在数据库会话上显式禁止写（失败则仅记录，不阻断连接）
        conn.apply_session_read_only();
        Ok(conn)
    }

    pub fn kind(&self) -> DbKind {
        self.kind
    }

    pub fn server_info(&self) -> &str {
        &self.server_info
    }

    pub fn driver_name(&self) -> &str {
        &self.driver_name
    }

    /// 连接当前的默认库名（部分驱动为空字符串）
    pub fn current_db(&self) -> &str {
        &self.current_db
    }

    /// 按驱动给出的引用字符包裹标识符（内部同字符加倍转义）
    pub fn quote_ident(&self, name: &str) -> String {
        let mut s = String::with_capacity(name.len() + 2);
        s.push(self.quote_open);
        for ch in name.chars() {
            if ch == self.quote_close {
                s.push(self.quote_close);
            }
            s.push(ch);
        }
        s.push(self.quote_close);
        s
    }

    /// 表的完整限定名（各库层级不同）
    pub fn qualified_name(&self, t: &TableInfo) -> String {
        match self.kind {
            DbKind::MySql | DbKind::SqlServer => {
                if t.catalog.trim().is_empty() {
                    self.quote_ident(&t.name)
                } else if t.schema.trim().is_empty() {
                    format!("{}.{}", self.quote_ident(&t.catalog), self.quote_ident(&t.name))
                } else {
                    format!(
                        "{}.{}.{}",
                        self.quote_ident(&t.catalog),
                        self.quote_ident(&t.schema),
                        self.quote_ident(&t.name)
                    )
                }
            }
            DbKind::PostgreSql | DbKind::Oracle => {
                if t.schema.trim().is_empty() {
                    self.quote_ident(&t.name)
                } else {
                    format!("{}.{}", self.quote_ident(&t.schema), self.quote_ident(&t.name))
                }
            }
            DbKind::Generic => {
                // 通用 ODBC：目标库由连接串/DSN 决定，表名不再加目录限定
                //（Access 文本驱动会把目录名当 catalog 返回，限定后反而语法错误）
                if t.schema.trim().is_empty() {
                    self.quote_ident(&t.name)
                } else {
                    format!(
                        "{}.{}",
                        self.quote_ident(&t.schema),
                        self.quote_ident(&t.name)
                    )
                }
            }
        }
    }

    /// 生成取数 SQL（`cols` 为空 = 全部列；`max_rows` = 0 表示不限制）
    pub fn select_sql(&self, t: &TableInfo, cols: &[String], max_rows: usize) -> String {
        let col_list = if cols.is_empty() {
            "*".to_string()
        } else {
            cols.iter()
                .map(|c| self.quote_ident(c))
                .collect::<Vec<_>>()
                .join(", ")
        };
        match self.kind {
            DbKind::SqlServer => {
                let top = if max_rows > 0 {
                    format!("TOP ({}) ", max_rows)
                } else {
                    String::new()
                };
                format!("SELECT {}{} FROM {}", top, col_list, self.qualified_name(t))
            }
            DbKind::MySql | DbKind::PostgreSql => {
                let limit = if max_rows > 0 {
                    format!(" LIMIT {}", max_rows)
                } else {
                    String::new()
                };
                format!(
                    "SELECT {} FROM {}{}",
                    col_list,
                    self.qualified_name(t),
                    limit
                )
            }
            DbKind::Oracle => {
                let limit = if max_rows > 0 {
                    format!(" FETCH FIRST {} ROWS ONLY", max_rows)
                } else {
                    String::new()
                };
                format!(
                    "SELECT {} FROM {}{}",
                    col_list,
                    self.qualified_name(t),
                    limit
                )
            }
            DbKind::Generic => format!("SELECT {} FROM {}", col_list, self.qualified_name(t)),
        }
    }

    /// 「数据库 / 模式」列表（界面第一级下拉）
    pub fn scopes(&self) -> Result<Vec<String>, String> {
        let sql = match self.kind {
            DbKind::MySql => {
                "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA \
                 WHERE SCHEMA_NAME NOT IN ('information_schema','mysql','performance_schema','sys') \
                 ORDER BY SCHEMA_NAME"
            }
            DbKind::SqlServer => {
                "SELECT name FROM sys.databases WHERE state = 0 ORDER BY name"
            }
            DbKind::PostgreSql => {
                "SELECT schema_name FROM information_schema.schemata \
                 WHERE schema_name NOT LIKE 'pg_%' AND schema_name <> 'information_schema' \
                 ORDER BY schema_name"
            }
            DbKind::Oracle => "SELECT username FROM all_users ORDER BY username",
            // 通用 ODBC：连接串/DSN 已确定目标库，不再做「库」这一级分组，
            // 直接列出当前库（或当前目录）下的表；这样对 Access 文本/Excel 等
            // 把目录当作 catalog 的驱动也能正常取数。
            DbKind::Generic => return Ok(Vec::new()),
        };
        self.query_single_column(sql)
    }

    /// 某个「库/模式」下的表与视图
    pub fn tables(&self, scope: &str) -> Result<Vec<TableInfo>, String> {
        let mut stmt: Handle = SQL_NULL_HANDLE;
        let rc = unsafe { SQLAllocHandle(SQL_HANDLE_STMT, self.dbc, &mut stmt) };
        if !ok(rc) {
            return Err(format!("无法分配语句句柄：{}", diag(SQL_HANDLE_DBC, self.dbc)));
        }
        let cat_w = wide(scope);
        let types_w = wide("TABLE,VIEW");
        // 各库的层级不同：MySQL/SQL Server 用 catalog，PostgreSQL/Oracle 用 schema；
        // 空字符串按「不限制」处理（传 NULL），避免驱动把空串当作空目录名匹配
        let given = if scope.trim().is_empty() || self.kind == DbKind::Generic {
            std::ptr::null()
        } else {
            cat_w.as_ptr()
        };
        let (cat_ptr, sch_ptr) = match self.kind {
            DbKind::PostgreSql | DbKind::Oracle => (std::ptr::null(), given),
            _ => (given, std::ptr::null()),
        };
        let rc = unsafe {
            SQLTablesW(
                stmt,
                cat_ptr,
                SQL_NTS,
                sch_ptr,
                SQL_NTS,
                std::ptr::null(),
                SQL_NTS,
                types_w.as_ptr(),
                SQL_NTS,
            )
        };
        if !ok(rc) {
            let e = diag(SQL_HANDLE_STMT, stmt);
            unsafe { SQLFreeHandle(SQL_HANDLE_STMT, stmt) };
            return Err(format!("读取表清单失败：{}", e));
        }
        let mut out: Vec<TableInfo> = Vec::new();
        loop {
            let rc = unsafe { SQLFetch(stmt) };
            if rc == SQL_NO_DATA {
                break;
            }
            if !ok(rc) {
                let e = diag(SQL_HANDLE_STMT, stmt);
                unsafe { SQLFreeHandle(SQL_HANDLE_STMT, stmt) };
                return Err(format!("读取表清单失败：{}", e));
            }
            let catalog = get_data(stmt, 1)?.unwrap_or_default();
            let schema = get_data(stmt, 2)?.unwrap_or_default();
            let name = get_data(stmt, 3)?.unwrap_or_default();
            let kind = get_data(stmt, 4)?.unwrap_or_default();
            if name.trim().is_empty() {
                continue;
            }
            out.push(TableInfo {
                catalog,
                schema,
                name,
                kind,
            });
        }
        unsafe { SQLFreeHandle(SQL_HANDLE_STMT, stmt) };
        out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(out)
    }

    /// 某表的列名（按数据库中的顺序）
    pub fn columns(&self, t: &TableInfo) -> Result<Vec<String>, String> {
        let mut stmt: Handle = SQL_NULL_HANDLE;
        let rc = unsafe { SQLAllocHandle(SQL_HANDLE_STMT, self.dbc, &mut stmt) };
        if !ok(rc) {
            return Err(format!("无法分配语句句柄：{}", diag(SQL_HANDLE_DBC, self.dbc)));
        }
        let cat_w = wide(&t.catalog);
        let sch_w = wide(&t.schema);
        let table_w = wide(&t.name);
        let rc = unsafe {
            SQLColumnsW(
                stmt,
                if t.catalog.trim().is_empty() {
                    std::ptr::null()
                } else {
                    cat_w.as_ptr()
                },
                SQL_NTS,
                if t.schema.trim().is_empty() {
                    std::ptr::null()
                } else {
                    sch_w.as_ptr()
                },
                SQL_NTS,
                table_w.as_ptr(),
                SQL_NTS,
                std::ptr::null(),
                SQL_NTS,
            )
        };
        if !ok(rc) {
            let e = diag(SQL_HANDLE_STMT, stmt);
            unsafe { SQLFreeHandle(SQL_HANDLE_STMT, stmt) };
            return Err(format!("读取字段清单失败：{}", e));
        }
        let mut out: Vec<String> = Vec::new();
        loop {
            let rc = unsafe { SQLFetch(stmt) };
            if rc == SQL_NO_DATA {
                break;
            }
            if !ok(rc) {
                let e = diag(SQL_HANDLE_STMT, stmt);
                unsafe { SQLFreeHandle(SQL_HANDLE_STMT, stmt) };
                return Err(format!("读取字段清单失败：{}", e));
            }
            if let Some(name) = get_data(stmt, 4)? {
                if !name.trim().is_empty() {
                    out.push(name);
                }
            }
        }
        unsafe { SQLFreeHandle(SQL_HANDLE_STMT, stmt) };
        Ok(out)
    }

    /// 执行只读查询，逐行回调（回调签名：列名, 当前行；返回行数）。
    /// 回调返回 Err 即中止。`max_rows` = 0 表示不限制。
    pub fn for_each_row(
        &self,
        sql: &str,
        max_rows: usize,
        expected_total: Option<u64>,
        progress: Option<ProgressFn>,
        mut on_row: impl FnMut(&[String], &[String]) -> Result<(), String>,
    ) -> Result<(usize, Vec<String>), String> {
        let mut stmt: Handle = SQL_NULL_HANDLE;
        let rc = unsafe { SQLAllocHandle(SQL_HANDLE_STMT, self.dbc, &mut stmt) };
        if !ok(rc) {
            return Err(format!("无法分配语句句柄：{}", diag(SQL_HANDLE_DBC, self.dbc)));
        }
        // 驱动侧兜底限制；不改动数据库任何状态（见 SQL_ATTR_MAX_ROWS 处的说明：
        // 这里刻意不设置 ODBC 的查询超时）
        if max_rows > 0 {
            unsafe {
                SQLSetStmtAttrW(stmt, SQL_ATTR_MAX_ROWS, max_rows as *mut c_void, 0);
            }
        }
        let sql_w = wide(sql);
        let rc = unsafe { SQLExecDirectW(stmt, sql_w.as_ptr(), SQL_NTS as Integer) };
        if !ok(rc) {
            let e = diag(SQL_HANDLE_STMT, stmt);
            unsafe { SQLFreeHandle(SQL_HANDLE_STMT, stmt) };
            return Err(format!("查询失败：{}", e));
        }
        let mut ncols: SmallInt = 0;
        let rc = unsafe { SQLNumResultCols(stmt, &mut ncols) };
        if !ok(rc) || ncols <= 0 {
            let e = diag(SQL_HANDLE_STMT, stmt);
            unsafe {
                SQLFreeStmt(stmt, SQL_CLOSE);
                SQLFreeHandle(SQL_HANDLE_STMT, stmt);
            }
            return Err(format!("查询没有返回结果集：{}", e));
        }
        let ncols = ncols as usize;
        // 列名（空名/重名补齐，保证 CSV 表头可用）
        let mut header: Vec<String> = Vec::with_capacity(ncols);
        for i in 1..=ncols {
            let mut name = vec![0u16; 512];
            let mut name_len: SmallInt = 0;
            let mut dtype: SmallInt = 0;
            let mut size: ULen = 0;
            let mut dec: SmallInt = 0;
            let mut nullable: SmallInt = 0;
            let rc = unsafe {
                SQLDescribeColW(
                    stmt,
                    i as USmallInt,
                    name.as_mut_ptr(),
                    name.len() as SmallInt,
                    &mut name_len,
                    &mut dtype,
                    &mut size,
                    &mut dec,
                    &mut nullable,
                )
            };
            let mut col = if ok(rc) {
                from_wide(&name, name_len as i32)
            } else {
                String::new()
            };
            if col.trim().is_empty() {
                col = format!("列{}", i);
            }
            // 重名：追加序号
            let mut candidate = col.clone();
            let mut n = 2;
            while header.contains(&candidate) {
                candidate = format!("{}__{}", col, n);
                n += 1;
            }
            header.push(candidate);
        }
        let mut rows = 0usize;
        let total = progress_total(expected_total, max_rows);
        let mut buf = vec![0u16; 8192];
        let mut values: Vec<String> = Vec::with_capacity(ncols);
        loop {
            if max_rows > 0 && rows >= max_rows {
                break;
            }
            let rc = unsafe { SQLFetch(stmt) };
            if rc == SQL_NO_DATA {
                break;
            }
            if !ok(rc) {
                let e = diag(SQL_HANDLE_STMT, stmt);
                unsafe {
                    SQLFreeStmt(stmt, SQL_CLOSE);
                    SQLFreeHandle(SQL_HANDLE_STMT, stmt);
                }
                return Err(format!("读取数据失败（已读 {} 行）：{}", rows, e));
            }
            values.clear();
            for i in 1..=ncols {
                values.push(get_data_buf(stmt, i as USmallInt, &mut buf)?.unwrap_or_default());
            }
            on_row(&header, &values)?;
            rows += 1;
            if let Some(p) = progress {
                if rows % 2000 == 0 {
                    p(rows as u64, total);
                }
            }
        }
        unsafe {
            SQLFreeStmt(stmt, SQL_CLOSE);
            SQLFreeHandle(SQL_HANDLE_STMT, stmt);
        }
        if let Some(p) = progress {
            p(rows as u64, total);
        }
        Ok((rows, header))
    }

    /// 批量取数（按列数组绑定 + 一次 `SQLFetch` 取多行）。
    ///
    /// 逐格 `SQLGetData` 每行每列都要一次 ODBC 调用（55 列的表 = 4.6 亿次/百万行），
    /// 数组绑定把调用次数降到「批次数」量级：实测 10 万行 55 列从 2.2 万行/秒
    /// 提升到约 10~12 万行/秒（4~5 倍）。
    ///
    /// 只在完全可行时使用；遇到下列情况返回带 [`BATCH_FALLBACK`] 前缀的错误，
    /// 由调用方回退到逐格路径（任何驱动、任何数据都能正确取回）：
    /// - 驱动不支持数组绑定（`SQLSetStmtAttr` / `SQLBindCol` 失败）；
    /// - 某列声明宽度过大（`BATCH_MAX_COL_CHARS` 以上，如 TEXT/LONGTEXT）；
    /// - 实际取值超出绑定缓冲（会被截断）或返回未知长度。
    fn for_each_row_batched<F>(
        &self,
        sql: &str,
        max_rows: usize,
        expected_total: Option<u64>,
        progress: Option<ProgressFn>,
        mut on_row: F,
    ) -> Result<(usize, Vec<String>), String>
    where
        F: FnMut(&[String], &[String]) -> Result<(), String>,
    {
        let mut stmt: Handle = SQL_NULL_HANDLE;
        let rc = unsafe { SQLAllocHandle(SQL_HANDLE_STMT, self.dbc, &mut stmt) };
        if !ok(rc) {
            return Err(batch_fallback("无法分配语句句柄"));
        }
        // 回退（换取数方式可能成功）与真错（换方式也一样失败）分开处理
        macro_rules! bail {
            ($($arg:tt)*) => {{
                let msg = batch_fallback(&format!($($arg)*));
                free_stmt(stmt);
                return Err(msg);
            }};
        }
        macro_rules! fail {
            ($($arg:tt)*) => {{
                let msg = format!($($arg)*);
                free_stmt(stmt);
                return Err(msg);
            }};
        }

        let sql_w = wide(sql);
        let rc = unsafe { SQLExecDirectW(stmt, sql_w.as_ptr(), SQL_NTS as Integer) };
        if !ok(rc) {
            let e = diag(SQL_HANDLE_STMT, stmt);
            fail!("查询失败：{}", e);
        }
        let mut ncols: SmallInt = 0;
        if !ok(unsafe { SQLNumResultCols(stmt, &mut ncols) }) || ncols <= 0 {
            let e = diag(SQL_HANDLE_STMT, stmt);
            fail!("查询没有返回结果集：{}", e);
        }
        let ncols = ncols as usize;

        // 列名 + 声明宽度（决定每列的绑定缓冲大小）
        let mut header: Vec<String> = Vec::with_capacity(ncols);
        let mut col_bytes: Vec<usize> = Vec::with_capacity(ncols);
        // MySQL 的连接串已固定 utf8mb4，可直接按 UTF-8 取回（省一轮 UTF-16 转换）；
        // 其它驱动用 UTF-16 取回，避免驱动按系统代码页返回而中文乱码。
        let use_char = self.kind == DbKind::MySql;
        let char_bytes = if use_char { 4 } else { 2 }; // UTF-8 最坏 4 字节/字符；UTF-16 固定 2
        for i in 1..=ncols {
            let mut name = vec![0u16; 512];
            let mut name_len: SmallInt = 0;
            let mut dtype: SmallInt = 0;
            let mut size: ULen = 0;
            let mut dec: SmallInt = 0;
            let mut nullable: SmallInt = 0;
            let rc = unsafe {
                SQLDescribeColW(
                    stmt,
                    i as USmallInt,
                    name.as_mut_ptr(),
                    name.len() as SmallInt,
                    &mut name_len,
                    &mut dtype,
                    &mut size,
                    &mut dec,
                    &mut nullable,
                )
            };
            let mut col = if ok(rc) {
                from_wide(&name, name_len as i32)
            } else {
                String::new()
            };
            if col.trim().is_empty() {
                col = format!("列{}", i);
            }
            let mut candidate = col.clone();
            let mut n = 2;
            while header.contains(&candidate) {
                candidate = format!("{}__{}", col, n);
                n += 1;
            }
            header.push(candidate);
            // 声明宽度过大（TEXT/LONGTEXT 等）→ 交给逐格路径，避免分配巨量缓冲
            let declared = if size == 0 { 255 } else { size };
            if declared > BATCH_MAX_COL_CHARS {
                bail!("第 {} 列声明宽度 {} 字符过大", i, declared);
            }
            col_bytes.push(
                (declared * char_bytes + char_bytes).max(BATCH_MIN_COL_BYTES),
            );
        }
        let row_bytes: usize = col_bytes.iter().sum();
        let want = if max_rows == 0 {
            BATCH_MAX_ROWS
        } else {
            max_rows.clamp(1, BATCH_MAX_ROWS)
        };
        let batch = (BATCH_BUF_LIMIT / row_bytes.max(1)).clamp(1, want);

        let mut buffers: Vec<Vec<u8>> = col_bytes.iter().map(|w| vec![0u8; w * batch]).collect();
        let mut inds: Vec<Vec<Len>> = (0..ncols).map(|_| vec![0 as Len; batch]).collect();
        let mut status = vec![0u16; batch];
        let mut fetched: ULen = 0;
        let target = if use_char { SQL_C_CHAR } else { SQL_C_WCHAR };
        unsafe {
            if !ok(SQLSetStmtAttrW(
                stmt,
                SQL_ATTR_ROW_ARRAY_SIZE,
                batch as *mut c_void,
                0,
            )) {
                bail!("驱动不支持数组绑定（设置批量行数失败）");
            }
            SQLSetStmtAttrW(
                stmt,
                SQL_ATTR_ROW_STATUS_PTR,
                status.as_mut_ptr() as *mut c_void,
                0,
            );
            SQLSetStmtAttrW(
                stmt,
                SQL_ATTR_ROWS_FETCHED_PTR,
                &mut fetched as *mut ULen as *mut c_void,
                0,
            );
            for i in 0..ncols {
                let rc = SQLBindCol(
                    stmt,
                    (i + 1) as USmallInt,
                    target,
                    buffers[i].as_mut_ptr() as *mut c_void,
                    col_bytes[i] as Len,
                    inds[i].as_mut_ptr(),
                );
                if !ok(rc) {
                    bail!("第 {} 列绑定失败（驱动可能不支持数组绑定）", i + 1);
                }
            }
        }

        // 每行的字段缓冲（复用容量，避免逐行分配）
        let mut pool: Vec<String> = (0..ncols).map(|_| String::new()).collect();
        let mut rows = 0usize;
        let total = progress_total(expected_total, max_rows);
        let mut last_report = 0usize;
        let mut fatal: Option<String> = None;
        unsafe {
            loop {
                if max_rows > 0 && rows >= max_rows {
                    break;
                }
                let rc = SQLFetch(stmt);
                if rc == SQL_NO_DATA {
                    break;
                }
                if !ok(rc) {
                    fatal = Some(format!(
                        "读取数据失败（已读 {} 行）：{}",
                        rows,
                        diag(SQL_HANDLE_STMT, stmt)
                    ));
                    break;
                }
                let got = fetched as usize;
                if got == 0 {
                    break;
                }
                let mut aborted = false;
                for r in 0..got {
                    if max_rows > 0 && rows >= max_rows {
                        break;
                    }
                    for i in 0..ncols {
                        let ind = inds[i][r];
                        pool[i].clear();
                        if ind == SQL_NULL_DATA {
                            continue;
                        }
                        if ind < 0 {
                            fatal = Some(batch_fallback(&format!(
                                "第 {} 列返回未知长度",
                                i + 1
                            )));
                            aborted = true;
                            break;
                        }
                        let used = ind as usize;
                        if used >= col_bytes[i] {
                            fatal = Some(batch_fallback(&format!(
                                "第 {} 列取值超出绑定缓冲（{} 字节）",
                                i + 1,
                                used
                            )));
                            aborted = true;
                            break;
                        }
                        let off = r * col_bytes[i];
                        let cell = &buffers[i][off..off + used];
                        if use_char {
                            match std::str::from_utf8(cell) {
                                Ok(t) => pool[i].push_str(t),
                                Err(_) => {
                                    fatal = Some(batch_fallback(&format!(
                                        "第 {} 列 UTF-8 数据不完整",
                                        i + 1
                                    )));
                                    aborted = true;
                                    break;
                                }
                            }
                        } else {
                            let w = std::slice::from_raw_parts(
                                cell.as_ptr() as *const u16,
                                used / 2,
                            );
                            pool[i].push_str(&String::from_utf16_lossy(w));
                        }
                    }
                    if aborted {
                        break;
                    }
                    if let Err(e) = on_row(&header, &pool) {
                        fatal = Some(e);
                        aborted = true;
                        break;
                    }
                    rows += 1;
                }
                if aborted {
                    break;
                }
                if let Some(p) = progress {
                    if rows - last_report >= 2000 {
                        last_report = rows;
                        p(rows as u64, total);
                    }
                }
            }
        }
        free_stmt(stmt);
        if let Some(e) = fatal {
            return Err(e);
        }
        if let Some(p) = progress {
            p(rows as u64, total);
        }
        Ok((rows, header))
    }

    /// 给语句加上「单条语句最长执行时间」的数据库原生写法（MySQL 优化器提示）。
    ///
    /// 只作用于当前语句，不会有 ODBC `SQL_ATTR_QUERY_TIMEOUT` 那种连接级副作用；
    /// 其它数据库暂不加限制（这些元数据查询本身很快）。
    fn with_max_exec_time(&self, sql: &str, ms: u64) -> String {
        if self.kind == DbKind::MySql {
            if let Some(rest) = sql.strip_prefix("SELECT ") {
                // 优化器提示必须紧跟在 SELECT 之后
                return format!("SELECT /*+ MAX_EXECUTION_TIME({}) */ {}", ms, rest);
            }
        }
        sql.to_string()
    }

    /// 表的行数**估算**（读各库的系统统计视图，毫秒级返回）。用于取数进度百分比。
    ///
    /// 不用 `COUNT(*)` 的原因：InnoDB 等引擎统计全表行数需要扫描，几百万行就要数十秒，
    /// 让用户白等；系统视图里的行数是统计信息，作为进度分母足够（进度封顶 99%，
    /// 结束时才报 100%，因此估算偏差不会显示成「假的完成」）。
    /// 取不到（视图/权限/未 ANALYZE）返回 `None`，界面退化为「已读取 N 行」。
    pub fn estimate_rows(&self, t: &TableInfo) -> Option<u64> {
        let q = |v: &str| v.replace('\'', "''");
        let sql = match self.kind {
            DbKind::MySql => format!(
                "SELECT TABLE_ROWS FROM information_schema.TABLES \
                 WHERE TABLE_SCHEMA = '{}' AND TABLE_NAME = '{}'",
                q(&t.catalog),
                q(&t.name)
            ),
            DbKind::PostgreSql => format!(
                "SELECT reltuples::bigint FROM pg_class WHERE relname = '{}' \
                 AND relnamespace = (SELECT oid FROM pg_namespace WHERE nspname = '{}')",
                q(&t.name),
                q(&t.schema)
            ),
            DbKind::SqlServer => format!(
                "SELECT SUM(p.rows) FROM sys.partitions p \
                 WHERE p.object_id = OBJECT_ID('{}{}') AND p.index_id IN (0, 1)",
                if t.schema.trim().is_empty() {
                    String::new()
                } else {
                    format!("{}.", q(&t.schema))
                },
                q(&t.name)
            ),
            DbKind::Oracle => format!(
                "SELECT num_rows FROM all_tables WHERE owner = '{}' AND table_name = '{}'",
                q(&t.schema).to_uppercase(),
                q(&t.name).to_uppercase()
            ),
            DbKind::Generic => return None,
        };
        let mut out: Option<u64> = None;
        // 估算查询只读元数据、通常毫秒级；这里刻意不设 ODBC 查询超时
        //（原因见 SQL_ATTR_MAX_ROWS 处的说明）
        let ok = self.query_scalar_u64(&sql, &mut out).is_ok();
        if ok {
            out.filter(|n| *n > 0)
        } else {
            None
        }
    }

    /// 执行取单个无符号整数的查询（估算行数用）
    fn query_scalar_u64(&self, sql: &str, out: &mut Option<u64>) -> Result<(), String> {
        let mut stmt: Handle = SQL_NULL_HANDLE;
        let rc = unsafe { SQLAllocHandle(SQL_HANDLE_STMT, self.dbc, &mut stmt) };
        if !ok(rc) {
            return Err(format!("无法分配语句句柄：{}", diag(SQL_HANDLE_DBC, self.dbc)));
        }
        let mut err: Option<String> = None;
        unsafe {
            let sql_w = wide(sql);
            let rc = SQLExecDirectW(stmt, sql_w.as_ptr(), SQL_NTS as Integer);
            if !ok(rc) {
                err = Some(diag(SQL_HANDLE_STMT, stmt));
            } else if ok(SQLFetch(stmt)) {
                let mut buf = vec![0u16; 64];
                let mut ind: Len = 0;
                let rc = SQLGetData(
                    stmt,
                    1,
                    SQL_C_WCHAR,
                    buf.as_mut_ptr() as *mut c_void,
                    (buf.len() * 2) as Len,
                    &mut ind,
                );
                if ok(rc) && ind > 0 {
                    let used = (ind as usize / 2).min(buf.len() - 1);
                    let text = String::from_utf16_lossy(&buf[..used]);
                    if let Ok(n) = text.trim().parse::<u64>() {
                        *out = Some(n);
                    }
                }
            }
        }
        free_stmt(stmt);
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// 表的实际总行数（`SELECT COUNT(*)`，只读）。用于取数进度百分比。
    ///
    /// 语句超时 10 秒：统计不出来（超大表/视图/权限不足）就返回错误，
    /// 由调用方忽略并改为「已读取 N 行」的不确定进度，不影响正常取数。
    pub fn count_rows(&self, t: &TableInfo) -> Result<u64, String> {
        let sql = self.with_max_exec_time(
            &format!("SELECT COUNT(*) FROM {}", self.qualified_name(t)),
            10_000,
        );
        let mut stmt: Handle = SQL_NULL_HANDLE;
        let rc = unsafe { SQLAllocHandle(SQL_HANDLE_STMT, self.dbc, &mut stmt) };
        if !ok(rc) {
            return Err(format!("无法分配语句句柄：{}", diag(SQL_HANDLE_DBC, self.dbc)));
        }
        let mut result: Result<u64, String> = Err("未取到行数".to_string());
        unsafe {
            let sql_w = wide(&sql);
            let rc = SQLExecDirectW(stmt, sql_w.as_ptr(), SQL_NTS as Integer);
            if !ok(rc) {
                result = Err(format!("统计行数失败：{}", diag(SQL_HANDLE_STMT, stmt)));
            } else {
                let rc = SQLFetch(stmt);
                if ok(rc) {
                    let mut buf = vec![0u16; 64];
                    let mut ind: Len = 0;
                    let rc = SQLGetData(
                        stmt,
                        1,
                        SQL_C_WCHAR,
                        buf.as_mut_ptr() as *mut c_void,
                        (buf.len() * 2) as Len,
                        &mut ind,
                    );
                    if ok(rc) && ind > 0 {
                        let used = (ind as usize / 2).min(buf.len() - 1);
                        let text = String::from_utf16_lossy(&buf[..used]);
                        result = text
                            .trim()
                            .parse::<u64>()
                            .map_err(|_| format!("行数无法解析：{:?}", text));
                    }
                } else {
                    result = Err(format!("统计行数失败：{}", diag(SQL_HANDLE_STMT, stmt)));
                }
            }
        }
        free_stmt(stmt);
        result
    }

    /// 性能诊断用：只执行查询并逐行推进结果集，**不读取任何列**。
    ///
    /// 用来把「驱动把结果行推进来」的开销与「逐格 `SQLGetData` + 字符串转换」的开销分开
    /// （见 `src/bin/db_bench.rs`）。返回值是实际读取到的行数。
    pub fn count_rows_only(&self, sql: &str, max_rows: usize) -> Result<usize, String> {
        let mut stmt: Handle = SQL_NULL_HANDLE;
        let rc = unsafe { SQLAllocHandle(SQL_HANDLE_STMT, self.dbc, &mut stmt) };
        if !ok(rc) {
            return Err(format!("无法分配语句句柄：{}", diag(SQL_HANDLE_DBC, self.dbc)));
        }
        let sql_w = wide(sql);
        let rc = unsafe { SQLExecDirectW(stmt, sql_w.as_ptr(), SQL_NTS as Integer) };
        if !ok(rc) {
            let e = diag(SQL_HANDLE_STMT, stmt);
            unsafe { SQLFreeHandle(SQL_HANDLE_STMT, stmt) };
            return Err(format!("查询失败：{}", e));
        }
        let mut rows = 0usize;
        loop {
            if max_rows > 0 && rows >= max_rows {
                break;
            }
            let rc = unsafe { SQLFetch(stmt) };
            if rc == SQL_NO_DATA {
                break;
            }
            if !ok(rc) {
                let e = diag(SQL_HANDLE_STMT, stmt);
                unsafe {
                    SQLFreeStmt(stmt, SQL_CLOSE);
                    SQLFreeHandle(SQL_HANDLE_STMT, stmt);
                }
                return Err(format!("读取数据失败（已读 {} 行）：{}", rows, e));
            }
            rows += 1;
        }
        unsafe {
            SQLFreeStmt(stmt, SQL_CLOSE);
            SQLFreeHandle(SQL_HANDLE_STMT, stmt);
        }
        Ok(rows)
    }

    /// 读取整表/视图（`cols` 为空 = 全部列）到内存，用于预览
    pub fn fetch_rows(
        &self,
        t: &TableInfo,
        cols: &[String],
        max_rows: usize,
    ) -> Result<(Vec<String>, Vec<Vec<String>>), String> {
        let sql = self.select_sql(t, cols, max_rows);
        let collect = |data: &mut Vec<Vec<String>>, _h: &[String], row: &[String]| {
            data.push(row.to_vec());
            Ok(())
        };
        let mut data: Vec<Vec<String>> = Vec::new();
        let (_n, header) = match self.for_each_row_batched(&sql, max_rows, None, None, |h, row| {
            collect(&mut data, h, row)
        }) {
            Ok(v) => v,
            Err(e) if is_batch_fallback(&e) => {
                // 批量取数不可用（驱动不支持/超长字段等）→ 回退逐格取数，结果一致
                data.clear();
                self.for_each_row(&sql, max_rows, None, None, |h, row| {
                    collect(&mut data, h, row)
                })?
            }
            Err(e) => return Err(e),
        };
        Ok((header, data))
    }

    /// 读取整表/视图并写成 UTF-8 CSV（供编辑器加载），返回 (行数, 表头)。
    /// 表中没有数据时仍会写出仅含表头的 CSV，保证表头结构不丢。
    pub fn export_to_csv(
        &self,
        t: &TableInfo,
        cols: &[String],
        out_path: &str,
        max_rows: usize,
        expected_total: Option<u64>,
        progress: Option<ProgressFn>,
    ) -> Result<(usize, Vec<String>), String> {
        let sql = self.select_sql(t, cols, max_rows);
        let mut writer: Option<CsvWriter> = None;
        let (rows, header) =
            match self.for_each_row_batched(&sql, max_rows, expected_total, progress, |h, row| {
                write_row_ensure_header(&mut writer, out_path, h, row)
            }) {
                Ok(v) => v,
                Err(e) if is_batch_fallback(&e) => {
                    // 回退逐格取数：重新创建写出器（截断已写入的部分），结果与批量路径一致
                    writer = None;
                    self.for_each_row(&sql, max_rows, expected_total, progress, |h, row| {
                        write_row_ensure_header(&mut writer, out_path, h, row)
                    })?
                }
                Err(e) => return Err(e),
            };
        match writer {
            Some(w) => w.finish().map_err(|e| e.to_string())?,
            None => {
                // 空结果：写一份只有表头的 CSV
                let w = CsvWriter::new_raw(out_path, &header).map_err(|e| e.to_string())?;
                w.finish().map_err(|e| e.to_string())?;
            }
        }
        Ok((rows, header))
    }

    /// 执行一条不返回结果集的语句（如会话设置），成功返回 Ok
    fn exec_direct(&self, sql: &str) -> Result<(), String> {
        let mut stmt: Handle = SQL_NULL_HANDLE;
        let rc = unsafe { SQLAllocHandle(SQL_HANDLE_STMT, self.dbc, &mut stmt) };
        if !ok(rc) {
            return Err(format!("无法分配语句句柄：{}", diag(SQL_HANDLE_DBC, self.dbc)));
        }
        let sql_w = wide(sql);
        let rc = unsafe { SQLExecDirectW(stmt, sql_w.as_ptr(), SQL_NTS as Integer) };
        let err = if ok(rc) {
            String::new()
        } else {
            diag(SQL_HANDLE_STMT, stmt)
        };
        unsafe {
            SQLFreeStmt(stmt, SQL_CLOSE);
            SQLFreeHandle(SQL_HANDLE_STMT, stmt);
        }
        if err.is_empty() {
            Ok(())
        } else {
            Err(err)
        }
    }

    /// 连接建立后设置会话级只读（失败不影响连接，仅少一道数据库侧保护）
    fn apply_session_read_only(&mut self) {
        let Some(sql) = read_only_session_sql(self.kind) else {
            return;
        };
        if self.exec_direct(sql).is_ok() {
            self.session_read_only = true;
        }
    }

    /// 数据库侧是否已确认进入只读会话（MySQL / PostgreSQL 有效）
    pub fn session_read_only(&self) -> bool {
        self.session_read_only
    }

    /// 单列查询（库/模式列表等元数据用）
    fn query_single_column(&self, sql: &str) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        self.for_each_row(sql, 0, None, None, |_h, row: &[String]| {
            if let Some(v) = row.first() {
                if !v.trim().is_empty() {
                    out.push(v.trim().to_string());
                }
            }
            Ok(())
        })?;
        out.sort();
        out.dedup();
        Ok(out)
    }

}

/// 仅用于测试：构造一个不持有真实句柄的连接（`Drop` 对空句柄安全），
/// 以便验证各数据库方言下的限定名 / 取数 SQL 生成。
#[cfg(test)]
impl DbConn {
    fn fake(kind: DbKind, quote_open: char, quote_close: char, current_db: &str) -> Self {
        Self {
            env: SQL_NULL_HANDLE,
            dbc: SQL_NULL_HANDLE,
            kind,
            quote_open,
            quote_close,
            server_info: String::new(),
            current_db: current_db.to_string(),
            driver_name: String::new(),
            session_read_only: false,
        }
    }
}

impl Drop for DbConn {
    fn drop(&mut self) {
        unsafe {
            if !self.dbc.is_null() {
                SQLDisconnect(self.dbc);
                SQLFreeHandle(SQL_HANDLE_DBC, self.dbc);
                self.dbc = SQL_NULL_HANDLE;
            }
            if !self.env.is_null() {
                SQLFreeHandle(SQL_HANDLE_ENV, self.env);
                self.env = SQL_NULL_HANDLE;
            }
        }
    }
}

/// 取回一列数据（UTF-16 分块读，长文本不截断）；NULL → None
fn get_data(stmt: Handle, col: USmallInt) -> Result<Option<String>, String> {
    let mut buf = vec![0u16; 4096];
    get_data_buf(stmt, col, &mut buf)
}

fn get_data_buf(stmt: Handle, col: USmallInt, buf: &mut [u16]) -> Result<Option<String>, String> {
    let mut out: Vec<u16> = Vec::new();
    loop {
        let mut ind: Len = 0;
        let rc = unsafe {
            SQLGetData(
                stmt,
                col,
                SQL_C_WCHAR,
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * 2) as Len,
                &mut ind,
            )
        };
        if rc == SQL_ERROR || rc == SQL_INVALID_HANDLE {
            // 单个字段取回失败（如二进制大对象）→ 视为空值，不影响整行
            return Ok(None);
        }
        if ind == SQL_NULL_DATA {
            return Ok(None);
        }
        let used = if ind > 0 {
            (ind as usize / 2).min(buf.len().saturating_sub(1))
        } else {
            buf.iter().position(|&c| c == 0).unwrap_or(0)
        };
        out.extend_from_slice(&buf[..used]);
        if rc == SQL_SUCCESS {
            break;
        }
        if used == 0 {
            break;
        }
        if out.len() > 64 * 1024 * 1024 {
            break; // 单字段 64M 字符上限，防御性保护
        }
    }
    Ok(Some(String::from_utf16_lossy(&out)))
}

/// 写出前确保 CSV 写出器已就绪（表头来自结果集列名）
fn write_row_ensure_header(
    writer: &mut Option<CsvWriter>,
    out_path: &str,
    header: &[String],
    row: &[String],
) -> Result<(), String> {
    if writer.is_none() {
        *writer = Some(CsvWriter::new_raw(out_path, header).map_err(|e| e.to_string())?);
    }
    writer
        .as_mut()
        .expect("写出器已创建")
        .write_row(row)
        .map_err(|e| e.to_string())
}

/// 读取驱动的字符型信息（如 DBMS 名）
fn get_info_str(dbc: Handle, info: USmallInt) -> String {
    let mut buf = vec![0u16; 512];
    let mut len: SmallInt = 0;
    let rc = unsafe {
        SQLGetInfoW(
            dbc,
            info,
            buf.as_mut_ptr() as *mut c_void,
            (buf.len() * 2) as SmallInt,
            &mut len,
        )
    };
    if !ok(rc) {
        return String::new();
    }
    // 注意：SQLGetInfo 的 StringLengthPtr 是「字节数」（不含结尾 NUL），需除以 2 得字符数
    let chars = (len.max(0) as usize) / 2;
    let mut out = from_wide(&buf, chars as i32);
    // 个别驱动会多返回 NUL 填充，统一截断
    if let Some(pos) = out.find('\0') {
        out.truncate(pos);
    }
    out
}

/// 读取驱动的单字符信息（标识符引用字符）
fn get_info_char(dbc: Handle, info: USmallInt) -> Option<char> {
    let s = get_info_str(dbc, info);
    s.chars().next()
}

/// 从连接串回读结果里取某个键的值
fn extract_conn_value(cs: &str, key: &str) -> Option<String> {
    cs.split(';').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        if k.trim().eq_ignore_ascii_case(key) {
            Some(v.trim().to_string())
        } else {
            None
        }
    })
}

// ==================== 临时文件管理 ====================

/// 数据库取数临时 CSV 的文件名前缀（自动清理据此识别本程序产生的文件）
pub const DB_TEMP_PREFIX: &str = "csv_editor_db_";

/// 取数使用的本机临时 CSV 路径（每次取数换一个名字，避免与正在使用的文件冲突）
pub fn temp_csv_path() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "{}{}_{}.csv",
        DB_TEMP_PREFIX,
        std::process::id(),
        n
    ))
}

/// 清理系统临时目录里本程序产生的数据库临时 CSV（返回「删除文件数, 释放字节数」）。
///
/// 之所以需要它：从数据库读出的数据要先落成本机临时 CSV 再交给编辑器（6GB 的表
/// 就是 5GB 的文件），程序若异常结束、或被直接关掉，这些文件会一直留在 `%TEMP%`
/// 里越积越多。调用时机：
/// - 程序启动时（清掉上次遗留的）；
/// - 程序退出时（清掉本次的）；
/// - 每次发起新的取数前（清掉上一次的）；
/// - 菜单「设置 → 清理数据库临时文件」可手动清理。
///
/// 安全说明：只删除匹配 [`DB_TEMP_PREFIX`] 的普通文件；正在被本程序
/// （或另一个实例）内存映射的文件在 Windows 上会删除失败，失败即跳过，
/// 因此不会影响正在使用的数据。
pub fn cleanup_temp_csvs() -> (usize, u64) {
    cleanup_temp_csvs_in(&std::env::temp_dir())
}

/// 清理本程序遗留的数据库临时 CSV，但**保留** `keep`（正在使用的那一个）。
/// 用于「刚导出新文件后顺带清理旧文件」的场景。
pub fn cleanup_temp_csvs_except(keep: &std::path::Path) -> (usize, u64) {
    cleanup_in(&std::env::temp_dir(), Some(keep))
}

/// 同 [`cleanup_temp_csvs`]，但指定目录（便于测试）
pub fn cleanup_temp_csvs_in(dir: &std::path::Path) -> (usize, u64) {
    cleanup_in(dir, None)
}

fn cleanup_in(dir: &std::path::Path, keep: Option<&std::path::Path>) -> (usize, u64) {
    let mut files = 0usize;
    let mut bytes = 0u64;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with(DB_TEMP_PREFIX) || !name.ends_with(".csv") {
            continue;
        }
        if let Some(k) = keep {
            if path == k {
                continue; // 正在使用中，保留
            }
        }
        // 只处理普通文件（避免误删同名目录）
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let size = meta.len();
        if std::fs::remove_file(&path).is_ok() {
            files += 1;
            bytes += size;
        }
    }
    (files, bytes)
}

// ==================== 连接配置持久化 ====================

/// 配置文件路径：exe 同目录（不可写则回退 %TEMP%）
pub fn config_path() -> PathBuf {
    const FILE: &str = "数据库连接配置.ini";
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // 只探测目录可写性（不直接创建配置文件）：否则启动即会留下一个空的
            // 「数据库连接配置.ini」，看起来像多了个莫名的文件
            if dir_writable(dir) {
                return dir.join(FILE);
            }
        }
    }
    std::env::temp_dir().join(FILE)
}

/// 目录是否可写（写一个探针文件再删掉，不在目标位置留下空文件）
fn dir_writable(dir: &std::path::Path) -> bool {
    let probe = dir.join(".csv_editor_write_probe");
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 值转义（`\` `\n` `\r`），保证一行一个键值
fn esc(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn unesc(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('\\') => out.push('\\'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// 读取本机保存的连接配置（文件不存在 → 空列表）
pub fn load_configs() -> Vec<DbConfig> {
    let Ok(text) = std::fs::read_to_string(config_path()) else {
        return Vec::new();
    };
    let mut out: Vec<DbConfig> = Vec::new();
    let mut cur: Option<DbConfig> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.eq_ignore_ascii_case("[conn]") {
            if let Some(c) = cur.take() {
                out.push(c);
            }
            cur = Some(DbConfig::default());
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let Some(c) = cur.as_mut() else { continue };
        let v = unesc(v.trim());
        match k.trim().to_ascii_lowercase().as_str() {
            "name" => c.name = v,
            "kind" => c.kind = DbKind::from_key(&v),
            "host" => c.host = v,
            "port" => c.port = v,
            "database" => c.database = v,
            "user" => c.user = v,
            "password" => c.password = v,
            "save_password" => c.save_password = v == "1" || v.eq_ignore_ascii_case("true"),
            "driver" => c.driver = v,
            "dsn" => c.dsn = v,
            "conn_str" => c.conn_str = v,
            _ => {}
        }
    }
    if let Some(c) = cur.take() {
        out.push(c);
    }
    out
}

/// 写入本机连接配置（密码仅在 `save_password` 为真时落盘，且为明文）
pub fn save_configs(list: &[DbConfig]) -> Result<(), String> {
    let path = config_path();
    let mut text = String::new();
    text.push_str("# 宿经03超大CSV文件实用编辑器 - 数据库连接配置\n");
    text.push_str("# 说明：勾选了「保存密码」的连接，密码以明文保存在本机该文件中；\n");
    text.push_str("#       如需修改，可直接编辑本文件后重启程序（本程序只读取数据库，不改数据库数据）。\n");
    for c in list {
        text.push_str("[conn]\n");
        text.push_str(&format!("name={}\n", esc(c.name.trim())));
        text.push_str(&format!("kind={}\n", c.kind.key()));
        text.push_str(&format!("host={}\n", esc(c.host.trim())));
        text.push_str(&format!("port={}\n", esc(c.port.trim())));
        text.push_str(&format!("database={}\n", esc(c.database.trim())));
        text.push_str(&format!("user={}\n", esc(c.user.trim())));
        text.push_str(&format!(
            "password={}\n",
            esc(if c.save_password { &c.password } else { "" })
        ));
        text.push_str(&format!("save_password={}\n", if c.save_password { 1 } else { 0 }));
        text.push_str(&format!("driver={}\n", esc(c.driver.trim())));
        text.push_str(&format!("dsn={}\n", esc(c.dsn.trim())));
        text.push_str(&format!("conn_str={}\n", esc(c.conn_str.trim())));
    }
    std::fs::write(&path, text).map_err(|e| format!("保存配置失败（{}）：{}", path.display(), e))
}

/// 按名称新增或覆盖一条配置（名称为空时用显示名兜底）
pub fn upsert_config(list: &mut Vec<DbConfig>, mut cfg: DbConfig) {
    if cfg.name.trim().is_empty() {
        cfg.name = cfg.display_name();
    }
    if let Some(pos) = list.iter().position(|c| c.name == cfg.name) {
        list[pos] = cfg;
    } else {
        list.push(cfg);
    }
}

/// 删除同名配置，返回是否删除了
pub fn remove_config(list: &mut Vec<DbConfig>, name: &str) -> bool {
    let before = list.len();
    list.retain(|c| c.name != name);
    before != list.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_conn_str_mysql() {
        let cfg = DbConfig {
            name: "t".into(),
            kind: DbKind::MySql,
            host: "127.0.0.1".into(),
            port: "3306".into(),
            database: "demo".into(),
            user: "root".into(),
            password: "秘密".into(),
            ..Default::default()
        };
        let drivers = vec!["MySQL ODBC 8.0 Unicode Driver".to_string()];
        let cs = cfg.build_conn_str(&drivers);
        assert!(cs.contains("DRIVER={MySQL ODBC 8.0 Unicode Driver}"));
        assert!(cs.contains("SERVER=127.0.0.1"));
        assert!(cs.contains("PORT=3306"));
        assert!(cs.contains("DATABASE=demo"));
        assert!(cs.contains("UID=root"));
        assert!(cs.contains("PWD=秘密"));
        // 脱敏后不出现密码
        assert!(!redact_password(&cs).contains("秘密"));
        assert!(redact_password(&cs).contains("PWD=***"));
    }

    #[test]
    fn build_conn_str_sqlserver_and_pg() {
        let ss = DbConfig {
            kind: DbKind::SqlServer,
            host: "srv".into(),
            port: "1433".into(),
            database: "db".into(),
            user: "sa".into(),
            password: "p".into(),
            ..Default::default()
        };
        let cs = ss.build_conn_str(&["ODBC Driver 18 for SQL Server".to_string()]);
        assert!(cs.contains("SERVER=srv,1433"));
        assert!(cs.contains("TrustServerCertificate=Yes"));
        assert!(cs.contains("ApplicationIntent=ReadOnly"));

        let pg = DbConfig {
            kind: DbKind::PostgreSql,
            host: "srv".into(),
            port: "5432".into(),
            database: "db".into(),
            user: "u".into(),
            ..Default::default()
        };
        let cs = pg.build_conn_str(&["PostgreSQL Unicode(x64)".to_string()]);
        assert!(cs.contains("DRIVER={PostgreSQL Unicode(x64)}"));
        assert!(cs.contains("ReadOnly=1"));
    }

    #[test]
    fn build_conn_str_dsn_and_manual() {
        let dsn = DbConfig {
            kind: DbKind::Generic,
            dsn: "MyDSN".into(),
            user: "u".into(),
            password: "p".into(),
            ..Default::default()
        };
        let cs = dsn.build_conn_str(&[]);
        assert!(cs.starts_with("DSN=MyDSN;"));
        assert!(cs.contains("UID=u"));

        // 手写连接串：未写 PWD 时自动补密码
        let manual = DbConfig {
            kind: DbKind::Oracle,
            conn_str: "DRIVER={Oracle in OraClient19Home1};DBQ=h:1521/svc".into(),
            password: "pw".into(),
            ..Default::default()
        };
        let cs = manual.build_conn_str(&[]);
        assert!(cs.ends_with("PWD=pw;"));
        // 已含 PWD 就不再追加
        let manual2 = DbConfig {
            conn_str: "DSN=x;PWD=abc".into(),
            password: "zzz".into(),
            ..Default::default()
        };
        assert_eq!(manual2.build_conn_str(&[]).matches("PWD=").count(), 1);
    }

    #[test]
    fn pick_driver_by_kind() {
        let drivers = vec![
            "SQL Server".to_string(),
            "ODBC Driver 17 for SQL Server".to_string(),
            "MySQL ODBC 5.3 ANSI Driver".to_string(),
            "MySQL ODBC 8.0 Unicode Driver".to_string(),
            "PostgreSQL ANSI".to_string(),
            "PostgreSQL Unicode".to_string(),
        ];
        assert_eq!(
            pick_driver(DbKind::MySql, &drivers).as_deref(),
            Some("MySQL ODBC 8.0 Unicode Driver")
        );
        assert_eq!(
            pick_driver(DbKind::SqlServer, &drivers).as_deref(),
            Some("ODBC Driver 17 for SQL Server")
        );
        assert_eq!(
            pick_driver(DbKind::PostgreSql, &drivers).as_deref(),
            Some("PostgreSQL Unicode")
        );
        assert_eq!(pick_driver(DbKind::Oracle, &drivers), None);
        // 只有老驱动时也能选到
        let legacy = vec!["SQL Server".to_string()];
        assert_eq!(
            pick_driver(DbKind::SqlServer, &legacy).as_deref(),
            Some("SQL Server")
        );
    }

    #[test]
    fn kind_roundtrip() {
        for k in DbKind::ALL {
            assert_eq!(DbKind::from_key(k.key()), k);
        }
        assert_eq!(DbKind::from_key("MariaDB"), DbKind::MySql);
        assert_eq!(DbKind::from_key("未知"), DbKind::Generic);
    }

    #[test]
    fn qualified_name_and_select_sql_per_kind() {
        let t = TableInfo {
            catalog: "demo".into(),
            schema: "dbo".into(),
            name: "订 单".into(),
            kind: "TABLE".into(),
        };

        // MySQL：库.表（LIMIT 限制行数）
        let my = DbConn::fake(DbKind::MySql, '`', '`', "demo");
        assert_eq!(my.qualified_name(&t), "`demo`.`dbo`.`订 单`");
        assert_eq!(my.select_sql(&t, &[], 100), "SELECT * FROM `demo`.`dbo`.`订 单` LIMIT 100");

        // SQL Server：三段名 + TOP，标识符用方括号
        let ss = DbConn::fake(DbKind::SqlServer, '[', ']', "demo");
        assert_eq!(ss.qualified_name(&t), "[demo].[dbo].[订 单]");
        assert_eq!(
            ss.select_sql(&t, &["编号".into(), "金额".into()], 10),
            "SELECT TOP (10) [编号], [金额] FROM [demo].[dbo].[订 单]"
        );
        // 上限为 0（不限制）时不加 TOP / LIMIT
        assert_eq!(
            ss.select_sql(&t, &[], 0),
            "SELECT * FROM [demo].[dbo].[订 单]"
        );

        // PostgreSQL：模式.表，忽略 catalog
        let pg = DbConn::fake(DbKind::PostgreSql, '"', '"', "demo");
        assert_eq!(pg.qualified_name(&t), "\"dbo\".\"订 单\"");
        assert_eq!(
            pg.select_sql(&t, &[], 50),
            "SELECT * FROM \"dbo\".\"订 单\" LIMIT 50"
        );

        // Oracle：owner.表 + FETCH FIRST
        let ora = DbConn::fake(DbKind::Oracle, '"', '"', "");
        assert_eq!(ora.qualified_name(&t), "\"dbo\".\"订 单\"");
        assert_eq!(
            ora.select_sql(&t, &[], 5),
            "SELECT * FROM \"dbo\".\"订 单\" FETCH FIRST 5 ROWS ONLY"
        );

        // 通用 ODBC：目标库由连接串决定，表名不加目录限定（只保留 schema）
        let gen = DbConn::fake(DbKind::Generic, '`', '`', "demo");
        assert_eq!(gen.qualified_name(&t), "`dbo`.`订 单`");
        let plain = TableInfo {
            catalog: "其它目录".into(),
            schema: String::new(),
            name: "orders.csv".into(),
            kind: "TABLE".into(),
        };
        assert_eq!(gen.qualified_name(&plain), "`orders.csv`");
    }

    #[test]
    fn quote_ident_escapes_quote_char() {
        let my = DbConn::fake(DbKind::MySql, '`', '`', "");
        assert_eq!(my.quote_ident("a`b"), "`a``b`");
        let ss = DbConn::fake(DbKind::SqlServer, '[', ']', "");
        assert_eq!(ss.quote_ident("a]b"), "[a]]b]");
        let q = DbConn::fake(DbKind::Oracle, '"', '"', "");
        assert_eq!(q.quote_ident("A\"B"), "\"A\"\"B\"");
    }

    #[test]
    fn redact_hides_password() {
        let cs = "DRIVER={x};SERVER=h;UID=u;PWD=secret;DATABASE=d";
        let r = redact_password(cs);
        assert!(!r.contains("secret"));
        assert!(r.contains("PWD=***"));
        assert!(r.contains("SERVER=h"));
    }

    #[test]
    fn config_save_load_roundtrip() {
        // 直接测试序列化/解析（不落盘到 exe 目录，避免影响本机配置）
        let cfg = DbConfig {
            name: "本地 MySQL".into(),
            kind: DbKind::MySql,
            host: "127.0.0.1".into(),
            port: "3306".into(),
            database: "demo".into(),
            user: "root".into(),
            password: "p@ss\\word\n换行".into(),
            save_password: true,
            driver: "MySQL ODBC 8.0 Unicode Driver".into(),
            dsn: String::new(),
            conn_str: String::new(),
        };
        let text = format!(
            "[conn]\nname={}\nkind={}\nhost={}\nport={}\ndatabase={}\nuser={}\npassword={}\nsave_password=1\ndriver={}\ndsn=\nconn_str=\n",
            esc(&cfg.name),
            cfg.kind.key(),
            esc(&cfg.host),
            esc(&cfg.port),
            esc(&cfg.database),
            esc(&cfg.user),
            esc(&cfg.password),
            esc(&cfg.driver),
        );
        // 复刻解析流程（避免写真实配置文件）
        let mut list: Vec<DbConfig> = Vec::new();
        let mut cur: Option<DbConfig> = None;
        for line in text.lines() {
            if line.trim().eq_ignore_ascii_case("[conn]") {
                if let Some(c) = cur.take() {
                    list.push(c);
                }
                cur = Some(DbConfig::default());
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let Some(c) = cur.as_mut() else { continue };
            let v = unesc(v.trim());
            match k.trim() {
                "name" => c.name = v,
                "kind" => c.kind = DbKind::from_key(&v),
                "host" => c.host = v,
                "port" => c.port = v,
                "database" => c.database = v,
                "user" => c.user = v,
                "password" => c.password = v,
                "save_password" => c.save_password = v == "1",
                "driver" => c.driver = v,
                _ => {}
            }
        }
        if let Some(c) = cur.take() {
            list.push(c);
        }
        assert_eq!(list.len(), 1);
        let got = &list[0];
        assert_eq!(got.name, cfg.name);
        assert_eq!(got.kind, cfg.kind);
        assert_eq!(got.password, cfg.password); // 含反斜杠与换行也能往返
        assert!(got.save_password);
    }

    #[test]
    fn upsert_and_remove() {
        let mut list: Vec<DbConfig> = Vec::new();
        let mut a = DbConfig::default();
        a.name = "A".into();
        a.host = "h1".into();
        upsert_config(&mut list, a.clone());
        assert_eq!(list.len(), 1);
        // 同名覆盖
        let mut b = DbConfig::default();
        b.name = "A".into();
        b.host = "h2".into();
        upsert_config(&mut list, b);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].host, "h2");
        assert!(remove_config(&mut list, "A"));
        assert!(list.is_empty());
        assert!(!remove_config(&mut list, "A"));
    }

    #[test]
    fn unesc_handles_backslash() {
        assert_eq!(unesc("a\\\\b"), "a\\b");
        assert_eq!(unesc("a\\nb"), "a\nb");
        assert_eq!(unesc(r"trailing\"), "trailing");
    }

    /// 没装对应 ODBC 驱动时，必须给出可操作的原因（而不是 ODBC 的 IM002 原文）
    #[test]
    fn connect_without_driver_gives_actionable_error() {
        // 传入空的驱动列表 = 本机没有该类型的 ODBC 驱动
        let cfg = DbConfig {
            kind: DbKind::MySql,
            host: "127.0.0.1".into(),
            port: "3308".into(),
            database: "demo".into(),
            user: "root".into(),
            password: "p".into(),
            ..Default::default()
        };
        let err = DbConn::connect(&cfg, &[]).err().expect("应当返回错误");
        assert!(err.contains("ODBC 驱动"), "错误信息应说明缺少驱动：{}", err);
        assert!(err.contains("MySQL Connector/ODBC"), "应给出安装指引：{}", err);
        assert!(err.contains("DSN"), "应给出 DSN 备选方案：{}", err);

        // 手写了 DRIVER= 的连接串时不再触发该提示（会走到真正的连接尝试）
        let manual = DbConfig {
            kind: DbKind::Generic,
            conn_str: "DRIVER={不存在但已指定的驱动};SERVER=x".into(),
            ..Default::default()
        };
        let err2 = DbConn::connect(&manual, &[]).err().expect("应当返回错误");
        assert!(!err2.contains("既没有 DRIVER 也没有 DSN"), "{}", err2);
    }

    /// 建一个真实 MySQL 测试连接（未设置 MYSQL_PWD 或连不上则返回 None → 调用方跳过）
    fn mysql_test_conn() -> Option<(DbConn, TableInfo)> {
        let pwd = std::env::var("MYSQL_PWD").ok()?;
        let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        let cfg = DbConfig {
            kind: DbKind::MySql,
            host: env("DB_TEST_HOST", "127.0.0.1"),
            port: env("DB_TEST_PORT", "3308"),
            database: env("DB_TEST_DB", "cbd2026"),
            user: env("DB_TEST_USER", "root"),
            password: pwd,
            ..Default::default()
        };
        let conn = match DbConn::connect(&cfg, &installed_drivers()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("跳过：无法连接测试库（{}）", e);
                return None;
            }
        };
        let t = TableInfo {
            catalog: cfg.database.clone(),
            schema: String::new(),
            name: env("DB_TEST_TABLE", "jymxb"),
            kind: "TABLE".into(),
        };
        Some((conn, t))
    }

    /// 元数据查询（行数估算）不得给同一连接留下「语句时长限制」。
    ///
    /// 背景（实测踩过的坑）：MySQL Connector/ODBC 会把 ODBC 的
    /// `SQL_ATTR_QUERY_TIMEOUT` 当成**连接级**限制 —— 给元数据查询设过 5 秒后，
    /// 同一连接上后续的全表取数会在 5.0 秒整被服务端中断
    /// （`Query execution was interrupted, maximum statement execution time exceeded`）。
    /// 本用例先做一次行数估算，再执行 7 秒的查询，确认不再被提前中断。
    #[test]
    fn estimate_does_not_throttle_later_queries() {
        let Some((conn, t)) = mysql_test_conn() else {
            eprintln!("跳过：需要本机可连的 MySQL（设 MYSQL_PWD）");
            return;
        };
        // 服务端自身若限制了语句时长，本用例失去意义 → 跳过
        let mut server_limit = String::new();
        let _ = conn.for_each_row(
            "SELECT @@session.max_execution_time",
            0,
            None,
            None,
            |_h, row| {
                server_limit = row.join("");
                Ok(())
            },
        );
        if server_limit.trim() != "0" {
            eprintln!("跳过：服务端 max_execution_time = {}", server_limit);
            return;
        }

        // 先做行数估算（曾经就是这一步埋下 5 秒限制）
        let _ = conn.estimate_rows(&t);

        let t0 = std::time::Instant::now();
        let mut rows = 0usize;
        conn.for_each_row("SELECT SLEEP(7)", 0, None, None, |_h, _r| {
            rows += 1;
            Ok(())
        })
        .expect("SLEEP(7) 应当能完整执行");
        let secs = t0.elapsed().as_secs_f64();
        assert_eq!(rows, 1);
        assert!(
            secs >= 6.5,
            "7 秒查询只用了 {:.1}s 就被中断：语句时长限制泄漏到了后续查询",
            secs
        );
        println!("元数据查询后长查询正常完成：SLEEP(7) 实际 {:.1}s", secs);
    }

    /// 批量取数（数组绑定）与逐格取数结果必须完全一致。
    ///
    /// 需要本机可连的 MySQL（用 MYSQL_PWD 提供密码，并可用 DB_TEST_* 覆盖连接参数）；
    /// 没有可用数据库时自动跳过，不影响其它环境跑测试。
    #[test]
    fn batched_matches_per_cell_on_real_mysql() {
        let Some((conn, t)) = mysql_test_conn() else {
            eprintln!("跳过：需要本机可连的 MySQL（设 MYSQL_PWD）");
            return;
        };
        let rows_wanted = std::env::var("DB_TEST_ROWS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20_000usize);
        let sql = conn.select_sql(&t, &[], rows_wanted);

        // 逐格路径
        let mut per_cell: Vec<Vec<String>> = Vec::new();
        let (n1, h1) = match conn.for_each_row(&sql, rows_wanted, None, None, |_h, row| {
            per_cell.push(row.to_vec());
            Ok(())
        }) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("跳过：逐格取数失败（{}）", e);
                return;
            }
        };
        // 批量路径
        let mut batched: Vec<Vec<String>> = Vec::new();
        let (n2, h2) = conn
            .for_each_row_batched(&sql, rows_wanted, None, None, |_h, row| {
                batched.push(row.to_vec());
                Ok(())
            })
            .expect("批量取数不应失败");

        assert_eq!(h1, h2, "两条路径的列名应一致");
        assert_eq!(n1, n2, "两条路径的行数应一致");
        assert_eq!(per_cell.len(), batched.len());
        for (i, (a, b)) in per_cell.iter().zip(batched.iter()).enumerate() {
            assert_eq!(a, b, "第 {} 行的取值不一致", i + 1);
        }
        println!(
            "批量与逐格取数完全一致：{} 行 × {} 列",
            batched.len(),
            h1.len()
        );
    }

    /// 进度分母的推导规则
    #[test]
    fn progress_total_rules() {
        // 不限行数 + 未知总数 → 不确定进度
        assert_eq!(progress_total(None, 0), None);
        // 不限行数 + 有总数 → 用总数
        assert_eq!(progress_total(Some(5000), 0), Some(5000));
        // 有行数上限、无总数 → 用上限
        assert_eq!(progress_total(None, 1000), Some(1000));
        // 上限小于总数 → 用上限（读满即结束）
        assert_eq!(progress_total(Some(5000), 1000), Some(1000));
        // 总数小于上限 → 用总数（读完即结束）
        assert_eq!(progress_total(Some(500), 1000), Some(500));
    }

    /// 临时 CSV 清理：只删本程序前缀的 .csv，不动其它文件
    #[test]
    fn cleanup_removes_only_our_temp_csvs() {
        let dir = std::env::temp_dir().join(format!("csv_db_cleanup_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 本程序的文件（1 个 1KB、1 个 2KB）+ 其它文件 + 同名目录（都不该被动）
        std::fs::write(dir.join(format!("{}1_0.csv", DB_TEMP_PREFIX)), vec![b'x'; 1024]).unwrap();
        std::fs::write(dir.join(format!("{}1_1.csv", DB_TEMP_PREFIX)), vec![b'y'; 2048]).unwrap();
        std::fs::write(dir.join("别人的数据.csv"), b"keep").unwrap();
        std::fs::write(dir.join(format!("{}notes.txt", DB_TEMP_PREFIX)), b"keep").unwrap();
        std::fs::create_dir_all(dir.join(format!("{}1_2.csv", DB_TEMP_PREFIX))).unwrap();

        let (files, bytes) = cleanup_temp_csvs_in(&dir);
        assert_eq!(files, 2, "只应删除两个临时 CSV");
        assert_eq!(bytes, 1024 + 2048);
        assert!(dir.join("别人的数据.csv").exists(), "不该删其它文件");
        assert!(
            dir.join(format!("{}notes.txt", DB_TEMP_PREFIX)).exists(),
            "不该删非 .csv 文件"
        );
        assert!(
            dir.join(format!("{}1_2.csv", DB_TEMP_PREFIX)).is_dir(),
            "不该删同名目录"
        );
        assert_eq!(cleanup_temp_csvs_in(&dir), (0, 0), "再清理应为 0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 临时文件名符合前缀约定（清理逻辑据此识别）
    #[test]
    fn temp_csv_path_has_prefix() {
        let p = temp_csv_path();
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(DB_TEMP_PREFIX), "{}", name);
        assert!(name.ends_with(".csv"), "{}", name);
        assert!(p.parent().is_some());
    }

    /// 会话级只读：MySQL / PostgreSQL 有、其它类型没有等价语句
    #[test]
    fn read_only_session_sql_per_kind() {
        assert_eq!(
            read_only_session_sql(DbKind::MySql),
            Some("SET SESSION TRANSACTION READ ONLY")
        );
        assert!(read_only_session_sql(DbKind::PostgreSql)
            .unwrap()
            .contains("READ ONLY"));
        assert_eq!(read_only_session_sql(DbKind::SqlServer), None);
        assert_eq!(read_only_session_sql(DbKind::Oracle), None);
        assert_eq!(read_only_session_sql(DbKind::Generic), None);
    }

    /// 驱动安装指引：五种类型都要有可读的中文提示
    #[test]
    fn driver_hint_is_present_for_all_kinds() {
        for k in DbKind::ALL {
            let h = driver_hint(k);
            assert!(!h.trim().is_empty(), "{:?} 缺少驱动提示", k);
        }
        assert!(driver_hint(DbKind::MySql).contains("MySQL"));
        assert!(driver_hint(DbKind::PostgreSql).contains("psqlODBC"));
        assert!(driver_hint(DbKind::SqlServer).contains("ODBC Driver"));
    }

    /// 连接失败必须是「返回 Err」而不是 panic（无驱动 / 无服务 / 无 DSN 时同样安全）
    #[test]
    fn connect_failure_is_error_not_panic() {
        // 1) 连接串为空
        let empty = DbConfig {
            kind: DbKind::Generic,
            host: String::new(),
            ..Default::default()
        };
        let r = DbConn::connect(&empty, &[]);
        assert!(r.is_err());
        assert!(r.err().unwrap().contains("连接串为空"));

        // 2) 不存在的 DSN：驱动会立刻报错（不涉及网络等待）
        let bad = DbConfig {
            kind: DbKind::Generic,
            dsn: "本程序测试用的不存在DSN".into(),
            user: "u".into(),
            ..Default::default()
        };
        let r = DbConn::connect(&bad, &[]);
        assert!(r.is_err(), "不存在的 DSN 应当返回错误");
        assert!(!r.err().unwrap().is_empty());
    }
}
