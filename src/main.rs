//! 剪贴板监控命令行。
//!
//! serve 前台运行，start 放到后台，stop / status 通过 pid 文件控制。

use anyhow::{anyhow, bail, Context, Result};
use clipboard_monitor::config::{
    default_config_dir, default_data_dir, restrict_dir, restrict_file, Config,
};
use clipboard_monitor::db::{CleanupSpec, Database};
use clipboard_monitor::http_server;
use clipboard_monitor::monitor::{self, App};
use clipboard_monitor::{init_log, log_info};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

fn main() {
    init_log();
    if let Err(err) = real_main() {
        eprintln!("错误: {err:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().map(|s| s.as_str()) == Some("--help")
        || args.first().map(|s| s.as_str()) == Some("-h")
    {
        print_help();
        return Ok(());
    }
    let cmd = match args.first().map(|s| s.as_str()) {
        Some("serve") | Some("start") | Some("stop") | Some("status") | Some("cleanup")
        | Some("autostart") | Some("service") => args.remove(0),
        _ => "serve".to_string(),
    };
    // autostart/service 有自己的子动作，不复用通用 parse_opts。
    let action = if matches!(cmd.as_str(), "autostart" | "service") {
        args.first()
            .cloned()
            .unwrap_or_else(|| "status".to_string())
    } else {
        String::new()
    };
    if matches!(cmd.as_str(), "autostart" | "service") && !args.is_empty() {
        args.remove(0);
    }
    let opt = parse_opts(&args)?;
    match cmd.as_str() {
        "serve" => serve(opt),
        "start" => start_daemon(opt),
        "stop" => stop_daemon(&opt),
        "status" => print_status(&opt),
        "cleanup" => cleanup_cli(&opt),
        "autostart" => autostart_cli(&action),
        "service" => service_cli(&action),
        _ => {
            print_help();
            Ok(())
        }
    }
}

fn autostart_cli(action: &str) -> Result<()> {
    match action {
        "enable" => {
            let s = clipboard_monitor::autostart::enable()?;
            println!("已启用开机自启\n位置: {}\n命令: {}", s.location, s.command);
        }
        "disable" => {
            let s = clipboard_monitor::autostart::disable()?;
            println!("已禁用开机自启\n位置: {}", s.location);
        }
        "status" => {
            let s = clipboard_monitor::autostart::status()?;
            println!(
                "开机自启: {}\n位置: {}\n命令: {}",
                if s.enabled { "已启用" } else { "未启用" },
                s.location,
                s.command
            );
        }
        other => bail!("autostart 只接受 enable|disable|status，收到 {other}"),
    }
    Ok(())
}

/// 输出可直接安装的 systemd user unit / launchd plist 内容（服务化）。
/// 只打印到 stdout，不写系统目录，由用户自行 `systemctl --user enable`。
fn service_cli(action: &str) -> Result<()> {
    match action {
        "unit" | "plist" => {}
        other => bail!("service 只接受 unit|plist，收到 {other}"),
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let exe = std::env::current_exe().context("找不到自身可执行文件")?;
    #[cfg(target_os = "linux")]
    {
        if action == "plist" {
            bail!("Linux 请使用 service unit；plist 是 macOS 专用");
        }
        print!(
            "[Unit]\n\
             Description=Clipboard Monitor (user)\n\
             After=graphical-session.target\n\n\
             [Service]\n\
             Type=simple\n\
             ExecStart={exe} serve\n\
             Restart=on-failure\n\
             RestartSec=5\n\n\
             [Install]\n\
             WantedBy=default.target\n",
            exe = exe.display()
        );
    }
    #[cfg(target_os = "macos")]
    {
        if action == "unit" {
            bail!("macOS 请使用 service plist；unit 是 Linux 专用");
        }
        print!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\"><dict>\n\
               <key>Label</key><string>com.clipboard-monitor.service</string>\n\
               <key>ProgramArguments</key><array><string>{exe}</string><string>serve</string></array>\n\
               <key>RunAtLoad</key><true/>\n\
               <key>KeepAlive</key><true/>\n\
             </dict></plist>\n",
            exe = exe.display()
        );
    }
    #[cfg(target_os = "windows")]
    {
        bail!("Windows 请用 autostart enable（HKCU Run）；service 仅支持 Linux/macOS 单元输出")
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        bail!("当前平台不支持服务化输出")
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        Ok(())
    }
}

struct Opt {
    data_dir: PathBuf,
    config_path: PathBuf,
    listen: Option<String>,
    from: Option<String>,
    to: Option<String>,
    content_type: Option<String>,
    op: Option<String>,
}

fn parse_opts(args: &[String]) -> Result<Opt> {
    let mut data_dir = None;
    let mut config = None;
    let mut listen = None;
    let mut from = None;
    let mut to = None;
    let mut content_type = None;
    let mut op = None;
    let mut i = 0;
    while i < args.len() {
        let key = args[i].as_str();
        let val = args.get(i + 1).cloned();
        match key {
            "--data-dir" => data_dir = Some(need(key, val)?),
            "--config" => config = Some(need(key, val)?),
            "--listen" => listen = Some(need(key, val)?),
            "--from" => from = Some(need(key, val)?),
            "--to" => to = Some(need(key, val)?),
            "--type" => content_type = Some(need(key, val)?),
            "--op" => op = Some(need(key, val)?),
            other if other.starts_with('-') => bail!("未知参数 {other}"),
            other => bail!("多余参数 {other}"),
        }
        i += 2;
    }
    let data_dir = data_dir.map(PathBuf::from).unwrap_or_else(default_data_dir);
    let config_path = config.map(PathBuf::from).unwrap_or_else(|| {
        if data_dir != default_data_dir() {
            data_dir.join("config.json")
        } else {
            default_config_dir().join("config.json")
        }
    });
    Ok(Opt {
        data_dir,
        config_path,
        listen,
        from,
        to,
        content_type,
        op,
    })
}

fn need(key: &str, val: Option<String>) -> Result<String> {
    val.ok_or_else(|| anyhow!("{key} 缺少取值"))
}

fn serve(opt: Opt) -> Result<()> {
    fs::create_dir_all(&opt.data_dir)?;
    restrict_dir(&opt.data_dir);
    let lock = lock_or_exit(&opt.data_dir)?;
    let mut cfg = Config::load(&opt.config_path)?;
    if let Some(listen) = opt.listen.clone() {
        cfg.listen = listen;
        cfg.validate()?;
    }
    let db_path = opt.data_dir.join("monitor.db");
    let db = Database::open_encrypted(&db_path)?;
    let app = App::new(db, cfg, opt.config_path.clone());
    write_pid(&opt.data_dir)?;
    let threads = monitor::spawn(Arc::clone(&app))?;
    install_signals()?;
    log_info(format!(
        "数据目录 {} ，配置 {}",
        opt.data_dir.display(),
        opt.config_path.display()
    ));
    let app_http = Arc::clone(&app);
    let http = std::thread::Builder::new()
        .name("clip-http".into())
        .spawn(move || {
            if let Err(err) = http_server::serve(Arc::clone(&app_http)) {
                eprintln!("界面退出: {err:#}");
                app_http.shutdown.store(true, Ordering::Relaxed);
            }
        })?;
    while !shutdown_requested() && !app.shutdown.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200));
    }
    app.shutdown.store(true, Ordering::Relaxed);
    monitor::shutdown(&app, threads);
    let _ = http.join();
    remove_pid(&opt.data_dir);
    drop(lock);
    Ok(())
}

