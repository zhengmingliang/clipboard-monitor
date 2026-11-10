//! Windows 平台的剪贴板元数据与变更信号采集。
//!
//! OS 可可靠提供的内容：
//! - `GetClipboardSequenceNumber`：每次剪贴板内容/所有权变化都会递增，
//!   不需要打开剪贴板，是最廉价的变化预检（避免每轮全量解码大图）。
//! - `EnumClipboardFormats` + `GetClipboardFormatNameW`：当前可用格式清单。
//! - `GetClipboardOwner` → `GetWindowThreadProcessId` → 进程路径：写入者。
//! - `GetForegroundWindow` + `GetWindowTextW`：动作发生时的触发主体。
//!
//! OS **不能**可靠提供的是"谁读取/粘贴了内容"：Windows 没有全局读取通知。
//! 唯一能间接推断的场景是"序号没变却收到 WM_CLIPBOARDUPDATE"（延迟渲染
//! 的所有者因某个请求重新声明格式），这类事件会被标为 `inferred_read`，
//! 绝不声称是可靠的读取审计。

use crate::monitor::observation::{SourceInfo, TriggerInfo};
use std::path::PathBuf;
use windows_sys::Win32::Foundation::{CloseHandle, HWND};
use windows_sys::Win32::System::DataExchange::{GetClipboardOwner, GetClipboardSequenceNumber};
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
};

/// 当前剪贴板变更序号。返回 0 表示系统拒绝访问（此时调用方应回退内容采样）。
pub fn clipboard_sequence() -> u64 {
    // GetClipboardSequenceNumber 无需 OpenClipboard，且不会因为剪贴板被
    // 其它进程占用而失败；返回 0 才代表无权限/会话异常。
    u64::from(unsafe { GetClipboardSequenceNumber() })
}

/// 列出当前剪贴板提供的所有格式（标准名或注册的自定义名）。
///
/// 需要临时打开剪贴板；失败时返回空列表，调用方退化为"格式未知"而不是
/// 让整个抓取失败。`clipboard-win` 的 `Clipboard` guard 负责 RAII 关闭。
pub fn clipboard_formats() -> Vec<String> {
    let Ok(_guard) = clipboard_win::Clipboard::new_attempts(3) else {
        return Vec::new();
    };
    let mut names: Vec<String> = clipboard_win::raw::EnumFormats::new()
        .filter_map(clipboard_win::raw::format_name_big)
        .collect();
    names.sort();
    names.dedup();
    names
}

/// 剪贴板所有者窗口对应的进程信息（写入者）。
///
/// 注意：延迟渲染下 `GetClipboardOwner` 返回的是"声明所有权"的窗口，
/// 不一定是发起复制的线程；这是 OS 能给出的最可靠归属，不做过度推断。
pub fn clipboard_source() -> Option<SourceInfo> {
    let owner: HWND = unsafe { GetClipboardOwner() };
    if owner.is_null() {
        return None;
    }
    let window_id = owner as usize as u32;
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(owner, &mut pid) };
    if pid == 0 {
        return Some(SourceInfo {
            window_id,
            ..Default::default()
        });
    }
    let path = process_image_path(pid);
    let app_name = path
        .as_ref()
        .and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    Some(SourceInfo {
        pid: Some(pid as i32),
        app_name,
        exec_path: path
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        window_id,
    })
}

/// 前台窗口/应用（触发主体）。与内容生产者可能不同，单列以免混淆。
pub fn foreground_trigger() -> Option<TriggerInfo> {
    let hwnd: HWND = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return None;
    }
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    let mut title = [0u16; 256];
    let len = unsafe { GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32) };
    let window_title = if len > 0 {
        String::from_utf16_lossy(&title[..len as usize])
    } else {
        String::new()
    };
    let app_name = if pid == 0 {
        String::new()
    } else {
        process_image_path(pid)
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_default()
    };
    let info = TriggerInfo {
        pid: (pid != 0).then_some(pid as i32),
        app_name,
        window_title,
    };
    info.has_any().then_some(info)
}

/// 用 `PROCESS_QUERY_LIMITED_INFORMATION` 取进程完整路径。
/// 与 `main.rs` 的 `process_path` 是同一套 Win32 调用，这里单独保留一份，
/// 避免把 CLI 的进程管理逻辑耦合进监控后端。
fn process_image_path(pid: u32) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let mut buffer = vec![0u16; 1024];
    let mut len = buffer.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut len) };
    unsafe { CloseHandle(handle) };
    if ok == 0 || len == 0 {
        return None;
    }
    buffer.truncate(len as usize);
    Some(std::ffi::OsString::from_wide(&buffer).into())
}
