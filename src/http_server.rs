//! 只给本机用的监控界面。默认绑在 127.0.0.1。

use crate::config::{parse_user_time, Config};
use crate::db::{CleanupSpec, EventFilter};
use crate::human_bytes;
use crate::monitor::App;
use crate::now_ms;
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

const INDEX_HTML: &str = include_str!("assets/index.html");

pub fn serve(app: Arc<App>) -> Result<()> {
    let listen = app
        .config
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .listen
        .clone();
    let listener = TcpListener::bind(&listen).with_context(|| format!("监听 {listen} 失败"))?;
    listener.set_nonblocking(false)?;
    let addr = listener.local_addr()?;
    let shown = addr.to_string();
    *app.listen.write().unwrap_or_else(|e| e.into_inner()) = shown.clone();
    if let Some(dir) = data_dir_from_db(&app) {
        let _ = std::fs::write(dir.join("listen"), format!("{shown}\n"));
    }
    crate::log_info(format!("界面 http://{shown}"));
    if !is_loopback(&shown) {
        crate::log_warn("监听地址不是回环地址，剪贴板日志可能被同网段访问");
    }
    listener.set_nonblocking(true)?;
    let port = addr.port();
    while !app.shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let app = Arc::clone(&app);
                // 请求很少，串行处理即可，避免再开线程堆内存。
                if let Err(err) = handle_client(app, stream, port) {
                    crate::log_debug(format!("HTTP: {err:#}"));
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(150));
            }
            Err(err) => {
                crate::log_warn(format!("接受连接失败: {err}"));
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    Ok(())
}

fn data_dir_from_db(app: &App) -> Option<std::path::PathBuf> {
    app.db
        .lock()
        .ok()
        .and_then(|db| db.path().parent().map(|p| p.to_path_buf()))
}

fn is_loopback(addr: &str) -> bool {
    addr.starts_with("127.") || addr.starts_with("[::1]") || addr.starts_with("localhost:")
}

fn handle_client(app: Arc<App>, mut stream: TcpStream, port: u16) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end;
    loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            bail!("连接提前关闭");
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 1_048_576 {
            bail!("请求过大");
        }
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.lines();
    let request = lines.next().unwrap_or("");
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let mut content_length = 0usize;
    let mut headers: HashMap<String, String> = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let value = v.trim().to_string();
            if key == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.insert(key, value);
        }
    }
    if content_length > 1_048_576 {
        bail!("请求体过大");
    }
    while buf.len() < header_end + content_length {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body =
        &buf[header_end..header_end + content_length.min(buf.len().saturating_sub(header_end))];
    if let Err(err) = authorize(&app, &method, &headers, port) {
        crate::log_debug(format!("HTTP 请求被拒绝: {err:#}"));
        return write_response(
            &mut stream,
            Response {
                status: 403,
                content_type: "application/json; charset=utf-8",
                body: json!({"error": err.to_string()}).to_string().into_bytes(),
            },
        );
    }
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let response = dispatch(&app, &method, path, query, body);
    match response {
        Ok(resp) => write_response(&mut stream, resp),
        Err(err) => {
            let status = if err.to_string().contains("不存在") {
                404
            } else {
                400
            };
            write_response(
                &mut stream,
                Response {
                    status,
                    content_type: "application/json; charset=utf-8",
                    body: json!({"error": err.to_string()}).to_string().into_bytes(),
                },
            )
        }
    }
}

struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

/// DNS 重绑定与跨站请求防护。
///
/// Host 只接受回环地址、IP 字面量或配置里写的监听主机名——浏览器在重绑定的
/// 域名下仍会把原域名放进 Host，域名不在白名单即被拒绝。写操作额外要求
/// JSON Content-Type 与同源标记，挡住 text/plain 表单 CSRF。
fn authorize(app: &App, method: &str, headers: &HashMap<String, String>, port: u16) -> Result<()> {
    let configured = host_part_of(&app.config.read().unwrap_or_else(|e| e.into_inner()).listen);
    authorize_request(method, headers, port, &configured)
}

