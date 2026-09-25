# 宿经03超大CSV文件实用编辑器（Rust 重构版）

原 Python/PyQt6 版（`1.3_nodask.py`，4227 行）的 Rust 重构，专注超大 CSV 文件（实测 6.5GB / 901 万行 / 55 列）的读取、预览、筛选、拆分、列操作、哈希与导出。

**关键能力**：打开 6.5GB 文件约 4 秒、峰值内存约 150MB；**兼容 Windows 7 的单文件 exe**（无任何外部 DLL 依赖）。

---

## 一、功能一览

| 功能 | 说明 |
|---|---|
| 打开 CSV | 单文件；智能编码检测（BOM → 严格 UTF-8 → UTF-16 → GB18030 兜底），支持 ANSI/GBK 中文文件与 UTF-16 |
| 数据预览 | 虚拟化表格（egui_extras），901 万行仅渲染可见部分，流畅滚动；显示行数可选 100/1000/10000/全部 |
| 筛选 | 10 种条件（等于/包含/不包含/大于/小于/大于等于/小于等于/不等于/空值/非空值）+ 且/或组合；并行分块，901 万行秒级完成；数值比较用 i128 精确（19 位卡号不丢精度） |
| 拆分 | 按行数 / 按字段（1~3 列组合）；按字段单遍哈希分组 O(N)，文件数上限保护，重名自动序号 |
| 列操作 | 拆分字段、合并字段、复制列、插入列、删除列、隐藏/显示列、生成行哈希（md5/sha1/sha256/sha512）；撤销/恢复（命令式，零复制） |
| 导出 | CSV（utf-8-sig 带 BOM）/ XLSX（长数字列文本格式防科学计数法；超过 1,048,576 行自动分 sheet） |
| 其他 | 操作日志、内存占用显示、示例数据、关于 |

## 二、与 Python 版（1.3_nodask）的差异与改进

### 修复的逻辑问题
1. **编码检测**：Python 版"chardet + GB18030 优先"会把 UTF-8 文件误判为 GB18030 产生**静默乱码**（实测证实）。Rust 版改为"严格 UTF-8 验证优先、GB18030 兜底"，UTF-8 文件绝不会再乱码；latin1 兜底导致的编码误判不重试问题一并消除。
2. **拆分性能**：Python 版按字段拆分对每个唯一值全表扫描 `df[df[col]==value]`（O(N×U)）；Rust 版单遍哈希分组 O(N)。多列组合键用 `\x1F` 连接，文件名清理覆盖控制字符。
3. **XLSX 行数上限**：Python 版 901 万行导出 XLSX 直接崩溃（Excel 单表上限 1,048,576）；Rust 版自动分多个 sheet。
4. **长数字**：XLSX 导出长数字列（账号/卡号/证件号等关键词 + 数据采样）强制文本格式，防精度丢失与科学计数法。
5. **撤销/恢复**：Python 版每次操作深拷贝整个 DataFrame（20 步历史），6.5GB 文件单次拷贝即 OOM；Rust 版为命令式操作栈（记录操作描述），零复制。
6. **错误处理**：Python 版大量 `except: pass` 吞错；Rust 版全部显式错误信息进入日志。
7. **死代码**：Python 版 ThreadPoolExecutor/ProcessPoolExecutor 从未使用；Rust 版按需使用 std::thread。

### 性能数据（实测 jymxb.csv：6.5GB / 9,013,661 行 / 55 列 / UTF-8）
| 指标 | Python 1.3 | Rust 版 |
|---|---|---|
| 打开+行索引 | 数分钟（pandas 全量载入） | **4.1 秒** |
| 峰值内存 | 15GB+（pandas 全量，低配机直接 OOM） | **148 MB** |
| 全量筛选 | 分钟级（逐条件全列转换） | 并行秒级 |
| 文件体积 | ~300MB（PyInstaller 单文件） | **6.5 MB** |

## 三、系统要求与 Win7 兼容

- **运行时**：Windows 7 SP1 及以上（x64），无需安装任何运行库。
- 单文件 exe 导入表仅含 Win7 内置系统 DLL（kernel32 / msvcrt / user32 / gdi32 / advapi32 / shell32 / ole32 / opengl32 / dwmapi 等），无 UCRT、无 API-set 依赖。
- 中文界面使用系统字体（微软雅黑/黑体）。
- 图形渲染依赖 OpenGL（Win7 显卡驱动通常支持；软件渲染亦可回退）。

