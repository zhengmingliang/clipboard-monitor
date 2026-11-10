//! 跨平台监控状态、日志落库和自动清理。
//!
//! Linux 保留 X11 事件管线；Windows 和 macOS 由各自原生后端检测变化。

#[cfg(any(target_os = "windows", target_os = "macos", test))]
use crate::classify::Captured;
use crate::classify::Limits;
use crate::config::Config;
use crate::db::Database;
#[cfg(any(target_os = "windows", target_os = "macos", test))]
use crate::db::NewEvent;
use crate::{log_info, log_warn};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub mod observation;

#[cfg(target_os = "linux")]
mod x11;
#[cfg(target_os = "linux")]
use x11 as backend;

#[cfg(any(target_os = "windows", target_os = "macos", test))]
mod portable;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use portable as backend;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod read_signal;
#[cfg(target_os = "windows")]
mod win32;

#[cfg(target_os = "linux")]
pub const BACKEND: &str = "x11";
#[cfg(target_os = "windows")]
pub const BACKEND: &str = "windows";
#[cfg(target_os = "macos")]
pub const BACKEND: &str = "macos";
/// 可靠的读取审计：只有 X11 的 RECORD 能拦到 ConvertSelection。
pub const SUPPORTS_READS: bool = cfg!(target_os = "linux");
pub const SUPPORTS_PRIMARY: bool = cfg!(target_os = "linux");
/// OS 能可靠给出内容生产者/触发主体信息的平台。
pub const SUPPORTS_SOURCE_INFO: bool = cfg!(any(target_os = "windows", target_os = "macos"));
/// 只能靠变更序号间接推断读取（不可靠、始终标注 inferred）。
pub const SUPPORTS_INFERRED_READS: bool = cfg!(target_os = "windows");
/// 能列出剪贴板当前格式清单的平台。
pub const SUPPORTS_FORMATS: bool = cfg!(any(target_os = "windows", target_os = "macos"));

#[derive(Default)]
pub struct Counters {
    pub writes: AtomicU64,
    pub reads: AtomicU64,
    pub ignored_self: AtomicU64,
    pub last_event_ms: AtomicI64,
}

pub struct App {
    pub db: Mutex<Database>,
    pub config: RwLock<Config>,
    pub config_path: std::path::PathBuf,
    pub enabled: AtomicBool,
    pub shutdown: AtomicBool,
    pub started: Instant,
    pub counters: Counters,
    pub listen: RwLock<String>,
    pub self_pid: u32,
    pub self_exe: String,
    pub last_error: Mutex<String>,
    pub changes_ok: AtomicBool,
    pub record_ok: AtomicBool,
    pub xfixes_ok: AtomicBool,
    snaps: Mutex<HashMap<String, Snap>>,
    #[cfg(target_os = "linux")]
    clients: RwLock<Vec<crate::x11util::ClientRange>>,
}

#[derive(Clone)]
pub(crate) struct Snap {
    /// 关联的 contents 行。白名单程序的内容不入库，这里为 None。
    content_id: Option<i64>,
    #[cfg(target_os = "linux")]
    event_id: Option<i64>,
    #[cfg(target_os = "linux")]
    source_app: String,
    #[cfg(target_os = "linux")]
    source_pid: Option<i32>,
    #[cfg(target_os = "linux")]
    owner: u32,
    #[cfg(target_os = "linux")]
    summary: String,
    #[cfg(target_os = "linux")]
    content_type: String,
    #[cfg(target_os = "linux")]
    size: u64,
    #[cfg(target_os = "linux")]
    truncated: bool,
    #[cfg(target_os = "linux")]
    targets: String,
}

impl App {
    pub fn new(db: Database, config: Config, config_path: std::path::PathBuf) -> Arc<Self> {
        let enabled = config.enabled_on_start;
        let self_exe = std::env::current_exe()
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        Arc::new(Self {
            db: Mutex::new(db),
            config: RwLock::new(config),
            config_path,
            enabled: AtomicBool::new(enabled),
            shutdown: AtomicBool::new(false),
            started: Instant::now(),
            counters: Counters::default(),
            listen: RwLock::new(String::new()),
            self_pid: std::process::id(),
            self_exe,
            last_error: Mutex::new(String::new()),
            changes_ok: AtomicBool::new(false),
            record_ok: AtomicBool::new(false),
            xfixes_ok: AtomicBool::new(false),
            snaps: Mutex::new(HashMap::new()),
            #[cfg(target_os = "linux")]
            clients: RwLock::new(Vec::new()),
        })
    }

