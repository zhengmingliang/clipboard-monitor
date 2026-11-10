//! Windows/macOS 的文本与图片抓取、元数据整理与平台原生变化检测。
//!
//! 与 X11 后端不同，这两个 OS 不推送剪贴板内容，也不提供全局读取拦截。
//! 因此这里用"变更序号快速预检 + 内容摘要采样兜底"的组合：
//! - 序号/计数变化 → 立即抓取；未变化 → 仍按兜底周期抓取，兼容无窗口会话。
//! - 每次抓取整理成平台无关的 [`ClipboardObservation`]，由
//!   [`observation::classify_event`] 决定事件语义（内容/清空/格式/序号/
//!   推断读取），再由 [`App::record_change`] 统一落库。
//!
//! 稳定性的几个要点：
//! - `changes_ok` 只在首次成功抓取后为 true；连续失败会置 false 并上报。
//! - Windows 监听窗口初始化失败会带退避重试，不会永久停摆。
//! - 单次抓取有超时保护，剪贴板被其它进程占用不会卡死整个后端线程。
//! - 去重命中也清 `last_error`，瞬时故障不会永久残留。

#[cfg(any(target_os = "windows", target_os = "macos"))]
use super::{limits_from, App};
use crate::classify::sha256_hex;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use crate::classify::{assemble, empty_capture, FetchInput};
#[cfg(any(target_os = "windows", target_os = "macos"))]
use crate::monitor::observation::{classify_event, ClipboardObservation, ObservationSnapshot};
#[cfg(any(target_os = "windows", target_os = "macos"))]
use crate::{log_debug, log_info, log_warn, now_ms};
use anyhow::{bail, Context, Result};
#[cfg(any(target_os = "windows", target_os = "macos"))]
use arboard::{Clipboard, Error as ClipboardError};
#[cfg(any(target_os = "windows", target_os = "macos"))]
use std::sync::atomic::Ordering;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use std::sync::{mpsc, Arc};
#[cfg(any(target_os = "windows", target_os = "macos"))]
use std::thread::{self, JoinHandle};
#[cfg(any(target_os = "windows", target_os = "macos"))]
use std::time::{Duration, Instant};

/// 平台剪贴板解码出的原始像素；摘要直接对 RGBA 计算，不做二次编码。
#[cfg(any(target_os = "windows", target_os = "macos"))]
const NATIVE_IMAGE_MIME: &str = "image/x-rgba";

/// 内容摘要兜底采样周期。变更序号不可用时（无窗口会话、权限受限），
/// 仍靠内容哈希发现变化；正常会话由序号变化即时触发，不受此周期限制。
#[cfg(any(target_os = "windows", target_os = "macos"))]
const FALLBACK_PROBE: Duration = Duration::from_secs(2);

/// 单次抓取的超时保护。arboard / 系统剪贴板偶尔会被其它进程长时间占用。
#[cfg(any(target_os = "windows", target_os = "macos"))]
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

/// 连续失败多少次后把 `changes_ready` 置为 false（可观测性）。
#[cfg(any(target_os = "windows", target_os = "macos"))]
const FAILURES_BEFORE_UNREADY: u32 = 3;

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

/// 一次抓取的结果（在工作线程里产生，通过 channel 送回后端线程）。
#[cfg(any(target_os = "windows", target_os = "macos"))]
type CaptureResult = Result<ClipboardObservation>;

