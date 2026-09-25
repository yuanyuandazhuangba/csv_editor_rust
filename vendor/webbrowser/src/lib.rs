//! webbrowser stub — 用 Win32 ShellExecuteW 打开系统默认浏览器打开 URL。
//! 替代 crates.io 上的 webbrowser crate，避免其 url/idna/icu 传递依赖
//! 要求 edition2024（Cargo 1.81 无法解析），同时保持 Win7 兼容。

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

#[derive(Debug, Clone)]
pub struct Error {
    message: String,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error {
            message: e.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Browser {
    Default,
    Firefox,
    Chrome,
    Safari,
    Edge,
    Windows,
    Mac,
    Android,
    Wry,
    Webview2,
}

pub fn open(url: &str) -> Result<(), Error> {
    open_browser(Browser::Default, url)
}

pub fn open_browser(_browser: Browser, url: &str) -> Result<(), Error> {
    let wide: Vec<u16> = OsStr::new(url).encode_wide().chain(std::iter::once(0)).collect();
    // ShellExecuteW(hwnd=NULL, op="open", file=url, params=NULL, dir=NULL, show=SW_SHOWNORMAL)
    let result = unsafe {
        winapi::um::shellapi::ShellExecuteW(
            std::ptr::null_mut(),
            wide_null("open").as_ptr(),
            wide.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            winapi::um::winuser::SW_SHOWNORMAL,
        )
    };
    let code = result as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(Error {
            message: format!("ShellExecuteW failed with code {}", code),
        })
    }
}

pub fn is_available() -> bool {
    true
}

fn wide_null(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}
