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
#[cfg(any(target_os = "windows", target_os = "macos"))]
use std::sync::atomic::Ordering;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use std::sync::Arc;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use std::thread::{self, JoinHandle};
#[cfg(any(target_os = "windows", target_os = "macos"))]
use std::time::{Duration, Instant};

/// 平台剪贴板解码出的原始像素；摘要直接对 RGBA 计算，不做二次编码。
#[cfg(any(target_os = "windows", target_os = "macos"))]
const NATIVE_IMAGE_MIME: &str = "image/x-rgba";

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
    let image = match clipboard.get_image() {
        Ok(image) => {
            let (width, height) = validate_rgba(image.width, image.height, &image.bytes)?;
            Some((width, height, image.bytes.into_owned()))
        }
        Err(ClipboardError::ContentNotAvailable) => None,
        Err(err) => return Err(err).context("读取图片"),
    };
    if plain.is_none() && image.is_none() {
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
        if text.is_empty() && image.is_none() {
            return Ok(empty_capture());
        }
    }
    if let Some((width, height, rgba)) = image {
        input.target_names.push(NATIVE_IMAGE_MIME.into());
        input.image_mime = Some(NATIVE_IMAGE_MIME.into());
        input.image_size = rgba.len() as u64;
        input.image_dims = Some((width, height));
        input.image_sha256 = Some(rgba_sha256(width, height, &rgba));
    }
    Ok(assemble(&input, limits))
}

/// 校验系统返回的 RGBA 尺寸与长度是否自洽。
fn validate_rgba(width: usize, height: usize, rgba: &[u8]) -> Result<(u32, u32)> {
    let width = u32::try_from(width).context("图片宽度超出上限")?;
    let height = u32::try_from(height).context("图片高度超出上限")?;
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
    Ok((width, height))
}

/// 宽高参与摘要，避免同字节不同尺寸被误判为同一内容。
fn rgba_sha256(width: u32, height: u32, rgba: &[u8]) -> String {
    let mut material = Vec::with_capacity(8 + rgba.len());
    material.extend_from_slice(&width.to_le_bytes());
    material.extend_from_slice(&height.to_le_bytes());
    material.extend_from_slice(rgba);
    sha256_hex(&material)
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    pub(super) fn run(app: Arc<App>) {
        app.changes_ok.store(true, Ordering::Relaxed);
        log_info("macOS 剪贴板监听已启动（内容摘要采样）");
        let mut was_enabled = false;
        let mut last_hash = None;
        let mut last_probe = Instant::now() - Duration::from_secs(1);
        while !app.shutdown.load(Ordering::Relaxed) {
            let enabled = app.enabled.load(Ordering::Relaxed);
            let overdue = last_probe.elapsed() >= Duration::from_secs(2);
            if enabled && (!was_enabled || overdue) {
                // NSPasteboard.changeCount 在部分无窗口会话中不递增，
                // 因而用低频内容摘要兜底；正常路径仍由变化计数驱动。
                capture_and_record(&app, &mut last_hash);
                last_probe = Instant::now();
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
    fn rgba_dims_are_validated_and_hashed() {
        let rgba = [255u8, 0, 0, 255, 0, 255, 0, 255];
        let (width, height) = validate_rgba(2, 1, &rgba).expect("合法尺寸");
        assert_eq!((width, height), (2, 1));
        assert!(validate_rgba(2, 1, &[0; 3]).is_err(), "长度不符应拒绝");
        assert!(validate_rgba(0, 1, &[]).is_err(), "零尺寸应拒绝");
        assert_ne!(
            rgba_sha256(2, 1, &rgba),
            rgba_sha256(1, 2, &rgba),
            "同字节不同尺寸应有不同摘要"
        );
    }
}
