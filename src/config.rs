//! 配置文件。默认值对应交付要求：文本 1MB、单文件内容 2MB、保留 7 天、库上限 500MB。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_TEXT_FULL_LIMIT: u64 = 1024 * 1024;
pub const DEFAULT_TEXT_PREVIEW: u64 = 4096;
pub const DEFAULT_FILE_CONTENT_LIMIT: u64 = 2 * 1024 * 1024;
pub const DEFAULT_RETENTION_DAYS: u32 = 7;
pub const DEFAULT_MAX_DB_MB: u32 = 500;
pub const DEFAULT_LISTEN: &str = "127.0.0.1:27890";

/// XDG 配置和数据目录的子目录名。
/// `clipboard-monitor` 已被另一套剪贴板工具占用，这里用单独的名字。
pub const APP_DIR_NAME: &str = "clipboard-x11";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    /// 本机界面监听地址。只建议绑定回环地址。
    pub listen: String,
    /// 日志保留天数。
    pub retention_days: u32,
    /// 数据库文件上限，单位 MB。达到后删除最旧记录。
    pub max_db_mb: u32,
    /// 文本小于该字节数时完整保存，大于等于该值时只保存摘要。
    pub text_full_limit_bytes: u64,
    /// 截断时保留的文本前缀字节数。
    pub text_preview_bytes: u64,
    /// 允许内联保存的单个文件大小上限。
    pub file_content_limit_bytes: u64,
    /// 为真时，单个小于上限的普通文件会保存内容。默认关闭。
    pub record_file_content: bool,
    /// 同时监控 PRIMARY 选择区（鼠标中键选中）。
    pub monitor_primary: bool,
    /// 进程名、可执行文件名或 WM_CLASS，命中后不记录该应用自己的操作。
    pub whitelist: Vec<String>,
    /// 进程启动时是否立即开始监控。
    pub enabled_on_start: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: DEFAULT_LISTEN.to_string(),
            retention_days: DEFAULT_RETENTION_DAYS,
            max_db_mb: DEFAULT_MAX_DB_MB,
            text_full_limit_bytes: DEFAULT_TEXT_FULL_LIMIT,
            text_preview_bytes: DEFAULT_TEXT_PREVIEW,
            file_content_limit_bytes: DEFAULT_FILE_CONTENT_LIMIT,
            record_file_content: false,
            monitor_primary: true,
            whitelist: Vec::new(),
            enabled_on_start: true,
        }
    }
}

impl Config {
    pub fn validate(&mut self) -> Result<()> {
        self.listen = self.listen.trim().to_string();
        if self.listen.is_empty() || !self.listen.contains(':') {
            bail!("listen 必须是 host:port，当前是 {}", self.listen);
        }
        if !(1..=3650).contains(&self.retention_days) {
            bail!("retention_days 必须在 1 到 3650 之间");
        }
        if !(32..=102_400).contains(&self.max_db_mb) {
            bail!("max_db_mb 必须在 32 到 102400 之间");
        }
        if !(4096..=8 * 1024 * 1024).contains(&self.text_full_limit_bytes) {
            bail!("text_full_limit_bytes 必须在 4KB 到 8MB 之间");
        }
        if !(256..=65_536).contains(&self.text_preview_bytes) {
            bail!("text_preview_bytes 必须在 256 到 65536 之间");
        }
        if self.text_preview_bytes > self.text_full_limit_bytes {
            self.text_preview_bytes = self.text_full_limit_bytes;
        }
        if self.file_content_limit_bytes > 8 * 1024 * 1024 {
            bail!("file_content_limit_bytes 不能超过 8MB");
        }
        self.whitelist = self
            .whitelist
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && !s.starts_with('#'))
            .collect();
        Ok(())
    }

    pub fn max_db_bytes(&self) -> u64 {
        u64::from(self.max_db_mb) * 1024 * 1024
    }

    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            let cfg = Config::default();
            cfg.save(path)?;
            return Ok(cfg);
        }
        let text = fs::read_to_string(path)
            .with_context(|| format!("读取配置失败: {}", path.display()))?;
        let mut cfg: Config = serde_json::from_str(&text)
            .with_context(|| format!("配置不是合法 JSON: {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
            restrict_dir(dir);
        }
        let text = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text + "\n")?;
        replace_file(&tmp, path)?;
        restrict_file(path);
        Ok(())
    }
}

#[cfg(not(target_os = "windows"))]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination).with_context(|| {
        format!(
            "替换配置失败: {} -> {}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let source_wide: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination_wide: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let ok = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "替换配置失败: {} -> {}",
                source.display(),
                destination.display()
            )
        });
    }
    Ok(())
}

