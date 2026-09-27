//! 数据库取数性能基准（诊断用）。
//!
//! 用途：把「从数据库读数据到编辑器」拆成几段分别计时，定位真正的瓶颈。
//!
//! 用法（密码从环境变量 MYSQL_PWD 读，避免出现在命令行里）：
//! ```text
//! set MYSQL_PWD=xxx
//! cargo run --release --bin db_bench -- --host 127.0.0.1 --port 3308 --db cbd2026 \
//!     --user root --table jymxb --rows 100000
//! ```
//! 参数缺省：host=127.0.0.1 port=3308 db=cbd2026 user=root table=jymxb rows=100000

use std::time::Instant;

use csv_editor_rust::core::db::{self, DbConfig, DbKind, TableInfo};
use csv_editor_rust::core::model::DataModel;

struct Args {
    progress: bool,
    host: String,
    port: String,
    db: String,
    user: String,
    table: String,
    rows: usize,
}

fn parse_args() -> Args {
    let mut a = Args {
        progress: false,
        host: "127.0.0.1".into(),
        port: "3308".into(),
        db: "cbd2026".into(),
        user: "root".into(),
        table: "jymxb".into(),
        rows: 100_000,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--progress" => {
                a.progress = true;
                i += 1;
            }
            flag => {
                let v = args.get(i + 1).cloned().unwrap_or_default();
                match flag {
                    "--host" => a.host = v,
                    "--port" => a.port = v,
                    "--db" => a.db = v,
                    "--user" => a.user = v,
                    "--table" => a.table = v,
                    "--rows" => a.rows = v.parse().unwrap_or(a.rows),
                    _ => {}
                }
                i += 2;
            }
        }
    }
    a
}

fn rate(rows: usize, secs: f64) -> String {
    if secs <= 0.0 {
        return "-".to_string();
    }
    format!("{:.0} 行/秒", rows as f64 / secs)
}