    pub fn set_enabled(&self, enabled: bool) -> Result<()> {
        self.enabled.store(enabled, Ordering::Relaxed);
        let mut cfg = self.config.write().unwrap_or_else(|e| e.into_inner());
        cfg.enabled_on_start = enabled;
        cfg.save(&self.config_path)?;
        Ok(())
    }

    pub fn replace_config(&self, mut cfg: Config) -> Result<()> {
        cfg.validate()?;
        // 启停只走监控开关。保存其它设置时保留当前开关，避免界面把监控重新打开。
        cfg.enabled_on_start = self.enabled.load(Ordering::Relaxed);
        cfg.save(&self.config_path)?;
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = cfg;
        Ok(())
    }

    pub fn keep_content_ids(&self) -> Vec<i64> {
        self.snaps
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter_map(|s| s.content_id)
            .collect()
    }

    #[cfg(target_os = "linux")]
    fn register_client(&self, range: crate::x11util::ClientRange) {
        let mut guard = self.clients.write().unwrap_or_else(|e| e.into_inner());
        if !guard
            .iter()
            .any(|c| c.base == range.base && c.mask == range.mask)
        {
            crate::log_debug(format!(
                "登记自身 X 客户端 base={:#x} mask={:#x}",
                range.base, range.mask
            ));
            guard.push(range);
        }
    }

    #[cfg(target_os = "linux")]
    fn is_self_client(&self, xid_base: u32, requestor: u32) -> bool {
        let guard = self.clients.read().unwrap_or_else(|e| e.into_inner());
        guard
            .iter()
            .any(|c| c.base == xid_base || crate::xparse::xid_belongs(requestor, c.base, c.mask))
    }

    pub(crate) fn note_error(&self, err: &str) {
        *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = err.to_string();
    }