fn authorize_request(
    method: &str,
    headers: &HashMap<String, String>,
    port: u16,
    configured: &str,
) -> Result<()> {
    if let Some(host) = headers.get("host") {
        check_host(host, port, configured)?;
    }
    if matches!(method, "POST" | "PUT" | "DELETE") {
        let content_type = headers
            .get("content-type")
            .map(|v| v.to_ascii_lowercase())
            .unwrap_or_default();
        if !content_type.starts_with("application/json") {
            bail!("写接口要求 Content-Type: application/json");
        }
        if let Some(site) = headers.get("sec-fetch-site") {
            if !site.eq_ignore_ascii_case("same-origin") {
                bail!("拒绝跨站点写请求");
            }
        }
        if let Some(origin) = headers.get("origin") {
            let authority = origin.split("://").nth(1).unwrap_or(origin);
            check_host(authority, port, configured)?;
        }
    }
    Ok(())
}

fn check_host(host: &str, port: u16, configured: &str) -> Result<()> {
    let (host_part, host_port) = split_host_port(host.trim())?;
    if let Some(p) = host_port {
        if p != port {
            bail!("Host 端口 {p} 与界面端口 {port} 不一致");
        }
    }
    let host_l = host_part.to_ascii_lowercase();
    if host_l == "localhost" || host_l.parse::<std::net::IpAddr>().is_ok() {
        return Ok(());
    }
    if !configured.is_empty() && host_l == configured.to_ascii_lowercase() {
        return Ok(());
    }
    bail!("Host {host_part} 不受信任");
}

/// 拆 "127.0.0.1:27890"、"[::1]:27890"、"myhost:80"，返回不带方括号的主机部分。
fn split_host_port(host: &str) -> Result<(String, Option<u16>)> {
    if let Some(rest) = host.strip_prefix('[') {
        let end = rest.find(']').context("Host 缺少右括号")?;
        let port = match rest[end + 1..].strip_prefix(':') {
            Some(p) => Some(p.parse().with_context(|| format!("Host 端口无效: {p}"))?),
            None => None,
        };
        return Ok((rest[..end].to_string(), port));
    }
    match host.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            Ok((h.to_string(), Some(p.parse().context("Host 端口无效")?)))
        }
        _ => Ok((host.to_string(), None)),
    }
}

fn host_part_of(addr: &str) -> String {
    if let Some(rest) = addr.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            return rest[..end].to_string();
        }
    }
    match addr.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            h.to_string()
        }
        _ => addr.to_string(),
    }
}

fn write_response(stream: &mut TcpStream, resp: Response) -> Result<()> {
    let reason = match resp.status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        resp.status,
        resp.content_type,
        resp.body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&resp.body)?;
    Ok(())
}