pub fn default_config_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        return env_path("APPDATA")
            .unwrap_or_else(|| home_dir().join("AppData").join("Roaming"))
            .join(APP_DIR_NAME);
    }
    #[cfg(target_os = "macos")]
    {
        return home_dir()
            .join("Library")
            .join("Application Support")
            .join(APP_DIR_NAME);
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        if let Some(dir) = env_path("XDG_CONFIG_HOME") {
            return dir.join(APP_DIR_NAME);
        }
        home_dir().join(".config").join(APP_DIR_NAME)
    }
}

pub fn default_data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        return env_path("LOCALAPPDATA")
            .unwrap_or_else(|| home_dir().join("AppData").join("Local"))
            .join(APP_DIR_NAME);
    }
    #[cfg(target_os = "macos")]
    {
        return home_dir()
            .join("Library")
            .join("Application Support")
            .join(APP_DIR_NAME);
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        if let Some(dir) = env_path("XDG_DATA_HOME") {
            return dir.join(APP_DIR_NAME);
        }
        home_dir().join(".local").join("share").join(APP_DIR_NAME)
    }
}

#[cfg(not(target_os = "macos"))]
fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).and_then(|v| {
        if v.is_empty() {
            None
        } else {
            Some(PathBuf::from(v))
        }
    })
}

pub fn home_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(target_os = "windows"))]
    let home = std::env::var_os("HOME");
    home.map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(std::env::temp_dir)
}

pub fn restrict_dir(_path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(_path, fs::Permissions::from_mode(0o700));
    }
}

pub fn restrict_file(_path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(_path, fs::Permissions::from_mode(0o600));
    }
}

/// 白名单匹配。自身进程始终排除，避免监控程序读剪贴板时把自己记进去。
pub fn is_whitelisted(
    app_name: &str,
    wm_class: &str,
    exec_path: &str,
    list: &[String],
    self_pid: u32,
    pid: Option<i32>,
    self_exe: &str,
) -> bool {
    if pid == Some(self_pid as i32) {
        return true;
    }
    if !self_exe.is_empty() && !exec_path.is_empty() && same_path(exec_path, self_exe) {
        return true;
    }
    if app_name == "clipboard-monitor" {
        return true;
    }
    let base = file_base(exec_path);
    let fields = [app_name, wm_class, base.as_str(), exec_path];
    for entry in list {
        let needle = entry.trim();
        if needle.is_empty() {
            continue;
        }
        // 纯数字或 pid:数字 只按进程号匹配，避免和同名进程（例如多个 python3）混在一起。
        if let Some(want) = entry_pid(needle) {
            if pid == Some(want) {
                return true;
            }
            continue;
        }
        let needle_l = needle.to_lowercase();
        for field in fields {
            if field.is_empty() {
                continue;
            }
            let field_l = field.to_lowercase();
            if field_l == needle_l {
                return true;
            }
            if field_l
                .split(['/', ' ', '\t', ':'])
                .any(|part| part == needle_l)
            {
                return true;
            }
        }
    }
    false
}

/// `1234`、`pid:1234`、`pid=1234` 都表示进程号。大小写不敏感，数字前后的空白忽略。
fn entry_pid(entry: &str) -> Option<i32> {
    let lower = entry.trim().to_ascii_lowercase();
    let digits = if let Some(rest) = lower
        .strip_prefix("pid:")
        .or_else(|| lower.strip_prefix("pid="))
    {
        rest.trim()
    } else if !lower.is_empty() && lower.chars().all(|c| c.is_ascii_digit()) {
        lower.as_str()
    } else {
        return None;
    };
    let n: i32 = digits.parse().ok()?;
    if n > 0 {
        Some(n)
    } else {
        None
    }
}

fn same_path(a: &str, b: &str) -> bool {
    Path::new(a) == Path::new(b)
}

