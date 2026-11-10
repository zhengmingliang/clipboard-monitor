//! Windows 读取（粘贴）信号的尽力采集。
//!
//! ## 为什么是"尽力"
//! Win32 只在剪贴板**内容变化**时广播 `WM_CLIPBOARDUPDATE` / 递增序号；
//! 读取粘贴是被动的 `OpenClipboard` + `GetClipboardData`，不产生任何系统
//! 通知，序号也不变。微软明确说明剪贴板监听器"仅用于检测内容变化"
//! （<https://learn.microsoft.com/en-us/windows/win32/dataxchg/using-the-clipboard>）。
//! 因此本模块只做两种间接信号的交叉验证，结果一律标为推断读取：
//!
//! 1. `GetOpenClipboardWindow()` 高频轮询：任何读取/粘贴都必须先打开
//!    剪贴板。捕获到别的窗口持有剪贴板、且期间剪贴板序号没变（说明不是
//!    写入）时，视为一次疑似读取。
//! 2. `WH_KEYBOARD_LL` 低级键盘钩子捕获 Ctrl+V：作为独立佐证，并补足
//!    OpenClipboard 轮询可能漏掉的极短读取。
//!
//! 两者都取不到时（例如菜单粘贴 + 极短读取、或权限受限），不会产生信号。
//! 这**不是**可靠的全局读取审计，事件始终带 `reason=inferred_read` 与
//! `meta_json.change.inferred=true`，界面显示"推断读取"。

pub use crate::monitor::observation::ReadSignal;
use crate::monitor::observation::{SourceInfo, TriggerInfo};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::DataExchange::{
    GetClipboardSequenceNumber, GetOpenClipboardWindow,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_LCONTROL, VK_RCONTROL, VK_V,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetWindowThreadProcessId, PeekMessageW, SetWindowsHookExW,
    TranslateMessage, UnhookWindowsHookEx, HC_ACTION, HHOOK, HOOKPROC, KBDLLHOOKSTRUCT, MSG,
    PM_REMOVE, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
};

/// 轮询间隔。读取时 `OpenClipboard` 通常只持续几毫秒，100ms 已属尽力而为，
/// 更短会明显抬高 CPU 占用。
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 最近一次 Ctrl+V 的毫秒时间戳；0 表示没有。
static LAST_CTRL_V_MS: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn ctrl_pressed() -> bool {
    unsafe {
        (GetAsyncKeyState(VK_CONTROL as i32) < 0)
            || (GetAsyncKeyState(VK_LCONTROL as i32) < 0)
            || (GetAsyncKeyState(VK_RCONTROL as i32) < 0)
    }
}

/// 低级键盘钩子回调：只写原子变量，绝不在这里做任何耗时操作。
unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && (wparam as u32 == WM_KEYDOWN || wparam as u32 == WM_SYSKEYDOWN) {
        let kb = &*(lparam as *const KBDLLHOOKSTRUCT);
        if kb.vkCode == VK_V as u32 && ctrl_pressed() {
            LAST_CTRL_V_MS.store(now_ms(), Ordering::Relaxed);
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}

/// 钩子 RAII：卸载失败也不阻塞进程退出。
struct HookGuard(HHOOK);
impl Drop for HookGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { UnhookWindowsHookEx(self.0) };
        }
    }
}

/// 启动读取信号采集线程，返回接收端与线程句柄。
///
/// `shutdown` 由调用方在退出时置位；线程每轮检查，不依赖 join 超时。
pub fn spawn(
    shutdown: Arc<std::sync::atomic::AtomicBool>,
) -> (Receiver<ReadSignal>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let handle = thread::Builder::new()
        .name("clip-read-signal".into())
        .spawn(move || run(shutdown, tx))
        .expect("启动 Windows 读取信号线程");
    (rx, handle)
}

