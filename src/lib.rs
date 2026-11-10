//! 本机剪贴板监控。
//!
//! Linux/X11 上用 XFixes 记录写入，用 RECORD 扩展记录 `ConvertSelection` 读取。
//! Windows/macOS 记录系统剪贴板的文本与图片变化。
//! 内容按类型截断后写入 SQLite，界面由本机 HTTP 服务提供。

pub mod classify;
pub mod config;
pub mod db;
#[cfg(target_os = "linux")]
pub mod fetch;
pub mod http_server;
pub mod monitor;
#[cfg(target_os = "linux")]
pub mod resolver;
#[cfg(target_os = "linux")]
pub mod x11util;
#[cfg(target_os = "linux")]
pub mod xparse;

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static DEBUG: AtomicBool = AtomicBool::new(false);

pub fn init_log() {
    let debug = std::env::var("CLIPBOARD_MONITOR_LOG")
        .map(|v| {
            let v = v.to_ascii_lowercase();
            v == "debug" || v == "trace" || v == "1"
        })
        .unwrap_or(false);
    DEBUG.store(debug, Ordering::Relaxed);
}

pub fn log_debug(msg: impl AsRef<str>) {
    if DEBUG.load(Ordering::Relaxed) {
        log_line("DEBUG", msg.as_ref());
    }
}

pub fn log_info(msg: impl AsRef<str>) {
    log_line("INFO", msg.as_ref());
}

pub fn log_warn(msg: impl AsRef<str>) {
    log_line("WARN", msg.as_ref());
}

fn log_line(level: &str, msg: &str) {
    let ms = now_ms();
    let secs = ms / 1000;
    let frac = ms % 1000;
    let t = secs.rem_euclid(86_400);
    let h = t / 3600;
    let m = (t % 3600) / 60;
    let s = t % 60;
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "[{h:02}:{m:02}:{s:02}.{frac:03}] {level} {msg}");
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}
