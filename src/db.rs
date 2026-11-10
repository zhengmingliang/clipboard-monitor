//! SQLite 日志。单连接由外层 Mutex 保护，WAL 模式，库文件权限 0600。

use crate::classify::Captured;
use crate::config::{restrict_dir, restrict_file};
use crate::human_bytes;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS contents (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    sha256 TEXT,
    content_type TEXT NOT NULL,
    size_bytes INTEGER NOT NULL DEFAULT 0,
    truncated INTEGER NOT NULL DEFAULT 0,
    body TEXT,
    blob BLOB,
    meta_json TEXT NOT NULL DEFAULT '{}',
    created_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_ms INTEGER NOT NULL,
    op TEXT NOT NULL,
    selection TEXT NOT NULL,
    pid INTEGER,
    app_name TEXT,
    wm_class TEXT,
    exec_path TEXT,
    window_id INTEGER,
    content_id INTEGER,
    content_type TEXT,
    content_size INTEGER,
    summary TEXT,
    targets TEXT,
    source_app TEXT,
    source_pid INTEGER,
    source_event_id INTEGER,
    truncated INTEGER NOT NULL DEFAULT 0,
    x_time INTEGER,
    reason TEXT,
    created_ms INTEGER NOT NULL,
    pending INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_events_ts ON events(ts_ms);
CREATE INDEX IF NOT EXISTS idx_events_app ON events(app_name);
CREATE INDEX IF NOT EXISTS idx_events_type ON events(content_type);
CREATE INDEX IF NOT EXISTS idx_events_op ON events(op);
CREATE INDEX IF NOT EXISTS idx_events_content ON events(content_id);
CREATE INDEX IF NOT EXISTS idx_events_source ON events(source_event_id);
CREATE INDEX IF NOT EXISTS idx_contents_created ON contents(created_ms);
"#;

#[derive(Debug, Clone)]
pub struct NewEvent {
    pub ts_ms: i64,
    pub op: String,
    pub selection: String,
    pub pid: Option<i32>,
    pub app_name: String,
    pub wm_class: String,
    pub exec_path: String,
    pub window_id: u32,
    pub content_id: Option<i64>,
    pub content_type: String,
    pub content_size: u64,
    pub summary: String,
    pub targets: String,
    pub source_app: String,
    pub source_pid: Option<i32>,
    pub source_event_id: Option<i64>,
    pub truncated: bool,
    pub x_time: u32,
    pub reason: String,
    /// 占位摘要等待抓取回填。回填后清零，不再依赖 summary 文本匹配。
    pub pending: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventRow {
    pub id: i64,
    pub ts_ms: i64,
    pub op: String,
    pub selection: String,
    pub pid: Option<i64>,
    pub app_name: String,
    pub wm_class: String,
    pub exec_path: String,
    pub window_id: Option<i64>,
    pub content_id: Option<i64>,
    pub content_type: String,
    pub content_size: i64,
    pub summary: String,
    pub targets: String,
    pub source_app: String,
    pub source_pid: Option<i64>,
    pub source_event_id: Option<i64>,
    pub truncated: bool,
    pub reason: String,
}

/// 流转链上的一跳。写入在前，读取按时间排在后面。
#[derive(Debug, Clone, Serialize)]
pub struct FlowHop {
    pub id: i64,
    pub ts_ms: i64,
    pub op: String,
    pub app_name: String,
    pub pid: Option<i64>,
    pub exec_path: String,
    pub wm_class: String,
    pub selection: String,
    pub summary: String,
}

/// 一次写入，以及之后读走这份内容的进程。
#[derive(Debug, Clone, Serialize)]
pub struct EventFlow {
    pub write: Option<FlowHop>,
    pub reads: Vec<FlowHop>,
    /// 读取超过上限时只返回前面一部分。
    pub reads_limited: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventDetail {
    #[serde(flatten)]
    pub event: EventRow,
    pub body: Option<String>,
    pub meta_json: Option<String>,
    pub sha256: Option<String>,
    pub has_blob: bool,
    pub flow: EventFlow,
}

#[derive(Debug, Clone, Serialize)]
pub struct DbStats {
    pub events: i64,
    pub contents: i64,
    pub reads_24h: i64,
    pub writes_24h: i64,
    pub oldest_ms: Option<i64>,
    pub newest_ms: Option<i64>,
    pub db_bytes: u64,
    pub db_human: String,
}

#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    pub app: Option<String>,
    pub content_type: Option<String>,
    pub op: Option<String>,
    pub selection: Option<String>,
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    pub query: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CleanupSpec {
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    pub content_type: Option<String>,
    pub op: Option<String>,
    pub dry_run: bool,
}

pub struct Database {
    conn: Connection,
    path: PathBuf,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
            restrict_dir(dir);
        }
        let conn =
            Connection::open(path).with_context(|| format!("打开数据库 {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        // auto_vacuum 只在建库时生效。新文件上，这条语句先于任何表。
        let _ = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0));
        let _ = conn.query_row("PRAGMA auto_vacuum=INCREMENTAL", [], |r| r.get::<_, i64>(0));
        conn.execute_batch(
            "PRAGMA synchronous=NORMAL;
             PRAGMA temp_store=MEMORY;
             PRAGMA cache_size=-2000;
             PRAGMA mmap_size=0;
             PRAGMA wal_autocheckpoint=1000;",
        )?;
        conn.execute_batch(SCHEMA)?;
        Self::migrate(&conn)?;
        restrict_db(path);
        Ok(Self {
            conn,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 仅供测试注入失败场景（如 RAISE(ABORT) 触发器）。
    #[cfg(test)]
    pub(crate) fn exec_for_test(&self, sql: &str) -> Result<()> {
        self.conn.execute_batch(sql)?;
        Ok(())
    }

    pub fn file_size(&self) -> u64 {
        db_file_size(&self.path)
    }

    /// 旧库补 pending 列。历史占位行用摘要文本近似回填一次。
    fn migrate(conn: &Connection) -> Result<()> {
        let has_pending: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('events') WHERE name = 'pending'",
            [],
            |r| r.get(0),
        )?;
        if has_pending == 0 {
            conn.execute_batch(
                "ALTER TABLE events ADD COLUMN pending INTEGER NOT NULL DEFAULT 0;
                 UPDATE events SET pending = 1
                 WHERE op = 'read' AND summary LIKE '%内容获取中%';",
            )?;
        }
        Ok(())
    }

    pub fn checkpoint(&self) {
        let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        restrict_db(&self.path);
    }

    pub fn insert_content(&self, cap: &Captured, ts_ms: i64) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO contents (sha256, content_type, size_bytes, truncated, body, blob, meta_json, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                cap.sha256,
                cap.content_type,
                cap.size as i64,
                cap.truncated as i32,
                cap.body,
                cap.blob,
                cap.meta_json,
                ts_ms,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn update_content(&self, id: i64, cap: &Captured) -> Result<()> {
        self.conn.execute(
            "UPDATE contents
             SET sha256=?1, content_type=?2, size_bytes=?3, truncated=?4, body=?5, blob=?6, meta_json=?7
             WHERE id=?8",
            params![
                cap.sha256,
                cap.content_type,
                cap.size as i64,
                cap.truncated as i32,
                cap.body,
                cap.blob,
                cap.meta_json,
                id,
            ],
        )?;
        Ok(())
    }

    pub fn insert_event(&self, ev: &NewEvent) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO events (
                ts_ms, op, selection, pid, app_name, wm_class, exec_path, window_id,
                content_id, content_type, content_size, summary, targets,
                source_app, source_pid, source_event_id, truncated, x_time, reason, created_ms,
                pending
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                ?9, ?10, ?11, ?12, ?13,
                ?14, ?15, ?16, ?17, ?18, ?19, ?20,
                ?21
             )",
            params![
                ev.ts_ms,
                ev.op,
                ev.selection,
                ev.pid,
                ev.app_name,
                ev.wm_class,
                ev.exec_path,
                ev.window_id as i64,
                ev.content_id,
                ev.content_type,
                ev.content_size as i64,
                ev.summary,
                ev.targets,
                ev.source_app,
                ev.source_pid,
                ev.source_event_id,
                ev.truncated as i32,
                ev.x_time as i64,
                ev.reason,
                ev.ts_ms,
                ev.pending as i32,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_read_burst(
        &self,
        id: i64,
        targets: &str,
        summary: &str,
        content_type: &str,
        content_size: u64,
        content_id: Option<i64>,
        source_app: &str,
        source_pid: Option<i32>,
        source_event_id: Option<i64>,
        truncated: bool,
        pending: bool,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE events
             SET targets=?1, summary=?2, content_type=?3, content_size=?4, content_id=?5,
                 source_app=?6, source_pid=?7, source_event_id=?8, truncated=?9, pending=?10
             WHERE id=?11",
            params![
                targets,
                summary,
                content_type,
                content_size as i64,
                content_id,
                source_app,
                source_pid,
                source_event_id,
                truncated as i32,
                pending as i32,
                id,
            ],
        )?;
        Ok(())
    }

    pub fn apply_capture_to_events(&self, content_id: i64, cap: &Captured) -> Result<()> {
        self.conn.execute(
            "UPDATE events
             SET content_type=?1, content_size=?2, summary=?3, truncated=?4, pending=0,
                 targets=CASE
                     WHEN op='write' AND ?5 != '' THEN ?5 ELSE targets END
             WHERE content_id=?6
               AND (op='write' OR pending=1)",
            params![
                cap.content_type,
                cap.size as i64,
                cap.summary,
                cap.truncated as i32,
                cap.targets.join(", "),
                content_id,
            ],
        )?;
        Ok(())
    }

    pub fn query_events(
        &self,
        filter: &EventFilter,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<EventRow>, i64)> {
        let (where_sql, args) = filter_clause(filter);
        let count_sql = format!(
            "SELECT COUNT(*) FROM events e LEFT JOIN contents c ON c.id = e.content_id {where_sql}"
        );
        let total: i64 =
            self.conn
                .query_row(&count_sql, rusqlite::params_from_iter(args.iter()), |r| {
                    r.get(0)
                })?;
        let mut args = args;
        args.push(limit.to_string());
        args.push(offset.to_string());
        let sql = format!(
            "SELECT {EVENT_COLS}
             FROM events e
             LEFT JOIN contents c ON c.id = e.content_id
             {where_sql}
             ORDER BY e.ts_ms DESC, e.id DESC
             LIMIT ?{limit_idx} OFFSET ?{offset_idx}",
            limit_idx = args.len() - 1,
            offset_idx = args.len(),
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), map_event)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok((out, total))
    }

    pub fn get_event(&self, id: i64) -> Result<Option<EventDetail>> {
        let sql = format!(
            "SELECT {EVENT_COLS}, c.body, c.meta_json, c.sha256, CASE WHEN c.blob IS NULL THEN 0 ELSE 1 END
             FROM events e
             LEFT JOIN contents c ON c.id = e.content_id
             WHERE e.id = ?1"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut row = stmt
            .query_row(params![id], |r| {
                let event = map_event(r)?;
                let body: Option<String> = r.get(19)?;
                let meta_json: Option<String> = r.get(20)?;
                let sha256: Option<String> = r.get(21)?;
                let has_blob: i64 = r.get(22)?;
                Ok(EventDetail {
                    event,
                    body,
                    meta_json,
                    sha256,
                    has_blob: has_blob != 0,
                    flow: EventFlow {
                        write: None,
                        reads: Vec::new(),
                        reads_limited: false,
                    },
                })
            })
            .optional()?;
        drop(stmt);
        if let Some(detail) = row.as_mut() {
            detail.flow = self.event_flow(&detail.event)?;
        }
        Ok(row)
    }

    /// 从任意一条记录找出它所属的写入，以及读过这份内容的进程。
    ///
    /// 读取优先用 `source_event_id`。没有这条外键时，退回同一份 `content_id` 上最早的写入。
    pub fn event_flow(&self, event: &EventRow) -> Result<EventFlow> {
        const READ_LIMIT: i64 = 200;
        let (write_id, content_id) = self.resolve_write(event)?;
        let write = match write_id {
            Some(id) if id == event.id && event.op == "write" => Some(hop_from_event(event)),
            Some(id) => self.hop_by_id(id)?,
            None => None,
        };
        let content_id = content_id.or(event.content_id);
        if write_id.is_none() && content_id.is_none() {
            return Ok(EventFlow {
                write,
                reads: Vec::new(),
                reads_limited: false,
            });
        }
        let mut conds = Vec::new();
        let mut args: Vec<i64> = Vec::new();
        if let Some(id) = write_id {
            args.push(id);
            conds.push(format!("e.source_event_id = ?{}", args.len()));
        }
        if let Some(cid) = content_id {
            args.push(cid);
            conds.push(format!(
                "(e.content_id = ?{} AND e.content_id IS NOT NULL)",
                args.len()
            ));
        }
        args.push(READ_LIMIT + 1);
        let limit_idx = args.len();
        let sql = format!(
            "SELECT {EVENT_COLS}
             FROM events e
             WHERE e.op = 'read' AND ({})
             ORDER BY e.ts_ms ASC, e.id ASC
             LIMIT ?{limit_idx}",
            conds.join(" OR ")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), map_event)?;
        let mut reads = Vec::new();
        for row in rows {
            reads.push(hop_from_event(&row?));
        }
        let reads_limited = reads.len() as i64 > READ_LIMIT;
        if reads_limited {
            reads.truncate(READ_LIMIT as usize);
        }
        Ok(EventFlow {
            write,
            reads,
            reads_limited,
        })
    }

    fn resolve_write(&self, event: &EventRow) -> Result<(Option<i64>, Option<i64>)> {
        if event.op == "write" {
            return Ok((Some(event.id), event.content_id));
        }
        if let Some(id) = event.source_event_id {
            let cid: Option<i64> = self
                .conn
                .query_row(
                    "SELECT content_id FROM events WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            return Ok((Some(id), cid.or(event.content_id)));
        }
        if let Some(cid) = event.content_id {
            let id: Option<i64> = self
                .conn
                .query_row(
                    "SELECT id FROM events
                     WHERE op = 'write' AND content_id = ?1
                     ORDER BY ts_ms ASC, id ASC
                     LIMIT 1",
                    params![cid],
                    |r| r.get(0),
                )
                .optional()?;
            return Ok((id, Some(cid)));
        }
        Ok((None, None))
    }

    fn hop_by_id(&self, id: i64) -> Result<Option<FlowHop>> {
        let sql = format!("SELECT {EVENT_COLS} FROM events e WHERE e.id = ?1");
        let mut stmt = self.conn.prepare(&sql)?;
        let row = stmt
            .query_row(params![id], map_event)
            .optional()?
            .map(|event| hop_from_event(&event));
        Ok(row)
    }

    pub fn get_blob_for_event(&self, event_id: i64) -> Result<Option<(String, Vec<u8>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.blob, COALESCE(e.app_name, 'file'), COALESCE(c.meta_json, '')
             FROM events e JOIN contents c ON c.id = e.content_id
             WHERE e.id = ?1 AND c.blob IS NOT NULL",
        )?;
        let row = stmt
            .query_row(params![event_id], |r| {
                let blob: Vec<u8> = r.get(0)?;
                let app_name: String = r.get(1)?;
                let meta_json: String = r.get(2)?;
                Ok((download_filename(&meta_json, &app_name), blob))
            })
            .optional()?;
        Ok(row)
    }

    pub fn distinct_apps(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT app_name FROM events
             WHERE app_name IS NOT NULL AND app_name != ''
             ORDER BY app_name COLLATE NOCASE
             LIMIT 500",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn stats(&self) -> Result<DbStats> {
        let events: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?;
        let contents: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM contents", [], |r| r.get(0))?;
        let day_ago = crate::now_ms() - 86_400_000;
        let reads_24h: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM events WHERE op='read' AND ts_ms >= ?1",
            params![day_ago],
            |r| r.get(0),
        )?;
        let writes_24h: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM events WHERE op='write' AND ts_ms >= ?1",
            params![day_ago],
            |r| r.get(0),
        )?;
        let oldest_ms: Option<i64> =
            self.conn
                .query_row("SELECT MIN(ts_ms) FROM events", [], |r| r.get(0))?;
        let newest_ms: Option<i64> =
            self.conn
                .query_row("SELECT MAX(ts_ms) FROM events", [], |r| r.get(0))?;
        let db_bytes = self.file_size();
        Ok(DbStats {
            events,
            contents,
            reads_24h,
            writes_24h,
            oldest_ms,
            newest_ms,
            db_bytes,
            db_human: human_bytes(db_bytes),
        })
    }

    pub fn cleanup(&self, spec: &CleanupSpec) -> Result<u64> {
        let mut filter = EventFilter {
            content_type: spec.content_type.clone(),
            op: spec.op.clone(),
            from_ms: spec.from_ms,
            to_ms: spec.to_ms,
            ..EventFilter::default()
        };
        if filter.from_ms.is_none()
            && filter.to_ms.is_none()
            && filter.content_type.as_deref().unwrap_or("").is_empty()
            && filter.op.as_deref().unwrap_or("").is_empty()
        {
            anyhow::bail!("清理条件为空，拒绝删除全部记录");
        }
        // 空字符串当作未设置。
        if filter.content_type.as_deref() == Some("") {
            filter.content_type = None;
        }
        if filter.op.as_deref() == Some("") {
            filter.op = None;
        }
        let (where_sql, args) = filter_clause(&filter);
        let count_sql = format!(
            "SELECT COUNT(*) FROM events e LEFT JOIN contents c ON c.id = e.content_id {where_sql}"
        );
        let matched: i64 =
            self.conn
                .query_row(&count_sql, rusqlite::params_from_iter(args.iter()), |r| {
                    r.get(0)
                })?;
        if spec.dry_run {
            return Ok(matched as u64);
        }
        let delete_sql = format!(
            "DELETE FROM events WHERE id IN (SELECT e.id FROM events e LEFT JOIN contents c ON c.id = e.content_id {where_sql})"
        );
        self.conn
            .execute(&delete_sql, rusqlite::params_from_iter(args.iter()))?;
        self.delete_orphans(&[])?;
        self.reclaim()?;
        Ok(matched as u64)
    }

    pub fn enforce_limits(
        &self,
        retention_days: u32,
        max_bytes: u64,
        keep_contents: &[i64],
    ) -> Result<(u64, u64)> {
        let cutoff = crate::now_ms() - i64::from(retention_days) * 86_400_000;
        let expired = self
            .conn
            .execute("DELETE FROM events WHERE ts_ms < ?1", params![cutoff])?
            as u64;
        self.delete_orphans(keep_contents)?;
        let mut trimmed = 0u64;
        for _ in 0..40 {
            if self.file_size() <= max_bytes {
                break;
            }
            let n = self.conn.execute(
                "DELETE FROM events WHERE id IN (SELECT id FROM events ORDER BY ts_ms ASC, id ASC LIMIT 500)",
                [],
            )?;
            if n == 0 {
                break;
            }
            trimmed += n as u64;
            self.delete_orphans(keep_contents)?;
            self.reclaim()?;
        }
        if expired > 0 || trimmed > 0 {
            self.reclaim()?;
            self.checkpoint();
        }
        Ok((expired, trimmed))
    }

    fn delete_orphans(&self, keep: &[i64]) -> Result<()> {
        if keep.is_empty() {
            self.conn.execute(
                "DELETE FROM contents
                 WHERE id NOT IN (SELECT content_id FROM events WHERE content_id IS NOT NULL)",
                [],
            )?;
            return Ok(());
        }
        let mut sql = String::from(
            "DELETE FROM contents
             WHERE id NOT IN (SELECT content_id FROM events WHERE content_id IS NOT NULL)
               AND id NOT IN (",
        );
        for i in 0..keep.len() {
            if i > 0 {
                sql.push(',');
            }
            sql.push('?');
            sql.push_str(&(i + 1).to_string());
        }
        sql.push(')');
        self.conn
            .execute(&sql, rusqlite::params_from_iter(keep.iter()))?;
        Ok(())
    }

    fn reclaim(&self) -> Result<()> {
        let _ = self.conn.execute_batch("PRAGMA incremental_vacuum(200);");
        Ok(())
    }
}

const EVENT_COLS: &str =
    "e.id, e.ts_ms, e.op, e.selection, e.pid, e.app_name, e.wm_class, e.exec_path,
    e.window_id, e.content_id, e.content_type, e.content_size, e.summary, e.targets,
    e.source_app, e.source_pid, e.source_event_id, e.truncated, e.reason";

fn hop_from_event(event: &EventRow) -> FlowHop {
    FlowHop {
        id: event.id,
        ts_ms: event.ts_ms,
        op: event.op.clone(),
        app_name: event.app_name.clone(),
        pid: event.pid,
        exec_path: event.exec_path.clone(),
        wm_class: event.wm_class.clone(),
        selection: event.selection.clone(),
        summary: event.summary.clone(),
    }
}

fn map_event(r: &rusqlite::Row<'_>) -> rusqlite::Result<EventRow> {
    Ok(EventRow {
        id: r.get(0)?,
        ts_ms: r.get(1)?,
        op: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
        selection: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
        pid: r.get(4)?,
        app_name: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
        wm_class: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
        exec_path: r.get::<_, Option<String>>(7)?.unwrap_or_default(),
        window_id: r.get(8)?,
        content_id: r.get(9)?,
        content_type: r.get::<_, Option<String>>(10)?.unwrap_or_default(),
        content_size: r.get::<_, Option<i64>>(11)?.unwrap_or(0),
        summary: r.get::<_, Option<String>>(12)?.unwrap_or_default(),
        targets: r.get::<_, Option<String>>(13)?.unwrap_or_default(),
        source_app: r.get::<_, Option<String>>(14)?.unwrap_or_default(),
        source_pid: r.get(15)?,
        source_event_id: r.get(16)?,
        truncated: r.get::<_, Option<i64>>(17)?.unwrap_or(0) != 0,
        reason: r.get::<_, Option<String>>(18)?.unwrap_or_default(),
    })
}

fn filter_clause(filter: &EventFilter) -> (String, Vec<String>) {
    let mut wheres = Vec::new();
    let mut args = Vec::new();
    if let Some(app) = nonempty(&filter.app) {
        args.push(format!("%{}%", escape_like(&app.to_lowercase())));
        let idx = args.len();
        wheres.push(format!(
            "(LOWER(COALESCE(e.app_name,'')) LIKE ?{idx} ESCAPE '\\' OR LOWER(COALESCE(e.source_app,'')) LIKE ?{idx} ESCAPE '\\')"
        ));
    }
    if let Some(t) = nonempty(&filter.content_type) {
        args.push(t);
        wheres.push(format!("e.content_type = ?{}", args.len()));
    }
    if let Some(op) = nonempty(&filter.op) {
        args.push(op);
        wheres.push(format!("e.op = ?{}", args.len()));
    }
    if let Some(sel) = nonempty(&filter.selection) {
        args.push(sel);
        wheres.push(format!("e.selection = ?{}", args.len()));
    }
    if let Some(from) = filter.from_ms {
        args.push(from.to_string());
        wheres.push(format!("e.ts_ms >= ?{}", args.len()));
    }
    if let Some(to) = filter.to_ms {
        args.push(to.to_string());
        wheres.push(format!("e.ts_ms <= ?{}", args.len()));
    }
    if let Some(q) = nonempty(&filter.query) {
        args.push(format!("%{}%", escape_like(&q.to_lowercase())));
        let idx = args.len();
        wheres.push(format!(
            "(LOWER(COALESCE(e.summary,'')) LIKE ?{idx} ESCAPE '\\' OR LOWER(COALESCE(c.body,'')) LIKE ?{idx} ESCAPE '\\' OR LOWER(COALESCE(e.app_name,'')) LIKE ?{idx} ESCAPE '\\' OR LOWER(COALESCE(e.targets,'')) LIKE ?{idx} ESCAPE '\\')"
        ));
    }
    if wheres.is_empty() {
        ("WHERE 1=1".to_string(), args)
    } else {
        (format!("WHERE {}", wheres.join(" AND ")), args)
    }
}

fn nonempty(v: &Option<String>) -> Option<String> {
    v.as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

pub fn db_file_size(path: &Path) -> u64 {
    let mut total = 0u64;
    for suffix in ["", "-wal", "-shm"] {
        let p = if suffix.is_empty() {
            path.to_path_buf()
        } else {
            PathBuf::from(format!("{}{suffix}", path.display()))
        };
        if let Ok(meta) = std::fs::metadata(&p) {
            total += meta.len();
        }
    }
    total
}

fn restrict_db(path: &Path) {
    restrict_file(path);
    for suffix in ["-wal", "-shm"] {
        let p = PathBuf::from(format!("{}{suffix}", path.display()));
        if p.exists() {
            restrict_file(&p);
        }
    }
}

/// 从事件元数据推断下载文件名：优先文件列表第一个条目名，
/// 其次条目路径的文件名部分，再退回应用名，最后 "file"。
fn download_filename(meta_json: &str, app_name: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(meta_json) {
        let entry = &v["files"]["entries"][0];
        if let Some(name) = entry["name"].as_str() {
            let name = name.trim();
            if !name.is_empty() {
                return name.to_string();
            }
        }
        if let Some(path) = entry["path"].as_str() {
            if let Some(base) = std::path::Path::new(path).file_name() {
                let base = base.to_string_lossy().into_owned();
                if !base.is_empty() {
                    return base;
                }
            }
        }
    }
    let app = app_name.trim();
    if !app.is_empty() && app != "file" {
        return app.to_string();
    }
    "file".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{assemble, empty_capture, FetchInput, Limits, TYPE_TEXT};

    fn tmp_db() -> (Database, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "clipmon-db-{}-{}-{}.sqlite",
            std::process::id(),
            crate::now_ms(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let db = Database::open(&path).unwrap();
        (db, path)
    }

    fn text_event(db: &Database, ts: i64, summary: &str, kind: &str) -> i64 {
        let cap = if kind == "empty" {
            empty_capture()
        } else {
            assemble(
                &FetchInput {
                    target_names: vec!["UTF8_STRING".into()],
                    plain: Some(summary.as_bytes().to_vec()),
                    plain_total: summary.len() as u64,
                    plain_mime: Some("UTF8_STRING".into()),
                    ..FetchInput::default()
                },
                &Limits {
                    text_full_limit: 1024 * 1024,
                    text_preview: 100,
                    file_content_limit: 100,
                    record_file_content: false,
                },
            )
        };
        let cid = db.insert_content(&cap, ts).unwrap();
        db.insert_event(&NewEvent {
            ts_ms: ts,
            op: "write".into(),
            selection: "CLIPBOARD".into(),
            pid: Some(1),
            app_name: "gedit".into(),
            wm_class: "Gedit".into(),
            exec_path: "/usr/bin/gedit".into(),
            window_id: 1,
            content_id: Some(cid),
            content_type: cap.content_type,
            content_size: cap.size,
            summary: cap.summary.clone(),
            targets: "UTF8_STRING".into(),
            source_app: "gedit".into(),
            source_pid: Some(1),
            source_event_id: None,
            truncated: cap.truncated,
            x_time: 0,
            reason: "owner_change".into(),
            pending: false,
        })
        .unwrap()
    }

    #[test]
    fn filters_and_manual_cleanup() {
        let (db, path) = tmp_db();
        let id = text_event(&db, 1_700_000_000_000, "alpha-token", TYPE_TEXT);
        text_event(&db, 1_700_000_000_500, "beta", TYPE_TEXT);
        let (rows, total) = db
            .query_events(
                &EventFilter {
                    query: Some("alpha-token".into()),
                    ..EventFilter::default()
                },
                50,
                0,
            )
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].id, id);
        let detail = db.get_event(id).unwrap().unwrap();
        assert_eq!(detail.body.as_deref(), Some("alpha-token"));

        let removed = db
            .cleanup(&CleanupSpec {
                content_type: Some(TYPE_TEXT.into()),
                from_ms: Some(0),
                to_ms: Some(1_700_000_000_200),
                dry_run: false,
                ..CleanupSpec::default()
            })
            .unwrap();
        assert_eq!(removed, 1);
        let stats = db.stats().unwrap();
        assert_eq!(stats.events, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn retention_drops_old_rows() {
        let (db, path) = tmp_db();
        text_event(&db, crate::now_ms() - 10 * 86_400_000, "old", TYPE_TEXT);
        text_event(&db, crate::now_ms(), "new", TYPE_TEXT);
        let (expired, _) = db.enforce_limits(7, u64::MAX, &[]).unwrap();
        assert!(expired >= 1);
        let (rows, total) = db.query_events(&EventFilter::default(), 10, 0).unwrap();
        assert_eq!(total, 1);
        assert!(rows[0].summary.contains("new"));
        let _ = std::fs::remove_file(&path);
    }

    fn read_event(
        db: &Database,
        ts: i64,
        app: &str,
        pid: i32,
        content_id: i64,
        source_event_id: Option<i64>,
    ) -> i64 {
        db.insert_event(&NewEvent {
            ts_ms: ts,
            op: "read".into(),
            selection: "CLIPBOARD".into(),
            pid: Some(pid),
            app_name: app.into(),
            wm_class: String::new(),
            exec_path: format!("/usr/bin/{app}"),
            window_id: 2,
            content_id: Some(content_id),
            content_type: TYPE_TEXT.into(),
            content_size: 4,
            summary: format!("来自写入：{app}"),
            targets: "UTF8_STRING".into(),
            source_app: "gedit".into(),
            source_pid: Some(1),
            source_event_id,
            truncated: false,
            x_time: 0,
            reason: "read".into(),
            pending: false,
        })
        .unwrap()
    }

    #[test]
    fn flow_lists_readers_of_a_write() {
        let (db, path) = tmp_db();
        let write_id = text_event(&db, 1_700_000_000_000, "shared-text", TYPE_TEXT);
        let write = db.get_event(write_id).unwrap().unwrap();
        let cid = write.event.content_id.unwrap();
        let other = text_event(&db, 1_700_000_000_100, "other-text", TYPE_TEXT);
        let other_cid = db
            .get_event(other)
            .unwrap()
            .unwrap()
            .event
            .content_id
            .unwrap();

        let firefox = read_event(&db, 1_700_000_000_200, "firefox", 20, cid, Some(write_id));
        let wechat = read_event(&db, 1_700_000_000_300, "wechat", 30, cid, None);
        let _unrelated = read_event(&db, 1_700_000_000_400, "gedit", 40, other_cid, Some(other));

        let from_write = db.get_event(write_id).unwrap().unwrap().flow;
        assert_eq!(from_write.write.as_ref().map(|h| h.id), Some(write_id));
        assert_eq!(
            from_write.reads.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![firefox, wechat]
        );
        assert_eq!(from_write.reads[0].app_name, "firefox");
        assert_eq!(from_write.reads[1].pid, Some(30));

        let from_read = db.get_event(wechat).unwrap().unwrap().flow;
        assert_eq!(
            from_read.write.as_ref().map(|h| h.app_name.as_str()),
            Some("gedit")
        );
        assert_eq!(from_read.reads.len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_unfiltered_cleanup() {
        let (db, path) = tmp_db();
        let err = db.cleanup(&CleanupSpec::default()).unwrap_err();
        assert!(err.to_string().contains("拒绝"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn apply_capture_updates_only_pending_reads() {
        let (db, path) = tmp_db();
        let placeholder = Captured::placeholder("内容获取中");
        let cid = db.insert_content(&placeholder, 1_700_000_000_000).unwrap();
        let mk = |op: &str, summary: &str, pending: bool| NewEvent {
            ts_ms: 1_700_000_000_000,
            op: op.into(),
            selection: "CLIPBOARD".into(),
            pid: Some(1),
            app_name: "gedit".into(),
            wm_class: String::new(),
            exec_path: "/usr/bin/gedit".into(),
            window_id: 1,
            content_id: Some(cid),
            content_type: placeholder.content_type.clone(),
            content_size: 0,
            summary: summary.into(),
            targets: String::new(),
            source_app: "gedit".into(),
            source_pid: Some(1),
            source_event_id: None,
            truncated: false,
            x_time: 0,
            reason: "test".into(),
            pending,
        };
        db.insert_event(&mk("write", "内容获取中", false)).unwrap();
        let pending_read = db
            .insert_event(&mk("read", "来自 gedit：内容获取中", true))
            .unwrap();
        let normal_read = db
            .insert_event(&mk("read", "来自 firefox：已完成内容获取中转", false))
            .unwrap();

        let final_cap = assemble(
            &FetchInput {
                target_names: vec!["UTF8_STRING".into()],
                plain: Some("内容获取中".as_bytes().to_vec()),
                plain_total: 15,
                plain_mime: Some("UTF8_STRING".into()),
                ..FetchInput::default()
            },
            &Limits::default(),
        );
        db.update_content(cid, &final_cap).unwrap();
        db.apply_capture_to_events(cid, &final_cap).unwrap();

        let backfilled = db.get_event(pending_read).unwrap().unwrap();
        assert_eq!(
            backfilled.event.summary, final_cap.summary,
            "pending 读应回填"
        );
        assert_eq!(backfilled.body.as_deref(), Some("内容获取中"));
        let untouched = db.get_event(normal_read).unwrap().unwrap();
        assert_eq!(
            untouched.event.summary, "来自 firefox：已完成内容获取中转",
            "正文恰好包含占位词的读取不应被改写"
        );

        // 回填后 pending 清零：再次回填不再改写摘要。
        let cap2 = assemble(
            &FetchInput {
                target_names: vec!["UTF8_STRING".into()],
                plain: Some(b"second".to_vec()),
                plain_total: 6,
                plain_mime: Some("UTF8_STRING".into()),
                ..FetchInput::default()
            },
            &Limits::default(),
        );
        db.update_content(cid, &cap2).unwrap();
        db.apply_capture_to_events(cid, &cap2).unwrap();
        let still = db.get_event(pending_read).unwrap().unwrap();
        assert_eq!(still.event.summary, final_cap.summary, "pending 已清零");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn legacy_db_is_migrated_with_pending_column() {
        let path = std::env::temp_dir().join(format!(
            "clipmon-legacy-{}-{}.sqlite",
            std::process::id(),
            crate::now_ms()
        ));
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    ts_ms INTEGER NOT NULL, op TEXT NOT NULL, selection TEXT NOT NULL,
                    pid INTEGER, app_name TEXT, wm_class TEXT, exec_path TEXT,
                    window_id INTEGER, content_id INTEGER, content_type TEXT,
                    content_size INTEGER, summary TEXT, targets TEXT, source_app TEXT,
                    source_pid INTEGER, source_event_id INTEGER,
                    truncated INTEGER NOT NULL DEFAULT 0, x_time INTEGER, reason TEXT,
                    created_ms INTEGER NOT NULL
                );
                INSERT INTO events (ts_ms, op, selection, created_ms, summary)
                    VALUES (1, 'read', 'CLIPBOARD', 1, '来自 gedit：内容获取中');
                INSERT INTO events (ts_ms, op, selection, created_ms, summary)
                    VALUES (2, 'read', 'CLIPBOARD', 2, '正常读取');",
            )
            .unwrap();
        }
        let db = Database::open(&path).unwrap();
        drop(db);
        let conn = rusqlite::Connection::open(&path).unwrap();
        let pending_sum: i64 = conn
            .query_row("SELECT COUNT(*) FROM events WHERE pending = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(pending_sum, 1, "历史占位读取应被回填为 pending");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn download_filename_prefers_file_entry() {
        assert_eq!(
            download_filename(
                r#"{"files":{"entries":[{"name":"a b.txt","path":"/tmp/a b.txt"}]}}"#,
                "gedit"
            ),
            "a b.txt"
        );
        assert_eq!(
            download_filename(
                r#"{"files":{"entries":[{"path":"/tmp/x/y.zip"}]}}"#,
                "gedit"
            ),
            "y.zip"
        );
        assert_eq!(download_filename("{}", "gedit"), "gedit");
        assert_eq!(download_filename("not json", ""), "file");
    }
}
