//! 开机自启配置（三平台原生方式）。
//!
//! - Linux: XDG Autostart `.desktop` 文件（`~/.config/autostart/`）。
//! - macOS: launchd `LaunchAgents` plist（`~/Library/LaunchAgents/`）。
//! - Windows: HKCU Run 注册表项（通过 `reg.exe` 写入，无需额外 crate）。
//!
//! 三种方式都只在**当前用户**范围内生效，不需要管理员权限，也不修改系统
//! 目录。卸载/禁用只删除本工具自己写入的那一个文件/值。

#[cfg(any(
    target_os = "windows",
    not(any(target_os = "linux", target_os = "macos", target_os = "windows"))
))]
use anyhow::bail;
use anyhow::{Context, Result};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::fs;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::path::PathBuf;

/// 自启状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutostartStatus {
    pub enabled: bool,
    /// 当前平台的配置位置（用于 status 输出）。
    pub location: String,
    /// 自启将要执行的命令行（便于用户核对）。
    pub command: String,
}

/// 生成/查询/删除自启配置。
pub fn enable() -> Result<AutostartStatus> {
    let command = launch_command()?;
    install(&command)?;
    Ok(AutostartStatus {
        enabled: true,
        location: location_string(),
        command,
    })
}

pub fn disable() -> Result<AutostartStatus> {
    uninstall()?;
    Ok(AutostartStatus {
        enabled: false,
        location: location_string(),
        command: launch_command()?,
    })
}

pub fn status() -> Result<AutostartStatus> {
    Ok(AutostartStatus {
        enabled: is_installed()?,
        location: location_string(),
        command: launch_command()?,
    })
}

