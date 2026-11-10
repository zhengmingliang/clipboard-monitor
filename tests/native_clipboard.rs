//! 真实系统剪贴板端到端验证，会替换当前 CLIPBOARD，默认不运行。
//!
//! CI: CLIPBOARD_MONITOR_NATIVE_LIVE=1 cargo test --release --test native_clipboard -- --ignored
//!
//! 覆盖三平台共享契约：文本/图片写入、去重、清空语义、元数据、能力声明。
//! 平台相关的差异（X11 的可靠读取审计、Windows 的推断读取）由各自的
//! capabilities 声明，测试按 cfg 分别校验，不要求三平台行为完全一致。

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
    let mut clipboard = Clipboard::new()?;
    // 结束后尽量恢复用户原来的剪贴板：原实现直接污染不还原。
    let original_text = clipboard.get_text().ok();
    let original_image = clipboard.get_image().ok();

    let result = (|| {
        let base = wait_listen(&dir, &mut child.0)?;
        wait_backend(&base, &mut child.0)?;
        verify_capabilities(&base)?;

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
        verify_source_metadata(&text_event, &text_detail)?;

        // 去重是 portable 后端（Win/macOS）的契约：X11 按所有权变化记事件，
        // 同一文本再次声明所有权会产生新的一条，不强求内容级去重。
        if !cfg!(target_os = "linux") {
            std::thread::sleep(Duration::from_millis(300));
            clipboard.set_text(token.clone())?;
            std::thread::sleep(Duration::from_secs(3));
            let duplicates = count_events(&base, |event| {
                event["summary"]
                    .as_str()
                    .unwrap_or_default()
                    .contains(&token)
            })?;
            assert_eq!(duplicates, 1, "同一内容重复写入应被去重为一条");
        }

        let text_id = text_event["id"].as_i64().ok_or("文本事件没有 ID")?;
        let rgba = vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];
        clipboard.set_image(ImageData {
            width: 2,
            height: 2,
            bytes: Cow::Owned(rgba),
        })?;
        let (image_event, image_detail) = wait_event(&base, Duration::from_secs(20), |event| {
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
        // X11 后端读取剪贴板里真实的 image/png target；Windows/macOS 后端直接
        // 对 arboard 解码出的 RGBA 像素求摘要，标记为 image/x-rgba。两者都是
        // 图片被完整捕获的有效表示，按平台分别校验。
        let expected_mime = if cfg!(target_os = "linux") {
            "image/png"
        } else {
            "image/x-rgba"
        };
        assert_eq!(meta["image"]["mime"], expected_mime);
        verify_source_metadata(&image_event, &image_detail)?;

        // 清空语义同样只属于 portable 后端：X11 的空文本目标由 fetch 层
        // 处理，语义不同，不在跨平台契约里强求。
        if !cfg!(target_os = "linux") && clipboard.set_text(String::new()).is_ok() {
            let (clear_event, _clear_detail) =
                wait_event(&base, Duration::from_secs(20), |event| {
                    event["op"] == "write"
                        && event["content_type"] == "empty"
                        && event["id"].as_i64().unwrap_or(0)
                            > image_event["id"].as_i64().unwrap_or(0)
                })?;
            assert_eq!(
                clear_event["reason"].as_str(),
                Some("cleared"),
                "清空应带 cleared 语义"
            );
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    })();

    // 恢复剪贴板（尽力而为，失败不影响测试结论）。
    if let Some(text) = original_text {
        let _ = clipboard.set_text(text);
    } else if let Some(image) = original_image {
        let _ = clipboard.set_image(ImageData {
            width: image.width,
            height: image.height,
            bytes: image.bytes,
        });
    }

    if let Err(err) = result {
        let logs = fs::read_to_string(&log_path).unwrap_or_default();
        return Err(format!("{err}\n监控日志:\n{logs}").into());
    }
    child.0.kill()?;
    child.0.wait()?;
    let _ = fs::remove_dir_all(dir);
    Ok(())
}

/// 能力声明必须与实际后端一致；`changes_ready` 为真时不能谎报能力。
fn verify_capabilities(base: &str) -> Result<(), Box<dyn std::error::Error>> {
    let status = get_json(&format!("{base}/api/status"))?;
    assert_eq!(status["changes_ready"], true, "后端应已就绪");
    let caps = &status["capabilities"];
    assert_eq!(caps["text"], true);
    assert_eq!(caps["image"], true);
    if cfg!(target_os = "linux") {
        assert_eq!(caps["reads"], true, "X11 应声明可靠读取");
        assert_eq!(caps["primary"], true);
        assert_eq!(caps["source_info"], false);
        assert_eq!(caps["reads_inferred"], false);
    } else {
        assert_eq!(caps["reads"], false, "非 X11 不得声称可靠读取");
        assert_eq!(caps["primary"], false);
        assert_eq!(caps["source_info"], true, "Win/mac 应声明来源信息能力");
        assert_eq!(caps["formats"], true);
        assert_eq!(
            caps["reads_inferred"],
            cfg!(target_os = "windows"),
            "只有 Windows 声明推断读取"
        );
    }
    Ok(())
}

/// Windows/macOS 应至少给出一项来源元数据，且 meta_json 带 change 判定依据。
/// Linux/X11 有自己的来源解析链路，这里只校验共享的 change 子对象。
fn verify_source_metadata(event: &Value, detail: &Value) -> Result<(), Box<dyn std::error::Error>> {
    let meta: Value = serde_json::from_str(detail["meta_json"].as_str().ok_or("事件没有元数据")?)?;
    if !cfg!(target_os = "linux") {
        let has_source = event["pid"].is_i64()
            || !event["app_name"].as_str().unwrap_or_default().is_empty()
            || !event["exec_path"].as_str().unwrap_or_default().is_empty()
            || event["window_id"].as_i64().unwrap_or(0) != 0;
        assert!(
            has_source,
            "非 X11 平台应采集到至少一项来源元数据，实际事件: {event}"
        );
        assert!(
            meta["change"]["kind"].is_string(),
            "meta_json.change 应给出事件语义"
        );
        assert!(
            meta["formats"].is_array(),
            "meta_json.formats 应记录格式清单"
        );
    }
    Ok(())
}

/// 统计满足条件的事件数（用于 portable 后端的去重断言）。
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn count_events(
    base: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Result<usize, Box<dyn std::error::Error>> {
    let events = get_json(&format!("{base}/api/events?limit=200"))?;
    Ok(events["events"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|event| predicate(event))
        .count())
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
