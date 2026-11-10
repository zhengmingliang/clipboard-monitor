//! 本机剪贴板监控。
//!
//! Linux/X11 上用 XFixes 记录写入，用 RECORD 扩展记录 `ConvertSelection` 读取。
//! Windows/macOS 记录系统剪贴板的文本与图片变化。
//! 内容按类型截断后写入 SQLite，界面由本机 HTTP 服务提供。

pub mod autostart;
pub mod classify;
pub mod config;
pub mod crypto;
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
    let stamp = local_timestamp(now_ms());
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "[{stamp}] {level} {msg}");
}

/// 本地时间 HH:MM:SS.mmm。此前按 UTC 秒数输出且无时区标注，跨时区排障需心算。
pub fn local_timestamp(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let frac = ms.rem_euclid(1000);
    #[cfg(unix)]
    {
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        let t = secs as libc::time_t;
        let parsed = unsafe { libc::localtime_r(&t, &mut tm) };
        if parsed.is_null() {
            return format!("??:??:??.{frac:03}");
        }
        format!(
            "{:02}:{:02}:{:02}.{:03}",
            tm.tm_hour, tm.tm_min, tm.tm_sec, frac
        )
    }
    #[cfg(windows)]
    {
        let offset = time::UtcOffset::current_local_offset().ok();
        match time::OffsetDateTime::from_unix_timestamp(secs)
            .ok()
            .zip(offset)
        {
            Some((t, off)) => format!(
                "{:02}:{:02}:{:02}.{:03}",
                t.to_offset(off).hour(),
                t.to_offset(off).minute(),
                t.to_offset(off).second(),
                t.millisecond()
            ),
            None => format!("??:??:??.{frac:03}"),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        format!("??:??:??.{frac:03}")
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_matches_local_hms_format() {
        let stamp = local_timestamp(now_ms());
        let re = regex_shape(&stamp);
        assert!(re, "时间戳应为 HH:MM:SS.mmm 本地格式，实际 {stamp}");
    }

    fn regex_shape(s: &str) -> bool {
        let bytes = s.as_bytes();
        bytes.len() == 12
            && bytes[2] == b':'
            && bytes[5] == b':'
            && bytes[8] == b'.'
            && (0..12)
                .filter(|i| ![2, 5, 8].contains(i))
                .all(|i| bytes[i].is_ascii_digit())
    }
}