fn run(shutdown: Arc<std::sync::atomic::AtomicBool>, tx: Sender<ReadSignal>) {
    // WH_KEYBOARD_LL 的回调在安装它的线程上执行，需要该线程有消息泵。
    // 这里用 PeekMessageW 维持一个最小消息循环；钩子安装失败不阻断
    // OpenClipboard 轮询（仍能捕获大部分读取）。
    let hook = install_hook();
    let _hook = HookGuard(hook.unwrap_or(std::ptr::null_mut()));

    let self_pid = std::process::id();
    // 上一轮剪贴板是否处于"被其它进程持有"状态。用边沿检测而不是"句柄
    // 变化"检测：同一窗口连续两次读取时句柄不变，仍应各记一次。
    let mut was_held_by_other = false;
    let mut last_sequence = clipboard_sequence();
    let mut last_ctrl_v = 0u64;
    let mut last_signal_ms = 0u64;
    const DEDUP_WINDOW_MS: u64 = 300;

    while !shutdown.load(Ordering::Relaxed) {
        pump_messages();
        let sequence = clipboard_sequence();
        let open_hwnd = unsafe { GetOpenClipboardWindow() };
        let ctrl_v_ms = LAST_CTRL_V_MS.load(Ordering::Relaxed);
        let wall = now_ms();

        let held_by_other = !open_hwnd.is_null() && !belongs_to_self(open_hwnd, self_pid);
        // 序号没变才可能是读取；序号变了说明是写入，交给主循环处理。
        let stable_clipboard = sequence == last_sequence;
        let open_is_read = held_by_other && !was_held_by_other && stable_clipboard;
        let paste_key = ctrl_v_ms > last_ctrl_v;
        let dedup_ok = wall.saturating_sub(last_signal_ms) >= DEDUP_WINDOW_MS;

        if (open_is_read || paste_key) && dedup_ok {
            let evidence = match (open_is_read, paste_key) {
                (true, true) => "both",
                (true, false) => "open_clipboard",
                (false, true) => "ctrl_v",
                (false, false) => unreachable!(),
            };
            let signal = ReadSignal {
                evidence,
                reader: if held_by_other {
                    window_source(open_hwnd, self_pid)
                } else {
                    // 纯 Ctrl+V 信号也尝试用前台窗口作为读取方上下文。
                    crate::monitor::win32::foreground_source().0
                },
                trigger: foreground_trigger(),
            };
            if tx.send(signal).is_err() {
                break; // 接收端已关闭
            }
            last_signal_ms = wall;
            last_ctrl_v = ctrl_v_ms;
        }
        was_held_by_other = held_by_other;
        last_sequence = sequence;
        thread::sleep(POLL_INTERVAL);
    }
}

fn install_hook() -> Option<HHOOK> {
    let hmod = unsafe { GetModuleHandleW(std::ptr::null()) };
    let proc: HOOKPROC = Some(keyboard_hook);
    let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, proc, hmod, 0) };
    (!hook.is_null()).then_some(hook)
}

fn pump_messages() {
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn clipboard_sequence() -> u64 {
    u64::from(unsafe { GetClipboardSequenceNumber() })
}

fn belongs_to_self(hwnd: HWND, self_pid: u32) -> bool {
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    pid != 0 && pid == self_pid
}

/// 打开剪贴板的窗口对应进程（读取方）。
fn window_source(hwnd: HWND, self_pid: u32) -> Option<SourceInfo> {
    if hwnd.is_null() {
        return None;
    }
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid == 0 || pid == self_pid {
        return None;
    }
    let path = crate::monitor::win32::process_image_path(pid);
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
        window_id: hwnd as usize as u32,
    })
}

/// Ctrl+V 时的前台应用（触发主体）。
fn foreground_trigger() -> Option<TriggerInfo> {
    let (source, _) = crate::monitor::win32::foreground_source();
    source.map(|s| TriggerInfo {
        pid: s.pid,
        app_name: s.app_name,
        window_title: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_signal_evidence_labels_are_stable() {
        // 纯函数不依赖 Windows 运行时，用结构体本身验证契约字段。
        let sig = ReadSignal {
            evidence: "both",
            reader: Some(SourceInfo {
                pid: Some(42),
                app_name: "notepad".into(),
                exec_path: "C:\\notepad.exe".into(),
                window_id: 7,
            }),
            trigger: Some(TriggerInfo {
                pid: Some(42),
                app_name: "notepad".into(),
                window_title: String::new(),
            }),
        };
        assert_eq!(sig.evidence, "both");
        assert_eq!(sig.reader.as_ref().unwrap().pid, Some(42));
        assert!(sig.trigger.as_ref().unwrap().has_any());
    }
}
