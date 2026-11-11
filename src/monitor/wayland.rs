//! Linux/Wayland 后端：通过 `ext-data-control-v1` 监听剪贴板选择区变化。
//!
//! 与 X11 后端的差异（能力降级是协议层面的，不是实现缺失）：
//! - ext-data-control 只通知"选择区换了新 offer"，**不提供读取/粘贴审计**，
//!   因此 Wayland 后端不产生 `op="read"` 事件，`SUPPORTS_READS` 语义由
//!   [`active_backend`] 动态决定。
//! - 没有窗口/进程归属协议，来源进程信息留空（与 Windows/macOS 一致）。
//! - 内容通过 offer.receive 的管道主动拉取，格式清单来自 offer.offer 事件。
//!
//! 本协议是"特权客户端"协议：compositor 可能拒绝绑定（例如 GNOME 只允许
//! 它内置的剪贴板管理器）。绑定失败时 [`probe`] 返回 false，由上层回退到
//! X11/XWayland 后端，不会让进程启动失败。
//!
//! 本机 X11 会话无 Wayland compositor，因此这里只能验证编译与纯逻辑单测；
//! 真实行为需在 Wayland 会话（KDE/GNOME/sway 等）实测。

use crate::classify::{assemble, empty_capture, Captured, FetchInput, Limits};
use crate::monitor::observation::{classify_event, ClipboardObservation, EventSpec};
use crate::monitor::{limits_from, App};
use crate::{log_debug, log_info, log_warn, now_ms};
use anyhow::{Context, Result};
use std::fs;
use std::io::Read;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_device_v1::ExtDataControlDeviceV1;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1::ExtDataControlManagerV1;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_offer_v1::ExtDataControlOfferV1;

/// 一次读取上限，避免恶意/异常 offer 无限写入。
const MAX_OFFER_BYTES: usize = 8 * 1024 * 1024;
/// 单次读取超时（靠非阻塞 + 轮询实现，避免永久卡住事件线程）。
const READ_TIMEOUT: Duration = Duration::from_secs(3);

/// 当前是否应使用 Wayland 后端（由 probe 在启动时写入）。
static WAYLAND_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn is_active() -> bool {
    WAYLAND_ACTIVE.load(Ordering::Relaxed)
}

/// 探测 Wayland 会话与 ext-data-control 全局是否可用。
/// 不修改任何系统状态；失败一律返回 false，由上层回退 X11。
pub fn probe() -> bool {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return false;
    }
    let Ok(conn) = Connection::connect_to_env() else {
        log_debug("Wayland 会话存在但无法连接，回退 X11");
        return false;
    };
    let Ok((globals, mut queue)) = registry_queue_init::<ProbeState>(&conn) else {
        return false;
    };
    let _ = queue.roundtrip(&mut ProbeState);
    let has_manager = globals.contents().with_list(|list| {
        list.iter()
            .any(|g| g.interface == ExtDataControlManagerV1::interface().name)
    });
    let has_seat = globals
        .contents()
        .with_list(|list| list.iter().any(|g| g.interface == WlSeat::interface().name));
    if !has_manager {
        log_debug("compositor 未提供 ext_data_control_manager_v1，回退 X11");
    }
    has_manager && has_seat
}

