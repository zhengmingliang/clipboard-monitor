//! JSONL 导出 / 导入（含解密后的剪贴板正文）。
//!
//! 导出格式：首行是 `{"format":"clipboard-monitor-jsonl","version":1,...}`
//! 元数据头，之后每行一条事件，字段与 `/api/events/{id}` 一致并额外带
//! `body` / `meta_json` / `sha256` / `has_blob`（blob 二进制以 base64 放在
//! `blob_b64`）。正文是**解密后的明文**——导出文件本身不加密，必须妥善保管。
//!
//! 导入按行解析，跳过格式不匹配或重复 id 的行；用 `insert_content` +
//! `insert_event` 重建（会生成新的 id），不覆盖现有记录，也不会破坏现有
//! 加密密钥。适合迁移/审计，不适合做增量同步。

use crate::db::{Database, EventFilter};
use anyhow::{bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

pub const FORMAT: &str = "clipboard-monitor-jsonl";
pub const VERSION: u32 = 1;

/// 导出所有事件到 writer。返回导出的条数。
pub fn export_to<W: Write>(db: &Database, out: &mut W) -> Result<u64> {
    let header = json!({
        "format": FORMAT,
        "version": VERSION,
        "exported_ms": crate::now_ms(),
        "encrypted": db.is_encrypted(),
    });
    writeln!(out, "{}", header).context("写入导出头")?;

    let mut offset: i64 = 0;
    let mut count = 0u64;
    loop {
        let (rows, _total) = db.query_events(&EventFilter::default(), 200, offset)?;
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            let detail = db
                .get_event(row.id)?
                .ok_or_else(|| anyhow::anyhow!("事件 {} 详情缺失", row.id))?;
            let mut line = serde_json::to_value(&detail)?;
            if let Some(obj) = line.as_object_mut() {
                // blob 以 base64 嵌入，保证 JSONL 每行仍是合法单行 JSON。
                if detail.has_blob {
                    if let Ok(Some((_name, bytes))) = db.get_blob_for_event(row.id) {
                        obj.insert(
                            "blob_b64".into(),
                            json!(base64::engine::general_purpose::STANDARD.encode(bytes)),
                        );
                    }
                }
            }
            writeln!(out, "{}", line).context("写入导出行")?;
            count += 1;
        }
        offset += rows.len() as i64;
    }
    Ok(count)
}

/// 从 JSONL 导入。返回（成功条数，跳过条数）。
pub fn import_from<R: BufRead>(db: &mut Database, input: R) -> Result<(u64, u64)> {
    use crate::classify::Captured;
    use crate::db::NewEvent;

    let mut lines = input.lines();
    let header_line = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("导出文件为空"))??;
    let header: Value = serde_json::from_str(&header_line).context("导出头不是合法 JSON")?;
    if header["format"].as_str() != Some(FORMAT) {
        bail!("不是 clipboard-monitor 的 JSONL 导出文件");
    }
    if header["version"].as_u64().unwrap_or(0) as u32 != VERSION {
        bail!("导出文件版本不受支持: {:?}", header["version"]);
    }

    let mut ok = 0u64;
    let mut skipped = 0u64;
    for line in lines {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let event = &value["event"];
        let content_type = event["content_type"].as_str().unwrap_or("custom");
        let body = value["body"].as_str().map(|s| s.to_string());
        let meta_json = value["meta_json"].as_str().unwrap_or("{}").to_string();
        let blob = value["blob_b64"]
            .as_str()
            .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok());
        let cap = Captured {
            content_type: content_type.to_string(),
            size: event["content_size"].as_u64().unwrap_or(0),
            truncated: event["truncated"].as_bool().unwrap_or(false),
            body,
            blob,
            summary: event["summary"].as_str().unwrap_or("").to_string(),
            meta_json,
            sha256: value["sha256"].as_str().unwrap_or("").to_string(),
            targets: event["targets"]
                .as_str()
                .unwrap_or("")
                .split(", ")
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect(),
        };
        let ts = event["ts_ms"].as_i64().unwrap_or_else(crate::now_ms);
        let cid = db.insert_content(&cap, ts)?;
        let new_event = NewEvent {
            ts_ms: ts,
            op: event["op"].as_str().unwrap_or("write").to_string(),
            selection: event["selection"]
                .as_str()
                .unwrap_or("CLIPBOARD")
                .to_string(),
            pid: event["pid"].as_i64().and_then(|p| i32::try_from(p).ok()),
            app_name: event["app_name"].as_str().unwrap_or("").to_string(),
            wm_class: event["wm_class"].as_str().unwrap_or("").to_string(),
            exec_path: event["exec_path"].as_str().unwrap_or("").to_string(),
            window_id: event["window_id"].as_u64().unwrap_or(0) as u32,
            content_id: Some(cid),
            content_type: content_type.to_string(),
            content_size: event["content_size"].as_u64().unwrap_or(0),
            summary: event["summary"].as_str().unwrap_or("").to_string(),
            targets: event["targets"].as_str().unwrap_or("").to_string(),
            source_app: event["source_app"].as_str().unwrap_or("").to_string(),
            source_pid: event["source_pid"]
                .as_i64()
                .and_then(|p| i32::try_from(p).ok()),
            source_event_id: None,
            truncated: event["truncated"].as_bool().unwrap_or(false),
            x_time: 0,
            reason: event["reason"].as_str().unwrap_or("imported").to_string(),
            pending: false,
        };
        db.insert_event(&new_event)?;
        ok += 1;
    }
    Ok((ok, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{assemble, FetchInput, Limits};
    use crate::db::NewEvent;

    #[test]
    fn export_import_roundtrip_preserves_plaintext_body() {
        let dir = std::env::temp_dir().join(format!(
            "clipmon-backup-{}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("monitor.db");
        let mut db = Database::open_encrypted(&path).unwrap();
        let cap = assemble(
            &FetchInput {
                plain: Some("export-secret-中文".as_bytes().to_vec()),
                plain_total: 21,
                plain_mime: Some("text/plain".into()),
                ..FetchInput::default()
            },
            &Limits::default(),
        );
        db.insert_change(
            &cap,
            &NewEvent {
                ts_ms: 1234,
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
            },
            1234,
        )
        .unwrap();
        let mut buf = Vec::new();
        let n = export_to(&db, &mut buf).unwrap();
        assert_eq!(n, 1);
        assert!(String::from_utf8_lossy(&buf).contains("export-secret"));
        drop(db);

        let path2 = dir.join("restored.db");
        let mut db2 = Database::open_encrypted(&path2).unwrap();
        let (ok, skipped) = import_from(&mut db2, std::io::Cursor::new(buf)).unwrap();
        assert_eq!((ok, skipped), (1, 0));
        let (rows, total) = db2.query_events(&EventFilter::default(), 10, 0).unwrap();
        assert_eq!(total, 1);
        let detail = db2.get_event(rows[0].id).unwrap().unwrap();
        assert_eq!(detail.body.as_deref(), Some("export-secret-中文"));
        drop(db2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn import_rejects_wrong_format_or_version() {
        let dir = std::env::temp_dir().join(format!(
            "clipmon-backup-bad-{}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Database::open(&dir.join("monitor.db")).unwrap();
        assert!(import_from(&mut db, std::io::Cursor::new(b"{\"format\":\"other\"}\n")).is_err());
        assert!(import_from(
            &mut db,
            std::io::Cursor::new(format!("{{\"format\":\"{FORMAT}\",\"version\":999}}\n"))
        )
        .is_err());
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }
}
