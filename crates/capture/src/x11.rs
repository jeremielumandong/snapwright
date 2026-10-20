//! Linux X11 backend: MIT-SHM, which copies the root window into shared memory in one
//! server-side blit, with a plain `GetImage` when SHM is missing (remote displays).
//!
//! Not used in Wayland sessions: an X11 `GetImage` under XWayland returns only X clients.

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, ImageFormat, MapState, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;

use crate::cursor::{Cursor, CursorKind};
use crate::{CaptureError, Display, Frame, Rect, Result, WindowInfo};

fn err(api: &'static str, e: impl std::fmt::Display) -> CaptureError {
    CaptureError::Platform {
        api,
        message: e.to_string(),
    }
}

// SAFETY: the X connection is owned and the shared memory segment is private to this
// backend; `Capturer` takes `&mut self` for every call, so it is never used from two threads
// at once.
unsafe impl Send for Backend {}

pub(crate) struct Backend {
    conn: RustConnection,
    root: Window,
    screen: Rect,
    atoms: Option<Atoms>,
    shm: Option<ShmSegment>,
    shm_available: bool,
    /// Whether XFixes can hand over the pointer image (for recordings).
    xfixes: bool,
}

/// A SysV shared memory segment attached to both this process and the X server.
struct ShmSegment {
    seg: shm::Seg,
    addr: *mut u8,
    size: usize,
}