fn dispatch(app: &App, method: &str, path: &str, query: &str, body: &[u8]) -> Result<Response> {
    if method == "GET" && path == "/" {
        return Ok(Response {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: INDEX_HTML.as_bytes().to_vec(),
        });
    }
    if method == "GET" && (path == "/api/health" || path == "/api/status") {
        return Ok(json_ok(status_json(app)?));
    }
    if method == "GET" && path == "/api/events" {
        let q = parse_query(query);
        let filter = EventFilter {
            app: q.get("app").cloned(),
            content_type: q.get("type").cloned(),
            op: q.get("op").cloned(),
            selection: q.get("selection").cloned(),
            from_ms: q.get("from").and_then(|s| s.parse().ok()),
            to_ms: q.get("to").and_then(|s| s.parse().ok()),
            query: q.get("q").cloned(),
        };
        let limit = q
            .get("limit")
            .and_then(|s| s.parse().ok())
            .unwrap_or(50)
            .clamp(1, 200);
        let offset = q
            .get("offset")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
            .max(0);
        let db = app.db.lock().unwrap_or_else(|e| e.into_inner());
        let (rows, total) = db.query_events(&filter, limit, offset)?;
        return Ok(json_ok(json!({"total": total, "events": rows})));
    }
    if method == "GET" && path == "/api/apps" {
        let db = app.db.lock().unwrap_or_else(|e| e.into_inner());
        return Ok(json_ok(json!({"apps": db.distinct_apps()?})));
    }
    if method == "GET" && path == "/api/settings" {
        let cfg = app.config.read().unwrap_or_else(|e| e.into_inner()).clone();
        return Ok(json_ok(serde_json::to_value(cfg)?));
    }
    if let Some(id) = path.strip_prefix("/api/events/") {
        if let Some(id) = id.strip_suffix("/file") {
            if method != "GET" {
                bail!("方法不支持");
            }
            let id: i64 = id.parse().map_err(|_| anyhow!("事件不存在"))?;
            let db = app.db.lock().unwrap_or_else(|e| e.into_inner());
            let Some((name, bytes)) = db.get_blob_for_event(id)? else {
                bail!("事件不存在或没有文件内容");
            };
            let _ = name;
            return Ok(Response {
                status: 200,
                content_type: "application/octet-stream",
                body: bytes,
            });
        }
        if method == "GET" {
            let id: i64 = id.parse().map_err(|_| anyhow!("事件不存在"))?;
            let db = app.db.lock().unwrap_or_else(|e| e.into_inner());
            let Some(detail) = db.get_event(id)? else {
                bail!("事件不存在");
            };
            return Ok(json_ok(serde_json::to_value(detail)?));
        }
    }
    if method == "POST" && path == "/api/monitor" {
        #[derive(Deserialize)]
        struct Body {
            enabled: bool,
        }
        let body: Body = serde_json::from_slice(body).context("需要 {\"enabled\": true|false}")?;
        app.set_enabled(body.enabled)?;
        return Ok(json_ok(json!({"enabled": body.enabled})));
    }
    if method == "PUT" && path == "/api/settings" {
        let mut cfg: Config = serde_json::from_slice(body).context("设置 JSON 无法解析")?;
        // 监听地址改了要重启进程才生效，先记住旧地址并写回去，避免这次保存把端口改丢。
        let current_listen = app
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .listen
            .clone();
        let listen_changed = cfg.listen != current_listen;
        if listen_changed {
            cfg.listen = current_listen;
        }
        app.replace_config(cfg)?;
        return Ok(json_ok(json!({
            "saved": true,
            "listen_changed_ignored": listen_changed,
        })));
    }
    if method == "POST" && path == "/api/cleanup" {
        #[derive(Deserialize)]
        struct Body {
            from_ms: Option<i64>,
            to_ms: Option<i64>,
            from: Option<String>,
            to: Option<String>,
            content_type: Option<String>,
            op: Option<String>,
            dry_run: Option<bool>,
        }
        let body: Body = serde_json::from_slice(body).context("清理条件无法解析")?;
        let from_ms = match body.from_ms {
            Some(v) => Some(v),
            None => match body.from.as_deref() {
                Some(s) if !s.is_empty() => Some(parse_user_time(s)?),
                _ => None,
            },
        };
        let to_ms = match body.to_ms {
            Some(v) => Some(v),
            None => match body.to.as_deref() {
                Some(s) if !s.is_empty() => Some(parse_user_time(s)?),
                _ => None,
            },
        };
        let spec = CleanupSpec {
            from_ms,
            to_ms,
            content_type: body.content_type,
            op: body.op,
            dry_run: body.dry_run.unwrap_or(false),
        };
        let db = app.db.lock().unwrap_or_else(|e| e.into_inner());
        let n = db.cleanup(&spec)?;
        return Ok(json_ok(json!({"matched": n, "dry_run": spec.dry_run})));
    }
    bail!("不存在的路径 {path}")
}

