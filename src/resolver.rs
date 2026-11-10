//! 把窗口 ID 解析成进程号、应用名和可执行文件。优先用 XRes，再退回 `_NET_WM_PID`。

use crate::x11util::{intern, XConn};
use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};
use x11rb::connection::RequestConnection;
use x11rb::protocol::res::{ClientIdMask, ClientIdSpec, ConnectionExt as ResExt};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Window};

#[derive(Clone, Debug)]
pub struct AppInfo {
    pub pid: Option<i32>,
    pub app_name: String,
    pub wm_class: String,
    pub exec_path: String,
    pub window: u32,
}

impl AppInfo {
    pub fn unknown(window: u32) -> Self {
        let name = if window == 0 {
            "none".to_string()
        } else {
            format!("window-{window}")
        };
        Self {
            pid: None,
            app_name: name,
            wm_class: String::new(),
            exec_path: String::new(),
            window,
        }
    }
}

struct ProcHit {
    comm: String,
    exe: String,
    at: Instant,
}

pub struct Resolver {
    conn: XConn,
    net_wm_pid: u32,
    net_wm_name: u32,
    res_ok: bool,
    windows: HashMap<u32, AppInfo>,
    procs: HashMap<i32, ProcHit>,
    atoms: HashMap<u32, String>,
}

impl Resolver {
    pub fn new(conn: XConn) -> Result<Self> {
        let res_ok = conn
            .extension_information(x11rb::protocol::res::X11_EXTENSION_NAME)?
            .is_some();
        if res_ok {
            let _ = conn.res_query_version(1, 2)?.reply();
        }
        let net_wm_pid = intern(&conn, "_NET_WM_PID")?;
        let net_wm_name = intern(&conn, "_NET_WM_NAME")?;
        Ok(Self {
            conn,
            net_wm_pid,
            net_wm_name,
            res_ok,
            windows: HashMap::new(),
            procs: HashMap::new(),
            atoms: HashMap::new(),
        })
    }

    pub fn resolve_window(&mut self, window: Window) -> AppInfo {
        if window == 0 {
            return AppInfo::unknown(0);
        }
        if let Some(hit) = self.windows.get(&window) {
            return hit.clone();
        }
        let info = self
            .lookup_window(window)
            .unwrap_or_else(|_| AppInfo::unknown(window));
        if self.windows.len() > 1024 {
            self.windows.clear();
        }
        self.windows.insert(window, info.clone());
        info
    }

    /// 轻量探测连接是否仍然可用。失败时调用方应重建 Resolver。
    pub fn ping(&self) -> bool {
        self.conn
            .get_input_focus()
            .map(|cookie| cookie.reply().is_ok())
            .unwrap_or(false)
    }

