@echo off
REM ============================================================
REM 宿经03超大CSV文件实用编辑器 - Win7 单文件构建脚本
REM 输出: target\x86_64-win7-windows-gnu\release\csv_editor_rust.exe
REM 前提: 项目内 rustup 工具链（nightly-2025-06-23 GNU + rust-src）
REM ============================================================
set RUSTUP_HOME=D:\project_xiangmu\.rustup
set CARGO_HOME=D:\project_xiangmu\.cargo
set PATH=D:\project_xiangmu\.cargo\bin;%PATH%
set RUSTFLAGS=-C target-feature=+crt-static -C link-self-contained=yes -C linker=rust-lld
cd /d %~dp0

echo [1/2] 编译 Win7 目标（nightly + build-std）...
cargo +nightly-2025-06-23-x86_64-pc-windows-gnu build --release --target x86_64-win7-windows-gnu -Z build-std=std,panic_abort
if errorlevel 1 (
  echo 构建失败！
  pause
  exit /b 1
)

echo.
echo [2/2] 构建完成：
echo    target\x86_64-win7-windows-gnu\release\csv_editor_rust.exe
echo 单文件，无外部 DLL 依赖（静态链接 msvcrt + Win7 内置系统 DLL）
pause