/// 超时后旧抓取线程可能仍阻塞在系统调用里，用它防止并发堆积。
#[cfg(any(target_os = "windows", target_os = "macos"))]
static CAPTURE_IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 在独立线程里执行抓取，避免一次阻塞的剪贴板调用卡死后端循环。
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn capture_with_timeout(app: &Arc<App>, inferred_read: bool) -> CaptureResult {
    use std::sync::atomic::Ordering as AtomicOrdering;

    if CAPTURE_IN_FLIGHT.swap(true, AtomicOrdering::SeqCst) {
        // 上一轮抓取尚未结束（多半是超时残留），本轮直接放弃，不叠加线程。
        return Err(anyhow::anyhow!("上一轮剪贴板抓取尚未结束"));
    }
    let app = Arc::clone(app);
    let (tx, rx) = mpsc::channel();
    let worker = thread::Builder::new()
        .name("clip-capture".into())
        .spawn(move || {
            let result = capture_observation(&app, inferred_read);
            let _ = tx.send(result);
            // 无论成功失败都清标志；超时的旧线程最终结束也会清。
            CAPTURE_IN_FLIGHT.store(false, AtomicOrdering::SeqCst);
        });
    if let Err(err) = worker {
        CAPTURE_IN_FLIGHT.store(false, AtomicOrdering::SeqCst);
        return Err(anyhow::anyhow!("启动剪贴板抓取线程失败: {err}"));
    }
    match rx.recv_timeout(CAPTURE_TIMEOUT) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // 工作线程仍在阻塞，让它自行结束；不 join 是为了不把后端卡住。
            // 标志由工作线程结束时清理，期间后续轮次会走上面的"放弃"分支。
            Err(anyhow::anyhow!(
                "读取系统剪贴板超时（{} 秒）",
                CAPTURE_TIMEOUT.as_secs()
            ))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            // 线程 panic 时不会走到自己的清理逻辑，这里兜底复位。
            CAPTURE_IN_FLIGHT.store(false, AtomicOrdering::SeqCst);
            Err(anyhow::anyhow!("剪贴板抓取线程意外退出"))
        }
    }
}

/// 整理一次完整观察：内容 + 格式清单 + 变更序号 + 来源/触发主体。
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn capture_observation(app: &App, inferred_read: bool) -> CaptureResult {
    let mut clipboard = Clipboard::new().context("初始化系统剪贴板")?;
    let limits = {
        let cfg = app.config.read().unwrap_or_else(|e| e.into_inner());
        limits_from(&cfg)
    };
    let mut obs = capture_from(&mut clipboard, &limits)?;
    // arboard 的 Clipboard 在 Windows 上内部持有 OpenClipboard；先释放再
    // 调平台的格式枚举/所有者查询，避免同线程嵌套打开剪贴板失败。
    drop(clipboard);
    decorate_observation(&mut obs, inferred_read);
    Ok(obs)
}

/// 把 arboard 抓到的文本/图片整理成 `ClipboardObservation` 的内容部分。
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn capture_from(
    clipboard: &mut Clipboard,
    limits: &crate::classify::Limits,
) -> Result<ClipboardObservation> {
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
        let mut obs = ClipboardObservation::new(empty_capture());
        obs.is_empty = true;
        return Ok(obs);
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
            let mut obs = ClipboardObservation::new(empty_capture());
            obs.is_empty = true;
            return Ok(obs);
        }
    }
    if let Some((width, height, rgba)) = image {
        input.target_names.push(NATIVE_IMAGE_MIME.into());
        input.image_mime = Some(NATIVE_IMAGE_MIME.into());
        input.image_size = rgba.len() as u64;
        input.image_dims = Some((width, height));
        input.image_sha256 = Some(rgba_sha256(width, height, &rgba));
    }
    let captured = assemble(&input, limits);
    let mut obs = ClipboardObservation::new(captured);
    obs.formats = input.target_names.clone();
    Ok(obs)
}