    pub(crate) fn clear_error(&self) {
        self.last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// 非 X11 后端已经取得完整内容，按观察契约写入一条变化记录。
    ///
    /// `meta_json` 追加 `change` / `source` / `trigger` 子对象，让详情页能
    /// 说明"这次事件按什么判定、谁产生、谁触发"，同时保持 `EventRow` 的列
    /// 集合与 X11 完全一致。白名单命中时正文零落盘（与 X11 一致）。
    #[cfg(any(target_os = "windows", target_os = "macos", test))]
    pub(crate) fn record_change(
        &self,
        obs: &crate::monitor::observation::ClipboardObservation,
        spec: &crate::monitor::observation::EventSpec,
        ts_ms: i64,
    ) -> Result<i64> {
        use crate::monitor::observation::{is_source_whitelisted, WhitelistContext};

        let captured = &obs.body;
        let source = obs.source.clone().unwrap_or_default();
        let trigger = obs.trigger.clone().unwrap_or_default();
        let (whitelist, self_pid, self_exe) = {
            let cfg = self.config.read().unwrap_or_else(|e| e.into_inner());
            (cfg.whitelist.clone(), self.self_pid, self.self_exe.clone())
        };
        let whitelisted = is_source_whitelisted(
            obs.source.as_ref(),
            &WhitelistContext {
                list: &whitelist,
                self_pid,
                self_exe: &self_exe,
            },
        );

        if whitelisted {
            // 只更新当前快照，不插 contents / events：敏感程序的内容零落盘。
            self.snaps.lock().unwrap_or_else(|e| e.into_inner()).insert(
                "CLIPBOARD".into(),
                Snap {
                    content_id: None,
                    #[cfg(target_os = "linux")]
                    event_id: None,
                    #[cfg(target_os = "linux")]
                    source_app: String::new(),
                    #[cfg(target_os = "linux")]
                    source_pid: None,
                    #[cfg(target_os = "linux")]
                    owner: 0,
                    #[cfg(target_os = "linux")]
                    summary: captured.summary.clone(),
                    #[cfg(target_os = "linux")]
                    content_type: captured.content_type.clone(),
                    #[cfg(target_os = "linux")]
                    size: captured.size,
                    #[cfg(target_os = "linux")]
                    truncated: captured.truncated,
                    #[cfg(target_os = "linux")]
                    targets: captured.targets.join(", "),
                },
            );
            self.counters.ignored_self.fetch_add(1, Ordering::Relaxed);
            return Ok(-1);
        }

        // 追加观察上下文到 meta_json，不改动共享 classify::assemble 的输出。
        let mut meta: serde_json::Value =
            serde_json::from_str(&captured.meta_json).unwrap_or_else(|_| serde_json::json!({}));
        meta["change"] = spec.change_meta();
        if source.has_any() {
            meta["source"] = serde_json::json!({
                "pid": source.pid,
                "app_name": source.app_name,
                "exec_path": source.exec_path,
                "window_id": source.window_id,
                "attribution": obs.source_attribution,
            });
        } else {
            // OS 无法给出归属时也要留下"为何缺失"的上下文，而不是让字段
            // 静默消失；保留后端给出的 attribution，只补原因说明。
            meta["source"] = serde_json::json!({
                "attribution": obs.source_attribution.clone().unwrap_or_else(|| "unavailable".into()),
                "reason": "当前会话无法取得剪贴板所有者或前台应用",
            });
        }
        if trigger.has_any() {
            meta["trigger"] = serde_json::json!({
                "pid": trigger.pid,
                "app_name": trigger.app_name,
                "window_title": trigger.window_title,
            });
        }
        if !obs.formats.is_empty() {
            meta["formats"] = serde_json::json!(obs.formats);
        }
        if let Some(seq) = obs.sequence {
            meta["sequence"] = serde_json::json!(seq);
        }
        let enriched = Captured {
            meta_json: meta.to_string(),
            ..captured.clone()
        };

        let (content_id, event_id) = {
            let mut db = self.db.lock().unwrap_or_else(|e| e.into_inner());
            db.insert_change(
                &enriched,
                &NewEvent {
                    ts_ms,
                    op: spec.op.into(),
                    selection: "CLIPBOARD".into(),
                    pid: source.pid,
                    app_name: source.app_name.clone(),
                    wm_class: String::new(), // X11 概念，其他平台恒空
                    exec_path: source.exec_path.clone(),
                    window_id: source.window_id,
                    content_id: None, // insert_change 在事务内回填
                    content_type: captured.content_type.clone(),
                    content_size: captured.size,
                    summary: captured.summary.clone(),
                    targets: if obs.formats.is_empty() {
                        captured.targets.join(", ")
                    } else {
                        obs.formats.join(", ")
                    },
                    source_app: source.app_name.clone(),
                    source_pid: source.pid,
                    source_event_id: None,
                    truncated: captured.truncated,
                    x_time: obs.sequence.unwrap_or(0) as u32,
                    reason: spec.reason.into(),
                    pending: false,
                },
                ts_ms,
            )?
        };
        #[cfg(not(target_os = "linux"))]
        let _ = event_id;
        self.snaps.lock().unwrap_or_else(|e| e.into_inner()).insert(
            "CLIPBOARD".into(),
            Snap {
                content_id: Some(content_id),
                #[cfg(target_os = "linux")]
                event_id: Some(event_id),
                #[cfg(target_os = "linux")]
                source_app: source.app_name.clone(),
                #[cfg(target_os = "linux")]
                source_pid: source.pid,
                #[cfg(target_os = "linux")]
                owner: 0,
                #[cfg(target_os = "linux")]
                summary: captured.summary.clone(),
                #[cfg(target_os = "linux")]
                content_type: captured.content_type.clone(),
                #[cfg(target_os = "linux")]
                size: captured.size,
                #[cfg(target_os = "linux")]
                truncated: captured.truncated,
                #[cfg(target_os = "linux")]
                targets: obs.formats.join(", "),
            },
        );
        if spec.op == "read" {
            self.counters.reads.fetch_add(1, Ordering::Relaxed);
        } else {
            self.counters.writes.fetch_add(1, Ordering::Relaxed);
        }
        self.counters.last_event_ms.store(ts_ms, Ordering::Relaxed);
        Ok(content_id)
    }

    /// Windows 推断读取：只插一条 `op="read"` 事件，复用当前剪贴板内容行，
    /// 不新建 contents、不改动快照。读取方进程信息来自 `ReadSignal`。
    #[cfg(any(target_os = "windows", test))]
    pub(crate) fn record_read_event(
        &self,
        signal: &crate::monitor::observation::ReadSignal,
        ts_ms: i64,
    ) -> Result<()> {
        use crate::monitor::observation::{is_source_whitelisted, WhitelistContext};

        let reader = signal.reader.clone().unwrap_or_default();
        let (whitelist, self_pid, self_exe) = {
            let cfg = self.config.read().unwrap_or_else(|e| e.into_inner());
            (cfg.whitelist.clone(), self.self_pid, self.self_exe.clone())
        };
        if is_source_whitelisted(
            signal.reader.as_ref(),
            &WhitelistContext {
                list: &whitelist,
                self_pid,
                self_exe: &self_exe,
            },
        ) {
            return Ok(());
        }
        let content_id = self
            .snaps
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get("CLIPBOARD")
            .and_then(|s| s.content_id);
        // 摘要、格式与内容类型沿用最近一条写入事件；没有写入记录时退化。
        let (mut summary, targets, content_type, size, truncated, source_app, source_pid) = {
            let db = self.db.lock().unwrap_or_else(|e| e.into_inner());
            let filter = crate::db::EventFilter {
                op: Some("write".into()),
                ..Default::default()
            };
            let (rows, _) = db.query_events(&filter, 1, 0)?;
            match rows.first() {
                Some(row) => (
                    row.summary.clone(),
                    row.targets.clone(),
                    row.content_type.clone(),
                    row.content_size as u64,
                    row.truncated,
                    row.source_app.clone(),
                    row.source_pid,
                ),
                None => (
                    "推断读取（剪贴板内容未变）".to_string(),
                    String::new(),
                    "custom".to_string(),
                    0,
                    false,
                    String::new(),
                    None,
                ),
            }
        };
        // events 表没有 meta_json 列，把判定依据编码进 summary 后缀，
        // 详情页会同时展示 reason=inferred_read 与这段说明。
        summary.push_str(&format!(" · 推断读取依据：{}", signal.evidence));
        if let Some(trigger) = &signal.trigger {
            if !trigger.app_name.is_empty() {
                summary.push_str(&format!(" · 前台：{}", trigger.app_name));
            }
        }
        if summary.chars().count() > 240 {
            summary = summary.chars().take(239).collect::<String>() + "…";
        }
        let db = self.db.lock().unwrap_or_else(|e| e.into_inner());
        db.insert_event(&NewEvent {
            ts_ms,
            op: "read".into(),
            selection: "CLIPBOARD".into(),
            pid: reader.pid,
            app_name: reader.app_name.clone(),
            wm_class: String::new(),
            exec_path: reader.exec_path.clone(),
            window_id: reader.window_id,
            content_id,
            content_type,
            content_size: size,
            summary,
            targets,
            source_app,
            source_pid: source_pid.and_then(|p| i32::try_from(p).ok()),
            source_event_id: None,
            truncated,
            x_time: 0,
            reason: "inferred_read".into(),
            pending: false,
        })?;
        drop(db);
        self.counters.reads.fetch_add(1, Ordering::Relaxed);
        self.counters.last_event_ms.store(ts_ms, Ordering::Relaxed);
        Ok(())
    }
}

pub struct Threads {
    joins: Vec<JoinHandle<()>>,
}

pub fn spawn(app: Arc<App>) -> Result<Threads> {
    let mut joins = backend::spawn(Arc::clone(&app))?;
    let clean_app = Arc::clone(&app);
    let cleaner = thread::Builder::new()
        .name("clip-cleanup".into())
        .spawn(move || cleanup_loop(clean_app))?;
    joins.push(cleaner);
    Ok(Threads { joins })
}

pub fn shutdown(app: &App, threads: Threads) {
    app.shutdown.store(true, Ordering::Relaxed);
    for join in threads.joins {
        let _ = join.join();
    }
    if let Ok(db) = app.db.lock() {
        db.checkpoint();
    }
}

fn cleanup_loop(app: Arc<App>) {
    while !app.shutdown.load(Ordering::Relaxed) {
        for _ in 0..30 {
            if app.shutdown.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_secs(1));
        }
        let (days, max_bytes) = {
            let cfg = app.config.read().unwrap_or_else(|e| e.into_inner());
            (cfg.retention_days, cfg.max_db_bytes())
        };
        let keep = app.keep_content_ids();
        match app.db.lock() {
            Ok(db) => match db.enforce_limits(days, max_bytes, &keep) {
                Ok((expired, trimmed)) => {
                    if expired > 0 || trimmed > 0 {
                        log_info(format!(
                            "自动清理：过期 {expired} 条，容量淘汰 {trimmed} 条"
                        ));
                    }
                }
                Err(err) => log_warn(format!("自动清理失败: {err:#}")),
            },
            Err(_) => log_warn("数据库锁中断，跳过本轮清理"),
        }
    }
}

