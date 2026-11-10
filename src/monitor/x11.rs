//! 监控线程。
//!
//! 写入来自 XFixes 的选择区所有者变化，读取来自 RECORD 拦截的 ConvertSelection。
//! 两条事件先进入约 40ms 的重排缓冲，按 X 服务器时间排序后再落库，避免读事件
//! 比对应的写事件更早入库。抓取内容在另一条连接上完成，不挡住事件读取。

use super::{limits_from, App, Snap};
use crate::classify::{empty_capture, unavailable_capture, Captured, Limits};
use crate::config::is_whitelisted;
use crate::db::NewEvent;
use crate::fetch::{self, Atoms};
use crate::log_debug;
use crate::log_info;
use crate::log_warn;
use crate::now_ms;
use crate::resolver::{AppInfo, Resolver};
use crate::x11util::{
    self, client_range, connect_x, create_helper_window, raw_fd, wait_readable, XConn,
};
use crate::xparse::{parse_convert_selections, partial_request_carry};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::record::{self, ConnectionExt as RecordExt};
use x11rb::protocol::xfixes::{self, ConnectionExt as XFixesExt};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, PropMode, Window};
use x11rb::protocol::Event;

const REORDER: Duration = Duration::from_millis(40);
const READ_BURST: i64 = 300;

/// 抓取完成前的写入占位摘要；读取事件据此标记 pending，等抓取回填。
const PENDING_SUMMARY: &str = "内容获取中";

struct RawWrite {
    seq: u64,
    wall_ms: i64,
    x_time: u32,
    arrived: Instant,
    selection_name: String,
    selection_atom: u32,
    owner: u32,
    reason: &'static str,
}

struct RawRead {
    seq: u64,
    wall_ms: i64,
    x_time: u32,
    arrived: Instant,
    selection_name: String,
    #[allow(dead_code)]
    selection_atom: u32,
    requestor: u32,
    target_atom: u32,
    xid_base: u32,
}

enum Incoming {
    Write(RawWrite),
    Read(RawRead),
}

enum Msg {
    Write(RawWrite),
    Read(RawRead),
    FetchDone(FetchDone),
}

struct FetchJob {
    selection: u32,
    selection_name: String,
    owner: u32,
    content_id: i64,
    limits: Limits,
}

struct FetchDone {
    content_id: i64,
    selection_name: String,
    captured: Captured,
}

enum Job {
    Fetch(FetchJob),
    Stop,
}

struct Burst {
    event_id: i64,
    #[allow(dead_code)]
    xid_base: u32,
    wall_ms: i64,
    targets: Vec<String>,
}

pub(super) fn spawn(app: Arc<App>) -> Result<Vec<JoinHandle<()>>> {
    let (tx, rx) = mpsc::channel::<Msg>();
    let pipeline_app = Arc::clone(&app);
    let pipeline_tx = tx.clone();
    let pipeline = thread::Builder::new()
        .name("clip-pipeline".into())
        .spawn(move || pipeline_main(pipeline_app, rx, pipeline_tx))?;
    let xthread = thread::Builder::new()
        .name("clip-x11".into())
        .spawn(move || x_supervisor(app, tx))?;
    Ok(vec![pipeline, xthread])
}

