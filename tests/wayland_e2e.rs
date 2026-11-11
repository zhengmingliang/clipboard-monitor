//! Wayland 端到端：`ext-data-control-v1` 的真实 selection/offer/receive 传输。
//!
//! 这个测试在进程内启动一个**最小 Wayland compositor**（wayland-server +
//! 同一份 ext-data-control-v1 协议的服务端绑定），通过真实 UNIX socket 与
//! 真实的 `clipboard-monitor serve` 进程通信：
//!
//!   compositor 发布 manager/seat 全局
//!     → 客户端绑定并 get_data_device
//!     → compositor 发 data_offer + offer("text/plain;charset=utf-8") + selection
//!     → 客户端对 offer 发 receive(mime, pipe_fd)
//!     → compositor 把 token 写进管道
//!     → 客户端 classify::assemble 落库
//!     → 测试通过 /api/events 断言 body == token
//!
//! 这验证了协议链路本身（而不是 mocks 或编译检查），不需要 cage/weston、
//! 不需要 sudo、不需要图形会话。仅 Linux 编译运行，默认 #[ignore]，由 CI
//! 的 `CLIPBOARD_MONITOR_WAYLAND_LIVE=1` 单独一步触发。

#![cfg(target_os = "linux")]

use std::fs;
use std::io::{Read, Write};

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use wayland_protocols::ext::data_control::v1::server::{
    ext_data_control_device_v1::ExtDataControlDeviceV1,
    ext_data_control_manager_v1::{ExtDataControlManagerV1, Request as ManagerRequest},
    ext_data_control_offer_v1::{ExtDataControlOfferV1, Request as OfferRequest},
};
use wayland_server::backend::ClientData;
use wayland_server::protocol::wl_seat::{Request as SeatRequest, WlSeat};
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, ListeningSocket, New,
    Resource,
};

#[derive(Debug)]
struct NoClientData;
impl ClientData for NoClientData {}

const TOKEN: &str = "WAYLAND-E2E-TOKEN-中文-42";

struct CompositorState {
    /// 客户端 get_data_device 时创建好的 device（用于主动推送 selection）。
    device: Option<ExtDataControlDeviceV1>,
    /// 已经推送过 selection，避免重复推送。
    sent: bool,
    /// 是否收到过 receive 请求（供测试断言真实传输）。
    received_tx: mpsc::Sender<String>,
}

impl Dispatch<ExtDataControlManagerV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ExtDataControlManagerV1,
        request: ManagerRequest,
        _data: &(),
        dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let ManagerRequest::GetDataDevice { id, .. } = request {
            let device = data_init.init(id, ());
            state.device = Some(device);
            // 绑定后立即推送一次 selection（协议允许，且能缩短测试时间）。
            state.push_selection(dhandle, data_init);
        }
    }
}

impl CompositorState {
    fn push_selection(&mut self, dhandle: &DisplayHandle, data_init: &mut DataInit<'_, Self>) {
        if self.sent {
            return;
        }
        let Some(device) = self.device.clone() else {
            return;
        };
        // 新建 offer 对象；client 侧会收到 data_offer 事件。
        let client = device.client().expect("device client");
        let offer = client
            .create_resource::<ExtDataControlOfferV1, _, CompositorState>(
                dhandle,
                device.version(),
                (),
            )
            .expect("create offer");
        device.data_offer(&offer);
        offer.offer("text/plain;charset=utf-8".into());
        device.selection(Some(&offer));
        self.sent = true;
        let _ = data_init;
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for CompositorState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ExtDataControlDeviceV1,
        _request: <ExtDataControlDeviceV1 as Resource>::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlOfferV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ExtDataControlOfferV1,
        request: OfferRequest,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let OfferRequest::Receive { mime_type, fd } = request {
            // 把 token 写进客户端给的管道写端；客户端读端会在 EOF 后结束。
            // fd 是 BorrowedFd；转成 OwnedFd 后交给 File，只 close 一次。
            let mut file = fs::File::from(fd);
            let _ = file.write_all(TOKEN.as_bytes());
            drop(file);
            let _ = state.received_tx.send(mime_type);
        }
    }
}

impl Dispatch<WlSeat, ()> for CompositorState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WlSeat,
        _request: SeatRequest,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl GlobalDispatch<ExtDataControlManagerV1, ()> for CompositorState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ExtDataControlManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl GlobalDispatch<WlSeat, ()> for CompositorState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WlSeat>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn http_get(addr: &str, path: &str) -> Option<String> {
    let mut stream = std::net::TcpStream::connect(addr).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    response.split_once("\r\n\r\n").map(|p| p.1.to_string())
}