> 说明：标准 Rust std 自 1.58+ 静态导入 Win8/Win10 API，普通编译产物在 Win7 无法加载。本项目使用 Rust 官方 Tier 3 目标 `x86_64-win7-windows-gnu` + nightly `build-std` 重新编译标准库，才得到真正 Win7 兼容的产物（已验证导入表）。构建环境为本机（Win10/11），**建议在 Win7 真机/虚拟机做最终验收**。

## 四、构建方法

### 开发/测试（stable）
```bat
set PATH=D:\project_xiangmu\.rust-toolchain\rust-1.88.0-x86_64-pc-windows-gnu\rustc\bin;D:\project_xiangmu\.rust-toolchain\rust-1.88.0-x86_64-pc-windows-gnu\cargo\bin;%PATH%
cargo test        :: 74 项单元测试
cargo run --bin csv_editor_rust
```

### Win7 单文件发布
运行 `build_win7.bat`（或手动）：
```bat
set RUSTUP_HOME=D:\project_xiangmu\.rustup
set CARGO_HOME=D:\project_xiangmu\.cargo
set RUSTFLAGS=-C target-feature=+crt-static -C link-self-contained=yes -C linker=rust-lld
cargo +nightly-2025-06-23-x86_64-pc-windows-gnu build --release --target x86_64-win7-windows-gnu -Z build-std=std,panic_abort
```
产物：`target\x86_64-win7-windows-gnu\release\csv_editor_rust.exe`（单文件，含图标/版本信息）。

### 工具链说明
项目内自带（不污染系统）：
- `D:\project_xiangmu\.rust-toolchain\`：standalone rustc 1.88 GNU（stable 开发）
- `D:\project_xiangmu\.rustup\` + `D:\project_xiangmu\.cargo\`：rustup + nightly GNU（发布 build-std）
- 依赖锁定：egui/eframe 0.27 + winit 0.29（Win7 兼容版本）；`vendor/webbrowser` 为本地 stub（绕开 idna/icu 的 edition2024 依赖）；indexmap/toml_edit 锁定兼容版本。

## 五、使用说明

1. **打开**：点击「选择CSV文件」选择 CSV（或 .txt）；打开过程在后台线程执行，进度窗口显示进度与预计信息。
2. **预览**：默认显示前 100 行，可切换 1000/10000/全部（全部为虚拟滚动）。
3. **筛选**：左侧面板选字段/条件/值/逻辑，点「添加筛选条件」后「应用筛选」；命中行显示在预览区，可「导出数据」保存筛选结果。
4. **拆分**：右侧「数据拆分」标签：按行数（每批行数）或按字段（勾选 1~3 个字段），选 CSV/XLSX 输出，选择保存文件夹执行。
5. **列操作**：右侧「列操作」标签：拆分字段/合并字段/生成行哈希/复制列/插入列/删除列/隐藏列/显示列，支持撤销/恢复。
6. **导出**：顶部「导出数据」导出当前筛选视图为 CSV/XLSX；XLSX 超 104 万行自动分 sheet。

## 六、项目结构

```
src/
├── main.rs                 # eframe 入口（窗口/字体）
├── lib.rs                  # 核心库入口
├── core/                   # 数据引擎（无 GUI 依赖，可独立测试）
│   ├── encoding.rs         # 严格编码检测与转码
│   ├── csv.rs              # CSV 字节级解析状态机（行索引/字段切分）
│   ├── reader.rs           # mmap + 流式行索引 + 惰性字段访问
│   ├── model.rs            # 数据模型 + 类型推断
│   ├── filter.rs           # 筛选引擎（并行）
│   ├── split.rs            # 拆分引擎（单遍哈希分组）
│   ├── column_ops.rs       # 列操作 + 命令式 undo/redo
│   ├── hash.rs             # 行哈希（4 算法并行）
│   └── export.rs           # CSV/XLSX 导出（自动分 sheet/长数字文本）
└── app/                    # GUI 层
    ├── panels.rs           # 菜单/控制/筛选/日志/预览布局
    ├── dialog_ui.rs        # 各操作对话框
    ├── dialogs.rs          # 对话框状态
    ├── table.rs            # 虚拟化预览表格
    └── background.rs       # 后台工作线程桥接
src/bin/                    # 命令行工具
    ├── inspect.rs          # 打开报告（行数/列数/编码/耗时/内存）
    ├── verify.rs           # 与 Python 参照对照（筛选/哈希）
    └── e2e.rs              # 端到端回归
```

## 七、已知限制

- 按字段拆分对唯一值超过 20,000 个的字段会拒绝（提示改用按行数拆分），避免生成海量文件。
- XLSX 导出行数超过 1,048,576 时自动分 sheet（每 sheet 满额即开新 sheet）。
- UTF-16 编码文件会整体转码载入内存（此类文件通常较小）；超大 UTF-16 文件建议先转码。