fn main() {
    let a = parse_args();
    // 进度回调要求 `Fn + Sync`（不可变），用原子量记录上次打印的百分比
    let last_shown = std::sync::atomic::AtomicU32::new(0);
    use std::sync::atomic::Ordering;
    let pwd = std::env::var("MYSQL_PWD").unwrap_or_default();
    if pwd.is_empty() {
        eprintln!("请先设置环境变量 MYSQL_PWD（数据库密码）");
        return;
    }
    let cfg = DbConfig {
        kind: DbKind::MySql,
        host: a.host.clone(),
        port: a.port.clone(),
        database: a.db.clone(),
        user: a.user.clone(),
        password: pwd,
        ..Default::default()
    };
    let drivers = db::installed_drivers();
    let t0 = Instant::now();
    let conn = match db::DbConn::connect(&cfg, &drivers) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("连接失败：{}", e);
            return;
        }
    };
    println!(
        "连接耗时 {:.3}s（{}，驱动 {}）",
        t0.elapsed().as_secs_f64(),
        conn.server_info(),
        conn.driver_name()
    );

    let t = TableInfo {
        catalog: a.db.clone(),
        schema: String::new(),
        name: a.table.clone(),
        kind: "TABLE".into(),
    };
    let cols = conn.columns(&t).unwrap_or_default();
    let sql = conn.select_sql(&t, &[], a.rows);
    println!(
        "表 {}：{} 列，读取上限 {} 行\nSQL: {}\n",
        a.table,
        cols.len(),
        a.rows,
        sql
    );

    // ① 只 fetch、不读取任何列 —— 驱动把结果行推进来的下限
    let t0 = Instant::now();
    match conn.count_rows_only(&sql, a.rows) {
        Ok(n) => {
            let s = t0.elapsed().as_secs_f64();
            println!("① 纯 SQLFetch（不取列）      : {:6.2}s  {}  （{} 行）", s, rate(n, s), n);
        }
        Err(e) => println!("① 纯 fetch 失败：{}", e),
    }

    // ② 逐行 + 逐列 SQLGetData + 每格 String 转换（当前实现的取数路径，不写文件）
    let t0 = Instant::now();
    match conn.for_each_row(&sql, a.rows, None, None, |_h, _row| Ok(())) {
        Ok((n, _)) => {
            let s = t0.elapsed().as_secs_f64();
            println!("② 取列（逐格 SQLGetData）    : {:6.2}s  {}  （{} 行）", s, rate(n, s), n);
        }
        Err(e) => println!("② 取列失败：{}", e),
    }

    // ③ 完整取数并写成 CSV（② + CSV 落盘）
    let out = std::env::temp_dir().join(format!("db_bench_{}.csv", std::process::id()));
    let out_s = out.to_string_lossy().into_owned();
    // 先取行数估算作为进度分母（与界面一致：读系统统计视图，毫秒级）
    let t0 = Instant::now();
    let total = conn.estimate_rows(&t);
    match total {
        Some(n) => println!("行数估算：{} 行（{:.3}s）", n, t0.elapsed().as_secs_f64()),
        None => println!("行数估算不可用（改为不确定进度）"),
    }
    if a.progress {
        println!("  COUNT(*) 精确统计（对照，可能较慢）：");
        let t1 = Instant::now();
        match conn.count_rows(&t) {
            Ok(n) => println!("    COUNT(*) = {} 行（{:.2}s）", n, t1.elapsed().as_secs_f64()),
            Err(e) => println!("    COUNT(*) 失败（{:.2}s）：{}", t1.elapsed().as_secs_f64(), e),
        }
    }
    if a.progress {
        last_shown.store(0, Ordering::Relaxed);
        println!("进度采样（每 10% 一行）：");
    }
    let t0 = Instant::now();
    let mut csv_rows = 0usize;
    let progress = |done: u64, hint: Option<u64>| {
        if !a.progress {
            return;
        }
        let total = hint.or(total);
        if let Some(t) = total {
            let pct = (done as f64 / t as f64 * 100.0).min(100.0) as u32;
            if pct >= last_shown.load(Ordering::Relaxed) + 10 {
                last_shown.store((pct / 10) * 10, Ordering::Relaxed);
                println!("   {:>3}%  {}/{} 行", pct, done, t);
            }
        }
    };
    match conn.export_to_csv(&t, &[], &out_s, a.rows, total, Some(&progress)) {
        Ok((n, _h)) => {
            csv_rows = n;
            let s = t0.elapsed().as_secs_f64();
            let mb = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0) as f64 / 1024.0 / 1024.0;
            println!(
                "③ 取数 + 写临时 CSV          : {:6.2}s  {}  （{} 行，{:.0} MB）",
                s,
                rate(n, s),
                n,
                mb
            );
        }
        Err(e) => println!("③ 写 CSV 失败：{}", e),
    }

    // ④ 编辑器加载该 CSV（mmap + 行索引 + 表头）
    if csv_rows > 0 {
        let t0 = Instant::now();
        match DataModel::open(&out_s, 1000) {
            Ok(m) => {
                let s = t0.elapsed().as_secs_f64();
                println!(
                    "④ 载入编辑器（mmap+建索引）  : {:6.2}s  {}  （{} 行 × {} 列）",
                    s,
                    rate(m.row_count(), s),
                    m.row_count(),
                    m.column_count()
                );
            }
            Err(e) => println!("④ 载入失败：{}", e),
        }
        let _ = std::fs::remove_file(&out);
    }

    // ⑤ 汇总：按 ② 的速率推算百万 / 千万行的等待时间
    println!(
        "\n提示：上面前 ③ 步是「从数据库读到临时 CSV」的全过程，④ 是编辑器加载；\n\
         若 ① 与 ② 差距很大，说明瓶颈在「逐格 SQLGetData + 字符串转换」，而不是驱动推进结果行。"
    );
}