#[test]
#[ignore]
fn wayland_ext_data_control_selection_offer_transfer() {
    if std::env::var("CLIPBOARD_MONITOR_WAYLAND_LIVE").as_deref() != Ok("1") {
        return;
    }
    let dir = std::env::temp_dir().join(format!(
        "clipmon-wayland-e2e-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    ));
    fs::create_dir_all(&dir).expect("create e2e dir");
    let socket_path: PathBuf = dir.join("wl.sock");

    let (received_tx, received_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (stop_tx, stop_rx) = mpsc::channel();
    let socket_for_thread = socket_path.clone();
    let server = std::thread::Builder::new()
        .name("fake-compositor".into())
        .spawn(move || {
            let socket =
                ListeningSocket::bind_absolute(socket_for_thread).expect("bind wayland socket");
            let mut display: Display<CompositorState> = Display::new().expect("new display");
            let handle = display.handle();
            handle.create_global::<CompositorState, ExtDataControlManagerV1, ()>(1, ());
            handle.create_global::<CompositorState, WlSeat, ()>(1, ());
            let mut state = CompositorState {
                device: None,
                sent: false,
                received_tx,
            };
            let _ = ready_tx.send(());
            // 事件循环：持续 dispatch + flush，直到测试进程把 socket 移除/结束。
            let deadline = Instant::now() + Duration::from_secs(60);
            while Instant::now() < deadline && stop_rx.try_recv().is_err() {
                // 必须显式 accept 新连接，否则 dispatch_clients 永远没有客户端。
                if let Ok(Some(stream)) = socket.accept() {
                    let mut handle = display.handle();
                    let _ = handle.insert_client(stream, std::sync::Arc::new(NoClientData));
                }
                let _ = display.dispatch_clients(&mut state);
                let _ = display.flush_clients();
                std::thread::sleep(Duration::from_millis(10));
            }
        })
        .expect("spawn compositor");
    ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("compositor 未在 10 秒内就绪");

    // 找空闲端口，启动真实监控进程，WAYLAND_DISPLAY 指向绝对 socket 路径。
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let listen = format!("127.0.0.1:{port}");
    let data_dir = dir.join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let exe = env!("CARGO_BIN_EXE_clipboard-monitor");
    let child = Command::new(exe)
        .args(["serve", "--data-dir"])
        .arg(&data_dir)
        .args(["--listen", &listen])
        .env("WAYLAND_DISPLAY", &socket_path)
        .env("XDG_RUNTIME_DIR", &dir)
        .env("CLIPBOARD_MONITOR_LOG", "debug")
        .stdout(Stdio::from(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("monitor.out"))
                .unwrap(),
        ))
        .stderr(Stdio::from(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("monitor.err"))
                .unwrap(),
        ))
        .spawn()
        .expect("spawn monitor");
    let mut child = ChildGuard(child);

    // 等后端就绪（probe 必须选中 Wayland 而不是回退 X11）。
    let ready = wait(Duration::from_secs(20), || {
        http_get(&listen, "/api/status")
            .and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok())
            .is_some_and(|v| v["backend"] == "wayland" && v["changes_ready"] == true)
    });
    assert!(
        ready,
        "Wayland 后端未就绪，日志: {} | out: {} | err: {}",
        fs::read_to_string(data_dir.join("monitor.log")).unwrap_or_default(),
        fs::read_to_string(dir.join("monitor.out")).unwrap_or_default(),
        fs::read_to_string(dir.join("monitor.err")).unwrap_or_default()
    );

    // 等真实 receive 请求发生（证明 offer.receive 管道传输真的跑了）。
    let mime = received_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("未收到 offer.receive 请求");
    assert_eq!(mime, "text/plain;charset=utf-8");

    // 等事件落库：body 必须等于通过管道传输的 token。
    let recorded = wait(Duration::from_secs(20), || {
        http_get(&listen, "/api/events?limit=20")
            .and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok())
            .is_some_and(|v| {
                v["events"].as_array().is_some_and(|arr| {
                    arr.iter().any(|e| {
                        e["content_type"] == "text"
                            && e["summary"].as_str().unwrap_or_default().contains(TOKEN)
                    })
                })
            })
    });
    assert!(
        recorded,
        "通过 Wayland 管道传输的内容未出现在事件中，日志: {}",
        fs::read_to_string(data_dir.join("monitor.log")).unwrap_or_default()
    );

    child.0.kill().ok();
    child.0.wait().ok();
    let _ = stop_tx.send(());
    let _ = server.join();
    let _ = fs::remove_dir_all(&dir);
}

fn wait<F: Fn() -> bool>(timeout: Duration, f: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}
