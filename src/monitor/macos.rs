//! macOS 平台的剪贴板元数据与变更信号采集。
//!
//! OS 可可靠提供的内容：
//! - `NSPasteboard.general.changeCount`：内容变化时递增，不需要读正文，
//!   是正常窗口会话里的廉价变化预检。
//! - `NSPasteboard.types`：当前可用格式清单（UTI / pasteboard type）。
//! - `NSWorkspace.frontmostApplication`：前台应用的名称、bundle id、PID，
//!   再用 `proc_pidpath` 取可执行路径（与 `main.rs` 同一套 libc 调用）。
//!
//! 已知的 headless 限制：部分无窗口/SSH 会话里 `changeCount` 不递增，因此
//! 后端仍保留"内容摘要采样"作为兜底（详见 portable.rs）。`changeCount` 只
//! 作为快速路径，**不能**替代内容摘要。
//!
//! macOS 没有任何全局读取（粘贴）通知，本模块不产生推断读取事件。

use crate::monitor::observation::{SourceInfo, TriggerInfo};
use objc2::rc::Retained;
use objc2_app_kit::{NSPasteboard, NSRunningApplication, NSWorkspace};
use std::path::PathBuf;

/// 通用剪贴板的变更计数；会话异常时返回 0，调用方应回退内容摘要采样。
pub fn pasteboard_change_count() -> u64 {
    let board = NSPasteboard::generalPasteboard();
    let count = board.changeCount();
    u64::try_from(count).unwrap_or(0)
}

/// 当前剪贴板提供的格式清单（已排序去重）。
pub fn pasteboard_formats() -> Vec<String> {
    let board = NSPasteboard::generalPasteboard();
    let Some(types) = board.types() else {
        return Vec::new();
    };
    let count = types.count();
    let mut names = Vec::with_capacity(count);
    for index in 0..count {
        let item = types.objectAtIndex(index);
        names.push(item.to_string());
    }
    names.sort();
    names.dedup();
    names
}

/// 前台应用（触发主体）。macOS 没有"剪贴板所有者"概念，能可靠给出的
/// 只有当前前台应用，因此来源与触发主体共用这份信息，不做过度归因。
pub fn foreground_app() -> Option<SourceInfo> {
    let workspace = NSWorkspace::sharedWorkspace();
    let app: Retained<NSRunningApplication> = workspace.frontmostApplication()?;
    let pid = app.processIdentifier();
    if pid <= 0 {
        return None;
    }
    let app_name = app
        .localizedName()
        .map(|name| name.to_string())
        .or_else(|| app.bundleIdentifier().map(|id| id.to_string()))
        .unwrap_or_default();
    let exec_path = process_image_path(pid as u32)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    Some(SourceInfo {
        pid: Some(pid),
        app_name,
        exec_path,
        window_id: 0, // AppKit 不暴露窗口句柄，显式留 0
    })
}

/// 触发主体与来源在 macOS 上同源，额外保留 bundle/窗口标题语义的接口。
pub fn foreground_trigger() -> Option<TriggerInfo> {
    let source = foreground_app()?;
    let workspace = NSWorkspace::sharedWorkspace();
    let app = workspace.frontmostApplication()?;
    let bundle = app
        .bundleIdentifier()
        .map(|id| id.to_string())
        .unwrap_or_default();
    let info = TriggerInfo {
        pid: source.pid,
        app_name: if bundle.is_empty() {
            source.app_name.clone()
        } else {
            bundle
        },
        window_title: String::new(), // 无窗口会话拿不到标题，显式留空
    };
    info.has_any().then_some(info)
}

/// `proc_pidpath` 取完整可执行路径，与 `main.rs` 的进程管理逻辑同源。
fn process_image_path(pid: u32) -> Option<PathBuf> {
    let mut buffer = vec![0u8; 4096];
    let len =
        unsafe { libc::proc_pidpath(pid as i32, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if len <= 0 {
        return None;
    }
    buffer.truncate(len as usize);
    Some(PathBuf::from(String::from_utf8_lossy(&buffer).into_owned()))
}
