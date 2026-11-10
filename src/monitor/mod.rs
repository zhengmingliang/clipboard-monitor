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

#[cfg(target_os = "linux")]
mod x11;
#[cfg(target_os = "linux")]
use x11 as backend;

#[cfg(any(target_os = "windows", target_os = "macos", test))]
mod portable;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use portable as backend;

#[cfg(target_os = "linux")]
pub const BACKEND: &str = "x11";
#[cfg(target_os = "windows")]
pub const BACKEND: &str = "windows";
#[cfg(target_os = "macos")]
pub const BACKEND: &str = "macos";
pub const SUPPORTS_READS: bool = cfg!(target_os = "linux");
pub const SUPPORTS_PRIMARY: bool = cfg!(target_os = "linux");

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

    /// 非 X11 后端已经取得完整内容，直接写入一条变化记录。
    #[cfg(any(target_os = "windows", target_os = "macos", test))]
    pub(crate) fn record_change(&self, captured: &Captured, ts_ms: i64) -> Result<i64> {
        let (content_id, event_id) = {
            let db = self.db.lock().unwrap_or_else(|e| e.into_inner());
            let content_id = db.insert_content(captured, ts_ms)?;
            let event_id = db.insert_event(&NewEvent {
                ts_ms,
                op: "write".into(),
                selection: "CLIPBOARD".into(),
                pid: None,
                app_name: String::new(),
                wm_class: String::new(),
                exec_path: String::new(),
                window_id: 0,
                content_id: Some(content_id),
                content_type: captured.content_type.clone(),
                content_size: captured.size,
                summary: captured.summary.clone(),
                targets: captured.targets.join(", "),
                source_app: String::new(),
                source_pid: None,
                source_event_id: None,
                truncated: captured.truncated,
                x_time: 0,
                reason: "changed".into(),
            })?;
            (content_id, event_id)
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
        self.counters.writes.fetch_add(1, Ordering::Relaxed);
        self.counters.last_event_ms.store(ts_ms, Ordering::Relaxed);
        Ok(content_id)
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
    fn native_change_is_stored_with_text_and_current_snapshot() {
        let dir = std::env::temp_dir().join(format!(
            "clipboard-monitor-test-{}-{}",
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
        let content_id = app.record_change(&captured, 1234).expect("record change");
        assert_eq!(app.counters.writes.load(Ordering::Relaxed), 1);
        assert_eq!(app.keep_content_ids(), vec![content_id]);
        let db = app.db.lock().unwrap();
        let (events, total) = db.query_events(&EventFilter::default(), 10, 0).unwrap();
        assert_eq!(total, 1);
        assert_eq!(events[0].content_id, Some(content_id));
        assert_eq!(events[0].content_type, "text");
        let detail = db.get_event(events[0].id).unwrap().unwrap();
        assert_eq!(detail.body.as_deref(), Some("cross-platform text"));
        drop(db);
        drop(app);
        let _ = std::fs::remove_dir_all(dir);
    }
}