fn start_daemon(opt: Opt) -> Result<()> {
    if process_alive(&opt.data_dir) {
        println!("已在运行");
        let _ = print_status(&opt);
        return Ok(());
    }
    fs::create_dir_all(&opt.data_dir)?;
    restrict_dir(&opt.data_dir);
    let log_path = opt.data_dir.join("monitor.log");
    rotate_log(&log_path, MAX_LOG_BYTES);
    let log_file = open_private(&log_path, true)?;
    let exe = std::env::current_exe().context("找不到自身可执行文件")?;
    let mut cmd = Command::new(exe);
    cmd.arg("serve")
        .arg("--data-dir")
        .arg(&opt.data_dir)
        .arg("--config")
        .arg(&opt.config_path);
    if let Some(listen) = &opt.listen {
        cmd.arg("--listen").arg(listen);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file);
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            Ok(())
        });
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    let child = cmd.spawn().context("启动后台进程失败")?;
    let listen_path = opt.data_dir.join("listen");
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while std::time::Instant::now() < deadline {
        if !pid_exists(child.id()) {
            bail!("后台进程提前退出，查看 {}", log_path.display());
        }
        if listen_path.exists() {
            let addr = fs::read_to_string(&listen_path).unwrap_or_default();
            println!("已在后台运行，界面 http://{}", addr.trim());
            println!("日志 {}", log_path.display());
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    println!(
        "进程已启动（pid {}），界面地址尚未写出，查看 {}",
        child.id(),
        log_path.display()
    );
    Ok(())
}

fn stop_daemon(opt: &Opt) -> Result<()> {
    let Some(pid) = read_pid(&opt.data_dir) else {
        println!("未运行");
        return Ok(());
    };
    if !is_our_process(pid) {
        println!("pid 文件里的进程不是剪贴板监控，已忽略");
        remove_pid(&opt.data_dir);
        return Ok(());
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    #[cfg(target_os = "windows")]
    terminate_process(pid)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if !pid_exists(pid) {
            remove_pid(&opt.data_dir);
            println!("已停止");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("进程 {pid} 没有在 5 秒内退出");
}

fn print_status(opt: &Opt) -> Result<()> {
    if !process_alive(&opt.data_dir) {
        println!("未运行");
        println!("数据目录 {}", opt.data_dir.display());
        return Ok(());
    }
    let pid = read_pid(&opt.data_dir).unwrap_or(0);
    let addr = fs::read_to_string(opt.data_dir.join("listen")).unwrap_or_default();
    let addr = addr.trim();
    println!("运行中 pid {pid}");
    if !addr.is_empty() {
        println!("界面 http://{addr}");
        if let Ok(body) = http_get(&format!("http://{addr}/api/status")) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                println!(
                    "监控 {} · 库 {} · 写入 {} · 读取 {}",
                    if v["enabled"].as_bool() == Some(true) {
                        "开"
                    } else {
                        "关"
                    },
                    v["db"]["db_human"].as_str().unwrap_or("?"),
                    v["writes_seen"].as_u64().unwrap_or(0),
                    v["reads_seen"].as_u64().unwrap_or(0)
                );
            }
        }
    }
    Ok(())
}

fn cleanup_cli(opt: &Opt) -> Result<()> {
    let db = Database::open_encrypted(&opt.data_dir.join("monitor.db"))?;
    let spec = CleanupSpec {
        from_ms: opt
            .from
            .as_deref()
            .map(clipboard_monitor::config::parse_user_time)
            .transpose()?,
        to_ms: opt
            .to
            .as_deref()
            .map(clipboard_monitor::config::parse_user_time)
            .transpose()?,
        content_type: opt.content_type.clone(),
        op: opt.op.clone(),
        dry_run: false,
    };
    let n = db.cleanup(&spec)?;
    println!("已删除 {n} 条");
    Ok(())
}

/// 后台日志超过该大小时轮转，避免长期挂机无限增长。
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// 日志达到上限时改名为 `.old`（覆盖上一档），随后新日志从空文件开始。
fn rotate_log(log_path: &Path, max_bytes: u64) {
    if let Ok(meta) = fs::metadata(log_path) {
        if meta.len() >= max_bytes {
            let old = log_path.with_extension("log.old");
            if fs::rename(log_path, &old).is_ok() {
                restrict_file(&old);
            }
        }
    }
}

fn open_private(path: &Path, append: bool) -> Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    if append {
        options.append(true);
    }
    #[cfg(unix)]
    options.mode(0o600);
    Ok(options.open(path)?)
}

