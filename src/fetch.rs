//! 从当前选择区所有者拉取内容。图片只读文件头，大文本在知道总长度后停止继续保存。

use crate::classify::{assemble, Captured, FetchInput, Limits};
use crate::log_debug;
use crate::x11util::{intern, poll_event_timeout, raw_fd, timeout_after, XConn};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::time::Instant;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt as _, Property, Window};
use x11rb::protocol::Event;
use x11rb::CURRENT_TIME;

pub struct Atoms {
    pub clipboard: Atom,
    pub primary: Atom,
    pub targets: Atom,
    pub utf8: Atom,
    pub string: Atom,
    pub text: Atom,
    pub plain: Atom,
    pub plain_utf8: Atom,
    pub html: Atom,
    pub rtf: Atom,
    pub app_rtf: Atom,
    pub rich: Atom,
    pub png: Atom,
    pub jpeg: Atom,
    pub bmp: Atom,
    pub gif: Atom,
    pub webp: Atom,
    pub uri_list: Atom,
    pub gnome: Atom,
    pub moz: Atom,
    pub kde_cut: Atom,
    pub incr: Atom,
    pub prop: Atom,
}

impl Atoms {
    pub fn load(conn: &XConn) -> Result<Self> {
        Ok(Self {
            clipboard: intern(conn, "CLIPBOARD")?,
            primary: intern(conn, "PRIMARY")?,
            targets: intern(conn, "TARGETS")?,
            utf8: intern(conn, "UTF8_STRING")?,
            string: intern(conn, "STRING")?,
            text: intern(conn, "TEXT")?,
            plain: intern(conn, "text/plain")?,
            plain_utf8: intern(conn, "text/plain;charset=utf-8")?,
            html: intern(conn, "text/html")?,
            rtf: intern(conn, "text/rtf")?,
            app_rtf: intern(conn, "application/rtf")?,
            rich: intern(conn, "text/richtext")?,
            png: intern(conn, "image/png")?,
            jpeg: intern(conn, "image/jpeg")?,
            bmp: intern(conn, "image/bmp")?,
            gif: intern(conn, "image/gif")?,
            webp: intern(conn, "image/webp")?,
            uri_list: intern(conn, "text/uri-list")?,
            gnome: intern(conn, "x-special/gnome-copied-files")?,
            moz: intern(conn, "text/x-moz-url")?,
            kde_cut: intern(conn, "application/x-kde-cutselection")?,
            incr: intern(conn, "INCR")?,
            prop: intern(conn, "CLIPMON_DATA")?,
        })
    }

    pub fn selection_name(&self, atom: Atom) -> Option<&'static str> {
        if atom == self.clipboard {
            Some("CLIPBOARD")
        } else if atom == self.primary {
            Some("PRIMARY")
        } else {
            None
        }
    }
}