/// 补齐平台相关的观察字段（格式清单、序号、来源、触发主体）。
/// 每个采集点独立降级：任一失败都不让整次抓取失败。
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn decorate_observation(obs: &mut ClipboardObservation, inferred_read: bool) {
    #[cfg(target_os = "windows")]
    {
        obs.sequence = Some(crate::monitor::win32::clipboard_sequence());
        let formats = crate::monitor::win32::clipboard_formats();
        if !formats.is_empty() {
            obs.formats = formats;
        }
        obs.source = crate::monitor::win32::clipboard_source();
        obs.trigger = crate::monitor::win32::foreground_trigger();
        // Windows 是唯一能间接推断读取的平台：序号没变却收到变更通知，
        // 说明某个客户端触发了延迟渲染。仅此一处允许置位。
        obs.inferred_read = inferred_read;
    }
    #[cfg(target_os = "macos")]
    {
        obs.sequence = Some(crate::monitor::macos::pasteboard_change_count());
        let formats = crate::monitor::macos::pasteboard_formats();
        if !formats.is_empty() {
            obs.formats = formats;
        }
        obs.source = crate::monitor::macos::foreground_app();
        obs.trigger = crate::monitor::macos::foreground_trigger();
        // macOS 没有任何读取信号，永不产生推断读取。
        obs.inferred_read = false;
        let _ = inferred_read;
    }
    if obs.formats.is_empty() && !obs.body.targets.is_empty() {
        obs.formats = obs.body.targets.clone();
    }
    obs.formats.sort();
    obs.formats.dedup();
}