/// 极简 registry 状态，仅用于 probe 的 roundtrip。
struct ProbeState;
impl Dispatch<WlRegistry, GlobalListContents> for ProbeState {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegistry,
        _event: <WlRegistry as Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

/// 后端主状态：持有当前 offer 的格式清单与对象。
struct WaylandState {
    app: Arc<App>,
    manager: Option<ExtDataControlManagerV1>,
    device: Option<ExtDataControlDeviceV1>,
    pending_formats: Vec<String>,
    current_offer: Option<ExtDataControlOfferV1>,
    conn: Option<Connection>,
}

pub(super) fn spawn(app: Arc<App>) -> Result<Vec<JoinHandle<()>>> {
    let join = thread::Builder::new()
        .name("clip-wayland".into())
        .spawn(move || {
            WAYLAND_ACTIVE.store(true, Ordering::Relaxed);
            if let Err(err) = run(app) {
                log_warn(format!("Wayland 监听后端退出: {err:#}"));
                // 后端失效后回退标记，让 /api/status 如实显示未就绪。
                WAYLAND_ACTIVE.store(false, Ordering::Relaxed);
            }
        })?;
    Ok(vec![join])
}

fn run(app: Arc<App>) -> Result<()> {
    let conn = Connection::connect_to_env().context("连接 Wayland compositor")?;
    let (globals, mut queue) =
        registry_queue_init::<WaylandState>(&conn).context("枚举 Wayland 全局对象")?;
    let qh = queue.handle();
    let manager: ExtDataControlManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .context("绑定 ext_data_control_manager_v1（compositor 可能未授权）")?;
    let seat: WlSeat = globals.bind(&qh, 1..=1, ()).context("绑定 wl_seat")?;
    let device = manager.get_data_device(&seat, &qh, ());

    let mut state = WaylandState {
        app: Arc::clone(&app),
        manager: Some(manager),
        device: Some(device),
        pending_formats: Vec::new(),
        current_offer: None,
        conn: Some(conn.clone()),
    };
    // 首次 roundtrip 会收到初始 selection 事件（协议保证）。
    queue.roundtrip(&mut state)?;
    app.changes_ok.store(true, Ordering::Relaxed);
    app.clear_error();
    log_info("Wayland 剪贴板监听已启动（ext-data-control-v1）");

    while !app.shutdown.load(Ordering::Relaxed) {
        match queue.blocking_dispatch(&mut state) {
            Ok(_) => {}
            Err(err) => {
                log_warn(format!("Wayland 事件循环错误: {err}"));
                app.note_error(&format!("Wayland 事件循环错误: {err}"));
                thread::sleep(Duration::from_millis(500));
            }
        }
    }
    app.changes_ok.store(false, Ordering::Relaxed);
    if let Some(device) = state.device.take() {
        device.destroy();
    }
    if let Some(manager) = state.manager.take() {
        manager.destroy();
    }
    Ok(())
}

impl WaylandState {
    /// 收到新 selection：同步读取内容并落一条 write 事件。
    fn handle_selection(&mut self, offer: Option<ExtDataControlOfferV1>) {
        if !self.app.enabled.load(Ordering::Relaxed) {
            return;
        }
        let Some(offer) = offer else {
            self.record(empty_capture(), Vec::new(), true);
            self.current_offer = None;
            return;
        };
        let formats = std::mem::take(&mut self.pending_formats);
        let captured = self
            .conn
            .as_ref()
            .map(|c| capture_offer(&offer, &formats, &self.app, c))
            .unwrap_or_else(|| assemble(&FetchInput::default(), &Limits::default()));
        self.record(captured, formats, false);
        self.current_offer = Some(offer);
    }

    fn record(&self, captured: Captured, formats: Vec<String>, is_empty: bool) {
        let mut obs = ClipboardObservation::new(captured);
        obs.formats = formats;
        obs.is_empty = is_empty;
        obs.source_attribution = Some("wayland_ext_data_control".into());
        let spec: EventSpec = classify_event(None, &obs).unwrap_or(EventSpec {
            op: "write",
            reason: "content_change",
            kind: crate::monitor::observation::ChangeKind::Content,
            inferred: false,
            first: false,
        });
        if let Err(err) = self.app.record_wayland_change(&obs, &spec, now_ms()) {
            log_warn(format!("记录 Wayland 剪贴板变化失败: {err:#}"));
            self.app
                .note_error(&format!("记录 Wayland 剪贴板变化失败: {err:#}"));
        }
        // 强制 flush，确保 offer.receive 的请求已经被 compositor 处理。
        if let Some(conn) = &self.conn {
            let _ = conn.flush();
        }
    }
}

/// 通过 offer.receive 的管道读取文本/图片，转成共享 Captured。
fn capture_offer(
    offer: &ExtDataControlOfferV1,
    formats: &[String],
    app: &App,
    conn: &Connection,
) -> Captured {
    let limits = {
        let cfg = app.config.read().unwrap_or_else(|e| e.into_inner());
        limits_from(&cfg)
    };
    let text_mime = formats
        .iter()
        .find(|m| {
            matches!(
                m.as_str(),
                "text/plain" | "text/plain;charset=utf-8" | "UTF8_STRING" | "STRING" | "TEXT"
            )
        })
        .cloned();
    let image_mime = formats.iter().find(|m| m.starts_with("image/")).cloned();

    let mut input = FetchInput {
        target_names: formats.to_vec(),
        ..FetchInput::default()
    };
    if let Some(mime) = text_mime {
        if let Some(bytes) = receive(offer, &mime, conn) {
            input.plain_total = bytes.len() as u64;
            input.plain_sha256 = Some(crate::classify::sha256_hex(&bytes));
            input.plain_mime = Some(mime.clone());
            input.plain = Some(bytes);
        }
    }
    if let Some(mime) = image_mime {
        if let Some(bytes) = receive(offer, &mime, conn) {
            input.image_mime = Some(mime.clone());
            input.image_size = bytes.len() as u64;
            input.image_header = bytes.into_iter().take(64 * 1024).collect();
        }
    }
    assemble(&input, &limits)
}

/// 用 pipe 向 source 请求指定 MIME 数据，读到 EOF 或超时/上限。
fn receive(offer: &ExtDataControlOfferV1, mime: &str, conn: &Connection) -> Option<Vec<u8>> {
    // 用 libc pipe（项目已有 cfg(unix) 的 libc 依赖），把写端交给 compositor，
    // 读端设非阻塞后轮询，避免恶意 source 不写数据导致事件线程永久卡住。
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return None;
    }
    let read_fd = fds[0];
    let write_fd = fds[1];
    let owned: OwnedFd = unsafe { OwnedFd::from_raw_fd(write_fd) };
    offer.receive(mime.to_string(), owned.as_fd());
    // 必须立刻 flush：请求只排队不会自动发出，compositor 收不到就不会
    // 往管道写数据。否则读端只会超时拿到空内容（真实 bug）。
    if conn.flush().is_err() {
        drop(owned);
        return None;
    }
    // 请求已随 socket 消息发出（fd 已复制给对端），本地写端必须关闭，
    // source 写完并关闭对端 fd 后读端才会看到 EOF。
    drop(owned);
    let flags = unsafe { libc::fcntl(read_fd, libc::F_GETFL) };
    if flags >= 0 {
        unsafe { libc::fcntl(read_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    }
    let mut file = unsafe { fs::File::from_raw_fd(read_fd) };
    let mut out = Vec::new();
    let deadline = std::time::Instant::now() + READ_TIMEOUT;
    let mut buf = [0u8; 8192];
    loop {
        if out.len() >= MAX_OFFER_BYTES || std::time::Instant::now() >= deadline {
            break;
        }
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10))
            }
            Err(_) => break,
        }
    }
    (!out.is_empty()).then_some(out)
}