pub(crate) fn limits_from(cfg: &Config) -> Limits {
    Limits {
        text_full_limit: cfg.text_full_limit_bytes,
        text_preview: cfg.text_preview_bytes,
        file_content_limit: cfg.file_content_limit_bytes,
        record_file_content: cfg.record_file_content,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{assemble, FetchInput};
    use crate::db::EventFilter;

    #[test]
    fn native_change_is_stored_with_text_metadata_and_current_snapshot() {
        use crate::monitor::observation::{
            classify_event, ClipboardObservation, SourceInfo, TriggerInfo,
        };

        let dir = std::env::temp_dir().join(format!(
            "clipboard-monitor-metadata-test-{}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        let db = Database::open(&dir.join("monitor.db")).expect("open test database");
        let app = App::new(db, Config::default(), dir.join("config.json"));
        let captured = assemble(
            &FetchInput {
                plain: Some(b"cross-platform text".to_vec()),
                plain_mime: Some("text/plain".into()),
                plain_total: 19,
                target_names: vec!["text/plain".into()],
                ..FetchInput::default()
            },
            &Limits::default(),
        );
        let mut obs = ClipboardObservation::new(captured);
        obs.formats = vec!["text/plain".into(), "text/html".into()];
        obs.sequence = Some(42);
        obs.source = Some(SourceInfo {
            pid: Some(1234),
            app_name: "Notepad".into(),
            exec_path: "C:\\Windows\\notepad.exe".into(),
            window_id: 7,
        });
        obs.trigger = Some(TriggerInfo {
            pid: Some(1234),
            app_name: "Notepad".into(),
            window_title: "Untitled".into(),
        });
        let spec = classify_event(None, &obs).expect("首条观察应落库");
        let content_id = app.record_change(&obs, &spec, 1234).expect("record change");
        assert_eq!(app.counters.writes.load(Ordering::Relaxed), 1);
        assert_eq!(app.keep_content_ids(), vec![content_id]);
        let db = app.db.lock().unwrap();
        let (events, total) = db.query_events(&EventFilter::default(), 10, 0).unwrap();
        assert_eq!(total, 1);
        let row = &events[0];
        assert_eq!(row.content_id, Some(content_id));
        assert_eq!(row.content_type, "text");
        assert_eq!(row.pid, Some(1234));
        assert_eq!(row.app_name, "Notepad");
        assert_eq!(row.exec_path, "C:\\Windows\\notepad.exe");
        assert_eq!(row.window_id, Some(7));
        assert_eq!(row.source_app, "Notepad");
        assert_eq!(row.source_pid, Some(1234));
        assert_eq!(row.reason, "snapshot");
        assert_eq!(row.targets, "text/plain, text/html");
        let detail = db.get_event(events[0].id).unwrap().unwrap();
        assert_eq!(detail.body.as_deref(), Some("cross-platform text"));
        let meta: serde_json::Value =
            serde_json::from_str(detail.meta_json.as_deref().unwrap()).unwrap();
        assert_eq!(meta["change"]["kind"], "snapshot");
        assert_eq!(meta["source"]["app_name"], "Notepad");
        assert_eq!(meta["trigger"]["window_title"], "Untitled");
        assert_eq!(meta["formats"][1], "text/html");
        assert_eq!(meta["sequence"], 42);
        drop(db);
        drop(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn whitelisted_source_keeps_content_out_of_db() {
        use crate::monitor::observation::{classify_event, ClipboardObservation, SourceInfo};

        let dir = std::env::temp_dir().join(format!(
            "clipboard-monitor-whitelist-test-{}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        let config = Config {
            whitelist: vec!["SecretApp".into()],
            ..Config::default()
        };
        let db = Database::open(&dir.join("monitor.db")).expect("open test database");
        let app = App::new(db, config, dir.join("config.json"));
        let mut obs = ClipboardObservation::new(assemble(
            &FetchInput {
                plain: Some(b"top secret".to_vec()),
                plain_mime: Some("text/plain".into()),
                plain_total: 10,
                target_names: vec!["text/plain".into()],
                ..FetchInput::default()
            },
            &Limits::default(),
        ));
        obs.source = Some(SourceInfo {
            app_name: "SecretApp".into(),
            ..Default::default()
        });
        let spec = classify_event(None, &obs).expect("首条观察应落库");
        let content_id = app.record_change(&obs, &spec, 1234).expect("record change");
        assert_eq!(content_id, -1, "白名单命中不应产生 contents 行");
        assert_eq!(app.counters.writes.load(Ordering::Relaxed), 0);
        assert_eq!(app.counters.ignored_self.load(Ordering::Relaxed), 1);
        let db = app.db.lock().unwrap();
        let (_events, total) = db.query_events(&EventFilter::default(), 10, 0).unwrap();
        assert_eq!(total, 0, "白名单命中不应产生 events 行");
        drop(db);
        drop(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn inferred_read_event_reuses_content_and_records_evidence() {
        use crate::monitor::observation::{
            classify_event, ClipboardObservation, ReadSignal, SourceInfo, TriggerInfo,
        };

        let dir = std::env::temp_dir().join(format!(
            "clipboard-monitor-read-test-{}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        let db = Database::open(&dir.join("monitor.db")).expect("open test database");
        let app = App::new(db, Config::default(), dir.join("config.json"));
        let mut obs = ClipboardObservation::new(assemble(
            &FetchInput {
                plain: Some(b"copied text".to_vec()),
                plain_mime: Some("text/plain".into()),
                plain_total: 11,
                target_names: vec!["text/plain".into()],
                ..FetchInput::default()
            },
            &Limits::default(),
        ));
        obs.formats = vec!["text/plain".into()];
        obs.sequence = Some(9);
        let spec = classify_event(None, &obs).expect("首条观察应落库");
        let content_id = app.record_change(&obs, &spec, 1000).expect("写入");

        let signal = ReadSignal {
            evidence: "both",
            reader: Some(SourceInfo {
                pid: Some(4321),
                app_name: "notepad".into(),
                exec_path: "C:\\notepad.exe".into(),
                window_id: 11,
            }),
            trigger: Some(TriggerInfo {
                pid: Some(4321),
                app_name: "notepad".into(),
                window_title: String::new(),
            }),
        };
        app.record_read_event(&signal, 1500).expect("推断读取");

        assert_eq!(app.counters.writes.load(Ordering::Relaxed), 1);
        assert_eq!(app.counters.reads.load(Ordering::Relaxed), 1);
        let db = app.db.lock().unwrap();
        let (events, total) = db.query_events(&EventFilter::default(), 10, 0).unwrap();
        assert_eq!(total, 2);
        let read = events.iter().find(|e| e.op == "read").expect("应有读事件");
        assert_eq!(
            read.content_id,
            Some(content_id),
            "读事件复用同一 contents 行"
        );
        assert_eq!(read.reason, "inferred_read");
        assert_eq!(read.pid, Some(4321));
        assert_eq!(read.app_name, "notepad");
        assert_eq!(read.exec_path, "C:\\notepad.exe");
        assert_eq!(read.window_id, Some(11));
        assert!(
            read.summary.contains("both"),
            "摘要应写明判定依据，实际: {}",
            read.summary
        );
        assert_eq!(read.content_type, "text");
        // 读事件不新建 contents 行。
        let stats = db.stats().unwrap();
        assert_eq!(stats.contents, 1);
        drop(db);
        drop(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn whitelisted_reader_suppresses_inferred_read() {
        use crate::monitor::observation::{ReadSignal, SourceInfo};

        let dir = std::env::temp_dir().join(format!(
            "clipboard-monitor-read-whitelist-test-{}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        let config = Config {
            whitelist: vec!["SecretApp".into()],
            ..Config::default()
        };
        let db = Database::open(&dir.join("monitor.db")).expect("open test database");
        let app = App::new(db, config, dir.join("config.json"));
        let signal = ReadSignal {
            evidence: "ctrl_v",
            reader: Some(SourceInfo {
                app_name: "SecretApp".into(),
                ..Default::default()
            }),
            trigger: None,
        };
        app.record_read_event(&signal, 1000).expect("推断读取");
        assert_eq!(app.counters.reads.load(Ordering::Relaxed), 0);
        let db = app.db.lock().unwrap();
        let (_events, total) = db.query_events(&EventFilter::default(), 10, 0).unwrap();
        assert_eq!(total, 0, "白名单读取方不应产生事件");
        drop(db);
        drop(app);
        let _ = std::fs::remove_dir_all(dir);
    }
}