fn file_base(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn parse_user_time(raw: &str) -> Result<i64> {
    let s = raw.trim();
    if s.is_empty() {
        bail!("时间为空");
    }
    if s.chars().all(|c| c.is_ascii_digit()) {
        let n: i64 = s.parse().context("时间不是整数")?;
        return Ok(if n < 100_000_000_000 { n * 1000 } else { n });
    }
    if let Some(ms) = parse_local_civil(s) {
        return Ok(ms);
    }
    bail!("无法解析时间 {s}，可用 YYYY-MM-DD、YYYY-MM-DDTHH:MM:SS 或毫秒时间戳");
}

fn parse_local_civil(s: &str) -> Option<i64> {
    let (date, time) = if let Some((d, t)) = s.split_once(['T', ' ']) {
        (d, Some(t.trim_end_matches('Z')))
    } else {
        (s, None)
    };
    let mut dp = date.split('-');
    let year: i32 = dp.next()?.parse().ok()?;
    let month: i32 = dp.next()?.parse().ok()?;
    let day: i32 = dp.next()?.parse().ok()?;
    if dp.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (hour, min, sec) = if let Some(time) = time {
        let mut tp = time.split(':');
        let h: i32 = tp.next()?.parse().ok()?;
        let m: i32 = tp.next()?.parse().ok()?;
        let sec_s = tp.next().unwrap_or("0");
        let sec: i32 = sec_s.split('.').next()?.parse().ok()?;
        if tp.next().is_some() {
            return None;
        }
        (h, m, sec)
    } else {
        (0, 0, 0)
    };
    if !(0..=23).contains(&hour) || !(0..=59).contains(&min) || !(0..=60).contains(&sec) {
        return None;
    }
    #[cfg(unix)]
    {
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        tm.tm_year = year - 1900;
        tm.tm_mon = month - 1;
        tm.tm_mday = day;
        tm.tm_hour = hour;
        tm.tm_min = min;
        tm.tm_sec = sec;
        tm.tm_isdst = -1;
        let t = unsafe { libc::mktime(&mut tm) };
        if t < 0 {
            None
        } else {
            Some(t * 1000)
        }
    }
    #[cfg(windows)]
    {
        let month = time::Month::try_from(month as u8).ok()?;
        let date = time::Date::from_calendar_date(year, month, day as u8).ok()?;
        let time = time::Time::from_hms(hour as u8, min as u8, sec.min(59) as u8).ok()?;
        let offset = time::UtcOffset::current_local_offset().ok()?;
        let seconds = time::PrimitiveDateTime::new(date, time)
            .assume_offset(offset)
            .unix_timestamp();
        seconds.checked_mul(1000)
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitelist_matches_comm_class_and_basename() {
        let list = vec!["firefox".into(), "org.keepassxc.KeePassXC".into()];
        assert!(is_whitelisted(
            "firefox",
            "Navigator/Firefox",
            "/usr/lib/firefox/firefox",
            &list,
            10,
            Some(20),
            "/usr/bin/clipboard-monitor"
        ));
        assert!(is_whitelisted(
            "keepassxc",
            "keepassxc/org.keepassxc.KeePassXC",
            "/usr/bin/keepassxc",
            &list,
            10,
            Some(20),
            ""
        ));
        assert!(!is_whitelisted(
            "gedit",
            "gedit/Gedit",
            "/usr/bin/gedit",
            &list,
            10,
            Some(20),
            ""
        ));
        assert!(is_whitelisted(
            "gedit",
            "",
            "/tmp/clipboard-monitor",
            &[],
            10,
            Some(10),
            "/tmp/clipboard-monitor"
        ));
    }

    #[test]
    fn whitelist_pid_does_not_match_same_comm() {
        let list = vec!["pid:4242".into(), "9001".into()];
        assert!(is_whitelisted(
            "python3",
            "",
            "/usr/bin/python3",
            &list,
            10,
            Some(4242),
            ""
        ));
        assert!(is_whitelisted(
            "python3",
            "",
            "/usr/bin/python3",
            &list,
            10,
            Some(9001),
            ""
        ));
        assert!(!is_whitelisted(
            "python3",
            "",
            "/usr/bin/python3",
            &list,
            10,
            Some(4243),
            ""
        ));
        assert!(is_whitelisted(
            "python3",
            "",
            "/usr/bin/python3",
            &["PID=4242".into()],
            10,
            Some(4242),
            ""
        ));
        assert!(!is_whitelisted(
            "4242",
            "",
            "/usr/bin/4242",
            &["4242".into()],
            10,
            Some(7),
            ""
        ));
    }

    #[test]
    fn saving_replaces_existing_config() {
        let dir = std::env::temp_dir().join(format!(
            "clipboard-monitor-config-test-{}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        let path = dir.join("config.json");
        let mut config = Config::default();
        config.save(&path).expect("create config");
        config.retention_days = 30;
        config.save(&path).expect("replace config");
        assert_eq!(Config::load(&path).unwrap().retention_days, 30);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn validates_defaults_and_rejects_bad_retention() {
        let mut cfg = Config::default();
        cfg.validate().unwrap();
        cfg.retention_days = 0;
        assert!(cfg.validate().is_err());
    }
}