fn status_json(app: &App) -> Result<Value> {
    let db = app.db.lock().unwrap_or_else(|e| e.into_inner());
    let stats = db.stats()?;
    let cfg = app.config.read().unwrap_or_else(|e| e.into_inner()).clone();
    let listen = app.listen.read().unwrap_or_else(|e| e.into_inner()).clone();
    Ok(json!({
        "enabled": app.enabled.load(Ordering::Relaxed),
        "uptime_sec": app.started.elapsed().as_secs(),
        "pid": app.self_pid,
        "listen": listen,
        "display": if cfg!(target_os = "linux") { std::env::var("DISPLAY").unwrap_or_default() } else { String::new() },
        "backend": crate::monitor::BACKEND,
        "changes_ready": app.changes_ok.load(Ordering::Relaxed),
        "capabilities": {
            "text": true,
            "image": true,
            "reads": crate::monitor::SUPPORTS_READS,
            "primary": crate::monitor::SUPPORTS_PRIMARY,
        },
        "xfixes": app.xfixes_ok.load(Ordering::Relaxed),
        "record": app.record_ok.load(Ordering::Relaxed),
        "writes_seen": app.counters.writes.load(Ordering::Relaxed),
        "reads_seen": app.counters.reads.load(Ordering::Relaxed),
        "ignored_self": app.counters.ignored_self.load(Ordering::Relaxed),
        "last_event_ms": app.counters.last_event_ms.load(Ordering::Relaxed),
        "last_error": app.last_error.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        "retention_days": cfg.retention_days,
        "max_db_bytes": cfg.max_db_bytes(),
        "max_db_human": human_bytes(cfg.max_db_bytes()),
        "db": stats,
        "now_ms": now_ms(),
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

fn json_ok(value: Value) -> Response {
    Response {
        status: 200,
        content_type: "application/json; charset=utf-8",
        body: value.to_string().into_bytes(),
    }
}

fn parse_query(q: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(percent_decode(k), percent_decode(v));
    }
    map
}

fn percent_decode(s: &str) -> String {
    let bytes = s.replace('+', " ");
    percent_encoding::percent_decode_str(&bytes)
        .decode_utf8_lossy()
        .into_owned()
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn host_allows_loopback_and_ips_rejects_domains() {
        assert!(check_host("127.0.0.1:27890", 27890, "").is_ok());
        assert!(check_host("localhost:27890", 27890, "").is_ok());
        assert!(check_host("[::1]:27890", 27890, "").is_ok());
        assert!(check_host("192.168.1.5:27890", 27890, "").is_ok());
        assert!(check_host("mybox.local:27890", 27890, "mybox.local").is_ok());
        assert!(check_host("evil.example:27890", 27890, "").is_err());
        assert!(check_host("evil.example", 27890, "").is_err());
        assert!(check_host("127.0.0.1:9999", 27890, "").is_err());
    }

    #[test]
    fn host_part_of_strips_port_and_brackets() {
        assert_eq!(host_part_of("127.0.0.1:27890"), "127.0.0.1");
        assert_eq!(host_part_of("[::1]:27890"), "::1");
        assert_eq!(host_part_of("mybox.local"), "mybox.local");
    }

    #[test]
    fn write_requests_require_json_and_same_origin() {
        let ok = hdr(&[
            ("host", "127.0.0.1:27890"),
            ("content-type", "application/json"),
        ]);
        assert!(authorize_request("POST", &ok, 27890, "").is_ok());

        let plain = hdr(&[("host", "127.0.0.1:27890"), ("content-type", "text/plain")]);
        assert!(authorize_request("POST", &plain, 27890, "").is_err());

        let no_ct = hdr(&[("host", "127.0.0.1:27890")]);
        assert!(authorize_request("POST", &no_ct, 27890, "").is_err());

        let cross = hdr(&[
            ("host", "127.0.0.1:27890"),
            ("content-type", "application/json"),
            ("sec-fetch-site", "cross-site"),
        ]);
        assert!(authorize_request("PUT", &cross, 27890, "").is_err());

        let evil_origin = hdr(&[
            ("host", "127.0.0.1:27890"),
            ("content-type", "application/json"),
            ("origin", "http://evil.example"),
        ]);
        assert!(authorize_request("POST", &evil_origin, 27890, "").is_err());

        let same_origin = hdr(&[
            ("host", "127.0.0.1:27890"),
            ("content-type", "application/json"),
            ("origin", "http://127.0.0.1:27890"),
        ]);
        assert!(authorize_request("POST", &same_origin, 27890, "").is_ok());
    }

    #[test]
    fn get_requests_do_not_need_content_type() {
        let get = hdr(&[("host", "127.0.0.1:27890")]);
        assert!(authorize_request("GET", &get, 27890, "").is_ok());
        let no_host = hdr(&[]);
        assert!(authorize_request("GET", &no_host, 27890, "").is_ok());
    }
}