impl Backend {
    pub(crate) fn new() -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(None).map_err(|e| err("XOpenDisplay", e))?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let bpp = conn
            .setup()
            .pixmap_formats
            .iter()
            .find(|f| f.depth == screen.root_depth)
            .map(|f| f.bits_per_pixel);
        if bpp != Some(32) {
            return Err(CaptureError::Unsupported(format!(
                "root window depth {} with {bpp:?} bits per pixel",
                screen.root_depth
            )));
        }
        let screen_rect = Rect::new(
            0,
            0,
            u32::from(screen.width_in_pixels),
            u32::from(screen.height_in_pixels),
        );
        let shm_available = conn
            .extension_information(shm::X11_EXTENSION_NAME)
            .ok()
            .flatten()
            .is_some()
            && !is_remote_display();
        // XFixes must be told which version the client speaks before `GetCursorImage` works.
        let xfixes = conn
            .xfixes_query_version(4, 0)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some();
        Ok(Self {
            conn,
            root,
            screen: screen_rect,
            atoms: None,
            shm: None,
            shm_available,
            xfixes,
        })
    }

    pub(crate) fn displays(&mut self) -> Result<Vec<Display>> {
        let monitors = self
            .conn
            .randr_get_monitors(self.root, true)
            .map_err(|e| err("RRGetMonitors", e))?
            .reply();
        let monitors = match monitors {
            Ok(m) if !m.monitors.is_empty() => m.monitors,
            // No RandR 1.5 (Xvfb without it, old servers): the root window is one display.
            _ => {
                return Ok(vec![Display {
                    id: 0,
                    name: "screen".into(),
                    bounds: self.screen,
                    scale: 1.0,
                    primary: true,
                    refresh_hz: None,
                }]);
            }
        };
        // Mode timings, to work out each monitor's refresh rate from its first output's CRTC.
        let modes = self
            .conn
            .randr_get_screen_resources_current(self.root)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.modes)
            .unwrap_or_default();
        let mut out = Vec::with_capacity(monitors.len());
        for m in monitors {
            let refresh_hz = m.outputs.first().and_then(|&output| {
                let info = self
                    .conn
                    .randr_get_output_info(output, 0)
                    .ok()?
                    .reply()
                    .ok()?;
                let crtc = self
                    .conn
                    .randr_get_crtc_info(info.crtc, 0)
                    .ok()?
                    .reply()
                    .ok()?;
                let mode = modes.iter().find(|mode| mode.id == crtc.mode)?;
                let dots = f64::from(mode.htotal) * f64::from(mode.vtotal);
                crate::whole_hz(f64::from(mode.dot_clock) / dots)
            });
            let name = self
                .conn
                .get_atom_name(m.name)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| String::from_utf8_lossy(&r.name).into_owned())
                .unwrap_or_else(|| format!("monitor-{}", m.name));
            out.push(Display {
                id: u64::from(m.name),
                name,
                bounds: Rect::new(
                    i32::from(m.x),
                    i32::from(m.y),
                    u32::from(m.width),
                    u32::from(m.height),
                ),
                // X11 has no per-monitor scale; toolkits read Xft.dpi. 1.0 until the app needs it.
                scale: 1.0,
                primary: m.primary,
                refresh_hz,
            });
        }
        Ok(out)
    }

    pub(crate) fn capture(&mut self, display: &Display, cursor: bool) -> Result<Frame> {
        let mut frame = self.capture_pixels(display)?;
        if cursor && self.xfixes {
            match self.cursor(display) {
                Ok(c) => frame.draw_cursor(&c),
                Err(e) => tracing::debug!(error = %e, "no pointer image"),
            }
        }
        Ok(frame)
    }

    /// The pointer image placed in `display`'s frame.
    fn cursor(&self, display: &Display) -> Result<Cursor> {
        let image = self
            .conn
            .xfixes_get_cursor_image()
            .map_err(|e| err("XFixesGetCursorImage", e))?
            .reply()
            .map_err(|e| err("XFixesGetCursorImage", e))?;
        // Premultiplied 0xAARRGGBB words; their little-endian bytes read B, G, R, A.
        let data = image
            .cursor_image
            .iter()
            .flat_map(|px| px.to_le_bytes())
            .collect();
        Ok(Cursor {
            kind: CursorKind::Premultiplied,
            x: i32::from(image.x) - i32::from(image.xhot) - display.bounds.x,
            y: i32::from(image.y) - i32::from(image.yhot) - display.bounds.y,
            width: u32::from(image.width),
            height: u32::from(image.height),
            pitch: usize::from(image.width) * 4,
            data,
        })
    }

    fn capture_pixels(&mut self, display: &Display) -> Result<Frame> {
        let r = display
            .bounds
            .intersect(&self.screen)
            .ok_or(CaptureError::NoSuchDisplay(display.id))?;
        if self.shm_available {
            match self.capture_shm(display.id, r) {
                Ok(frame) => return Ok(frame),
                Err(e) => {
                    tracing::warn!(error = %e, "MIT-SHM capture failed; using GetImage from now on");
                    self.shm_available = false;
                    self.shm = None;
                }
            }
        }
        let reply = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                r.x as i16,
                r.y as i16,
                r.width as u16,
                r.height as u16,
                !0,
            )
            .map_err(|e| err("GetImage", e))?
            .reply()
            .map_err(|e| err("GetImage", e))?;
        Ok(Frame::from_bgrx_rows(
            display.id,
            r.width,
            r.height,
            r.width as usize * 4,
            &reply.data,
        ))
    }

    pub(crate) fn windows(&mut self) -> Result<Vec<WindowInfo>> {
        let atoms = match self.atoms {
            Some(a) => a,
            None => {
                let a = Atoms::intern(&self.conn)?;
                self.atoms = Some(a);
                a
            }
        };
        let conn = &self.conn;
        let tree = conn
            .query_tree(self.root)
            .map_err(|e| err("QueryTree", e))?
            .reply()
            .map_err(|e| err("QueryTree", e))?;
        // Pipeline every request, then read the replies: one round trip per pass, not per window.
        let attrs: Vec<_> = tree
            .children
            .iter()
            .map(|&w| conn.get_window_attributes(w).ok())
            .collect();
        let geoms: Vec<_> = tree
            .children
            .iter()
            .map(|&w| conn.translate_coordinates(w, self.root, 0, 0).ok())
            .collect();
        let sizes: Vec<_> = tree
            .children
            .iter()
            .map(|&w| conn.get_geometry(w).ok())
            .collect();
        let mut tops = Vec::new();
        for (((&w, a), g), sz) in tree.children.iter().zip(attrs).zip(geoms).zip(sizes) {
            let (Some(a), Some(g), Some(sz)) = (
                a.and_then(|c| c.reply().ok()),
                g.and_then(|c| c.reply().ok()),
                sz.and_then(|c| c.reply().ok()),
            ) else {
                continue;
            };
            if a.map_state != MapState::VIEWABLE
                || a.class != WindowClass::INPUT_OUTPUT
                || a.override_redirect
                || sz.width < 2
                || sz.height < 2
            {
                continue;
            }
            let bounds = Rect::new(
                i32::from(g.dst_x),
                i32::from(g.dst_y),
                u32::from(sz.width),
                u32::from(sz.height),
            );
            tops.push((w, bounds));
        }
        let mut out = Vec::with_capacity(tops.len());
        // QueryTree lists children bottom to top; callers want front to back.
        for (frame, bounds) in tops.into_iter().rev() {
            // Reparenting window managers put the client inside a frame; the client carries the
            // properties (WM_STATE marks it).
            let client = find_client(conn, frame, atoms.wm_state, 2).unwrap_or(frame);
            if let Some(kind) = card32(
                conn,
                client,
                atoms.net_wm_window_type,
                AtomEnum::ATOM.into(),
            )
            .and_then(|v| v.first().copied())
                && (kind == atoms.type_desktop || kind == atoms.type_dock)
            {
                continue;
            }
            let mut bounds = bounds;
            // GTK client-side decorations draw a shadow inside the window; trim it.
            if let Some(ext) = card32(
                conn,
                client,
                atoms.gtk_frame_extents,
                AtomEnum::CARDINAL.into(),
            ) && let [l, r, t, b] = ext[..]
                && l + r < bounds.width
                && t + b < bounds.height
            {
                bounds = Rect::new(
                    bounds.x + l as i32,
                    bounds.y + t as i32,
                    bounds.width - l - r,
                    bounds.height - t - b,
                );
            }
            let title = text(conn, client, atoms.net_wm_name, atoms.utf8_string)
                .or_else(|| {
                    text(
                        conn,
                        client,
                        AtomEnum::WM_NAME.into(),
                        AtomEnum::STRING.into(),
                    )
                })
                .unwrap_or_default();
            let app = text(
                conn,
                client,
                AtomEnum::WM_CLASS.into(),
                AtomEnum::STRING.into(),
            )
            .and_then(|c| c.split('\0').nth(1).map(str::to_owned))
            .unwrap_or_default();
            let pid = card32(conn, client, atoms.net_wm_pid, AtomEnum::CARDINAL.into())
                .and_then(|v| v.first().copied())
                .unwrap_or(0);
            out.push(WindowInfo {
                id: u64::from(client),
                pid,
                app,
                title,
                bounds,
            });
        }
        Ok(out)
    }

    fn capture_shm(&mut self, display_id: u64, r: Rect) -> Result<Frame> {
        let size = r.width as usize * r.height as usize * 4;
        if self.shm.as_ref().is_none_or(|s| s.size < size) {
            self.shm = None;
            // Size the segment for the whole root window so every monitor reuses it.
            let full = self.screen.width as usize * self.screen.height as usize * 4;
            self.shm = Some(ShmSegment::new(&self.conn, full.max(size))?);
        }
        let Some(seg) = self.shm.as_ref() else {
            return Err(err("shmget", "no segment"));
        };
        self.conn
            .shm_get_image(
                self.root,
                r.x as i16,
                r.y as i16,
                r.width as u16,
                r.height as u16,
                !0,
                ImageFormat::Z_PIXMAP.into(),
                seg.seg,
                0,
            )
            .map_err(|e| err("ShmGetImage", e))?
            .reply()
            .map_err(|e| err("ShmGetImage", e))?;
        // SAFETY: the segment is `seg.size >= size` bytes, mapped for this process's lifetime of
        // `seg`, and the server finished writing it before the reply above arrived.
        let pixels = unsafe { std::slice::from_raw_parts(seg.addr, size) };
        Ok(Frame::from_bgrx_rows(
            display_id,
            r.width,
            r.height,
            r.width as usize * 4,
            pixels,
        ))
    }
}

