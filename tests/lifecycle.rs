//! 端到端生命周期验证：`start` → 就绪 → `stop` → 优雅退出与 WAL checkpoint。
//!
//! 这是唯一真正跨进程验证 Windows 命名停止事件的测试：CI 的
//! windows-x86_64 runner 上会启动真实后台进程、发 stop 命令、断言
//! `GRACEFUL_STOP=1`（而不是回退到 TerminateProcess），并确认
//! SQLite WAL 已被 checkpoint。Unix 上同样验证 SIGTERM 优雅路径。
//!
//! 默认 `#[ignore]`：它会启动真实后台进程并占用端口，只在设置了
//! `CLIPBOARD_MONITOR_LIFECYCLE_LIVE=1` 时运行（CI 单独一步设置）。

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn live() -> bool {
    std::env::var("CLIPBOARD_MONITOR_LIFECYCLE_LIVE").as_deref() == Ok("1")
}

/// 找一个当前空闲的回环端口（绑定后立刻释放，存在极小竞态但可接受）。
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
    listener.local_addr().unwrap().port()
}

fn http_get(addr: &str, path: &str) -> Option<String> {
    let mut stream = std::net::TcpStream::connect(addr).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    response.split_once("\r\n\r\n").map(|p| p.1.to_string())
}

fn wait_until<F: Fn() -> bool>(timeout: Duration, label: &str, f: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    eprintln!("等待超时: {label}");
    false
}

fn read_pid(dir: &Path) -> Option<u32> {
    fs::read_to_string(dir.join("monitor.pid"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

#[test]
fn start_stop_graceful_shutdown_checkpoints_wal() {
    if !live() {
        return;
    }
    let exe = env!("CARGO_BIN_EXE_clipboard-monitor");
    let dir = std::env::temp_dir().join(format!(
        "clipmon-lifecycle-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    ));
    fs::create_dir_all(&dir).expect("create data dir");
    let port = free_port();
    let listen = format!("127.0.0.1:{port}");

    let start = Command::new(exe)
        .args(["start", "--data-dir"])
        .arg(&dir)
        .args(["--listen", &listen])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run start");
    assert!(
        start.status.success(),
        "start 失败: stdout={} stderr={}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr)
    );

    // 等后台进程写出 pid 与监听地址，并通过 HTTP 确认已就绪。
    assert!(
        wait_until(Duration::from_secs(20), "后台进程就绪", || {
            let addr_ok = fs::read_to_string(dir.join("listen"))
                .map(|s| s.trim().contains(&port.to_string()))
                .unwrap_or(false);
            let http_ok = http_get(&listen, "/api/health")
                .map(|b| b.contains("ok") || b.contains("\""))
                .unwrap_or(false);
            addr_ok && http_ok && read_pid(&dir).is_some()
        }),
        "后台进程未在 20 秒内就绪；日志: {}",
        fs::read_to_string(dir.join("monitor.log")).unwrap_or_default()
    );

    // 制造一点数据库写入量，确保 WAL 里确实有东西需要 checkpoint。
    // 通过 /api/monitor 开关触发配置落库不是 DB 写入；改用 export 端点
    // 确认 DB 可打开，并直接检查 monitor.db 已创建。
    let db_path: PathBuf = dir.join("monitor.db");
    assert!(db_path.exists(), "加密数据库应已创建");
    let _ = http_get(&listen, "/api/export");
    // 确保 WAL 里确实有未 checkpoint 的数据，否则后面的断言是空验证。
    let wal_before = dir.join("monitor.db-wal");
    let wal_before_len = fs::metadata(&wal_before).map(|m| m.len()).unwrap_or(0);
    assert!(
        wal_before_len > 0,
        "停止前 WAL 应有未 checkpoint 的页（实际 {wal_before_len} 字节），否则无法验证 checkpoint"
    );

    let stop = Command::new(exe)
        .args(["stop", "--data-dir"])
        .arg(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run stop");
    let stop_out = format!(
        "{}{}",
        String::from_utf8_lossy(&stop.stdout),
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(stop.status.success(), "stop 失败: {stop_out}");
    assert!(
        stop_out.contains("GRACEFUL_STOP=1"),
        "必须走优雅退出（Windows 命名事件 / Unix SIGTERM），实际输出: {stop_out}"
    );
    assert!(
        !stop_out.contains("优雅退出超时") && !stop_out.contains("强制结束"),
        "不应回退到 TerminateProcess: {stop_out}"
    );

    // pid 文件应被清理。
    assert!(
        wait_until(Duration::from_secs(5), "pid 文件清理", || read_pid(
            &dir
        )
        .is_none()),
        "pid 文件未被删除"
    );

    // WAL checkpoint：优雅退出后 -wal 应不存在或为 0 字节。
    let wal = dir.join("monitor.db-wal");
    let wal_len = fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
    assert!(
        wal_len == 0,
        "优雅退出应执行 wal_checkpoint(TRUNCATE)，-wal 仍为 {wal_len} 字节"
    );

    let _ = fs::remove_dir_all(&dir);
}