/// 处理一次观察：判定语义 → 落库/去重 → 维护快照、就绪状态与计数器。
///
/// 返回 `true` 表示这一轮抓取成功（含"无变化"去重），调用方据此清错误。
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn process_observation(
    app: &App,
    obs: &ClipboardObservation,
    snapshot: &mut Option<ObservationSnapshot>,
) -> bool {
    let Some(spec) = classify_event(snapshot.as_ref(), obs) else {
        // 内容与格式、序号都没变：不落库，但仍算一次成功探测，清掉瞬时错误。
        log_debug("剪贴板内容与格式未变化，已去重");
        return true;
    };
    match app.record_change(obs, &spec, now_ms()) {
        Ok(_) => {
            let mut next = ObservationSnapshot::from_observation(obs);
            // 清掉暂停标记：本轮已成功记录过恢复后的状态。
            next.suspended = false;
            *snapshot = Some(next);
            true
        }
        Err(err) => {
            let message = format!("写入剪贴板日志失败: {err:#}");
            log_warn(&message);
            app.note_error(&message);
            false
        }
    }
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

/// 两个平台共用的主循环骨架。
///
/// `signal_changed` 返回本轮是否收到"序号/消息"层面的变化信号；`inferred`
/// 表示该信号是否可能来自读取触发的延迟渲染（仅 Windows 用）。
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn run_loop(
    app: Arc<App>,
    mut wait_for_signal: impl FnMut(Duration) -> bool,
    mut inferred_signal: impl FnMut() -> bool,
) {
    let mut snapshot: Option<ObservationSnapshot> = None;
    let mut last_probe = Instant::now() - FALLBACK_PROBE;
    let mut failures: u32 = 0;
    let mut was_enabled = false;

    loop {
        if app.shutdown.load(Ordering::Relaxed) {
            break;
        }
        let enabled = app.enabled.load(Ordering::Relaxed);
        // 启用沿：清空暂停标记，保证恢复后即使内容相同也留一条 reenabled。
        if !enabled {
            if was_enabled {
                if let Some(s) = snapshot.as_mut() {
                    s.suspended = true;
                }
                app.changes_ok.store(false, Ordering::Relaxed);
            }
            was_enabled = false;
            last_probe = Instant::now() - FALLBACK_PROBE;
            wait_for_signal(Duration::from_millis(200));
            continue;
        }
        let resumed = !was_enabled;
        was_enabled = true;

        let signal = wait_for_signal(Duration::from_millis(200));
        let overdue = last_probe.elapsed() >= FALLBACK_PROBE;
        if !signal && !overdue && !resumed {
            continue;
        }

        // 只有"收到消息/序号变化"这一轮才可能带推断读取信号；兜底采样
        // 轮不消费该标志，避免把上一轮的残留当成新一轮读取。
        let inferred = signal && inferred_signal();
        match capture_with_timeout(&app, inferred) {
            Ok(obs) => {
                last_probe = Instant::now();
                if process_observation(&app, &obs, &mut snapshot) {
                    failures = 0;
                    app.changes_ok.store(true, Ordering::Relaxed);
                    app.clear_error();
                } else {
                    failures += 1;
                }
            }
            Err(err) => {
                last_probe = Instant::now();
                failures += 1;
                let message = format!("读取系统剪贴板失败: {err:#}");
                log_warn(&message);
                app.note_error(&message);
            }
        }
        if failures >= FAILURES_BEFORE_UNREADY {
            app.changes_ok.store(false, Ordering::Relaxed);
        }
    }
    app.changes_ok.store(false, Ordering::Relaxed);
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    pub(super) fn run(app: Arc<App>) {
        log_info("macOS 剪贴板监听已启动（变更计数 + 内容摘要兜底）");
        // changeCount 快速路径：计数变化即视为有信号；为 0（无权限/无窗口
        // 会话）时一律交回内容摘要兜底，不阻断主循环。
        let mut last_count = crate::monitor::macos::pasteboard_change_count();
        run_loop(
            app,
            move |timeout| {
                thread::sleep(timeout);
                let count = crate::monitor::macos::pasteboard_change_count();
                let changed = count != 0 && count != last_count;
                last_count = count;
                changed
            },
            || false,
        );
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

    /// 创建监听窗口；失败时带退避重试，避免一次瞬时失败让后端永久停摆。
    fn create_with_retry(app: &App) -> Option<ListenerWindow> {
        let mut backoff = Duration::from_millis(200);
        loop {
            if app.shutdown.load(Ordering::Relaxed) {
                return None;
            }
            match create_listener() {
                Ok(window) => {
                    app.clear_error();
                    return Some(window);
                }
                Err(err) => {
                    let message = format!("Windows 剪贴板监听初始化失败，将重试: {err:#}");
                    log_warn(&message);
                    app.note_error(&message);
                    app.changes_ok.store(false, Ordering::Relaxed);
                    thread::sleep(backoff);
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                }
            }
        }
    }

    pub(super) fn run(app: Arc<App>) {
        // 初始化失败不再直接 return：重试直到成功或进程退出。
        let Some(window) = create_with_retry(&app) else {
            return;
        };
        let hwnd = window.0;
        log_info("Windows 剪贴板监听已启动（WM_CLIPBOARDUPDATE + 序号预检）");
        let mut last_sequence = crate::monitor::win32::clipboard_sequence();
        // 两个闭包共享同一个标志，避免 move 各自捕获一份副本导致推断信号丢失。
        let pending_notification = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let signal_flag = Arc::clone(&pending_notification);
        let read_flag = Arc::clone(&pending_notification);

        run_loop(
            Arc::clone(&app),
            move |timeout| {
                // 排空本窗口的消息队列，逐条消费 WM_CLIPBOARDUPDATE；同时用
                // 序号变化作为"内容确实变了"的廉价确认，避免对每条通知都
                // 解码大图。hwnd 过滤避免取走其它线程/窗口的消息。
                let mut notified = false;
                unsafe {
                    let mut msg: MSG = std::mem::zeroed();
                    let deadline = Instant::now() + timeout;
                    loop {
                        while PeekMessageW(&mut msg, hwnd, 0, 0, PM_REMOVE) != 0 {
                            if msg.message == WM_CLIPBOARDUPDATE {
                                notified = true;
                            }
                            TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                        if Instant::now() >= deadline {
                            break;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                }
                let sequence = crate::monitor::win32::clipboard_sequence();
                let seq_changed = sequence != 0 && sequence != last_sequence;
                // 序号没变但收到通知：可能是延迟渲染被某个读取请求触发。
                signal_flag.store(
                    notified && !seq_changed,
                    std::sync::atomic::Ordering::Relaxed,
                );
                last_sequence = sequence;
                notified || seq_changed
            },
            move || read_flag.swap(false, std::sync::atomic::Ordering::Relaxed),
        );
        // window 在此析构：移除监听并销毁窗口。
        drop(window);
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
