//! Windows/macOS 的文本与图片抓取，以及平台原生变化检测。

#[cfg(any(target_os = "windows", target_os = "macos"))]
use super::{limits_from, App};
use crate::classify::sha256_hex;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use crate::classify::{assemble, empty_capture, Captured, FetchInput};
#[cfg(any(target_os = "windows", target_os = "macos"))]
use crate::{log_debug, log_info, log_warn, now_ms};
use anyhow::{bail, Context, Result};
#[cfg(any(target_os = "windows", target_os = "macos"))]
use arboard::{Clipboard, Error as ClipboardError};
use std::io::Cursor;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use std::sync::atomic::Ordering;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use std::sync::Arc;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use std::thread::{self, JoinHandle};
#[cfg(any(target_os = "windows", target_os = "macos"))]
use std::time::Duration;

#[cfg(any(target_os = "windows", target_os = "macos"))]
pub(super) fn spawn(app: Arc<App>) -> Result<Vec<JoinHandle<()>>> {
    let name = if cfg!(target_os = "windows") {
        "clip-windows"
    } else {
        "clip-macos"
    };
    let join = thread::Builder::new()
        .name(name.into())
        .spawn(move || platform::run(app))?;
    Ok(vec![join])
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn capture_and_record(app: &App, last_hash: &mut Option<String>) {
    let mut last_failure = None;
    for attempt in 0..3 {
        match capture_clipboard(app) {
            Ok(captured) => {
                if last_hash.as_deref() == Some(captured.sha256.as_str()) {
                    log_debug("剪贴板通知与上一份内容相同，已去重");
                    return;
                }
                match app.record_change(&captured, now_ms()) {
                    Ok(_) => {
                        *last_hash = Some(captured.sha256);
                        app.clear_error();
                        return;
                    }
                    Err(err) => last_failure = Some(format!("写入剪贴板日志失败: {err:#}")),
                }
            }
            Err(err) => last_failure = Some(format!("读取系统剪贴板失败: {err:#}")),
        }
        if attempt < 2 && !app.shutdown.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(50));
        }
    }
    if let Some(message) = last_failure {
        log_warn(&message);
        app.note_error(&message);
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn capture_clipboard(app: &App) -> Result<Captured> {
    let mut clipboard = Clipboard::new().context("初始化系统剪贴板")?;
    let limits = {
        let cfg = app.config.read().unwrap_or_else(|e| e.into_inner());
        limits_from(&cfg)
    };
    capture_from(&mut clipboard, &limits)
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn capture_from(clipboard: &mut Clipboard, limits: &crate::classify::Limits) -> Result<Captured> {
    let plain = match clipboard.get_text() {
        Ok(text) => Some(text.into_bytes()),
        Err(ClipboardError::ContentNotAvailable) => None,
        Err(err) => return Err(err).context("读取 Unicode 文本"),
    };
    let png = match clipboard.get_image() {
        Ok(image) => Some(encode_png(image.width, image.height, &image.bytes)?),
        Err(ClipboardError::ContentNotAvailable) => None,
        Err(err) => return Err(err).context("读取图片"),
    };
    if plain.is_none() && png.is_none() {
        return Ok(empty_capture());
    }

    let mut input = FetchInput {
        plain_total: plain.as_ref().map_or(0, |text| text.len() as u64),
        plain_mime: plain.as_ref().map(|_| "text/plain; charset=utf-8".into()),
        plain_sha256: plain.as_ref().map(|text| sha256_hex(text)),
        plain,
        ..FetchInput::default()
    };
    if let Some(text) = &input.plain {
        input.target_names.push("text/plain".into());
        if text.is_empty() && png.is_none() {
            return Ok(empty_capture());
        }
    }
    if let Some(png) = png {
        input.target_names.push("image/png".into());
        input.image_mime = Some("image/png".into());
        input.image_size = png.len() as u64;
        input.image_header = png.iter().copied().take(64).collect();
        input.image_sha256 = Some(sha256_hex(&png));
    }
    Ok(assemble(&input, limits))
}

fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>> {
    let width = u32::try_from(width).context("图片宽度超出 PNG 上限")?;
    let height = u32::try_from(height).context("图片高度超出 PNG 上限")?;
    if width == 0 || height == 0 {
        bail!("系统返回了空图片");
    }
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .context("图片尺寸溢出")?;
    if rgba.len() != expected {
        bail!("RGBA 数据长度 {} 与尺寸预期 {expected} 不符", rgba.len());
    }
    let mut output = Cursor::new(Vec::new());
    {
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().context("创建 PNG 编码器")?;
        writer.write_image_data(rgba).context("编码剪贴板图片")?;
        writer.finish().context("完成 PNG 编码")?;
    }
    Ok(output.into_inner())
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use objc2_app_kit::NSPasteboard;

    pub(super) fn run(app: Arc<App>) {
        let pasteboard = NSPasteboard::generalPasteboard();
        let mut last_count = pasteboard.changeCount();
        let mut was_enabled = false;
        let mut last_hash = None;
        app.changes_ok.store(true, Ordering::Relaxed);
        log_info("macOS 剪贴板监听已启动（NSPasteboard.changeCount）");
        while !app.shutdown.load(Ordering::Relaxed) {
            let count = pasteboard.changeCount();
            let changed = count != last_count;
            last_count = count;
            let enabled = app.enabled.load(Ordering::Relaxed);
            if enabled && (!was_enabled || changed) {
                capture_and_record(&app, &mut last_hash);
            }
            was_enabled = enabled;
            thread::sleep(Duration::from_millis(200));
        }
        app.changes_ok.store(false, Ordering::Relaxed);
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::System::DataExchange::{
        AddClipboardFormatListener, RemoveClipboardFormatListener,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, DispatchMessageW, PeekMessageW, TranslateMessage,
        HWND_MESSAGE, MSG, PM_REMOVE, WM_CLIPBOARDUPDATE,
    };

    struct ListenerWindow(HWND);
    impl Drop for ListenerWindow {
        fn drop(&mut self) {
            unsafe {
                RemoveClipboardFormatListener(self.0);
                DestroyWindow(self.0);
            }
        }
    }

    pub(super) fn run(app: Arc<App>) {
        let window = match create_listener() {
            Ok(window) => window,
            Err(err) => {
                let message = format!("Windows 剪贴板监听初始化失败: {err:#}");
                log_warn(&message);
                app.note_error(&message);
                return;
            }
        };
        app.changes_ok.store(true, Ordering::Relaxed);
        log_info("Windows 剪贴板监听已启动（WM_CLIPBOARDUPDATE）");
        let mut was_enabled = false;
        let mut last_hash = None;
        while !app.shutdown.load(Ordering::Relaxed) {
            let enabled = app.enabled.load(Ordering::Relaxed);
            if enabled && !was_enabled {
                capture_and_record(&app, &mut last_hash);
            }
            unsafe {
                let mut msg: MSG = std::mem::zeroed();
                while PeekMessageW(&mut msg, window.0, 0, 0, PM_REMOVE) != 0 {
                    if msg.message == WM_CLIPBOARDUPDATE && app.enabled.load(Ordering::Relaxed) {
                        // 逐条处理通知，避免把一轮消息队列里的多次复制合并成最后一次。
                        capture_and_record(&app, &mut last_hash);
                    }
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            was_enabled = enabled;
            thread::sleep(Duration::from_millis(50));
        }
        drop(window);
        app.changes_ok.store(false, Ordering::Relaxed);
    }

    fn create_listener() -> Result<ListenerWindow> {
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        let title: Vec<u16> = "clipboard-monitor\0".encode_utf16().collect();
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            return Err(std::io::Error::last_os_error()).context("创建 message-only 窗口");
        }
        let window = ListenerWindow(hwnd);
        if unsafe { AddClipboardFormatListener(hwnd) } == 0 {
            return Err(std::io::Error::last_os_error()).context("AddClipboardFormatListener");
        }
        Ok(window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_is_encoded_as_png_with_full_digest() {
        let rgba = [
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let bytes = encode_png(2, 2, &rgba).expect("encode PNG");
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(bytes[24], 8);
        assert_ne!(sha256_hex(&bytes), sha256_hex(&bytes[..64]));
    }

    #[test]
    fn invalid_rgba_length_is_rejected() {
        assert!(encode_png(2, 2, &[0; 3]).is_err());
    }
}
