//! X11 连接、窗口和原子的小工具。协议实现使用纯 Rust 的 x11rb，不依赖 libX11。

use anyhow::{bail, Context, Result};
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;

pub type XConn = RustConnection;

#[derive(Clone, Copy, Debug)]
pub struct ClientRange {
    pub base: u32,
    pub mask: u32,
}

pub fn connect_x() -> Result<(XConn, usize)> {
    let display = std::env::var("DISPLAY").unwrap_or_default();
    if display.is_empty() {
        bail!(
            "未设置 DISPLAY。这个工具监控 X11 剪贴板，Wayland 会话请在 XWayland 或 X11 会话中运行"
        );
    }
    x11rb::connect(None).with_context(|| format!("连接 X11 {display} 失败"))
}

pub fn client_range(conn: &XConn) -> ClientRange {
    let setup = conn.setup();
    ClientRange {
        base: setup.resource_id_base,
        mask: setup.resource_id_mask,
    }
}

pub fn raw_fd(conn: &XConn) -> i32 {
    conn.stream().as_raw_fd()
}

pub fn wait_readable(fd: i32, timeout_ms: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms.max(0)) };
    rc > 0
}

pub fn intern(conn: &XConn, name: &str) -> Result<Atom> {
    Ok(conn
        .intern_atom(false, name.as_bytes())?
        .reply()
        .with_context(|| format!("intern {name}"))?
        .atom)
}

pub fn create_helper_window(conn: &XConn, screen: usize) -> Result<Window> {
    let root = conn
        .setup()
        .roots
        .get(screen)
        .context("无效的 screen")?
        .root;
    let window = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        root,
        -1,
        -1,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?
    .check()?;
    conn.flush()?;
    Ok(window)
}

pub fn atom_eq_primary() -> Atom {
    AtomEnum::PRIMARY.into()
}

pub fn poll_event_timeout(
    conn: &XConn,
    fd: i32,
    deadline: Instant,
) -> Result<Option<x11rb::protocol::Event>> {
    loop {
        if let Some(event) = conn.poll_for_event()? {
            return Ok(Some(event));
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        let left = deadline.saturating_duration_since(now);
        let ms = left.as_millis().min(1_000) as i32;
        if !wait_readable(fd, ms.max(1)) && Instant::now() >= deadline {
            return Ok(None);
        }
    }
}

pub fn remaining_ms(deadline: Instant) -> i32 {
    let left = deadline.saturating_duration_since(Instant::now());
    left.as_millis().min(i32::MAX as u128) as i32
}

pub fn timeout_after(ms: u64) -> Instant {
    Instant::now() + Duration::from_millis(ms)
}
