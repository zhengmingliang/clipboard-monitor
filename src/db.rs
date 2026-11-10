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
    /// None = 明文模式（仅测试用）。生产路径总是 Some。
    key: Option<crate::crypto::DbKey>,
}

/// 迁移/重建索引时批量读取的行类型。
type ContentRow = (i64, Option<String>, Option<String>, Option<Vec<u8>>);
type FtsRow = (
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

impl Database {
    /// 明文打开，仅用于单元测试。生产代码必须用 [`Database::open_encrypted`]。
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_inner(path, None)
    }

    /// 生产入口：加载/创建系统钥匙串保护的密钥，加密正文后再打开数据库。
    /// 首次在旧库上启用时会自动把历史明文行重加密（渐进迁移）。
    pub fn open_encrypted(path: &Path) -> Result<Self> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let key = crate::crypto::DbKey::load_or_create(dir).context("加载数据库加密密钥")?;
        Self::open_inner(path, Some(key))
    }

    fn open_inner(path: &Path, key: Option<crate::crypto::DbKey>) -> Result<Self> {
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
        Self::ensure_fts(&conn)?;
        let mut db = Self {
            conn,
            path: path.to_path_buf(),
            key,
        };
        if db.key.is_some() {
            db.migrate_plaintext_to_encrypted()?;
            db.rebuild_fts()?;
        }
        restrict_db(path);
        Ok(db)
    }

    /// 把历史明文 contents / events 行重加密。只处理不带 `enc:v1:` 前缀的
    /// 值，重复运行是幂等的；中途失败靠"前缀判定"下次继续。
    fn migrate_plaintext_to_encrypted(&mut self) -> Result<()> {
        let key = self.key.as_ref().unwrap();
        // contents：body / meta_json / blob
        let mut stmt = self.conn.prepare(
            "SELECT id, body, meta_json, blob FROM contents
             WHERE (body IS NOT NULL AND body NOT LIKE 'enc:v1:%')
                OR (meta_json IS NOT NULL AND meta_json NOT LIKE 'enc:v1:%')
                OR (blob IS NOT NULL AND length(blob) > 0)",
        )?;
        let rows: Vec<ContentRow> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        for (id, body, meta, blob) in rows {
            let enc_body = key.encrypt_text(body.as_deref())?;
            let enc_meta = key
                .encrypt_text(meta.as_deref())?
                .unwrap_or_else(|| "{}".into());
            let enc_blob = blob.as_deref().map(|b| key.encrypt(b)).transpose()?;
            self.conn.execute(
                "UPDATE contents SET body=?1, meta_json=?2, blob=?3 WHERE id=?4",
                params![
                    enc_body,
                    enc_meta,
                    enc_blob.as_deref().map(|s| s.as_bytes().to_vec()),
                    id
                ],
            )?;
        }
        // events.summary
        let mut stmt = self
            .conn
            .prepare("SELECT id, summary FROM events WHERE summary IS NOT NULL AND summary NOT LIKE 'enc:v1:%'")?;
        let rows: Vec<(i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        for (id, summary) in rows {
            let enc = key.encrypt(summary.as_bytes())?;
            self.conn
                .execute("UPDATE events SET summary=?1 WHERE id=?2", params![enc, id])?;
        }
        Ok(())
    }

    /// 加密字段的写入助手。明文模式下原样返回。
    fn enc_text(&self, value: Option<&str>) -> Result<Option<String>> {
        match &self.key {
            Some(key) => key.encrypt_text(value),
            None => Ok(value.map(|s| s.to_string())),
        }
    }

    fn enc_bytes(&self, value: Option<&[u8]>) -> Result<Option<Vec<u8>>> {
        match (&self.key, value) {
            (Some(key), Some(v)) => Ok(Some(key.encrypt(v)?.into_bytes())),
            (Some(_), None) => Ok(None),
            (None, v) => Ok(v.map(|b| b.to_vec())),
        }
    }

    fn dec_text(&self, value: Option<String>) -> Result<Option<String>> {
        match &self.key {
            Some(key) => key.decrypt_text(value.as_deref()),
            None => Ok(value),
        }
    }

    fn dec_bytes(&self, value: Option<Vec<u8>>) -> Result<Option<Vec<u8>>> {
        match (&self.key, value) {
            (Some(key), Some(v)) => {
                let text = String::from_utf8(v).context("加密 blob 不是 UTF-8")?;
                Ok(Some(key.decrypt(&text)?))
            }
            (Some(_), None) | (None, None) => Ok(None),
            (None, Some(v)) => Ok(Some(v)),
        }
    }

    /// 一次 Captured 的四个敏感列，统一加密后返回，供各写入路径复用。
    fn stored_content(&self, cap: &Captured) -> Result<(Option<String>, Option<Vec<u8>>, String)> {
        let body = self.enc_text(cap.body.as_deref())?;
        let blob = self.enc_bytes(cap.blob.as_deref())?;
        let meta = self
            .enc_text(Some(cap.meta_json.as_str()))?
            .unwrap_or_else(|| "{}".into());
        Ok((body, blob, meta))
    }

    fn stored_summary(&self, summary: &str) -> Result<String> {
        Ok(self.enc_text(Some(summary))?.unwrap_or_default())
    }

    /// 解密一个从 SQL 直接映射出来的 EventRow 的 summary。
    fn decrypt_row(&self, mut row: EventRow) -> Result<EventRow> {
        row.summary = self.dec_text(Some(row.summary))?.unwrap_or_default();
        Ok(row)
    }

    /// FTS5 虚拟表 + 同步触发器。索引的是**解密后的**正文，由 Rust 侧在
    /// 写入时显式维护，避免把密文写进索引（那样既搜不到也泄露长度）。
    fn ensure_fts(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS events_fts USING fts5(
                body, summary, app_name, targets,
                content='', contentless_delete=1, tokenize='trigram'
             );",
        )?;
        Ok(())
    }

    /// 重建 FTS 索引（迁移后调用；也供写入路径增量维护失败时兜底）。
    /// 索引按 contents.id 作为 rowid，一条 content 对应一行（正文 + 首个
    /// 关联事件的应用名/格式）。
    fn rebuild_fts(&self) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.body,
                    (SELECT summary FROM events e WHERE e.content_id = c.id ORDER BY e.id LIMIT 1),
                    (SELECT app_name FROM events e WHERE e.content_id = c.id ORDER BY e.id LIMIT 1),
                    (SELECT targets FROM events e WHERE e.content_id = c.id ORDER BY e.id LIMIT 1)
             FROM contents c",
        )?;
        let rows: Vec<FtsRow> = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        self.conn.execute("DELETE FROM events_fts", [])?;
        for (id, body, summary, app, targets) in rows {
            let body = self.fts_text(body)?;
            let summary = self.fts_text(summary)?;
            self.conn.execute(
                "INSERT INTO events_fts(rowid, body, summary, app_name, targets) VALUES (?1,?2,?3,?4,?5)",
                params![id, body, summary, app.unwrap_or_default(), targets.unwrap_or_default()],
            )?;
        }
        Ok(())
    }

    /// 写入/更新 contents 时增量维护 FTS 行。用**解密后的**明文建索引，
    /// 绝不能把密文写进索引（既搜不到也泄露长度信息）。
    fn fts_upsert_content(&self, id: i64, cap: &Captured) -> Result<()> {
        self.conn
            .execute("DELETE FROM events_fts WHERE rowid = ?1", params![id])?;
        self.conn.execute(
            "INSERT INTO events_fts(rowid, body, summary, app_name, targets) VALUES (?1,?2,?3,'','')",
            params![
                id,
                cap.body.as_deref().unwrap_or_default(),
                cap.summary
            ],
        )?;
        Ok(())
    }

    /// 从数据库取出的可能是密文或明文；统一转成可索引文本。
    fn fts_text(&self, raw: Option<String>) -> Result<String> {
        let Some(text) = raw else {
            return Ok(String::new());
        };
        match &self.key {
            Some(key) if text.starts_with("enc:v1:") => {
                Ok(String::from_utf8_lossy(&key.decrypt(&text)?).into_owned())
            }
            _ => Ok(text),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 当前数据库是否启用了字段级加密（供 /api/status 展示）。
    pub fn is_encrypted(&self) -> bool {
        self.key.is_some()
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
        let (body, blob, meta) = self.stored_content(cap)?;
        self.conn.execute(
            "INSERT INTO contents (sha256, content_type, size_bytes, truncated, body, blob, meta_json, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                cap.sha256,
                cap.content_type,
                cap.size as i64,
                cap.truncated as i32,
                body,
                blob,
                meta,
                ts_ms,
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        self.fts_upsert_content(id, cap)?;
        Ok(id)
    }

    pub fn update_content(&self, id: i64, cap: &Captured) -> Result<()> {
        let (body, blob, meta) = self.stored_content(cap)?;
        self.conn.execute(
            "UPDATE contents
             SET sha256=?1, content_type=?2, size_bytes=?3, truncated=?4, body=?5, blob=?6, meta_json=?7
             WHERE id=?8",
            params![
                cap.sha256,
                cap.content_type,
                cap.size as i64,
                cap.truncated as i32,
                body,
                blob,
                meta,
                id,
            ],
        )?;
        self.fts_upsert_content(id, cap)?;
        Ok(())
    }

    pub fn insert_event(&self, ev: &NewEvent) -> Result<i64> {
        let summary = self.stored_summary(&ev.summary)?;
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
                summary,
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

    /// 一条变化记录（contents + events）在同一个事务里落库。
    ///
    /// Windows/macOS 后端在通知到达时同步持有完整内容，不需要 X11 的
    /// "先插占位、再异步回填"。拆成两次独立 execute 时，events 插入失败会
    /// 留下孤儿 contents，且重试会重复插入。事务化后两者同时成功或同时回滚。
    pub fn insert_change(
        &mut self,
        cap: &Captured,
        ev: &NewEvent,
        ts_ms: i64,
    ) -> Result<(i64, i64)> {
        let (body, blob, meta) = self.stored_content(cap)?;
        let summary = self.stored_summary(&ev.summary)?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO contents (sha256, content_type, size_bytes, truncated, body, blob, meta_json, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                cap.sha256,
                cap.content_type,
                cap.size as i64,
                cap.truncated as i32,
                body,
                blob,
                meta,
                ts_ms,
            ],
        )?;
        let content_id = tx.last_insert_rowid();
        tx.execute(
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
                content_id,
                ev.content_type,
                ev.content_size as i64,
                summary,
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
        let event_id = tx.last_insert_rowid();
        tx.commit()?;
        // FTS 增量维护在事务提交后做：索引明文，失败只记警告不影响落库。
        if let Err(err) = self.fts_upsert_content(content_id, cap) {
            crate::log_warn(format!("更新全文索引失败: {err:#}"));
        }
        Ok((content_id, event_id))
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
        let enc_summary = self.stored_summary(summary)?;
        self.conn.execute(
            "UPDATE events
             SET targets=?1, summary=?2, content_type=?3, content_size=?4, content_id=?5,
                 source_app=?6, source_pid=?7, source_event_id=?8, truncated=?9, pending=?10
             WHERE id=?11",
            params![
                targets,
                enc_summary,
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
        let enc_summary = self.stored_summary(&cap.summary)?;
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
                enc_summary,
                cap.truncated as i32,
                cap.targets.join(", "),
                content_id,
            ],
        )?;
        // 占位内容此时才补全，同步刷新 FTS 索引里的正文/摘要。
        if let Err(err) = self.fts_upsert_content(content_id, cap) {
            crate::log_warn(format!("更新全文索引失败: {err:#}"));
        }
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
            out.push(self.decrypt_row(row?)?);
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
            detail.event.summary = self
                .dec_text(Some(detail.event.summary.clone()))?
                .unwrap_or_default();
            detail.body = self.dec_text(detail.body.take())?;
            detail.meta_json = self.dec_text(detail.meta_json.take())?;
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
            reads.push(hop_from_event(&self.decrypt_row(row?)?));
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
        let row = stmt.query_row(params![id], map_event).optional()?;
        Ok(match row {
            Some(event) => Some(hop_from_event(&self.decrypt_row(event)?)),
            None => None,
        })
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
                Ok((blob, app_name, meta_json))
            })
            .optional()?;
        let Some((blob, app_name, meta_json)) = row else {
            return Ok(None);
        };
        let meta_json = self.dec_text(Some(meta_json))?.unwrap_or_default();
        let blob = self.dec_bytes(Some(blob))?.unwrap_or_default();
        Ok(Some((download_filename(&meta_json, &app_name), blob)))
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
        // 全文检索走 FTS5（索引的是解密后的明文）。trigram 分词对 ≥3 字符
        // 的查询（含中文子串）效果好；短查询退回 LIKE 匹配应用名/格式，
        // 那些列本身是明文，不受加密影响。
        if q.chars().count() >= 3 {
            let phrase = format!("\"{}\"", q.replace('"', "\"\""));
            args.push(phrase);
            let idx = args.len();
            wheres.push(format!(
                "(e.content_id IN (SELECT rowid FROM events_fts WHERE events_fts MATCH ?{idx})
                  OR LOWER(COALESCE(e.app_name,'')) LIKE ?{idx2} ESCAPE '\\'
                  OR LOWER(COALESCE(e.targets,'')) LIKE ?{idx2} ESCAPE '\\')",
                idx2 = idx + 1
            ));
            args.push(format!("%{}%", escape_like(&q.to_lowercase())));
        } else {
            args.push(format!("%{}%", escape_like(&q.to_lowercase())));
            let idx = args.len();
            wheres.push(format!(
                "(LOWER(COALESCE(e.app_name,'')) LIKE ?{idx} ESCAPE '\\' OR LOWER(COALESCE(e.targets,'')) LIKE ?{idx} ESCAPE '\\')"
            ));
        }
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

    #[test]
    fn insert_change_writes_content_and_event_atomically() {
        let (mut db, path) = tmp_db();
        let cap = assemble(
            &FetchInput {
                target_names: vec!["text/plain".into(), "text/html".into()],
                plain: Some(b"cross-platform".to_vec()),
                plain_total: 14,
                plain_mime: Some("text/plain".into()),
                ..FetchInput::default()
            },
            &Limits::default(),
        );
        let event = NewEvent {
            ts_ms: 4242,
            op: "write".into(),
            selection: "CLIPBOARD".into(),
            pid: Some(321),
            app_name: "Notepad".into(),
            wm_class: String::new(),
            exec_path: "C:\\Windows\\notepad.exe".into(),
            window_id: 7,
            content_id: None, // 由 insert_change 在事务内回填
            content_type: cap.content_type.clone(),
            content_size: cap.size,
            summary: cap.summary.clone(),
            targets: "text/plain, text/html".into(),
            source_app: "Notepad".into(),
            source_pid: Some(321),
            source_event_id: None,
            truncated: cap.truncated,
            x_time: 99,
            reason: "content_change".into(),
            pending: false,
        };
        let (content_id, event_id) = db.insert_change(&cap, &event, 4242).unwrap();
        let (events, total) = db.query_events(&EventFilter::default(), 10, 0).unwrap();
        assert_eq!(total, 1);
        let row = &events[0];
        assert_eq!(row.id, event_id);
        assert_eq!(row.content_id, Some(content_id));
        assert_eq!(row.pid, Some(321));
        assert_eq!(row.app_name, "Notepad");
        assert_eq!(row.exec_path, "C:\\Windows\\notepad.exe");
        assert_eq!(row.window_id, Some(7));
        assert_eq!(row.source_app, "Notepad");
        assert_eq!(row.source_pid, Some(321));
        assert_eq!(row.reason, "content_change");
        assert_eq!(row.targets, "text/plain, text/html");
        let detail = db.get_event(event_id).unwrap().unwrap();
        assert_eq!(detail.body.as_deref(), Some("cross-platform"));
        drop(db);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn insert_change_rolls_back_when_event_insert_fails() {
        let (mut db, path) = tmp_db();
        let cap = assemble(
            &FetchInput {
                target_names: vec!["text/plain".into()],
                plain: Some(b"will-rollback".to_vec()),
                plain_total: 13,
                ..FetchInput::default()
            },
            &Limits::default(),
        );
        // 用触发器让 events 插入中途失败，验证 contents 随事务一起回滚。
        db.exec_for_test("CREATE TRIGGER reject_events BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT, 'blocked'); END;").unwrap();
        let event = NewEvent {
            ts_ms: 1,
            op: "write".into(),
            selection: "CLIPBOARD".into(),
            pid: None,
            app_name: String::new(),
            wm_class: String::new(),
            exec_path: String::new(),
            window_id: 0,
            content_id: None,
            content_type: cap.content_type.clone(),
            content_size: cap.size,
            summary: cap.summary.clone(),
            targets: String::new(),
            source_app: String::new(),
            source_pid: None,
            source_event_id: None,
            truncated: false,
            x_time: 0,
            reason: "content_change".into(),
            pending: false,
        };
        assert!(db.insert_change(&cap, &event, 1).is_err());
        let (events, total) = db.query_events(&EventFilter::default(), 10, 0).unwrap();
        assert_eq!(total, 0, "events 插入失败不应留下事件");
        // contents 也应回滚：统计里不应出现孤儿行。
        let stats = db.stats().unwrap();
        assert_eq!(stats.contents, 0, "contents 必须随事务一起回滚");
        drop(db);
        let _ = std::fs::remove_file(path);
        let _ = events;
    }

    fn encrypted_db() -> (Database, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "clipmon-enc-{}-{}-{}",
            std::process::id(),
            crate::now_ms(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("monitor.db");
        let db = Database::open_encrypted(&path).unwrap();
        (db, dir)
    }

    fn sample_cap(text: &str) -> crate::classify::Captured {
        assemble(
            &FetchInput {
                target_names: vec!["UTF8_STRING".into()],
                plain: Some(text.as_bytes().to_vec()),
                plain_total: text.len() as u64,
                plain_mime: Some("UTF8_STRING".into()),
                ..FetchInput::default()
            },
            &Limits::default(),
        )
    }

    fn sample_event(cap: &crate::classify::Captured) -> NewEvent {
        NewEvent {
            ts_ms: 1000,
            op: "write".into(),
            selection: "CLIPBOARD".into(),
            pid: Some(7),
            app_name: "editor".into(),
            wm_class: String::new(),
            exec_path: "/usr/bin/editor".into(),
            window_id: 3,
            content_id: None,
            content_type: cap.content_type.clone(),
            content_size: cap.size,
            summary: cap.summary.clone(),
            targets: "UTF8_STRING".into(),
            source_app: "editor".into(),
            source_pid: Some(7),
            source_event_id: None,
            truncated: cap.truncated,
            x_time: 0,
            reason: "content_change".into(),
            pending: false,
        }
    }

    #[test]
    fn encrypted_db_never_stores_plaintext_on_disk() {
        let (mut db, dir) = encrypted_db();
        let secret = "hunter2 token=AKIAEXAMPLE 中文机密";
        let cap = sample_cap(secret);
        let (cid, eid) = db.insert_change(&cap, &sample_event(&cap), 1000).unwrap();

        // 直接读原始 SQLite 列，确认没有任何明文片段。
        let raw_body: String = db
            .conn
            .query_row("SELECT body FROM contents WHERE id=?1", params![cid], |r| {
                r.get(0)
            })
            .unwrap();
        let raw_summary: String = db
            .conn
            .query_row(
                "SELECT summary FROM events WHERE id=?1",
                params![eid],
                |r| r.get(0),
            )
            .unwrap();
        assert!(raw_body.starts_with("enc:v1:"));
        assert!(raw_summary.starts_with("enc:v1:"));
        assert!(!raw_body.contains("hunter2"));
        assert!(!raw_summary.contains("hunter2"));
        let file = std::fs::read(dir.join("monitor.db")).unwrap();
        assert!(!String::from_utf8_lossy(&file).contains("hunter2"));

        // 通过公开 API 读回必须得到原文。
        let detail = db.get_event(eid).unwrap().unwrap();
        assert_eq!(detail.body.as_deref(), Some(secret));
        assert_eq!(detail.event.summary, cap.summary);
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn encrypted_db_supports_fts_search_and_reopening() {
        let (mut db, dir) = encrypted_db();
        let texts = [
            "部署密钥 DEPLOY-KEY 与服务器地址",
            "会议纪要：下周三评审剪贴板方案",
            "ordinary shopping list milk and eggs",
        ];
        let mut ids = Vec::new();
        for (i, t) in texts.iter().enumerate() {
            let cap = sample_cap(t);
            let mut ev = sample_event(&cap);
            ev.ts_ms = 1000 + i as i64;
            let (_, eid) = db.insert_change(&cap, &ev, ev.ts_ms).unwrap();
            ids.push(eid);
        }
        let f = EventFilter {
            query: Some("部署密钥".into()),
            ..Default::default()
        };
        let (rows, total) = db.query_events(&f, 10, 0).unwrap();
        assert_eq!(total, 1, "FTS5 应命中中文短语");
        assert_eq!(rows[0].id, ids[0]);
        let f2 = EventFilter {
            query: Some("milk".into()),
            ..Default::default()
        };
        let (_, total2) = db.query_events(&f2, 10, 0).unwrap();
        assert_eq!(total2, 1, "FTS5 应命中英文单词");
        drop(db);

        // 重新打开同一个加密库，密钥从数据目录恢复，历史记录仍可读。
        let db2 = Database::open_encrypted(&dir.join("monitor.db")).unwrap();
        let detail = db2.get_event(ids[1]).unwrap().unwrap();
        assert_eq!(detail.body.as_deref(), Some(texts[1]));
        drop(db2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn legacy_plaintext_rows_are_readable_after_encryption_enabled() {
        // 先用明文模式写入历史数据（模拟旧版本），再用加密模式打开。
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "clipmon-migrate-{}-{}-{}",
            std::process::id(),
            crate::now_ms(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("monitor.db");
        let old_id = {
            let mut old = Database::open(&path).unwrap();
            let cap = sample_cap("旧版本明文记录 legacy-secret");
            let (_, eid) = old.insert_change(&cap, &sample_event(&cap), 500).unwrap();
            eid
        };
        let db = Database::open_encrypted(&path).unwrap();
        let detail = db.get_event(old_id).unwrap().unwrap();
        assert_eq!(detail.body.as_deref(), Some("旧版本明文记录 legacy-secret"));
        // 迁移后该行应已重加密。
        let raw: String = db
            .conn
            .query_row("SELECT body FROM contents LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert!(raw.starts_with("enc:v1:"));
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn wrong_key_cannot_read_encrypted_database() {
        let (mut db, dir) = encrypted_db();
        let cap = sample_cap("top-secret-value");
        let (_, eid) = db.insert_change(&cap, &sample_event(&cap), 1000).unwrap();
        drop(db);
        // 换掉密钥文件，模拟密钥丢失/不匹配。
        std::fs::write(
            dir.join("master.key"),
            format!("key:v1:{}", "00".repeat(32)),
        )
        .unwrap();
        // 密钥不匹配时打开就应失败（迁移阶段解密失败），避免静默丢数据。
        let result = Database::open_encrypted(&dir.join("monitor.db"));
        assert!(
            result.is_err(),
            "密钥不匹配时应明确报错而不是静默继续: {eid}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