impl ShmSegment {
    fn new(conn: &RustConnection, size: usize) -> Result<Self> {
        // SAFETY: plain SysV calls; every return value is checked before use.
        let id = unsafe { libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600) };
        if id < 0 {
            return Err(err("shmget", std::io::Error::last_os_error()));
        }
        // SAFETY: `id` is the segment just created; a null address lets the kernel choose.
        let addr = unsafe { libc::shmat(id, std::ptr::null(), 0) };
        if addr as isize == -1 {
            let e = std::io::Error::last_os_error();
            // SAFETY: removes the segment we created and failed to map.
            unsafe { libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut()) };
            return Err(err("shmat", e));
        }
        let attach = || -> Result<shm::Seg> {
            let seg = conn.generate_id().map_err(|e| err("shm id", e))?;
            conn.shm_attach(seg, id as u32, false)
                .map_err(|e| err("ShmAttach", e))?
                .check()
                .map_err(|e| err("ShmAttach", e))?;
            Ok(seg)
        };
        let seg = attach();
        // Mark the segment for removal now: the kernel frees it once both this process and the
        // X server detach, so a crash cannot leak it.
        // SAFETY: `id` is our segment; removal is deferred until the last detach.
        unsafe { libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut()) };
        match seg {
            Ok(seg) => Ok(Self {
                seg,
                addr: addr.cast(),
                size,
            }),
            Err(e) => {
                // SAFETY: `addr` was returned by `shmat` above and is detached once.
                unsafe { libc::shmdt(addr) };
                Err(e)
            }
        }
    }
}