/// 自启执行的命令：`"<exe>" start [--data-dir ...] [--config ...]`。
/// 只有用户显式传过 --data-dir / --config 时才带上参数，默认目录由程序
/// 自行解析，避免把当前工作目录或临时路径写死进配置。
fn launch_command() -> Result<String> {
    let exe = std::env::current_exe().context("找不到自身可执行文件")?;
    let exe = quote(&exe.to_string_lossy());
    let mut args = format!("{exe} start");
    let parsed = std::env::args().skip(1).collect::<Vec<_>>();
    if let Some(i) = parsed.iter().position(|s| s == "--data-dir") {
        if let Some(v) = parsed.get(i + 1) {
            args.push_str(&format!(" --data-dir {}", quote(v)));
        }
    }
    if let Some(i) = parsed.iter().position(|s| s == "--config") {
        if let Some(v) = parsed.get(i + 1) {
            args.push_str(&format!(" --config {}", quote(v)));
        }
    }
    Ok(args)
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn quote(s: &str) -> String {
    if s.contains([' ', '\t', '"']) {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

fn location_string() -> String {
    #[cfg(target_os = "linux")]
    {
        desktop_path().display().to_string()
    }
    #[cfg(target_os = "macos")]
    {
        plist_path().display().to_string()
    }
    #[cfg(target_os = "windows")]
    {
        "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run\\ClipboardMonitor".to_string()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        "(当前平台不支持自启)".to_string()
    }
}

#[allow(clippy::needless_return)] // 各平台 cfg 块内 return 是结构需要
fn install(_command: &str) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let path = desktop_path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let content = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Clipboard Monitor\n\
             Comment=Record local clipboard changes\n\
             Exec={command}\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n",
            command = _command
        );
        fs::write(&path, content)?;
        crate::config::restrict_file(&path);
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let path = plist_path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        // plist 里把命令行按空格拆成 ProgramArguments；路径不含空格时正确，
        // 含空格时用 /bin/sh -c 包一层保证可靠。
        let exe = std::env::current_exe().context("找不到自身可执行文件")?;
        let content = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
               <key>Label</key><string>com.clipboard-monitor</string>\n\
               <key>ProgramArguments</key>\n\
               <array>\n\
                 <string>/bin/sh</string><string>-c</string><string>{}</string>\n\
               </array>\n\
               <key>RunAtLoad</key><true/>\n\
               <key>KeepAlive</key><false/>\n\
               <key>StandardOutPath</key><string>{}</string>\n\
               <key>StandardErrorPath</key><string>{}</string>\n\
             </dict>\n\
             </plist>\n",
            xml_escape(_command),
            xml_escape(log_path("stdout.log").to_string_lossy().as_ref()),
            xml_escape(log_path("stderr.log").to_string_lossy().as_ref())
        );
        fs::write(&path, content)?;
        crate::config::restrict_file(&path);
        let _ = exe;
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    {
        use std::process::Command;
        let exe = std::env::current_exe().context("找不到自身可执行文件")?;
        let value = format!("\"{}\" start", exe.display());
        let out = Command::new("reg")
            .args([
                "add",
                "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run",
                "/v",
                "ClipboardMonitor",
                "/t",
                "REG_SZ",
                "/d",
                &value,
                "/f",
            ])
            .output()
            .context("执行 reg.exe 失败")?;
        if !out.status.success() {
            bail!(
                "写入注册表失败: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        return Ok(());
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = _command;
        bail!("当前平台不支持开机自启")
    }
}

#[allow(clippy::needless_return)] // 各平台 cfg 块内 return 是结构需要
fn uninstall() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let path = desktop_path();
        if path.exists() {
            fs::remove_file(&path)?;
        }
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let path = plist_path();
        if path.exists() {
            // 先 unload 再删文件；未加载时忽略错误。
            let _ = std::process::Command::new("launchctl")
                .args(["unload", &path.display().to_string()])
                .status();
            fs::remove_file(&path)?;
        }
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    {
        use std::process::Command;
        let out = Command::new("reg")
            .args([
                "delete",
                "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run",
                "/v",
                "ClipboardMonitor",
                "/f",
            ])
            .output()
            .context("执行 reg.exe 失败")?;
        // 值不存在时 reg 返回非 0，视为已禁用。
        if !out.status.success()
            && !String::from_utf8_lossy(&out.stderr).contains("无法找到")
            && !String::from_utf8_lossy(&out.stderr).contains("Unable to find")
        {
            bail!(
                "删除注册表值失败: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        return Ok(());
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        bail!("当前平台不支持开机自启")
    }
}

#[allow(clippy::needless_return)] // 各平台 cfg 块内 return 是结构需要
fn is_installed() -> Result<bool> {
    #[cfg(target_os = "linux")]
    {
        return Ok(desktop_path().exists());
    }
    #[cfg(target_os = "macos")]
    {
        return Ok(plist_path().exists());
    }
    #[cfg(target_os = "windows")]
    {
        use std::process::Command;
        let out = Command::new("reg")
            .args([
                "query",
                "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run",
                "/v",
                "ClipboardMonitor",
            ])
            .output()
            .context("执行 reg.exe 失败")?;
        return Ok(out.status.success());
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Ok(false)
    }
}

#[cfg(target_os = "linux")]
fn desktop_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::config::home_dir().join(".config"));
    base.join("autostart").join("clipboard-monitor.desktop")
}

#[cfg(target_os = "macos")]
fn plist_path() -> PathBuf {
    crate::config::home_dir()
        .join("Library")
        .join("LaunchAgents")
        .join("com.clipboard-monitor.plist")
}

#[cfg(target_os = "macos")]
fn log_path(name: &str) -> PathBuf {
    crate::config::default_data_dir().join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_only_wraps_paths_with_whitespace() {
        assert_eq!(
            quote("/usr/bin/clipboard-monitor"),
            "/usr/bin/clipboard-monitor"
        );
        assert_eq!(
            quote("C:\\Program Files\\app.exe"),
            "\"C:\\Program Files\\app.exe\""
        );
    }

    #[test]
    fn launch_command_contains_start_subcommand() {
        let cmd = launch_command().unwrap();
        assert!(cmd.contains(" start"));
    }

    #[test]
    fn location_is_nonempty() {
        assert!(!location_string().is_empty());
    }
}