    pub fn atom_name(&mut self, atom: u32) -> String {
        if atom == 0 {
            return "NONE".to_string();
        }
        if let Some(name) = self.atoms.get(&atom) {
            return name.clone();
        }
        let name = self
            .conn
            .get_atom_name(atom)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| String::from_utf8_lossy(&r.name).into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("ATOM_{atom}"));
        if self.atoms.len() > 2048 {
            self.atoms.clear();
        }
        self.atoms.insert(atom, name.clone());
        name
    }

    fn lookup_window(&mut self, window: Window) -> Result<AppInfo> {
        let mut current = window;
        let mut wm_class = String::new();
        let mut title = String::new();
        let mut pid = None;
        for _ in 0..10 {
            if pid.is_none() {
                pid = self.pid_of(current);
            }
            if wm_class.is_empty() {
                wm_class = self.wm_class(current);
            }
            if title.is_empty() {
                title = self.utf8_prop(current, self.net_wm_name);
            }
            if pid.is_some() && !wm_class.is_empty() {
                break;
            }
            let parent = self.conn.query_tree(current)?.reply()?.parent;
            if parent == 0 || parent == current {
                break;
            }
            current = parent;
        }
        let (comm, exe) = match pid {
            Some(pid) => self.proc_of(pid),
            None => (String::new(), String::new()),
        };
        let class_part = wm_class
            .split('/')
            .nth(1)
            .filter(|s| !s.is_empty())
            .unwrap_or("");
        let app_name = if !comm.is_empty() {
            comm
        } else if !class_part.is_empty() {
            class_part.to_string()
        } else if !wm_class.is_empty() {
            wm_class.clone()
        } else if !title.is_empty() {
            title
        } else {
            format!("window-{window}")
        };
        Ok(AppInfo {
            pid,
            app_name,
            wm_class,
            exec_path: exe,
            window,
        })
    }

    fn pid_of(&self, window: Window) -> Option<i32> {
        if self.res_ok {
            if let Some(pid) = self.pid_from_xres(window) {
                return Some(pid);
            }
        }
        self.pid_from_property(window)
    }

    fn pid_from_xres(&self, window: Window) -> Option<i32> {
        let reply = self
            .conn
            .res_query_client_ids(&[ClientIdSpec {
                client: window,
                mask: ClientIdMask::LOCAL_CLIENT_PID,
            }])
            .ok()?
            .reply()
            .ok()?;
        let want = u32::from(ClientIdMask::LOCAL_CLIENT_PID);
        for id in reply.ids {
            if u32::from(id.spec.mask) & want != 0 {
                if let Some(&pid) = id.value.first() {
                    if pid > 0 {
                        return Some(pid as i32);
                    }
                }
            }
        }
        None
    }

    fn pid_from_property(&self, window: Window) -> Option<i32> {
        let reply = self
            .conn
            .get_property(false, window, self.net_wm_pid, AtomEnum::CARDINAL, 0, 1)
            .ok()?
            .reply()
            .ok()?;
        if reply.value.len() >= 4 && reply.format == 32 {
            let pid = u32::from_ne_bytes(reply.value[0..4].try_into().ok()?);
            if pid > 0 {
                return Some(pid as i32);
            }
        }
        None
    }

    fn wm_class(&self, window: Window) -> String {
        let reply =
            match self
                .conn
                .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::ANY, 0, 256)
            {
                Ok(cookie) => match cookie.reply() {
                    Ok(reply) => reply,
                    Err(_) => return String::new(),
                },
                Err(_) => return String::new(),
            };
        let parts: Vec<String> = reply
            .value
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        match parts.as_slice() {
            [instance, class] => format!("{instance}/{class}"),
            [only] => only.clone(),
            _ => String::new(),
        }
    }

    fn utf8_prop(&self, window: Window, atom: u32) -> String {
        let reply = match self
            .conn
            .get_property(false, window, atom, AtomEnum::ANY, 0, 256)
        {
            Ok(cookie) => cookie.reply().ok(),
            Err(_) => None,
        };
        reply
            .map(|r| {
                String::from_utf8_lossy(&r.value)
                    .trim_matches('\u{0}')
                    .trim()
                    .to_string()
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_default()
    }

    fn proc_of(&mut self, pid: i32) -> (String, String) {
        if let Some(hit) = self.procs.get(&pid) {
            if hit.at.elapsed() < Duration::from_secs(60) {
                return (hit.comm.clone(), hit.exe.clone());
            }
        }
        let comm = fs::read_to_string(format!("/proc/{pid}/comm"))
            .unwrap_or_default()
            .trim()
            .trim_end_matches('\0')
            .to_string();
        let exe = fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let exe = exe.trim_end_matches(" (deleted)").to_string();
        if self.procs.len() > 1024 {
            self.procs.clear();
        }
        self.procs.insert(
            pid,
            ProcHit {
                comm: comm.clone(),
                exe: exe.clone(),
                at: Instant::now(),
            },
        );
        let _ = Path::new(&exe);
        (comm, exe)
    }
}