// ---------------------------------------------------------------- Dispatch impls

impl Dispatch<ExtDataControlManagerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtDataControlManagerV1,
        _event: <ExtDataControlManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for WaylandState {
    // data_offer(opcode 0) 与 primary_selection(opcode 3) 是携带 new_id 的
    // 事件，wayland-client 要求显式给出子对象的 user-data 构造器；缺失会
    // 在收到第一条 selection 时 panic。这里为 offer 对象绑定 `()`。
    event_created_child!(WaylandState, ExtDataControlDeviceV1, [
        0 => (ExtDataControlOfferV1, ()),
        3 => (ExtDataControlOfferV1, ()),
    ]);

    fn event(
        state: &mut Self,
        _proxy: &ExtDataControlDeviceV1,
        event: <ExtDataControlDeviceV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use wayland_protocols::ext::data_control::v1::client::ext_data_control_device_v1::Event;
        match event {
            Event::DataOffer { id } => {
                // 新 offer 的 offer 事件紧随其后，先清空待收集的格式。
                state.pending_formats.clear();
                state.current_offer = Some(id);
            }
            Event::Selection { id } => state.handle_selection(id),
            Event::PrimarySelection { id } => {
                // PRIMARY 默认关闭，与 X11 后端保持一致，避免选中文字刷屏。
                let monitor_primary = state
                    .app
                    .config
                    .read()
                    .map(|cfg| cfg.monitor_primary)
                    .unwrap_or(false);
                if monitor_primary {
                    state.handle_selection(id);
                } else if let Some(offer) = id {
                    offer.destroy();
                }
            }
            Event::Finished => {
                log_warn("Wayland data-control 设备已失效");
                state.app.note_error("Wayland data-control 设备已失效");
                state.app.changes_ok.store(false, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtDataControlOfferV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &ExtDataControlOfferV1,
        event: <ExtDataControlOfferV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use wayland_protocols::ext::data_control::v1::client::ext_data_control_offer_v1::Event;
        if let Event::Offer { mime_type } = event {
            state.pending_formats.push(mime_type);
        }
    }
}

impl Dispatch<WlSeat, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        _event: <WlSeat as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegistry,
        _event: <WlRegistry as Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{empty_capture, Limits};

    #[test]
    fn empty_offer_produces_empty_capture() {
        // capture_offer 需要真实 offer，这里验证等价的 assemble 路径。
        let cap = assemble(&FetchInput::default(), &Limits::default());
        assert_eq!(cap.content_type, "empty");
        assert_eq!(cap.sha256, empty_capture().sha256);
    }

    #[test]
    fn text_formats_are_recognized() {
        let mime = |s: &str| s.to_string();
        let formats = [
            mime("text/plain;charset=utf-8"),
            mime("image/png"),
            mime("text/html"),
        ];
        let text = formats.iter().find(|m| {
            matches!(
                m.as_str(),
                "text/plain" | "text/plain;charset=utf-8" | "UTF8_STRING" | "STRING" | "TEXT"
            )
        });
        assert_eq!(text.map(String::as_str), Some("text/plain;charset=utf-8"));
        assert!(formats.iter().any(|m| m.starts_with("image/")));
    }
}