impl Drop for ShmSegment {
    fn drop(&mut self) {
        // The server detaches when the connection closes; this process detaches here.
        // SAFETY: `addr` came from `shmat` and is detached exactly once.
        unsafe { libc::shmdt(self.addr.cast()) };
    }
}

#[derive(Clone, Copy)]
struct Atoms {
    wm_state: Atom,
    net_wm_name: Atom,
    net_wm_pid: Atom,
    net_wm_window_type: Atom,
    type_desktop: Atom,
    type_dock: Atom,
    gtk_frame_extents: Atom,
    utf8_string: Atom,
}

impl Atoms {
    fn intern(conn: &RustConnection) -> Result<Self> {
        let names = [
            "WM_STATE",
            "_NET_WM_NAME",
            "_NET_WM_PID",
            "_NET_WM_WINDOW_TYPE",
            "_NET_WM_WINDOW_TYPE_DESKTOP",
            "_NET_WM_WINDOW_TYPE_DOCK",
            "_GTK_FRAME_EXTENTS",
            "UTF8_STRING",
        ];
        let cookies = names
            .iter()
            .map(|n| conn.intern_atom(false, n.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| err("InternAtom", e))?;
        let mut atoms = Vec::with_capacity(names.len());
        for c in cookies {
            atoms.push(c.reply().map_err(|e| err("InternAtom", e))?.atom);
        }
        let [
            wm_state,
            net_wm_name,
            net_wm_pid,
            net_wm_window_type,
            type_desktop,
            type_dock,
            gtk_frame_extents,
            utf8_string,
        ] = atoms[..]
        else {
            return Err(err("InternAtom", "missing reply"));
        };
        Ok(Self {
            wm_state,
            net_wm_name,
            net_wm_pid,
            net_wm_window_type,
            type_desktop,
            type_dock,
            gtk_frame_extents,
            utf8_string,
        })
    }
}

fn property(conn: &RustConnection, w: Window, prop: Atom, kind: Atom) -> Option<Vec<u8>> {
    let reply = conn
        .get_property(false, w, prop, kind, 0, 1024)
        .ok()?
        .reply()
        .ok()?;
    (reply.type_ != u32::from(AtomEnum::NONE)).then_some(reply.value)
}

fn card32(conn: &RustConnection, w: Window, prop: Atom, kind: Atom) -> Option<Vec<u32>> {
    let bytes = property(conn, w, prop, kind)?;
    let (words, _) = bytes.as_chunks::<4>();
    Some(words.iter().map(|b| u32::from_ne_bytes(*b)).collect())
}

fn text(conn: &RustConnection, w: Window, prop: Atom, kind: Atom) -> Option<String> {
    let bytes = property(conn, w, prop, kind)?;
    let s = String::from_utf8_lossy(&bytes)
        .trim_end_matches('\0')
        .to_owned();
    (!s.is_empty()).then_some(s)
}

/// The client window inside a window manager frame: the first descendant with WM_STATE.
fn find_client(conn: &RustConnection, w: Window, wm_state: Atom, depth: u32) -> Option<Window> {
    if property(conn, w, wm_state, AtomEnum::ANY.into()).is_some() {
        return Some(w);
    }
    if depth == 0 {
        return None;
    }
    let tree = conn.query_tree(w).ok()?.reply().ok()?;
    tree.children
        .iter()
        .rev()
        .find_map(|&c| find_client(conn, c, wm_state, depth - 1))
}

/// MIT-SHM needs the server on this machine; `DISPLAY=host:0` means it is not.
fn is_remote_display() -> bool {
    std::env::var("DISPLAY").is_ok_and(|d| !d.starts_with(':') && !d.starts_with("unix"))
}