fn x_supervisor(app: Arc<App>, tx: Sender<Msg>) {
    while !app.shutdown.load(Ordering::Relaxed) {
        match run_x_pair(&app, &tx) {
            Ok(()) => break,
            Err(err) => {
                let msg = format!("X11 监控中断，1 秒后重试: {err:#}");
                log_warn(&msg);
                app.note_error(&msg);
                app.record_ok.store(false, Ordering::Relaxed);
                app.xfixes_ok.store(false, Ordering::Relaxed);
                app.changes_ok.store(false, Ordering::Relaxed);
                thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

fn run_x_pair(app: &Arc<App>, tx: &Sender<Msg>) -> Result<()> {
    let (ctx_tx, ctx_rx) = mpsc::channel::<Result<u32>>();
    let reader_app = Arc::clone(app);
    let reader_tx = tx.clone();
    let reader = thread::Builder::new()
        .name("clip-record".into())
        .spawn(move || {
            let start = match ctx_rx.recv_timeout(Duration::from_secs(8)) {
                Ok(Ok(id)) => id,
                Ok(Err(err)) => {
                    log_warn(format!("读取监控不可用: {err:#}"));
                    return;
                }
                Err(_) => {
                    log_warn("等待 RECORD 上下文超时");
                    return;
                }
            };
            if let Err(err) = read_loop(&reader_app, &reader_tx, start) {
                log_warn(format!("读取监控退出: {err:#}"));
                reader_app.note_error(&format!("读取监控退出: {err:#}"));
                reader_app.record_ok.store(false, Ordering::Relaxed);
            }
        })?;

    let write_result = write_loop(app, tx, ctx_tx);
    let _ = reader.join();
    write_result
}

fn write_loop(app: &App, tx: &Sender<Msg>, ctx_tx: Sender<Result<u32>>) -> Result<()> {
    let (conn, screen) = connect_x()?;
    app.register_client(client_range(&conn));
    let fd = raw_fd(&conn);
    let record_id = match setup_record(&conn) {
        Ok(id) => {
            let _ = ctx_tx.send(Ok(id));
            Some(id)
        }
        Err(err) => {
            let _ = ctx_tx.send(Err(err));
            None
        }
    };
    let _disable = record_id.map(|id| RecordGuard { conn: &conn, id });
    if app.shutdown.load(Ordering::Relaxed) {
        return Ok(());
    }
    conn.extension_information(xfixes::X11_EXTENSION_NAME)?
        .context("X 服务器没有 XFIXES 扩展")?;
    conn.xfixes_query_version(5, 0)?.reply()?;
    let window = create_helper_window(&conn, screen)?;
    let atoms = Atoms::load(&conn)?;
    let mask = xfixes::SelectionEventMask::SET_SELECTION_OWNER
        | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
        | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE;
    conn.xfixes_select_selection_input(window, atoms.clipboard, mask)?
        .check()?;
    conn.xfixes_select_selection_input(window, atoms.primary, mask)?
        .check()?;
    conn.flush()?;
    app.xfixes_ok.store(true, Ordering::Relaxed);
    app.changes_ok.store(true, Ordering::Relaxed);
    app.clear_error();
    log_info("写入监控已挂上 XFixes");

    let mut was_enabled = false;
    let mut seq = 0u64;
    while !app.shutdown.load(Ordering::Relaxed) {
        let enabled = app.enabled.load(Ordering::Relaxed);
        if enabled && !was_enabled {
            send_snapshot(app, &conn, &atoms, tx, &mut seq)?;
        }
        was_enabled = enabled;
        while let Some(event) = conn.poll_for_event()? {
            if let Event::XfixesSelectionNotify(ev) = event {
                handle_xfixes(
                    app,
                    &atoms,
                    tx,
                    &mut seq,
                    ev.selection,
                    ev.owner,
                    ev.timestamp,
                    ev.subtype,
                );
            }
        }
        wait_readable(fd, 200);
    }
    Ok(())
}

struct RecordGuard<'a> {
    conn: &'a XConn,
    id: u32,
}

impl Drop for RecordGuard<'_> {
    fn drop(&mut self) {
        let _ = self
            .conn
            .record_disable_context(self.id)
            .map(|c| c.ignore_error());
        let _ = self.conn.flush();
    }
}

fn setup_record(conn: &XConn) -> Result<u32> {
    conn.extension_information(record::X11_EXTENSION_NAME)?
        .context("X 服务器没有 RECORD 扩展，无法记录读取")?;
    let ver = conn.record_query_version(1, 13)?.reply()?;
    log_info(format!(
        "RECORD 扩展版本 {}.{}",
        ver.major_version, ver.minor_version
    ));
    let rc = conn.generate_id()?;
    let empty = record::Range8 { first: 0, last: 0 };
    let empty_ext = record::ExtRange {
        major: empty,
        minor: record::Range16 { first: 0, last: 0 },
    };
    let range = record::Range {
        core_requests: record::Range8 {
            first: x11rb::protocol::xproto::CONVERT_SELECTION_REQUEST,
            last: x11rb::protocol::xproto::CONVERT_SELECTION_REQUEST,
        },
        core_replies: empty,
        ext_requests: empty_ext,
        ext_replies: empty_ext,
        delivered_events: empty,
        device_events: empty,
        errors: empty,
        client_started: false,
        client_died: false,
    };
    conn.record_create_context(rc, 0, &[record::CS::ALL_CLIENTS.into()], &[range])?
        .check()?;
    Ok(rc)
}

fn send_snapshot(
    app: &App,
    conn: &XConn,
    atoms: &Atoms,
    tx: &Sender<Msg>,
    seq: &mut u64,
) -> Result<()> {
    let cfg = app.config.read().unwrap_or_else(|e| e.into_inner()).clone();
    for (name, atom) in selection_pairs(atoms, cfg.monitor_primary) {
        let owner = conn.get_selection_owner(atom)?.reply()?.owner;
        *seq += 1;
        tx.send(Msg::Write(RawWrite {
            seq: *seq,
            wall_ms: now_ms(),
            x_time: 0,
            arrived: Instant::now(),
            selection_name: name.to_string(),
            selection_atom: atom,
            owner,
            reason: "snapshot",
        }))?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_xfixes(
    app: &App,
    atoms: &Atoms,
    tx: &Sender<Msg>,
    seq: &mut u64,
    selection: u32,
    owner: u32,
    x_time: u32,
    subtype: xfixes::SelectionEvent,
) {
    if !app.enabled.load(Ordering::Relaxed) {
        return;
    }
    let Some(name) = atoms.selection_name(selection) else {
        return;
    };
    if name == "PRIMARY"
        && !app
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .monitor_primary
    {
        return;
    }
    let reason = if owner == 0 {
        "cleared"
    } else if u8::from(subtype) == u8::from(xfixes::SelectionEvent::SELECTION_WINDOW_DESTROY) {
        "window_destroy"
    } else if u8::from(subtype) == u8::from(xfixes::SelectionEvent::SELECTION_CLIENT_CLOSE) {
        "client_close"
    } else {
        "owner_change"
    };
    *seq += 1;
    app.counters
        .last_event_ms
        .store(now_ms(), Ordering::Relaxed);
    if tx
        .send(Msg::Write(RawWrite {
            seq: *seq,
            wall_ms: now_ms(),
            x_time,
            arrived: Instant::now(),
            selection_name: name.to_string(),
            selection_atom: selection,
            owner,
            reason,
        }))
        .is_err()
    {
        log_warn("事件管道已关闭，停止上报写入");
    }
}

fn read_loop(app: &App, tx: &Sender<Msg>, context: u32) -> Result<()> {
    let (conn, _screen) = connect_x()?;
    app.register_client(client_range(&conn));
    conn.extension_information(record::X11_EXTENSION_NAME)?
        .context("数据连接看不到 RECORD")?;
    let atoms = Atoms::load(&conn)?;
    app.record_ok.store(true, Ordering::Relaxed);
    log_info("读取监控已挂上 RECORD");
    let mut seq = 1_000_000u64;
    // RECORD 数据块可能把请求拦腰截断；按客户端暂存残片，与下一块拼接。
    let mut carries: HashMap<u32, Vec<u8>> = HashMap::new();
    for reply in conn.record_enable_context(context)? {
        if app.shutdown.load(Ordering::Relaxed) {
            break;
        }
        let reply = reply?;
        if reply.category == 5 {
            break;
        }
        // 1 = FromClient。只订阅了 ConvertSelection。
        if reply.category != 1 || reply.data.is_empty() {
            continue;
        }
        let reqs = {
            let mut buf = carries.remove(&reply.xid_base).unwrap_or_default();
            buf.extend_from_slice(&reply.data);
            let (reqs, consumed) = parse_convert_selections(&buf, reply.client_swapped);
            if let Some(carry) = partial_request_carry(&buf[consumed..], reply.client_swapped) {
                carries.insert(reply.xid_base, carry);
            }
            reqs
        };
        if !app.enabled.load(Ordering::Relaxed) {
            continue;
        }
        for req in reqs {
            if app.is_self_client(reply.xid_base, req.requestor) {
                app.counters.ignored_self.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let Some(name) = atoms.selection_name(req.selection) else {
                continue;
            };
            if name == "PRIMARY"
                && !app
                    .config
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .monitor_primary
            {
                continue;
            }
            seq += 1;
            let wall = now_ms();
            app.counters.last_event_ms.store(wall, Ordering::Relaxed);
            if tx
                .send(Msg::Read(RawRead {
                    seq,
                    wall_ms: wall,
                    x_time: reply.server_time,
                    arrived: Instant::now(),
                    selection_name: name.to_string(),
                    selection_atom: req.selection,
                    requestor: req.requestor,
                    target_atom: req.target,
                    xid_base: reply.xid_base,
                }))
                .is_err()
            {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn selection_pairs(atoms: &Atoms, primary: bool) -> Vec<(&'static str, u32)> {
    let mut pairs = vec![("CLIPBOARD", atoms.clipboard)];
    if primary {
        pairs.push(("PRIMARY", atoms.primary));
    }
    pairs
}

fn pipeline_main(app: Arc<App>, rx: Receiver<Msg>, self_tx: Sender<Msg>) {
    let (job_tx, job_rx) = mpsc::channel::<Job>();
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let worker_app = Arc::clone(&app);
    let worker_tx = self_tx;
    let worker = thread::Builder::new()
        .name("clip-fetch".into())
        .spawn(move || fetch_worker(worker_app, job_rx, worker_tx, ready_tx))
        .ok();
    let _ = ready_rx.recv_timeout(Duration::from_secs(5));

    let mut resolver = rebuild_resolver(&app);
    if resolver.is_none() {
        log_warn("窗口解析连接失败，来源将退化为 window-N，每 5 秒自动重试");
    }
    // 解析连接断开后在独立线程重建；connect 卡住时也不阻塞事件管线。
    let (resolver_tx, resolver_rx) = mpsc::channel::<Option<Resolver>>();
    let mut rebuilding = false;
    let mut buf: Vec<Incoming> = Vec::new();
    let mut bursts: HashMap<String, Burst> = HashMap::new();
    let mut last_probe = Instant::now();
    loop {
        if app.shutdown.load(Ordering::Relaxed) {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(Msg::Write(ev)) => buf.push(Incoming::Write(ev)),
            Ok(Msg::Read(ev)) => buf.push(Incoming::Read(ev)),
            Ok(Msg::FetchDone(done)) => apply_fetch(&app, done),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if last_probe.elapsed() >= Duration::from_secs(5) {
            last_probe = Instant::now();
            if let Some(res) = resolver.as_ref() {
                if !res.ping() {
                    log_warn("窗口解析连接失效，开始重建");
                    resolver = None;
                }
            }
            if resolver.is_none() && !rebuilding {
                rebuilding = true;
                let rebuild_app = Arc::clone(&app);
                let rebuild_tx = resolver_tx.clone();
                let spawned =
                    thread::Builder::new()
                        .name("clip-resolver".into())
                        .spawn(move || {
                            let _ = rebuild_tx.send(rebuild_resolver(&rebuild_app));
                        });
                if spawned.is_err() {
                    rebuilding = false;
                }
            }
        }
        while let Ok(result) = resolver_rx.try_recv() {
            rebuilding = false;
            if result.is_some() {
                log_info("窗口解析连接已就绪");
                resolver = result;
            }
        }
        if let Err(err) = flush_ready(&app, &mut resolver, &mut buf, &mut bursts, &job_tx) {
            log_warn(format!("写入日志失败: {err:#}"));
            app.note_error(&format!("写入日志失败: {err:#}"));
        }
    }
    let _ = job_tx.send(Job::Stop);
    if let Some(worker) = worker {
        let _ = worker.join();
    }
}

/// 连一次 X 并构建窗口解析器。失败返回 None，由调用方稍后重试。
fn rebuild_resolver(app: &App) -> Option<Resolver> {
    let (conn, _) = match connect_x() {
        Ok(pair) => pair,
        Err(err) => {
            log_debug(format!("窗口解析连接失败: {err:#}"));
            return None;
        }
    };
    app.register_client(client_range(&conn));
    match Resolver::new(conn) {
        Ok(resolver) => Some(resolver),
        Err(err) => {
            log_warn(format!("重建窗口解析器失败: {err:#}"));
            None
        }
    }
}

fn flush_ready(
    app: &App,
    resolver: &mut Option<Resolver>,
    buf: &mut Vec<Incoming>,
    bursts: &mut HashMap<String, Burst>,
    job_tx: &Sender<Job>,
) -> Result<()> {
    let mut ready = Vec::new();
    let mut keep = Vec::new();
    for ev in buf.drain(..) {
        let arrived = match &ev {
            Incoming::Write(w) => w.arrived,
            Incoming::Read(r) => r.arrived,
        };
        if arrived.elapsed() >= REORDER {
            ready.push(ev);
        } else {
            keep.push(ev);
        }
    }
    *buf = keep;
    ready.sort_by(cmp_x_time);
    // 单条落库失败不中断整批：其余事件继续处理，最后只上报第一个错误。
    let mut first_err = None;
    for ev in ready {
        let outcome = match ev {
            Incoming::Write(w) => handle_write(app, resolver, bursts, job_tx, w),
            Incoming::Read(r) => handle_read(app, resolver, bursts, r),
        };
        if let Err(err) = outcome {
            if first_err.is_none() {
                first_err = Some(err);
            }
        }
    }
    match first_err {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn cmp_x_time(a: &Incoming, b: &Incoming) -> std::cmp::Ordering {
    let (ta, sa) = x_time_seq(a);
    let (tb, sb) = x_time_seq(b);
    match (ta == 0, tb == 0) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        (true, true) => sa.cmp(&sb),
        (false, false) => (ta.wrapping_sub(tb) as i32).cmp(&0).then(sa.cmp(&sb)),
    }
}

fn x_time_seq(ev: &Incoming) -> (u32, u64) {
    match ev {
        Incoming::Write(w) => (w.x_time, w.seq),
        Incoming::Read(r) => (r.x_time, r.seq),
    }
}

fn handle_write(
    app: &App,
    resolver: &mut Option<Resolver>,
    bursts: &mut HashMap<String, Burst>,
    job_tx: &Sender<Job>,
    ev: RawWrite,
) -> Result<()> {
    // 新写入关闭该选择区所有读突发；键带 xid_base 前缀，须按前缀整体清理。
    let prefix = format!("{}:", ev.selection_name);
    bursts.retain(|key, _| !key.starts_with(&prefix));
    let info = if ev.owner == 0 {
        AppInfo::unknown(0)
    } else if let Some(resolver) = resolver.as_mut() {
        resolver.resolve_window(ev.owner)
    } else {
        AppInfo::unknown(ev.owner)
    };
    {
        let snaps = app.snaps.lock().unwrap_or_else(|e| e.into_inner());
        if ev.reason == "snapshot" {
            if let Some(prev) = snaps.get(&ev.selection_name) {
                if prev.owner == ev.owner {
                    return Ok(());
                }
            }
        }
    }
    let cfg = app.config.read().unwrap_or_else(|e| e.into_inner()).clone();
    let whitelisted = ev.owner != 0
        && is_whitelisted(
            &info.app_name,
            &info.wm_class,
            &info.exec_path,
            &cfg.whitelist,
            app.self_pid,
            info.pid,
            &app.self_exe,
        );
    let limits = limits_from(&cfg);
    let placeholder = if ev.owner == 0 {
        empty_capture()
    } else {
        Captured::placeholder(PENDING_SUMMARY)
    };
    // 白名单程序只留一条"内容未记录"的快照占位，不插内容行、不下发抓取，
    // 保证剪贴板正文完全不落盘。
    let (content_id, event_id) = {
        let db = app.db.lock().unwrap_or_else(|e| e.into_inner());
        let id = if whitelisted {
            None
        } else {
            Some(db.insert_content(&placeholder, ev.wall_ms)?)
        };
        let event_id = if whitelisted {
            None
        } else {
            let event_id = db.insert_event(&NewEvent {
                ts_ms: ev.wall_ms,
                op: "write".into(),
                selection: ev.selection_name.clone(),
                pid: info.pid,
                app_name: info.app_name.clone(),
                wm_class: info.wm_class.clone(),
                exec_path: info.exec_path.clone(),
                window_id: ev.owner,
                content_id: id,
                content_type: placeholder.content_type.clone(),
                content_size: placeholder.size,
                summary: placeholder.summary.clone(),
                targets: String::new(),
                source_app: info.app_name.clone(),
                source_pid: info.pid,
                source_event_id: None,
                truncated: false,
                x_time: ev.x_time,
                reason: ev.reason.to_string(),
                pending: false,
            })?;
            app.counters.writes.fetch_add(1, Ordering::Relaxed);
            Some(event_id)
        };
        (id, event_id)
    };
    {
        let mut snaps = app.snaps.lock().unwrap_or_else(|e| e.into_inner());
        snaps.insert(
            ev.selection_name.clone(),
            Snap {
                content_id,
                event_id,
                source_app: info.app_name.clone(),
                source_pid: info.pid,
                owner: ev.owner,
                summary: if whitelisted {
                    "内容未记录".to_string()
                } else {
                    placeholder.summary.clone()
                },
                content_type: placeholder.content_type,
                size: 0,
                truncated: false,
                targets: String::new(),
            },
        );
    }
    if ev.owner != 0 && !whitelisted {
        if let Some(cid) = content_id {
            let _ = job_tx.send(Job::Fetch(FetchJob {
                selection: ev.selection_atom,
                selection_name: ev.selection_name,
                owner: ev.owner,
                content_id: cid,
                limits,
            }));
        }
    }
    Ok(())
}

fn handle_read(
    app: &App,
    resolver: &mut Option<Resolver>,
    bursts: &mut HashMap<String, Burst>,
    ev: RawRead,
) -> Result<()> {
    if app.is_self_client(ev.xid_base, ev.requestor) {
        app.counters.ignored_self.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
    let info = if let Some(resolver) = resolver.as_mut() {
        resolver.resolve_window(ev.requestor)
    } else {
        AppInfo::unknown(ev.requestor)
    };
    let cfg = app.config.read().unwrap_or_else(|e| e.into_inner()).clone();
    if is_whitelisted(
        &info.app_name,
        &info.wm_class,
        &info.exec_path,
        &cfg.whitelist,
        app.self_pid,
        info.pid,
        &app.self_exe,
    ) {
        return Ok(());
    }
    let target_name = if let Some(resolver) = resolver.as_mut() {
        resolver.atom_name(ev.target_atom)
    } else {
        format!("ATOM_{}", ev.target_atom)
    };
    let snap = app
        .snaps
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&ev.selection_name)
        .cloned();
    let key = format!("{}:{}", ev.selection_name, ev.xid_base);
    if let Some(burst) = bursts.get_mut(&key) {
        if ev.wall_ms.saturating_sub(burst.wall_ms) <= READ_BURST {
            if !burst.targets.iter().any(|t| t == &target_name) {
                burst.targets.push(target_name);
            }
            let summary = read_summary(snap.as_ref(), &burst.targets);
            let db = app.db.lock().unwrap_or_else(|e| e.into_inner());
            db.update_read_burst(
                burst.event_id,
                &burst.targets.join(", "),
                &summary,
                snap.as_ref()
                    .map(|s| s.content_type.as_str())
                    .unwrap_or("custom"),
                snap.as_ref().map(|s| s.size).unwrap_or(0),
                snap.as_ref().and_then(|s| s.content_id),
                snap.as_ref().map(|s| s.source_app.as_str()).unwrap_or(""),
                snap.as_ref().and_then(|s| s.source_pid),
                snap.as_ref().and_then(|s| s.event_id),
                snap.as_ref().is_some_and(|s| s.truncated),
                snap.as_ref().is_some_and(|s| s.summary == PENDING_SUMMARY),
            )?;
            return Ok(());
        }
    }
    let targets = vec![target_name];
    let summary = read_summary(snap.as_ref(), &targets);
    let db = app.db.lock().unwrap_or_else(|e| e.into_inner());
    let event_id = db.insert_event(&NewEvent {
        ts_ms: ev.wall_ms,
        op: "read".into(),
        selection: ev.selection_name.clone(),
        pid: info.pid,
        app_name: info.app_name,
        wm_class: info.wm_class,
        exec_path: info.exec_path,
        window_id: ev.requestor,
        content_id: snap.as_ref().and_then(|s| s.content_id),
        content_type: snap
            .as_ref()
            .map(|s| s.content_type.clone())
            .unwrap_or_else(|| "custom".into()),
        content_size: snap.as_ref().map(|s| s.size).unwrap_or(0),
        summary,
        targets: targets.join(", "),
        source_app: snap
            .as_ref()
            .map(|s| s.source_app.clone())
            .unwrap_or_default(),
        source_pid: snap.as_ref().and_then(|s| s.source_pid),
        source_event_id: snap.as_ref().and_then(|s| s.event_id),
        truncated: snap.as_ref().is_some_and(|s| s.truncated),
        x_time: ev.x_time,
        reason: "read".into(),
        pending: snap.as_ref().is_some_and(|s| s.summary == PENDING_SUMMARY),
    })?;
    drop(db);
    bursts.insert(
        key,
        Burst {
            event_id,
            xid_base: ev.xid_base,
            wall_ms: ev.wall_ms,
            targets,
        },
    );
    app.counters.reads.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

fn read_summary(snap: Option<&Snap>, targets: &[String]) -> String {
    let source = snap
        .map(|s| s.source_app.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("未知来源");
    let body = snap
        .map(|s| s.summary.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(PENDING_SUMMARY);
    let mut text = format!("来自 {source}：{body}");
    if !targets.is_empty() {
        text.push_str(" · 目标 ");
        text.push_str(&targets.join(", "));
    }
    if text.chars().count() > 300 {
        text = text.chars().take(300).collect();
        text.push('…');
    }
    text
}

fn apply_fetch(app: &App, done: FetchDone) {
    let db = app.db.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(err) = db.update_content(done.content_id, &done.captured) {
        log_warn(format!("更新内容失败: {err:#}"));
        return;
    }
    if let Err(err) = db.apply_capture_to_events(done.content_id, &done.captured) {
        log_warn(format!("更新事件摘要失败: {err:#}"));
    }
    drop(db);
    let mut snaps = app.snaps.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(snap) = snaps.get_mut(&done.selection_name) {
        if snap.content_id == Some(done.content_id) {
            snap.summary = done.captured.summary;
            snap.content_type = done.captured.content_type;
            snap.size = done.captured.size;
            snap.truncated = done.captured.truncated;
            snap.targets = done.captured.targets.join(", ");
        }
    }
}

fn fetch_worker(app: Arc<App>, rx: Receiver<Job>, tx: Sender<Msg>, ready: Sender<()>) {
    let mut state = match connect_fetch() {
        Ok(state) => {
            app.register_client(client_range(&state.conn));
            Some(state)
        }
        Err(err) => {
            log_warn(format!("内容抓取连接失败: {err:#}"));
            app.note_error(&format!("内容抓取连接失败: {err:#}"));
            None
        }
    };
    let _ = ready.send(());
    while let Ok(job) = rx.recv() {
        let Job::Fetch(job) = job else {
            break;
        };
        if state.is_none() {
            if let Ok(fresh) = connect_fetch() {
                app.register_client(client_range(&fresh.conn));
                state = Some(fresh);
            }
        }
        let outcome = if let Some(current) = state.as_mut() {
            fetch::capture_selection(
                &current.conn,
                current.window,
                &current.atoms,
                &mut current.names,
                job.selection,
                job.owner,
                &job.limits,
            )
            .map_err(|err| err.to_string())
        } else {
            Err("抓取连接不可用".into())
        };
        let captured = match outcome {
            Ok(cap) => cap,
            Err(err) => {
                log_debug(format!("抓取失败，重建连接: {err}"));
                state = None;
                if let Ok(fresh) = connect_fetch() {
                    app.register_client(client_range(&fresh.conn));
                    state = Some(fresh);
                }
                unavailable_capture("内容抓取失败")
            }
        };
        if tx
            .send(Msg::FetchDone(FetchDone {
                content_id: job.content_id,
                selection_name: job.selection_name,
                captured,
            }))
            .is_err()
        {
            break;
        }
    }
}

struct FetchState {
    conn: XConn,
    window: Window,
    atoms: Atoms,
    names: HashMap<u32, String>,
}

fn connect_fetch() -> Result<FetchState> {
    let (conn, screen) = connect_x()?;
    let window = create_helper_window(&conn, screen)?;
    let atoms = Atoms::load(&conn)?;
    let pid = std::process::id();
    let bytes = pid.to_ne_bytes();
    let _ = conn.change_property(
        PropMode::REPLACE,
        window,
        intern_pid(&conn)?,
        AtomEnum::CARDINAL,
        32,
        1,
        &bytes,
    );
    let _ = conn.flush();
    // 调用方拿到状态后再登记客户端。这里先把窗口建好。
    Ok(FetchState {
        conn,
        window,
        atoms,
        names: HashMap::new(),
    })
}

fn intern_pid(conn: &XConn) -> Result<u32> {
    x11util::intern(conn, "_NET_WM_PID")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::{Database, EventFilter};

    fn test_app(cfg: Config) -> (Arc<App>, std::path::PathBuf) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "clipmon-x11-test-{}-{}-{}",
            std::process::id(),
            crate::now_ms(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Database::open(&dir.join("monitor.db")).unwrap();
        let app = App::new(db, cfg, dir.join("config.json"));
        (app, dir)
    }

    fn write_event(seq: u64, owner: u32) -> RawWrite {
        RawWrite {
            seq,
            wall_ms: crate::now_ms(),
            x_time: 0,
            arrived: Instant::now(),
            selection_name: "CLIPBOARD".into(),
            selection_atom: 1,
            owner,
            reason: "owner_change",
        }
    }

    #[test]
    fn whitelisted_write_keeps_content_out_of_db() {
        let cfg = Config {
            whitelist: vec!["window-123".to_string()],
            ..Default::default()
        };
        let (app, dir) = test_app(cfg);
        let (job_tx, job_rx) = mpsc::channel();
        let mut resolver = None;
        let mut bursts = HashMap::new();
        handle_write(
            &app,
            &mut resolver,
            &mut bursts,
            &job_tx,
            write_event(1, 123),
        )
        .unwrap();
        assert!(
            job_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "白名单程序不应触发内容抓取"
        );
        {
            let db = app.db.lock().unwrap();
            assert_eq!(db.stats().unwrap().contents, 0, "正文不应落盘");
            let (_, total) = db.query_events(&EventFilter::default(), 10, 0).unwrap();
            assert_eq!(total, 0, "白名单写入不应生成事件");
        }
        assert!(app.keep_content_ids().is_empty());
        drop(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn normal_write_still_fetches_and_records() {
        let (app, dir) = test_app(Config::default());
        let (job_tx, job_rx) = mpsc::channel();
        let mut resolver = None;
        let mut bursts = HashMap::new();
        handle_write(
            &app,
            &mut resolver,
            &mut bursts,
            &job_tx,
            write_event(1, 456),
        )
        .unwrap();
        match job_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("普通写入应有抓取任务")
        {
            Job::Fetch(f) => assert_eq!(f.owner, 456),
            Job::Stop => panic!("不应收到停止指令"),
        }
        {
            let db = app.db.lock().unwrap();
            assert_eq!(db.stats().unwrap().contents, 1);
            let (_, total) = db.query_events(&EventFilter::default(), 10, 0).unwrap();
            assert_eq!(total, 1);
        }
        drop(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn flush_keeps_rest_of_batch_after_single_failure() {
        let (app, dir) = test_app(Config::default());
        {
            let db = app.db.lock().unwrap();
            db.exec_for_test(
                "CREATE TRIGGER fail_one BEFORE INSERT ON events
                 WHEN NEW.op = 'read' AND NEW.window_id = 22016
                 BEGIN SELECT RAISE(ABORT, 'boom'); END;",
            )
            .unwrap();
        }
        let (job_tx, _job_rx) = mpsc::channel();
        let mut resolver = None;
        let mut bursts = HashMap::new();
        // arrived 早于重排窗口，flush 时两条都会进入同一批。
        let arrived = Instant::now() - REORDER - Duration::from_millis(10);
        let mut buf = Vec::new();
        buf.push(Incoming::Read(RawRead {
            seq: 1,
            wall_ms: crate::now_ms(),
            x_time: 0,
            arrived,
            selection_name: "CLIPBOARD".into(),
            selection_atom: 1,
            requestor: 22016,
            target_atom: 2,
            xid_base: 0x3a0_0000,
        }));
        buf.push(Incoming::Read(RawRead {
            seq: 2,
            wall_ms: crate::now_ms() + 1,
            x_time: 0,
            arrived,
            selection_name: "CLIPBOARD".into(),
            selection_atom: 1,
            requestor: 22017,
            target_atom: 2,
            xid_base: 0x3a0_0000,
        }));
        let outcome = flush_ready(&app, &mut resolver, &mut buf, &mut bursts, &job_tx);
        assert!(outcome.is_err(), "应把第一个错误上报给调用方");
        let db = app.db.lock().unwrap();
        let (rows, _) = db.query_events(&EventFilter::default(), 10, 0).unwrap();
        let reads: Vec<_> = rows.iter().filter(|r| r.op == "read").collect();
        assert_eq!(reads.len(), 1, "同批失败不应丢弃后续事件");
        assert_eq!(reads[0].window_id, Some(22017));
        drop(db);
        drop(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cmp_x_time_orders_wraparound_and_snapshots() {
        let write = |seq: u64, x_time: u32| {
            Incoming::Write(RawWrite {
                seq,
                wall_ms: 0,
                x_time,
                arrived: Instant::now(),
                selection_name: "CLIPBOARD".into(),
                selection_atom: 1,
                owner: 1,
                reason: "owner_change",
            })
        };
        // 跨 wrap：0xffff_fff0 之后是 0x10。
        let before = write(1, 0xffff_fff0);
        let after = write(2, 0x10);
        assert_eq!(cmp_x_time(&before, &after), std::cmp::Ordering::Less);
        assert_eq!(cmp_x_time(&after, &before), std::cmp::Ordering::Greater);

        // 快照（x_time=0）排在任何真实时间之前。
        let snap = write(3, 0);
        assert_eq!(cmp_x_time(&snap, &before), std::cmp::Ordering::Less);
        assert_eq!(cmp_x_time(&before, &snap), std::cmp::Ordering::Greater);

        // 时间相同按 seq 决定稳定顺序；同为 0 亦然。
        let a = write(7, 1000);
        let b = write(8, 1000);
        assert_eq!(cmp_x_time(&a, &b), std::cmp::Ordering::Less);
        let a0 = write(4, 0);
        let b0 = write(5, 0);
        assert_eq!(cmp_x_time(&a0, &b0), std::cmp::Ordering::Less);
    }

    #[test]
    fn new_write_closes_read_burst_window() {
        let (app, dir) = test_app(Config::default());
        let (job_tx, _job_rx) = mpsc::channel();
        let mut resolver = None;
        let mut bursts = HashMap::new();
        let mk_read = |seq: u64, ms: i64| RawRead {
            seq,
            wall_ms: ms,
            x_time: 0,
            arrived: Instant::now(),
            selection_name: "CLIPBOARD".into(),
            selection_atom: 1,
            requestor: 0x5600,
            target_atom: 2,
            xid_base: 0x3a0_0000,
        };
        handle_read(
            &app,
            &mut resolver,
            &mut bursts,
            mk_read(1, crate::now_ms()),
        )
        .unwrap();
        handle_write(
            &app,
            &mut resolver,
            &mut bursts,
            &job_tx,
            write_event(1, 999),
        )
        .unwrap();
        assert!(bursts.is_empty(), "新写入应关闭全部读突发窗口");
        handle_read(
            &app,
            &mut resolver,
            &mut bursts,
            mk_read(2, crate::now_ms() + 100),
        )
        .unwrap();
        let db = app.db.lock().unwrap();
        let (_, total) = db.query_events(&EventFilter::default(), 10, 0).unwrap();
        drop(db);
        // 写入 1 条 + 两次读取各 1 条；修复前第二次读取会合并进旧读取，总数只有 2。
        assert_eq!(total, 3);
        drop(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn default_config_does_not_monitor_primary() {
        // 选中文字会连续产生 PRIMARY 写入与读取，默认配置必须只监控
        // CLIPBOARD；用户显式打开 monitor_primary 才加入 PRIMARY。
        let atoms = crate::fetch::Atoms {
            clipboard: 1,
            primary: 2,
            targets: 3,
            utf8: 4,
            string: 5,
            text: 6,
            plain: 7,
            plain_utf8: 8,
            html: 9,
            rtf: 10,
            app_rtf: 11,
            rich: 12,
            png: 13,
            jpeg: 14,
            bmp: 15,
            gif: 16,
            webp: 17,
            uri_list: 18,
            gnome: 19,
            moz: 20,
            kde_cut: 21,
            incr: 22,
            prop: 23,
        };
        let cfg = Config::default();
        assert!(!cfg.monitor_primary);
        assert_eq!(
            selection_pairs(&atoms, cfg.monitor_primary),
            vec![("CLIPBOARD", 1u32)]
        );
        assert_eq!(
            selection_pairs(&atoms, true),
            vec![("CLIPBOARD", 1u32), ("PRIMARY", 2u32)]
        );
    }
}