pub fn capture_selection(
    conn: &XConn,
    window: Window,
    atoms: &Atoms,
    names: &mut HashMap<Atom, String>,
    selection: Atom,
    expected_owner: Window,
    limits: &Limits,
) -> Result<Captured> {
    let owner = conn.get_selection_owner(selection)?.reply()?.owner;
    if owner == 0 {
        return Ok(crate::classify::empty_capture());
    }
    // 剪贴板管理器经常会马上把所有权接过去。内容通常还是同一份，向当前所有者抓取。
    let owner_changed = expected_owner != 0 && owner != expected_owner;
    if owner_changed {
        log_debug(format!(
            "选择区所有者已从 {expected_owner} 变为 {owner}，仍向当前所有者抓取"
        ));
    }
    let targets = read_target_list(conn, window, atoms, selection)?;
    let target_names: Vec<String> = targets
        .iter()
        .copied()
        .map(|a| atom_name(conn, names, a))
        .collect();
    log_debug(format!(
        "抓取 selection={selection} owner={owner} targets={}",
        target_names.join(",")
    ));

    let mut input = FetchInput {
        target_names,
        ..FetchInput::default()
    };

    if targets.iter().any(|a| *a == atoms.kde_cut) {
        input.kde_cut = true;
    }

    if let Some(atom) = pick(&targets, &[atoms.gnome, atoms.uri_list, atoms.moz]) {
        let keep = limits.text_full_limit.min(1024 * 1024) as usize;
        if let Some(data) = read_target(conn, window, atoms, selection, atom, keep, keep)? {
            let text = if atom == atoms.moz {
                decode_moz(&data.bytes)
            } else {
                String::from_utf8_lossy(&data.bytes).into_owned()
            };
            input.files_format = Some(if atom == atoms.gnome {
                "gnome".into()
            } else if atom == atoms.moz {
                "moz".into()
            } else {
                "uri-list".into()
            });
            input.files_raw = Some(text);
        }
    }

    if let Some(atom) = pick(&targets, &[atoms.png, atoms.webp, atoms.gif, atoms.jpeg, atoms.bmp]) {
        let mime = atom_name(conn, names, atom);
        if let Some(data) = read_target(conn, window, atoms, selection, atom, 64 * 1024, 64 * 1024)? {
            input.image_size = data.total;
            input.image_header = data.bytes;
            input.image_mime = Some(mime);
        }
    }

    if let Some(atom) = pick(&targets, &[atoms.html, atoms.rtf, atoms.app_rtf, atoms.rich]) {
        if let Some(data) = read_target(
            conn,
            window,
            atoms,
            selection,
            atom,
            limits.text_full_limit as usize,
            limits.text_preview as usize,
        )? {
            input.rich_mime = Some(atom_name(conn, names, atom));
            input.rich_total = data.total;
            input.rich = Some(data.bytes);
        }
    }

    if let Some(atom) = pick(
        &targets,
        &[atoms.utf8, atoms.plain_utf8, atoms.plain, atoms.string, atoms.text],
    ) {
        if let Some(data) = read_target(
            conn,
            window,
            atoms,
            selection,
            atom,
            limits.text_full_limit as usize,
            limits.text_preview as usize,
        )? {
            input.plain_mime = Some(atom_name(conn, names, atom));
            input.plain_total = data.total;
            input.plain = Some(data.bytes);
        }
    }

    // 没有 TARGETS 时，很多老程序仍接受直接要 UTF8_STRING。
    if targets.is_empty() {
        if let Some(data) = read_target(
            conn,
            window,
            atoms,
            selection,
            atoms.utf8,
            limits.text_full_limit as usize,
            limits.text_preview as usize,
        )? {
            input.plain_mime = Some("UTF8_STRING".into());
            input.plain_total = data.total;
            input.plain = Some(data.bytes);
            input.target_names.push("UTF8_STRING".into());
        }
    }

    let mut captured = assemble(&input, limits);
    if owner_changed {
        if let Ok(mut meta) = serde_json::from_str::<serde_json::Value>(&captured.meta_json) {
            meta["owner_changed"] = serde_json::json!(true);
            meta["observed_owner"] = serde_json::json!(owner);
            captured.meta_json = meta.to_string();
        }
    }
    Ok(captured)
}

fn atom_name(conn: &XConn, cache: &mut HashMap<Atom, String>, atom: Atom) -> String {
    if let Some(name) = cache.get(&atom) {
        return name.clone();
    }
    let name = conn
        .get_atom_name(atom)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| String::from_utf8_lossy(&r.name).into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("ATOM_{atom}"));
    if cache.len() > 2048 {
        cache.clear();
    }
    cache.insert(atom, name.clone());
    name
}

fn pick(have: &[Atom], prefer: &[Atom]) -> Option<Atom> {
    prefer.iter().copied().find(|a| have.contains(a))
}

struct BytesOut {
    bytes: Vec<u8>,
    total: u64,
}

fn read_target_list(
    conn: &XConn,
    window: Window,
    atoms: &Atoms,
    selection: Atom,
) -> Result<Vec<Atom>> {
    let Some(data) = read_target(conn, window, atoms, selection, atoms.targets, 64 * 1024, 64 * 1024)?
    else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for chunk in data.bytes.chunks_exact(4) {
        let atom = u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if atom != 0 {
            out.push(atom);
        }
    }
    Ok(out)
}

fn read_target(
    conn: &XConn,
    window: Window,
    atoms: &Atoms,
    selection: Atom,
    target: Atom,
    full_limit: usize,
    preview: usize,
) -> Result<Option<BytesOut>> {
    let fd = raw_fd(conn);
    let deadline = timeout_after(1500);
    conn.convert_selection(window, selection, target, atoms.prop, CURRENT_TIME)?
        .ignore_error();
    conn.flush()?;
    let property = match wait_selection(conn, fd, window, selection, target, deadline) {
        Ok(property) => property,
        Err(err) if err.to_string().contains("超时") => {
            log_debug(format!("目标 {target} 等待超时"));
            let _ = conn.delete_property(window, atoms.prop).map(|c| c.ignore_error());
            let _ = conn.flush();
            return Ok(None);
        }
        Err(err) => return Err(err),
    };
    if property == 0 {
        return Ok(None);
    }
    let data = read_property(conn, fd, window, property, atoms.incr, full_limit, preview, deadline)?;
    let _ = conn.delete_property(window, property).map(|c| c.ignore_error());
    let _ = conn.flush();
    Ok(Some(data))
}