fn lock_or_exit(dir: &Path) -> Result<std::fs::File> {
    let path = dir.join("monitor.lock");
    let file = open_private(&path, false)?;
    fs2::FileExt::try_lock_exclusive(&file)
        .map_err(|_| anyhow!("已有一个监控进程在使用 {}", dir.display()))?;
    restrict_file(&path);
    Ok(file)
}

fn write_pid(dir: &Path) -> Result<()> {
    let path = dir.join("monitor.pid");
    fs::write(&path, format!("{}\n", std::process::id()))?;
    restrict_file(&path);
    Ok(())
}

fn remove_pid(dir: &Path) {
    let _ = fs::remove_file(dir.join("monitor.pid"));
}

fn read_pid(dir: &Path) -> Option<u32> {
    let text = fs::read_to_string(dir.join("monitor.pid")).ok()?;
    text.trim().parse().ok()
}

fn process_alive(dir: &Path) -> bool {
    read_pid(dir).is_some_and(is_our_process)
}

#[cfg(unix)]
fn pid_exists(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(target_os = "windows")]
fn pid_exists(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let alive = unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT;
    unsafe { CloseHandle(handle) };
    alive
}

fn is_our_process(pid: u32) -> bool {
    if !pid_exists(pid) {
        return false;
    }
    let Some(path) = process_path(pid) else {
        return false;
    };
    let Ok(current) = std::env::current_exe() else {
        return false;
    };
    let process = fs::canonicalize(&path).unwrap_or(path);
    let current = fs::canonicalize(&current).unwrap_or(current);
    #[cfg(target_os = "windows")]
    {
        process
            .to_string_lossy()
            .eq_ignore_ascii_case(current.to_string_lossy().as_ref())
    }
    #[cfg(not(target_os = "windows"))]
    {
        process == current
    }
}

