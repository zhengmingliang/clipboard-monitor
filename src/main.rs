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
use clipboard_monitor::{init_log, log_info, now_ms};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
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
    if args.first().map(|s| s.as_str()) == Some("--help") || args.first().map(|s| s.as_str()) == Some("-h") {
        print_help();
        return Ok(());
    }
    let cmd = match args.first().map(|s| s.as_str()) {
        Some("serve") | Some("start") | Some("stop") | Some("status") | Some("cleanup") => {
            args.remove(0)
        }
        _ => "serve".to_string(),
    };
    let opt = parse_opts(&args)?;
    match cmd.as_str() {
        "serve" => serve(opt),
        "start" => start_daemon(opt),
        "stop" => stop_daemon(&opt),
        "status" => print_status(&opt),
        "cleanup" => cleanup_cli(&opt),
        _ => {
            print_help();
            Ok(())
        }
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
    let data_dir = data_dir
        .map(PathBuf::from)
        .unwrap_or_else(default_data_dir);
    let config_path = config
        .map(PathBuf::from)
        .unwrap_or_else(|| {
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
    let db = Database::open(&db_path)?;
    let app = App::new(db, cfg, opt.config_path.clone());
    write_pid(&opt.data_dir)?;
    let threads = monitor::spawn(Arc::clone(&app))?;
    install_signals();
    log_info(format!(
        "数据目录 {} ，配置 {}",
        opt.data_dir.display(),
        opt.config_path.display()
    ));
    let app_http = Arc::clone(&app);
    let http = std::thread::Builder::new()
        .name("clip-http".into())
        .spawn(move || {
            if let Err(err) = http_server::serve(app_http) {
                eprintln!("界面退出: {err:#}");
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
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log_path)?;
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
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            Ok(())
        });
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
    println!("进程已启动（pid {}），界面地址尚未写出，查看 {}", child.id(), log_path.display());
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
    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
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
                    if v["enabled"].as_bool() == Some(true) { "开" } else { "关" },
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
    let db = Database::open(&opt.data_dir.join("monitor.db"))?;
    let spec = CleanupSpec {
        from_ms: opt.from.as_deref().map(clipboard_monitor::config::parse_user_time).transpose()?,
        to_ms: opt.to.as_deref().map(clipboard_monitor::config::parse_user_time).transpose()?,
        content_type: opt.content_type.clone(),
        op: opt.op.clone(),
        dry_run: false,
    };
    let n = db.cleanup(&spec)?;
    println!("已删除 {n} 条");
    let _ = now_ms();
    Ok(())
}

fn lock_or_exit(dir: &Path) -> Result<std::fs::File> {
    let path = dir.join("monitor.lock");
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&path)?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        bail!("已有一个监控进程在使用 {}", dir.display());
    }
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

fn pid_exists(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn is_our_process(pid: u32) -> bool {
    if !pid_exists(pid) {
        return false;
    }
    let Ok(cmd) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let text = String::from_utf8_lossy(&cmd);
    text.contains("clipboard-monitor")
}

fn install_signals() {
    unsafe {
        libc::signal(
            libc::SIGTERM,
            handle_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            handle_signal as *const () as libc::sighandler_t,
        );
    }
}

static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn handle_signal(_: i32) {
    STOP.store(true, Ordering::Relaxed);
}

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
    let body = buf.split_once("\r\n\r\n").map(|x| x.1).unwrap_or("").to_string();
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

不带子命令时等价于 serve，在前台运行并打开本机界面。
默认界面 http://127.0.0.1:27890
默认数据目录 ~/.local/share/clipboard-x11
默认配置 ~/.config/clipboard-x11/config.json

时间可以是 YYYY-MM-DD、YYYY-MM-DDTHH:MM:SS（本地时区）或毫秒时间戳。
内容类型: text richtext image files custom mixed empty

环境变量 CLIPBOARD_MONITOR_LOG=debug 会打印协议级调试信息，不打印剪贴板正文。
"#
    );
}
