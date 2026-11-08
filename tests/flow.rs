//! 在当前 X11 上验证剪贴板写入、读取、流转和清理。
//!
//! 默认不跑，避免改掉正在使用的剪贴板。需要时：
//! CLIPBOARD_MONITOR_LIVE=1 cargo test --test flow -- --nocapture
//!
//! 只替换 CLIPBOARD，不碰 PRIMARY。结束时尽量把原来的文本交还。

use clipboard_monitor::now_ms;
use clipboard_monitor::x11util::{connect_x, create_helper_window, intern, raw_fd, wait_readable};
use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, EventMask, PropMode, SelectionNotifyEvent,
    SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;

#[test]
fn live_x11_clipboard_flow() {
    if std::env::var("CLIPBOARD_MONITOR_LIVE").ok().as_deref() != Some("1") {
        return;
    }
    let display = std::env::var("DISPLAY").unwrap_or_default();
    assert!(!display.is_empty(), "DISPLAY 未设置，无法做 X11 验证");
    if let Err(err) = live() {
        panic!("实时验证失败: {err:#}");
    }
}

#[test]
fn restore_holder() {
    let Ok(path) = std::env::var("CLIPMON_RESTORE_FILE") else {
        return;
    };
    let text = fs::read(&path).unwrap_or_default();
    if text.is_empty() {
        clear_clipboard();
        return;
    }
    let _ = serve_text(&text, Duration::from_secs(60), true);
}

fn live() -> Result<(), Box<dyn std::error::Error>> {
    let saved = read_utf8_clipboard().unwrap_or(None);
    let dir = std::env::temp_dir().join(format!("clipmon-live-{}-{}", std::process::id(), now_ms()));
    fs::create_dir_all(&dir)?;
    let log_path = dir.join("test.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_clipboard-monitor"))
        .args([
            "serve",
            "--data-dir",
            dir.to_str().unwrap(),
            "--listen",
            "127.0.0.1:0",
        ])
        .env("CLIPBOARD_MONITOR_LOG", "debug")
        .env("DISPLAY", std::env::var("DISPLAY")?)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()?;

    let result = (|| {
        let base = wait_listen(&dir, &mut child)?;
        wait_backend(&base, &mut child, &log_path)?;
        let token = format!("CMON-{}-{}", std::process::id(), now_ms());
        let (t_write, offer) = offer_clipboard(token.as_bytes())?;
        let t_read = now_ms();
        let got = read_utf8_clipboard()?.unwrap_or_default();
        assert!(
            got.windows(token.len()).any(|w| w == token.as_bytes()),
            "自己读回的剪贴板不含测试文本"
        );
        let events = wait_token(&base, &token, Duration::from_secs(6))?;
        let mut writes: Vec<Value> = events
            .iter()
            .filter(|e| e["op"] == "write" && text_of(e).contains(&token))
            .cloned()
            .collect();
        writes.sort_by_key(|e| e["ts_ms"].as_i64().unwrap_or(0));
        let write = writes.first().cloned().ok_or("没有记录到包含测试文本的写入")?;
        let read = events
            .iter()
            .filter(|e| e["op"] == "read" && e["ts_ms"].as_i64().unwrap_or(0) + 30 >= t_read)
            .min_by_key(|e| (e["ts_ms"].as_i64().unwrap_or(0) - t_read).unsigned_abs())
            .cloned()
            .ok_or("没有记录到这次读取")?;
        let write_ts = write["ts_ms"].as_i64().unwrap_or(0);
        let read_ts = read["ts_ms"].as_i64().unwrap_or(0);
        assert!(
            write_ts + 30 >= t_write && write_ts - t_write < 100,
            "写入检测延迟 {} ms，事件 {}",
            write_ts - t_write,
            write
        );
        assert!(
            read_ts + 30 >= t_read && read_ts - t_read < 100,
            "读取检测延迟 {} ms，事件 {}",
            read_ts - t_read,
            read
        );
        assert_eq!(write["content_type"], "text", "写入类型应为纯文本: {write}");
        assert_eq!(read["selection"], "CLIPBOARD");
        let linked = events.iter().any(|r| {
            r["op"] == "read"
                && events.iter().any(|w| {
                    w["op"] == "write"
                        && (r["source_event_id"] == w["id"]
                            || (r["source_app"].as_str().unwrap_or("").len() > 0
                                && r["source_app"] == w["app_name"]))
                })
        });
        assert!(linked, "读取没有连上来源写入: {events:?}");
        let detail = get_json(&format!("{base}/api/events/{}", write["id"]))?;
        let body = detail["body"].as_str().unwrap_or("");
        assert!(body.contains(&token), "详情里没有完整文本: {detail}");

        let comm = fs::read_to_string("/proc/self/comm")
            .unwrap_or_default()
            .trim()
            .to_string();
        let self_pid = i64::from(std::process::id());
        let mut names = vec![comm];
        if write["pid"].as_i64() == Some(self_pid) {
            if let Some(name) = write["app_name"].as_str() {
                names.push(name.to_string());
            }
            if let Some(path) = write["exec_path"].as_str() {
                if let Some(base_name) = std::path::Path::new(path).file_name() {
                    names.push(base_name.to_string_lossy().into_owned());
                }
            }
        }
        names.retain(|s| !s.is_empty());
        names.sort();
        names.dedup();
        let mut cfg = get_json(&format!("{base}/api/settings"))?;
        cfg["whitelist"] = serde_json::json!(names);
        put_json(&format!("{base}/api/settings"), &cfg)?;

        let token2 = format!("CMON-SKIP-{}-{}", std::process::id(), now_ms());
        let (_t2, offer2) = offer_clipboard(token2.as_bytes())?;
        let _ = read_utf8_clipboard();
        std::thread::sleep(Duration::from_millis(800));
        let skipped = query_events(&base, &token2)?;
        let leaked = skipped.iter().any(|e| {
            e["pid"].as_i64() == Some(self_pid)
                || names.iter().any(|n| e["app_name"].as_str() == Some(n.as_str()))
        });
        assert!(!leaked, "白名单没有挡住本进程: {skipped:?}");
        drop(offer2);
        drop(offer);

        let dry = post_json(
            &format!("{base}/api/cleanup"),
            &serde_json::json!({"dry_run": true}),
        );
        assert!(dry.is_err(), "空条件清理应该被拒绝");

        let removed = post_json(
            &format!("{base}/api/cleanup"),
            &serde_json::json!({
                "content_type": "text",
                "from_ms": t_write.saturating_sub(5_000),
                "dry_run": false
            }),
        )?;
        assert!(removed["matched"].as_u64().unwrap_or(0) >= 1, "清理没有删到记录: {removed}");
        let left = query_events(&base, &token)?;
        assert!(left.is_empty(), "清理后仍能查到测试文本: {left:?}");
        Ok(())
    })();

    let _ = child.kill();
    let _ = child.wait();
    if result.is_err() {
        if let Ok(text) = fs::read_to_string(&log_path) {
            let tail: String = text.chars().rev().take(2500).collect::<String>().chars().rev().collect();
            eprintln!("---- monitor log tail ----\n{tail}");
        }
    }
    restore_saved(saved);
    let _ = fs::remove_dir_all(&dir);
    result
}