fn wait_selection(
    conn: &XConn,
    fd: i32,
    window: Window,
    selection: Atom,
    target: Atom,
    deadline: Instant,
) -> Result<Atom> {
    while Instant::now() < deadline {
        match poll_event_timeout(conn, fd, deadline)? {
            None => break,
            Some(Event::SelectionNotify(ev))
                if ev.requestor == window && ev.selection == selection && ev.target == target =>
            {
                return Ok(ev.property);
            }
            Some(Event::Error(err)) => {
                log_debug(format!("抓取时收到 X 错误 {err:?}"));
            }
            Some(_) => {}
        }
    }
    bail!("等待 SelectionNotify 超时 target={target}");
}

fn read_property(
    conn: &XConn,
    fd: i32,
    window: Window,
    property: Atom,
    incr_atom: Atom,
    full_limit: usize,
    preview: usize,
    deadline: Instant,
) -> Result<BytesOut> {
    let first = conn
        .get_property(false, window, property, AtomEnum::ANY, 0, ((full_limit.max(4) + 3) / 4) as u32)?
        .reply()
        .context("GetProperty")?;
    if first.type_ == incr_atom || first.type_ == intern_incr_fallback(first.type_, incr_atom) {
        return read_incr(conn, fd, window, property, full_limit, preview, deadline);
    }
    let total = first.value.len() as u64 + u64::from(first.bytes_after);
    let keep = if total >= full_limit as u64 {
        preview.min(first.value.len())
    } else {
        first.value.len()
    };
    let mut bytes = first.value;
    bytes.truncate(keep);
    if total < full_limit as u64 && first.bytes_after > 0 {
        // 小于完整保存阈值，把剩余部分读完。
        let mut got_len = (total as usize).saturating_sub(first.bytes_after as usize);
        while got_len < total as usize && Instant::now() < deadline {
            let long_off = (got_len / 4) as u32;
            let reply = conn
                .get_property(
                    false,
                    window,
                    property,
                    AtomEnum::ANY,
                    long_off,
                    ((full_limit - bytes.len()).max(4) / 4) as u32,
                )?
                .reply()?;
            if reply.value.is_empty() {
                break;
            }
            bytes.extend_from_slice(&reply.value);
            got_len += reply.value.len();
            if reply.bytes_after == 0 || bytes.len() >= full_limit {
                break;
            }
        }
    }
    Ok(BytesOut { bytes, total })
}

fn intern_incr_fallback(actual: Atom, incr: Atom) -> Atom {
    // 占位，避免把普通类型误判成 INCR。调用方已经用 incr_atom 比较。
    if actual == incr {
        incr
    } else {
        Atom::MAX
    }
}

fn read_incr(
    conn: &XConn,
    fd: i32,
    window: Window,
    property: Atom,
    full_limit: usize,
    preview: usize,
    deadline: Instant,
) -> Result<BytesOut> {
    conn.delete_property(window, property)?.check()?;
    conn.flush()?;
    let mut kept = Vec::new();
    let mut total = 0u64;
    while Instant::now() < deadline {
        let ev = match poll_event_timeout(conn, fd, deadline)? {
            Some(ev) => ev,
            None => break,
        };
        let Event::PropertyNotify(ev) = ev else {
            continue;
        };
        if ev.window != window || ev.atom != property || ev.state != Property::NEW_VALUE {
            continue;
        }
        let chunk = conn
            .get_property(true, window, property, AtomEnum::ANY, 0, (256 * 1024) / 4)?
            .reply()?;
        if chunk.value.is_empty() {
            break;
        }
        total += chunk.value.len() as u64;
        let limit_now = if total >= full_limit as u64 { preview } else { full_limit };
        if kept.len() < limit_now {
            let room = limit_now - kept.len();
            kept.extend(chunk.value.into_iter().take(room));
        }
    }
    if total == 0 && kept.is_empty() {
        bail!("INCR 传输没有数据");
    }
    let total = total.max(kept.len() as u64);
    Ok(BytesOut { bytes: kept, total })
}

fn decode_moz(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let text = String::from_utf16_lossy(&units);
    text.replace('\u{0}', "\n")
}
