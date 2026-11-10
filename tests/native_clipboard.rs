//! 真实系统剪贴板端到端验证，会替换当前 CLIPBOARD，默认不运行。
//!
//! CI: CLIPBOARD_MONITOR_NATIVE_LIVE=1 cargo test --release --test native_clipboard -- --ignored

use arboard::{Clipboard, ImageData};
use clipboard_monitor::now_ms;
use serde_json::Value;
use std::borrow::Cow;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[test]
#[ignore]
fn native_text_and_image_changes() {
    if std::env::var("CLIPBOARD_MONITOR_NATIVE_LIVE")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }
    if let Err(err) = run_native_test() {
        panic!("原生剪贴板验证失败: {err}");
    }
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run_native_test() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::temp_dir().join(format!(
        "clipboard-monitor-native-{}-{}",
        std::process::id(),
        now_ms()
    ));
    fs::create_dir_all(&dir)?;
    let log_path = dir.join("native.log");
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let child = Command::new(env!("CARGO_BIN_EXE_clipboard-monitor"))
        .args([
            "serve",
            "--data-dir",
            dir.to_str().ok_or("临时目录不是 UTF-8")?,
            "--listen",
            "127.0.0.1:0",
        ])
        .env("CLIPBOARD_MONITOR_LOG", "debug")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()?;
    let mut child = ChildGuard(child);
    let result = (|| {
        let base = wait_listen(&dir, &mut child.0)?;
        wait_backend(&base, &mut child.0)?;
        let mut clipboard = Clipboard::new()?;
        let token = format!("NATIVE-TEXT-{}-{}", std::process::id(), now_ms());
        clipboard.set_text(token.clone())?;
        let (text_event, text_detail) = wait_event(&base, Duration::from_secs(20), |event| {
            event["op"] == "write"
                && event["content_type"] == "text"
                && event["summary"]
                    .as_str()
                    .unwrap_or_default()
                    .contains(&token)
        })?;
        assert_eq!(text_detail["body"].as_str(), Some(token.as_str()));

        let text_id = text_event["id"].as_i64().ok_or("文本事件没有 ID")?;
        let rgba = vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];
        clipboard.set_image(ImageData {
            width: 2,
            height: 2,
            bytes: Cow::Owned(rgba),
        })?;
        let (_image_event, image_detail) = wait_event(&base, Duration::from_secs(20), |event| {
            event["op"] == "write"
                && event["content_type"] == "image"
                && event["id"].as_i64().unwrap_or(0) > text_id
        })?;
        let meta: Value = serde_json::from_str(
            image_detail["meta_json"]
                .as_str()
                .ok_or("图片事件没有元数据")?,
        )?;
        assert_eq!(meta["image"]["width"], 2);
        assert_eq!(meta["image"]["height"], 2);
        assert_eq!(meta["image"]["mime"], "image/png");
        Ok::<(), Box<dyn std::error::Error>>(())
    })();
    if let Err(err) = result {
        let logs = fs::read_to_string(&log_path).unwrap_or_default();
        return Err(format!("{err}\n监控日志:\n{logs}").into());
    }
    child.0.kill()?;
    child.0.wait()?;
    let _ = fs::remove_dir_all(dir);
    Ok(())
}

fn wait_listen(dir: &Path, child: &mut Child) -> Result<String, Box<dyn std::error::Error>> {
    let path = dir.join("listen");
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            return Err(format!("监控进程提前退出: {status}").into());
        }
        if let Ok(addr) = fs::read_to_string(&path) {
            let addr = addr.trim();
            if !addr.is_empty() {
                return Ok(format!("http://{addr}"));
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("等待监听地址超时".into())
}

fn wait_backend(base: &str, child: &mut Child) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            return Err(format!("监控后端退出: {status}").into());
        }
        if let Ok(status) = get_json(&format!("{base}/api/status")) {
            if status["changes_ready"] == true {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("等待原生监听后端就绪超时".into())
}

fn wait_event(
    base: &str,
    timeout: Duration,
    predicate: impl Fn(&Value) -> bool,
) -> Result<(Value, Value), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + timeout;
    let mut last_http_error = None;
    while Instant::now() < deadline {
        let events = match get_json(&format!("{base}/api/events?limit=200")) {
            Ok(events) => events,
            Err(err) => {
                last_http_error = Some(err);
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        if let Some(event) = events["events"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|event| predicate(event))
        {
            let id = event["id"].as_i64().ok_or("事件 ID 无效")?;
            match get_json(&format!("{base}/api/events/{id}")) {
                Ok(detail) => return Ok((event.clone(), detail)),
                Err(err) => last_http_error = Some(err),
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if let Some(err) = last_http_error {
        return Err(format!("等待剪贴板事件超时，最后一次 HTTP 错误: {err}").into());
    }
    Err("等待目标剪贴板事件超时".into())
}

fn get_json(url: &str) -> Result<Value, Box<dyn std::error::Error>> {
    let rest = url.trim_start_matches("http://");
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let mut stream = std::net::TcpStream::connect(host)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.write_all(
        format!("GET /{path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
    )?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let body = response
        .split_once("\r\n\r\n")
        .map(|parts| parts.1)
        .ok_or("HTTP 响应没有正文")?;
    Ok(serde_json::from_str(body)?)
}
