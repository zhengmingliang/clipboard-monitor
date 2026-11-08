//! 把抓到的剪贴板目标分类，并按大小规则决定保存原文、摘要还是元数据。

use crate::config::{DEFAULT_FILE_CONTENT_LIMIT, DEFAULT_TEXT_FULL_LIMIT, DEFAULT_TEXT_PREVIEW};
use percent_encoding::percent_decode_str;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

pub const TYPE_TEXT: &str = "text";
pub const TYPE_RICHTEXT: &str = "richtext";
pub const TYPE_IMAGE: &str = "image";
pub const TYPE_FILES: &str = "files";
pub const TYPE_CUSTOM: &str = "custom";
pub const TYPE_MIXED: &str = "mixed";
pub const TYPE_EMPTY: &str = "empty";

#[derive(Debug, Clone)]
pub struct Limits {
    pub text_full_limit: u64,
    pub text_preview: u64,
    pub file_content_limit: u64,
    pub record_file_content: bool,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            text_full_limit: DEFAULT_TEXT_FULL_LIMIT,
            text_preview: DEFAULT_TEXT_PREVIEW,
            file_content_limit: DEFAULT_FILE_CONTENT_LIMIT,
            record_file_content: false,
        }
    }
}

/// 一次抓取得到的原始片段。二进制图片只留文件头和总大小。
#[derive(Debug, Clone, Default)]
pub struct FetchInput {
    pub target_names: Vec<String>,
    pub plain: Option<Vec<u8>>,
    pub plain_mime: Option<String>,
    pub plain_total: u64,
    pub rich: Option<Vec<u8>>,
    pub rich_mime: Option<String>,
    pub rich_total: u64,
    pub files_raw: Option<String>,
    pub files_format: Option<String>,
    pub kde_cut: bool,
    pub image_mime: Option<String>,
    pub image_size: u64,
    pub image_header: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Captured {
    pub content_type: String,
    pub size: u64,
    pub truncated: bool,
    pub body: Option<String>,
    pub blob: Option<Vec<u8>>,
    pub summary: String,
    pub meta_json: String,
    pub sha256: String,
    pub targets: Vec<String>,
}

impl Captured {
    pub fn placeholder(summary: &str) -> Self {
        Self {
            content_type: TYPE_CUSTOM.to_string(),
            size: 0,
            truncated: false,
            body: None,
            blob: None,
            summary: summary.to_string(),
            meta_json: "{}".to_string(),
            sha256: String::new(),
            targets: Vec::new(),
        }
    }
}

pub fn empty_capture() -> Captured {
    let mut cap = Captured::placeholder("剪贴板已清空");
    cap.content_type = TYPE_EMPTY.to_string();
    cap.sha256 = sha256_hex(b"empty");
    cap.meta_json = json!({"kind": "empty"}).to_string();
    cap
}

pub fn unavailable_capture(reason: &str) -> Captured {
    let mut cap = Captured::placeholder(reason);
    cap.content_type = TYPE_CUSTOM.to_string();
    cap.sha256 = sha256_hex(reason.as_bytes());
    cap.meta_json = json!({"kind": "unavailable", "reason": reason}).to_string();
    cap
}

pub fn assemble(input: &FetchInput, limits: &Limits) -> Captured {
    let files = input
        .files_raw
        .as_deref()
        .map(|raw| parse_file_payload(raw, input.files_format.as_deref(), input.kde_cut))
        .unwrap_or_default();
    let image = input.image_mime.as_ref().map(|mime| {
        let (w, h) = image_dimensions(mime, &input.image_header).unwrap_or((None, None));
        (mime.clone(), w, h, input.image_size)
    });
    let plain = input.plain.as_ref().map(|b| {
        limit_text(b, input.plain_total.max(b.len() as u64), limits)
    });
    let rich = input.rich.as_ref().map(|b| {
        limit_text(b, input.rich_total.max(b.len() as u64), limits)
    });

    let has_files = !files.is_empty();
    let has_image = image.is_some();
    let plain_chars = plain
        .as_ref()
        .map(|t| t.text.chars().count())
        .unwrap_or(0);
    let has_plain = plain.as_ref().is_some_and(|t| !t.text.trim().is_empty());
    let has_rich = rich.as_ref().is_some_and(|t| !t.text.trim().is_empty());

    let content_type = if has_files && has_image {
        TYPE_MIXED
    } else if has_files {
        TYPE_FILES
    } else if has_image && has_plain && plain_chars > 40 {
        TYPE_MIXED
    } else if has_image {
        TYPE_IMAGE
    } else if has_rich {
        TYPE_RICHTEXT
    } else if has_plain {
        TYPE_TEXT
    } else if input
        .target_names
        .iter()
        .any(|n| !is_noise_target(n))
    {
        TYPE_CUSTOM
    } else {
        TYPE_EMPTY
    };

    let mut meta = json!({
        "targets": input.target_names,
        "kind": content_type,
    });
    let mut size: u64 = 0;
    let mut truncated = false;
    let mut body: Option<String> = None;
    let mut blob: Option<Vec<u8>> = None;
    let mut hash_material: Vec<u8> = Vec::new();

    if has_files {
        let op = files
            .first()
            .map(|f| f.op.clone())
            .unwrap_or_else(|| "copy".into());
        let listed: Vec<Value> = files
            .iter()
            .take(500)
            .map(|f| {
                json!({
                    "name": f.name,
                    "path": f.path,
                    "size": f.size,
                    "exists": f.exists,
                    "is_file": f.is_file,
                })
            })
            .collect();
        meta["files"] = json!({
            "operation": op,
            "count": files.len(),
            "format": input.files_format,
            "list_truncated": files.len() > 500,
            "entries": listed,
        });
        size = files.iter().filter_map(|f| f.size).sum();
        if size == 0 {
            size = input.files_raw.as_ref().map(|s| s.len() as u64).unwrap_or(0);
        }
        for f in &files {
            hash_material.extend(f.path.as_bytes());
            hash_material.push(b'\n');
        }
        if limits.record_file_content && files.len() == 1 {
            if let Some(stored) = maybe_read_file(&files[0], limits) {
                meta["file_content_stored"] = json!(true);
                meta["file_content_encoding"] = json!(if stored.is_text { "utf-8" } else { "binary" });
                if stored.is_text {
                    body = Some(stored.text);
                } else {
                    blob = Some(stored.bytes);
                }
            }
        }
    }

    if let Some((mime, w, h, image_size)) = &image {
        meta["image"] = json!({
            "mime": mime,
            "width": w,
            "height": h,
            "size": image_size,
            "stored": false,
        });
        if !has_files {
            size = *image_size;
        }
        hash_material.extend(mime.as_bytes());
        hash_material.extend(image_size.to_le_bytes());
        hash_material.extend(input.image_header.iter().take(64).copied());
    }

    if let Some(text) = &rich {
        meta["rich"] = json!({
            "mime": input.rich_mime,
            "size": text.total,
            "truncated": text.truncated,
        });
        truncated |= text.truncated;
        if content_type == TYPE_RICHTEXT || content_type == TYPE_MIXED {
            if body.is_none() {
                body = Some(text.text.clone());
            }
            if !has_files && !has_image {
                size = text.total;
            }
        }
        hash_material.extend(text.hash_bytes.iter().copied());
    }

    if let Some(text) = &plain {
        meta["plain"] = json!({
            "mime": input.plain_mime,
            "size": text.total,
            "truncated": text.truncated,
        });
        truncated |= text.truncated;
        if matches!(content_type, TYPE_TEXT | TYPE_MIXED | TYPE_RICHTEXT) && body.is_none() {
            body = Some(text.text.clone());
        } else if content_type == TYPE_RICHTEXT {
            meta["plain_text"] = json!(text.text);
        } else if content_type == TYPE_TEXT {
            body = Some(text.text.clone());
            size = text.total;
        }
        if content_type == TYPE_TEXT {
            size = text.total;
            hash_material.extend(text.hash_bytes.iter().copied());
        } else if !has_files && !has_image && content_type != TYPE_RICHTEXT {
            hash_material.extend(text.hash_bytes.iter().copied());
        }
    }

    if content_type == TYPE_CUSTOM {
        let names: Vec<&str> = input
            .target_names
            .iter()
            .filter(|n| !is_noise_target(n))
            .map(|s| s.as_str())
            .collect();
        meta["custom_targets"] = json!(names);
        for n in &names {
            hash_material.extend(n.as_bytes());
            hash_material.push(0);
        }
    }

    if content_type == TYPE_EMPTY && hash_material.is_empty() {
        hash_material.extend(b"empty");
    }

    let summary = build_summary(content_type, &files, &image, &plain, &rich, &input.target_names, truncated);
    if body.is_none() && content_type == TYPE_TEXT {
        body = plain.map(|t| t.text);
    }

    Captured {
        content_type: content_type.to_string(),
        size,
        truncated,
        body,
        blob,
        summary,
        meta_json: meta.to_string(),
        sha256: sha256_hex(&hash_material),
        targets: input.target_names.clone(),
    }
}

struct StoredFile {
    is_text: bool,
    text: String,
    bytes: Vec<u8>,
}

struct LimitedText {
    text: String,
    total: u64,
    truncated: bool,
    hash_bytes: Vec<u8>,
}

fn limit_text(bytes: &[u8], total_hint: u64, limits: &Limits) -> LimitedText {
    let total = total_hint.max(bytes.len() as u64);
    let truncated = total >= limits.text_full_limit;
    let keep = if truncated {
        limits.text_preview.min(bytes.len() as u64) as usize
    } else {
        bytes.len()
    };
    let slice = &bytes[..keep.min(bytes.len())];
    let text = sanitize_text(&String::from_utf8_lossy(slice));
    let hash_bytes = if truncated {
        slice.to_vec()
    } else {
        bytes.to_vec()
    };
    LimitedText {
        text,
        total,
        truncated,
        hash_bytes,
    }
}

fn sanitize_text(s: &str) -> String {
    s.chars().filter(|c| *c != '\u{0}').collect()
}

fn build_summary(
    content_type: &str,
    files: &[ParsedFile],
    image: &Option<(String, Option<u32>, Option<u32>, u64)>,
    plain: &Option<LimitedText>,
    rich: &Option<LimitedText>,
    targets: &[String],
    truncated: bool,
) -> String {
    let mut parts = Vec::new();
    if !files.is_empty() {
        let names: Vec<&str> = files.iter().take(6).map(|f| f.name.as_str()).collect();
        let more = if files.len() > names.len() {
            format!(" 等 {} 个", files.len())
        } else if files.len() > 1 {
            format!("（{} 个）", files.len())
        } else {
            String::new()
        };
        let op = if files.first().is_some_and(|f| f.op == "cut") {
            "剪切"
        } else {
            "复制"
        };
        parts.push(format!("{op}文件 {names}{more}", names = names.join("、")));
    }
    if let Some((mime, w, h, size)) = image {
        let dim = match (w, h) {
            (Some(w), Some(h)) => format!("{w}×{h} "),
            _ => String::new(),
        };
        parts.push(format!(
            "图片 {mime} {dim}{}",
            crate::human_bytes(*size)
        ));
    }
    if let Some(text) = plain.as_ref().or(rich.as_ref()) {
        if matches!(content_type, TYPE_TEXT | TYPE_RICHTEXT | TYPE_MIXED) || files.is_empty() && image.is_none()
        {
            let one = one_line(&text.text, 180);
            if !one.is_empty() {
                parts.push(one);
            }
        }
    }
    if parts.is_empty() {
        let custom: Vec<&str> = targets
            .iter()
            .filter(|n| !is_noise_target(n))
            .take(6)
            .map(|s| s.as_str())
            .collect();
        if custom.is_empty() {
            return "空内容".to_string();
        }
        parts.push(format!("自定义格式 {}", custom.join(", ")));
    }
    let mut summary = parts.join(" · ");
    if truncated {
        summary.push_str(" · 已截断");
    }
    if summary.chars().count() > 240 {
        summary = summary.chars().take(240).collect();
        summary.push('…');
    }
    summary
}

fn one_line(s: &str, max_chars: usize) -> String {
    let flat: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        flat
    } else {
        let mut t: String = flat.chars().take(max_chars).collect();
        t.push('…');
        t
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedFile {
    op: String,
    path: String,
    name: String,
    size: Option<u64>,
    exists: bool,
    is_file: bool,
}

fn parse_file_payload(raw: &str, format: Option<&str>, kde_cut: bool) -> Vec<ParsedFile> {
    let mut op = if kde_cut { "cut" } else { "copy" }.to_string();
    let mut lines: Vec<&str> = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    if format == Some("gnome") {
        if let Some(first) = lines.first().copied() {
            if first == "copy" || first == "cut" {
                op = first.to_string();
                lines.remove(0);
            }
        }
    }
    if format == Some("moz") {
        // text/x-moz-url 已在抓取阶段解成 uri\n 文本。
    }
    let mut out = Vec::new();
    for line in lines {
        let uri = line.split_whitespace().next().unwrap_or(line);
        let Some(path) = file_uri_to_path(uri).or_else(|| {
            if uri.starts_with('/') {
                Some(uri.to_string())
            } else {
                None
            }
        }) else {
            continue;
        };
        let meta = fs::metadata(&path).ok();
        let name = Path::new(&path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| path.clone());
        out.push(ParsedFile {
            op: op.clone(),
            path,
            name,
            size: meta.as_ref().map(|m| m.len()),
            exists: meta.is_some(),
            is_file: meta.as_ref().is_some_and(|m| m.is_file()),
        });
    }
    out
}

pub fn file_uri_to_path(uri: &str) -> Option<String> {
    let uri = uri.trim().trim_matches('\0');
    let rest = uri.strip_prefix("file://")?;
    let decoded = percent_decode_str(rest).decode_utf8_lossy().into_owned();
    if let Some(path) = decoded.strip_prefix("localhost") {
        let path = if path.is_empty() {
            "/".to_string()
        } else if path.starts_with('/') {
            path.to_string()
        } else {
            return None;
        };
        return Some(path);
    }
    if decoded.starts_with('/') {
        return Some(decoded);
    }
    let idx = decoded.find('/')?;
    Some(decoded[idx..].to_string())
}

fn maybe_read_file(file: &ParsedFile, limits: &Limits) -> Option<StoredFile> {
    if !file.is_file {
        return None;
    }
    let size = file.size?;
    if size >= limits.file_content_limit || size > 8 * 1024 * 1024 {
        return None;
    }
    let bytes = read_capped(&file.path, size as usize)?;
    if bytes.len() as u64 >= limits.file_content_limit {
        return None;
    }
    match String::from_utf8(bytes.clone()) {
        Ok(text) if !text.contains('\u{0}') => Some(StoredFile {
            is_text: true,
            text,
            bytes,
        }),
        _ => Some(StoredFile {
            is_text: false,
            text: String::new(),
            bytes,
        }),
    }
}

fn read_capped(path: &str, size: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let file = fs::File::open(path).ok()?;
    let mut limited = file.take(size as u64);
    let mut buf = Vec::new();
    limited.read_to_end(&mut buf).ok()?;
    Some(buf)
}

pub fn image_dimensions(mime: &str, header: &[u8]) -> Option<(Option<u32>, Option<u32>)> {
    let mime = mime.to_ascii_lowercase();
    if mime.contains("png") || header.starts_with(b"\x89PNG\r\n\x1a\n") {
        return png_size(header).map(|(w, h)| (Some(w), Some(h)));
    }
    if mime.contains("jpeg") || mime.contains("jpg") || header.starts_with(&[0xff, 0xd8]) {
        return jpeg_size(header).map(|(w, h)| (Some(w), Some(h)));
    }
    if mime.contains("gif") || header.starts_with(b"GIF87a") || header.starts_with(b"GIF89a") {
        return gif_size(header).map(|(w, h)| (Some(w), Some(h)));
    }
    if mime.contains("webp") || (header.len() >= 12 && &header[0..4] == b"RIFF" && &header[8..12] == b"WEBP")
    {
        return webp_size(header).map(|(w, h)| (Some(w), Some(h)));
    }
    if mime.contains("bmp") || header.starts_with(b"BM") {
        return bmp_size(header).map(|(w, h)| (Some(w), Some(h)));
    }
    None
}

fn png_size(h: &[u8]) -> Option<(u32, u32)> {
    if h.len() < 24 || &h[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(h[16..20].try_into().ok()?);
    let hgt = u32::from_be_bytes(h[20..24].try_into().ok()?);
    Some((w, hgt))
}

fn gif_size(h: &[u8]) -> Option<(u32, u32)> {
    if h.len() < 10 {
        return None;
    }
    let w = u16::from_le_bytes([h[6], h[7]]) as u32;
    let hgt = u16::from_le_bytes([h[8], h[9]]) as u32;
    Some((w, hgt))
}

fn bmp_size(h: &[u8]) -> Option<(u32, u32)> {
    if h.len() < 26 {
        return None;
    }
    let w = i32::from_le_bytes(h[18..22].try_into().ok()?).unsigned_abs();
    let hgt = i32::from_le_bytes(h[22..26].try_into().ok()?).unsigned_abs();
    Some((w, hgt))
}

fn jpeg_size(h: &[u8]) -> Option<(u32, u32)> {
    if h.len() < 4 || h[0] != 0xff || h[1] != 0xd8 {
        return None;
    }
    let mut i = 2;
    while i + 9 < h.len() {
        if h[i] != 0xff {
            i += 1;
            continue;
        }
        while i < h.len() && h[i] == 0xff {
            i += 1;
        }
        if i >= h.len() {
            break;
        }
        let marker = h[i];
        i += 1;
        if marker == 0xd8 || marker == 0xd9 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        if i + 1 >= h.len() {
            break;
        }
        let len = u16::from_be_bytes([h[i], h[i + 1]]) as usize;
        if len < 2 || i + len > h.len() {
            break;
        }
        if matches!(marker, 0xc0 | 0xc1 | 0xc2 | 0xc3 | 0xc5 | 0xc6 | 0xc7 | 0xc9 | 0xca | 0xcb | 0xcd | 0xce | 0xcf)
        {
            if len >= 7 {
                let hgt = u16::from_be_bytes([h[i + 3], h[i + 4]]) as u32;
                let w = u16::from_be_bytes([h[i + 5], h[i + 6]]) as u32;
                return Some((w, hgt));
            }
        }
        i += len;
    }
    None
}

fn webp_size(h: &[u8]) -> Option<(u32, u32)> {
    if h.len() < 16 {
        return None;
    }
    let chunk = &h[12..16];
    if chunk == b"VP8X" && h.len() >= 30 {
        let w = 1 + u32::from_le_bytes([h[24], h[25], h[26], 0]);
        let hgt = 1 + u32::from_le_bytes([h[27], h[28], h[29], 0]);
        return Some((w, hgt));
    }
    if chunk == b"VP8 " && h.len() >= 30 {
        // 12..16 chunk, 16..20 size, 20..23 frame tag, then start code.
        let start = 23;
        if h.len() >= start + 7 && h[start] == 0x9d && h[start + 1] == 0x01 && h[start + 2] == 0x2a {
            let w = u16::from_le_bytes([h[start + 3], h[start + 4]]) as u32 & 0x3fff;
            let hgt = u16::from_le_bytes([h[start + 5], h[start + 6]]) as u32 & 0x3fff;
            return Some((w, hgt));
        }
    }
    if chunk == b"VP8L" && h.len() >= 25 && h[20] == 0x2f {
        let b = &h[21..25];
        let bits = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        let w = (bits & 0x3fff) + 1;
        let hgt = ((bits >> 14) & 0x3fff) + 1;
        return Some((w, hgt));
    }
    None
}

pub fn is_noise_target(name: &str) -> bool {
    matches!(
        name,
        "TARGETS"
            | "MULTIPLE"
            | "TIMESTAMP"
            | "SAVE_TARGETS"
            | "DELETE"
            | "INSERT_PROPERTY"
            | "INSERT_SELECTION"
            | "ATOM"
            | "ATOM_PAIR"
            | "CLIPBOARD"
            | "PRIMARY"
            | "NONE"
    )
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let dig = Sha256::digest(bytes);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(64);
    for b in dig {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

pub fn summary_preview(s: &str) -> String {
    one_line(s, 180)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            text_full_limit: 32,
            text_preview: 8,
            file_content_limit: 64,
            record_file_content: false,
        }
    }

    #[test]
    fn small_text_is_kept_and_large_text_is_truncated() {
        let small = assemble(
            &FetchInput {
                target_names: vec!["UTF8_STRING".into()],
                plain: Some(b"hello".to_vec()),
                plain_mime: Some("UTF8_STRING".into()),
                plain_total: 5,
                ..FetchInput::default()
            },
            &limits(),
        );
        assert_eq!(small.content_type, TYPE_TEXT);
        assert_eq!(small.body.as_deref(), Some("hello"));
        assert!(!small.truncated);

        let big = vec![b'a'; 40];
        let large = assemble(
            &FetchInput {
                target_names: vec!["UTF8_STRING".into()],
                plain: Some(big[..8].to_vec()),
                plain_mime: Some("UTF8_STRING".into()),
                plain_total: 40,
                ..FetchInput::default()
            },
            &limits(),
        );
        assert!(large.truncated);
        assert_eq!(large.size, 40);
        assert_eq!(large.body.as_deref(), Some("aaaaaaaa"));
        assert!(large.summary.contains("已截断"));
    }

    #[test]
    fn files_images_and_custom_are_classified() {
        let files = assemble(
            &FetchInput {
                target_names: vec!["text/uri-list".into(), "x-special/gnome-copied-files".into()],
                files_raw: Some("copy\nfile:///tmp/a%20b.txt\nfile:///tmp/c.txt\n".into()),
                files_format: Some("gnome".into()),
                ..FetchInput::default()
            },
            &limits(),
        );
        assert_eq!(files.content_type, TYPE_FILES);
        let meta: Value = serde_json::from_str(&files.meta_json).unwrap();
        assert_eq!(meta["files"]["count"], 2);
        assert_eq!(meta["files"]["entries"][0]["name"], "a b.txt");
        assert!(files.blob.is_none());
        assert!(files.body.is_none());

        let mut png = vec![0u8; 24];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        png[12..16].copy_from_slice(b"IHDR");
        png[16..20].copy_from_slice(&800u32.to_be_bytes());
        png[20..24].copy_from_slice(&600u32.to_be_bytes());
        let image = assemble(
            &FetchInput {
                target_names: vec!["image/png".into()],
                image_mime: Some("image/png".into()),
                image_size: 12345,
                image_header: png,
                ..FetchInput::default()
            },
            &limits(),
        );
        assert_eq!(image.content_type, TYPE_IMAGE);
        assert!(image.body.is_none());
        assert!(image.blob.is_none());
        let meta: Value = serde_json::from_str(&image.meta_json).unwrap();
        assert_eq!(meta["image"]["width"], 800);
        assert_eq!(meta["image"]["height"], 600);
        assert_eq!(meta["image"]["stored"], false);

        let custom = assemble(
            &FetchInput {
                target_names: vec!["TARGETS".into(), "application/x-my-app".into()],
                ..FetchInput::default()
            },
            &limits(),
        );
        assert_eq!(custom.content_type, TYPE_CUSTOM);
        assert!(custom.summary.contains("application/x-my-app"));
    }

    #[test]
    fn rich_text_and_file_uri_parser() {
        let rich = assemble(
            &FetchInput {
                target_names: vec!["text/html".into(), "UTF8_STRING".into()],
                plain: Some(b"hello".to_vec()),
                plain_total: 5,
                plain_mime: Some("UTF8_STRING".into()),
                rich: Some(b"<b>hello</b>".to_vec()),
                rich_total: 12,
                rich_mime: Some("text/html".into()),
                ..FetchInput::default()
            },
            &limits(),
        );
        assert_eq!(rich.content_type, TYPE_RICHTEXT);
        assert_eq!(rich.body.as_deref(), Some("<b>hello</b>"));

        assert_eq!(
            file_uri_to_path("file:///home/user/a%20b.txt").as_deref(),
            Some("/home/user/a b.txt")
        );
        assert_eq!(
            file_uri_to_path("file://localhost/tmp/x").as_deref(),
            Some("/tmp/x")
        );
    }

    #[test]
    fn image_headers() {
        // SOF0 段长度 11：精度、高、宽，再加一个颜色分量。
        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xc0, 0x00, 0x0b, 0x08];
        jpeg.extend(&10u16.to_be_bytes());
        jpeg.extend(&20u16.to_be_bytes());
        jpeg.extend([0x01, 0x01, 0x11, 0x00]);
        assert_eq!(jpeg_size(&jpeg), Some((20, 10)));

        let gif = b"GIF89a\x2c\x01\x90\x00";
        assert_eq!(gif_size(gif), Some((300, 144)));
    }

    #[test]
    fn optional_small_file_body_can_be_recorded() {
        let path = std::env::temp_dir().join(format!(
            "clipmon-file-{}-{}.txt",
            std::process::id(),
            crate::now_ms()
        ));
        fs::write(&path, "file-body").unwrap();
        let uri = format!("file://{}", path.display());
        let mut limits = limits();
        limits.record_file_content = true;
        limits.file_content_limit = 1024;
        let cap = assemble(
            &FetchInput {
                target_names: vec!["text/uri-list".into()],
                files_raw: Some(uri),
                files_format: Some("uri-list".into()),
                ..FetchInput::default()
            },
            &limits,
        );
        assert_eq!(cap.content_type, TYPE_FILES);
        assert_eq!(cap.body.as_deref(), Some("file-body"));
        let _ = fs::remove_file(&path);
    }
}