#[cfg(target_os = "linux")]
fn process_path(pid: u32) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(target_os = "macos")]
fn process_path(pid: u32) -> Option<PathBuf> {
    let mut buffer = vec![0u8; 4096];
    let len =
        unsafe { libc::proc_pidpath(pid as i32, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if len <= 0 {
        return None;
    }
    buffer.truncate(len as usize);
    Some(PathBuf::from(String::from_utf8_lossy(&buffer).into_owned()))
}

#[cfg(target_os = "windows")]
fn process_path(pid: u32) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let mut buffer = vec![0u16; 32768];
    let mut len = buffer.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut len) };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        None
    } else {
        buffer.truncate(len as usize);
        Some(std::ffi::OsString::from_wide(&buffer).into())
    }
}

#[cfg(target_os = "windows")]
fn terminate_process(pid: u32) -> Result<()> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error()).context("打开要停止的进程");
    }
    let ok = unsafe { TerminateProcess(handle, 0) };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).context("停止 Windows 监控进程");
    }
    Ok(())
}

fn install_signals() -> Result<()> {
    ctrlc::set_handler(|| STOP.store(true, Ordering::Relaxed)).context("安装退出信号处理")?;
    Ok(())
}

static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn shutdown_requested() -> bool {
    STOP.load(Ordering::Relaxed)
}

fn http_get(url: &str) -> Result<String> {
    let rest = url.trim_start_matches("http://");
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let mut stream = std::net::TcpStream::connect_timeout(
        &host.parse().with_context(|| format!("解析 {host}"))?,
        Duration::from_secs(2),
    )?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let req = format!("GET /{path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes())?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf)?;
    let body = buf
        .split_once("\r\n\r\n")
        .map(|x| x.1)
        .unwrap_or("")
        .to_string();
    Ok(body)
}

fn print_help() {
    println!(
        r#"剪贴板监控

用法:
  clipboard-monitor serve [--listen 地址] [--data-dir 目录] [--config 文件]
  clipboard-monitor start [与 serve 相同的选项]
  clipboard-monitor stop [--data-dir 目录]
  clipboard-monitor status [--data-dir 目录]
  clipboard-monitor cleanup [--data-dir 目录] --from 时间 --to 时间 [--type 类型] [--op read|write]
  clipboard-monitor autostart enable|disable|status
  clipboard-monitor service unit|plist

不带子命令时等价于 serve，在前台运行并打开本机界面。
autostart 用三平台原生方式配置当前用户开机自启（Linux .desktop /
macOS LaunchAgents plist / Windows HKCU Run），无需管理员权限。
service 只打印 systemd user unit 或 launchd plist 内容，不写系统目录。
默认界面 http://127.0.0.1:27890
默认目录遵循 Windows/macOS/Linux 系统规范，详见 README。
也可用 --data-dir 和 --config 显式指定。

时间可以是 YYYY-MM-DD、YYYY-MM-DDTHH:MM:SS（本地时区）或毫秒时间戳。
内容类型: text richtext image files custom mixed empty

环境变量 CLIPBOARD_MONITOR_LOG=debug 会打印协议级调试信息，不打印剪贴板正文。
"#
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_log_is_rotated_and_small_log_is_kept() {
        let dir = std::env::temp_dir().join(format!(
            "clipmon-rotate-{}-{}",
            std::process::id(),
            clipboard_monitor::now_ms()
        ));
        fs::create_dir_all(&dir).unwrap();
        let log_path = dir.join("monitor.log");
        fs::write(&log_path, b"1234567890").unwrap();
        rotate_log(&log_path, 8);
        assert!(!log_path.exists(), "超限日志应被改名");
        let old = log_path.with_extension("log.old");
        assert_eq!(fs::read(&old).unwrap(), b"1234567890");

        // 小文件不轮转；再次超限时覆盖上一档。
        fs::write(&log_path, b"abc").unwrap();
        rotate_log(&log_path, 8);
        assert!(log_path.exists());
        fs::write(&log_path, b"9876543210").unwrap();
        rotate_log(&log_path, 8);
        assert_eq!(fs::read(&old).unwrap(), b"9876543210", "新档应覆盖旧档");
        let _ = fs::remove_dir_all(dir);
    }
}