fn restore_saved(saved: Option<Vec<u8>>) {
    let path = std::env::temp_dir().join(format!("clipmon-restore-{}", std::process::id()));
    if let Some(bytes) = saved {
        if fs::write(&path, bytes).is_err() {
            return;
        }
        let _ = Command::new(std::env::current_exe().unwrap_or_default())
            .args(["--exact", "restore_holder", "--nocapture"])
            .env("CLIPMON_RESTORE_FILE", &path)
            .env_remove("CLIPBOARD_MONITOR_LIVE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

struct Offer {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for Offer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn offer_clipboard(bytes: &[u8]) -> Result<(i64, Offer), Box<dyn std::error::Error>> {
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let owned = bytes.to_vec();
    let handle = std::thread::Builder::new()
        .name("clip-offer".into())
        .spawn(move || {
            if let Err(err) = serve_until(&owned, &stop_thread, &tx) {
                let _ = tx.send(Err(err));
            }
        })?;
    let started = rx.recv_timeout(Duration::from_secs(3))??;
    Ok((
        started,
        Offer {
            stop,
            handle: Some(handle),
        },
    ))
}

fn serve_until(bytes: &[u8], stop: &AtomicBool, ready: &mpsc::Sender<Result<i64, String>>) -> Result<(), String> {
    let (conn, screen) = connect_x().map_err(|e| e.to_string())?;
    let window = create_helper_window(&conn, screen).map_err(|e| e.to_string())?;
    let clipboard = intern(&conn, "CLIPBOARD").map_err(|e| e.to_string())?;
    let utf8 = intern(&conn, "UTF8_STRING").map_err(|e| e.to_string())?;
    let targets = intern(&conn, "TARGETS").map_err(|e| e.to_string())?;
    let string_atom = Atom::from(AtomEnum::STRING);
    let started = now_ms();
    conn.set_selection_owner(window, clipboard, x11rb::CURRENT_TIME)
        .map_err(|e| e.to_string())?
        .check()
        .map_err(|e| e.to_string())?;
    conn.flush().map_err(|e| e.to_string())?;
    ready.send(Ok(started)).map_err(|e| e.to_string())?;
    let fd = raw_fd(&conn);
    while !stop.load(Ordering::Relaxed) {
        loop {
            let ev = conn.poll_for_event().map_err(|e| e.to_string())?;
            let Some(ev) = ev else { break };
            if let Event::SelectionRequest(req) = ev {
                let _ = answer(&conn, &req, bytes, utf8, targets, string_atom);
            }
        }
        wait_readable(fd, 40);
    }
    Ok(())
}

fn answer(
    conn: &clipboard_monitor::x11util::XConn,
    req: &x11rb::protocol::xproto::SelectionRequestEvent,
    bytes: &[u8],
    utf8: Atom,
    targets: Atom,
    string_atom: Atom,
) -> Result<(), String> {
    let mut property = if req.property == 0 { req.target } else { req.property };
    let delivered = if req.target == targets {
        let atoms = [targets, utf8, string_atom];
        let mut data = Vec::with_capacity(atoms.len() * 4);
        for atom in atoms {
            data.extend_from_slice(&atom.to_ne_bytes());
        }
        conn.change_property(
            PropMode::REPLACE,
            req.requestor,
            property,
            AtomEnum::ATOM,
            32,
            atoms.len() as u32,
            &data,
        )
        .map_err(|e| e.to_string())?
        .check()
        .is_ok()
    } else if req.target == utf8 || req.target == string_atom {
        conn.change_property(
            PropMode::REPLACE,
            req.requestor,
            property,
            req.target,
            8,
            bytes.len() as u32,
            bytes,
        )
        .map_err(|e| e.to_string())?
        .check()
        .is_ok()
    } else {
        property = 0;
        true
    };
    if !delivered {
        property = 0;
    }
    let event = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: req.time,
        requestor: req.requestor,
        selection: req.selection,
        target: req.target,
        property,
    };
    conn.send_event(false, req.requestor, EventMask::NO_EVENT, event)
        .map_err(|e| e.to_string())?;
    conn.flush().map_err(|e| e.to_string())?;
    Ok(())
}

fn serve_text(bytes: &[u8], limit: Duration, release_on_clear: bool) -> Result<(), String> {
    let (conn, screen) = connect_x().map_err(|e| e.to_string())?;
    let window = create_helper_window(&conn, screen).map_err(|e| e.to_string())?;
    let clipboard = intern(&conn, "CLIPBOARD").map_err(|e| e.to_string())?;
    let utf8 = intern(&conn, "UTF8_STRING").map_err(|e| e.to_string())?;
    let targets = intern(&conn, "TARGETS").map_err(|e| e.to_string())?;
    let string_atom = Atom::from(AtomEnum::STRING);
    conn.set_selection_owner(window, clipboard, x11rb::CURRENT_TIME)
        .map_err(|e| e.to_string())?
        .check()
        .map_err(|e| e.to_string())?;
    conn.flush().map_err(|e| e.to_string())?;
    let fd = raw_fd(&conn);
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        loop {
            let ev = conn.poll_for_event().map_err(|e| e.to_string())?;
            let Some(ev) = ev else { break };
            match ev {
                Event::SelectionRequest(req) => {
                    let _ = answer(&conn, &req, bytes, utf8, targets, string_atom);
                }
                Event::SelectionClear(_) if release_on_clear => return Ok(()),
                _ => {}
            }
        }
        wait_readable(fd, 100);
    }
    Ok(())
}

fn clear_clipboard() {
    if let Ok((conn, _)) = connect_x() {
        if let Ok(clipboard) = intern(&conn, "CLIPBOARD") {
            let _ = conn.set_selection_owner(x11rb::NONE, clipboard, x11rb::CURRENT_TIME);
            let _ = conn.flush();
        }
    }
}

fn read_utf8_clipboard() -> Result<Option<Vec<u8>>, Box<dyn std::error::Error>> {
    let (conn, screen) = connect_x()?;
    let window = create_helper_window(&conn, screen)?;
    let clipboard = intern(&conn, "CLIPBOARD")?;
    let utf8 = intern(&conn, "UTF8_STRING")?;
    let prop = intern(&conn, "CLIPMON_PROBE")?;
    conn.convert_selection(window, clipboard, utf8, prop, x11rb::CURRENT_TIME)?
        .check()?;
    conn.flush()?;
    let fd = raw_fd(&conn);
    let deadline = Instant::now() + Duration::from_millis(1200);
    let mut property = None;
    while Instant::now() < deadline {
        if let Some(ev) = conn.poll_for_event()? {
            if let Event::SelectionNotify(ev) = ev {
                if ev.requestor == window && ev.selection == clipboard {
                    property = Some(ev.property);
                    break;
                }
            }
        } else {
            let left = deadline.saturating_duration_since(Instant::now()).as_millis() as i32;
            if !wait_readable(fd, left.max(1)) {
                break;
            }
        }
    }
    let Some(property) = property else {
        return Ok(None);
    };
    if property == 0 {
        return Ok(None);
    }
    let reply = conn
        .get_property(true, window, property, AtomEnum::ANY, 0, 256 * 1024)?
        .reply()?;
    if reply.bytes_after > 0 {
        return Ok(None);
    }
    Ok(Some(reply.value))
}

fn wait_listen(dir: &PathBuf, child: &mut Child) -> Result<String, Box<dyn std::error::Error>> {
    let path = dir.join("listen");
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            return Err(format!("监控进程提前退出 {status}").into());
        }
        if let Ok(text) = fs::read_to_string(&path) {
            let addr = text.trim();
            if !addr.is_empty() {
                return Ok(format!("http://{addr}"));
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err("界面没有在 8 秒内开始监听".into())
}

fn wait_backend(base: &str, child: &mut Child, log_path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut last = String::new();
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            let log = fs::read_to_string(log_path).unwrap_or_default();
            return Err(format!("监控进程退出 {status}\n{log}").into());
        }
        if let Ok(status) = get_json(&format!("{base}/api/status")) {
            last = status.to_string();
            if status["xfixes"] == true && status["record"] == true && status["enabled"] == true {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!("X11 监控没有就绪: {last}").into())
}

fn wait_token(base: &str, token: &str, limit: Duration) -> Result<Vec<Value>, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + limit;
    let mut last = Vec::new();
    while Instant::now() < deadline {
        last = query_events(base, token)?;
        let write_ready = last.iter().any(|e| {
            e["op"] == "write" && text_of(e).contains(token) && !text_of(e).contains("内容获取中")
        });
        let read_ready = last.iter().any(|e| e["op"] == "read");
        if write_ready && read_ready {
            return Ok(last);
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    Err(format!("超时仍未同时看到写入和读取: {last:?}").into())
}

fn query_events(base: &str, token: &str) -> Result<Vec<Value>, Box<dyn std::error::Error>> {
    let url = format!(
        "{base}/api/events?q={}&limit=50",
        url_encode(token)
    );
    let v = get_json(&url)?;
    Ok(v["events"].as_array().cloned().unwrap_or_default())
}

fn text_of(ev: &Value) -> String {
    format!(
        "{} {}",
        ev["summary"].as_str().unwrap_or(""),
        ev["targets"].as_str().unwrap_or("")
    )
}

fn get_json(url: &str) -> Result<Value, Box<dyn std::error::Error>> {
    let body = http("GET", url, None)?;
    Ok(serde_json::from_str(&body)?)
}

fn put_json(url: &str, value: &Value) -> Result<Value, Box<dyn std::error::Error>> {
    let body = http("PUT", url, Some(value.to_string()))?;
    Ok(serde_json::from_str(&body)?)
}

fn post_json(url: &str, value: &Value) -> Result<Value, Box<dyn std::error::Error>> {
    let body = http("POST", url, Some(value.to_string()))?;
    Ok(serde_json::from_str(&body)?)
}

fn http(method: &str, url: &str, body: Option<String>) -> Result<String, Box<dyn std::error::Error>> {
    let rest = url.trim_start_matches("http://");
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let mut stream = std::net::TcpStream::connect(host)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let payload = body.unwrap_or_default();
    let req = format!(
        "{method} /{path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
        payload.len()
    );
    stream.write_all(req.as_bytes())?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, resp_body) = text.split_once("\r\n\r\n").unwrap_or(("", ""));
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}: {resp_body}").into());
    }
    Ok(resp_body.to_string())
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
