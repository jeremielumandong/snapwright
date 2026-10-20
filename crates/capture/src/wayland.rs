//! Linux Wayland backend: `wlr-screencopy` (Hyprland, Sway, river, Wayfire and other wlroots
//! compositors) into a reused `wl_shm` buffer, with output layout from `xdg-output`.
//!
//! Coordinates: Wayland lays outputs out in logical (scaled) units. A [`Display`]'s bounds
//! are that logical position times the output's scale, with the size of the buffer the
//! compositor copies, so frames and bounds agree in pixels as on the other backends.
//!
//! Window lists come from Hyprland's IPC socket when it is running; other compositors have
//! no portable way to list windows, so snapping is off there.
//!
//! GNOME and KDE do not offer `wlr-screencopy`; they need the portal backend (`PLAN.md`).

use std::io::{Read as _, Write as _};
use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy as _, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

use crate::{CaptureError, Display, Frame, Rect, Result, WindowInfo};

fn err(api: &'static str, e: impl std::fmt::Display) -> CaptureError {
    CaptureError::Platform {
        api,
        message: e.to_string(),
    }
}

/// How long a copy may take. A repaint is at most a frame or two away on an awake display.
const COPY_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) struct Backend {
    _conn: Connection,
    queue: EventQueue<State>,
    state: State,
    screencopy: zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    shm: wl_shm::WlShm,
    buffer: Option<ShmBuffer>,
}

/// Everything the event handlers write to.
#[derive(Default)]
struct State {
    xdg_output_manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    outputs: Vec<Output>,
    copy: Copy,
}

/// One `wl_output`, keyed by its registry name.
struct Output {
    global: u32,
    wl: wl_output::WlOutput,
    xdg: Option<zxdg_output_v1::ZxdgOutputV1>,
    name: String,
    /// Current mode in hardware pixels, before the output transform.
    mode: Option<(i32, i32)>,
    /// Current mode's refresh rate in millihertz.
    refresh_mhz: i32,
    position: (i32, i32),
    integer_scale: i32,
    transform: wl_output::Transform,
    logical_position: Option<(i32, i32)>,
    logical_size: Option<(i32, i32)>,
}

/// The screencopy frame in flight.
#[derive(Default)]
struct Copy {
    /// `wl_shm` format, width, height and stride the compositor offers.
    shm: Option<(WEnum<wl_shm::Format>, u32, u32, u32)>,
    buffer_done: bool,
    y_invert: bool,
    /// `Some(true)` once copied, `Some(false)` if the compositor gave up.
    outcome: Option<bool>,
}

impl Backend {
    pub(crate) fn new() -> Result<Self> {
        let conn = Connection::connect_to_env().map_err(|e| err("wl_display_connect", e))?;
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).map_err(|e| err("wl_registry", e))?;
        let qh = queue.handle();
        let screencopy = globals.bind(&qh, 1..=3, ()).map_err(|_| {
            CaptureError::Unsupported(
                "this Wayland compositor has no wlr-screencopy (GNOME and KDE need the portal \
                 backend, planned for Phase 2)"
                    .into(),
            )
        })?;
        let shm = globals.bind(&qh, 1..=1, ()).map_err(|e| err("wl_shm", e))?;
        let mut state = State {
            xdg_output_manager: globals.bind(&qh, 2..=3, ()).ok(),
            ..State::default()
        };
        globals.contents().with_list(|list| {
            for g in list.iter().filter(|g| g.interface == "wl_output") {
                state.add_output(globals.registry(), g.name, g.version, &qh);
            }
        });
        // Two round trips: one for the outputs' first events, one for their xdg-output info.
        for _ in 0..2 {
            queue
                .roundtrip(&mut state)
                .map_err(|e| err("wl_display_roundtrip", e))?;
        }
        Ok(Self {
            _conn: conn,
            queue,
            state,
            screencopy,
            shm,
            buffer: None,
        })
    }

    pub(crate) fn displays(&mut self) -> Result<Vec<Display>> {
        // Picks up outputs plugged in or changed since the last call.
        self.queue
            .roundtrip(&mut self.state)
            .map_err(|e| err("wl_display_roundtrip", e))?;
        let mut out: Vec<Display> = self.state.outputs.iter().filter_map(display).collect();
        out.sort_by_key(|d| (d.bounds.y, d.bounds.x));
        // Wayland has no primary output; call the top-left one that.
        if let Some(first) = out.first_mut() {
            first.primary = true;
        }
        Ok(out)
    }

    pub(crate) fn capture(&mut self, display: &Display, cursor: bool) -> Result<Frame> {
        let output = self
            .state
            .outputs
            .iter()
            .find(|o| u64::from(o.global) == display.id)
            .ok_or(CaptureError::NoSuchDisplay(display.id))?;
        if output.transform != wl_output::Transform::Normal {
            return Err(CaptureError::Unsupported(format!(
                "rotated or flipped outputs ({} is {:?})",
                output.name, output.transform
            )));
        }
        let qh = self.queue.handle();
        self.state.copy = Copy::default();
        let frame = self
            .screencopy
            .capture_output(i32::from(cursor), &output.wl, &qh, ());
        let result = self.copy_frame(&frame, display.id);
        frame.destroy();
        result
    }

    fn copy_frame(
        &mut self,
        frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        display_id: u64,
    ) -> Result<Frame> {
        // Version 3 ends the buffer offers with `buffer_done`; older ones send only `buffer`.
        let v3 = self.screencopy.version() >= 3;
        let deadline = Instant::now() + COPY_TIMEOUT;
        while !(self.state.copy.buffer_done || (!v3 && self.state.copy.shm.is_some()))
            && self.state.copy.outcome.is_none()
        {
            dispatch(&mut self.queue, &mut self.state, deadline)?;
        }
        let Some((format, width, height, stride)) = self.state.copy.shm else {
            return Err(err(
                "zwlr_screencopy_frame_v1",
                "no shared-memory buffer offered",
            ));
        };
        let (format, swap_rb) = match format {
            WEnum::Value(f @ (wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888)) => (f, false),
            WEnum::Value(f @ (wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888)) => (f, true),
            other => {
                return Err(CaptureError::Unsupported(format!(
                    "screencopy pixel format {other:?}"
                )));
            }
        };
        let size = stride as usize * height as usize;
        let reuse = self.buffer.as_ref().is_some_and(|b| {
            (b.format, b.width, b.height, b.stride) == (format, width, height, stride)
        });
        if !reuse {
            self.buffer = None;
            let qh = self.queue.handle();
            self.buffer = Some(ShmBuffer::new(
                &self.shm, &qh, format, width, height, stride,
            )?);
        }
        let Some(buffer) = self.buffer.as_ref() else {
            return Err(err("wl_shm", "no buffer"));
        };
        frame.copy(&buffer.buffer);
        while self.state.copy.outcome.is_none() {
            dispatch(&mut self.queue, &mut self.state, deadline)?;
        }
        if self.state.copy.outcome != Some(true) {
            return Err(err(
                "zwlr_screencopy_frame_v1",
                "the compositor failed the copy",
            ));
        }
        let pixels = buffer.pixels(size);
        let mut frame = if self.state.copy.y_invert {
            let row = stride as usize;
            let flipped: Vec<u8> = pixels.chunks_exact(row).rev().flatten().copied().collect();
            Frame::from_bgrx_rows(display_id, width, height, row, &flipped)
        } else {
            Frame::from_bgrx_rows(display_id, width, height, stride as usize, pixels)
        };
        if swap_rb {
            crate::frame::bgra_to_rgba_in_place(&mut frame.data);
        }
        Ok(frame)
    }

    pub(crate) fn windows(&mut self) -> Result<Vec<WindowInfo>> {
        if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
            return Ok(Vec::new());
        }
        hyprland::windows()
    }
}

/// Waits for and handles the next events, like `blocking_dispatch`, but gives up at
/// `deadline`: compositors copy on their next repaint, and a display asleep (DPMS off) never
/// repaints.
fn dispatch(queue: &mut EventQueue<State>, state: &mut State, deadline: Instant) -> Result<()> {
    let fail = |e: &dyn std::fmt::Display| err("wl_display_dispatch", e);
    if queue.dispatch_pending(state).map_err(|e| fail(&e))? > 0 {
        return Ok(());
    }
    queue.flush().map_err(|e| fail(&e))?;
    if let Some(guard) = queue.prepare_read() {
        let mut fd = libc::pollfd {
            fd: guard.connection_fd().as_raw_fd(),
            events: libc::POLLIN | libc::POLLERR,
            revents: 0,
        };
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(err(
                    "zwlr_screencopy_frame_v1",
                    "timed out waiting for the compositor (is the display asleep?)",
                ));
            }
            // SAFETY: one valid pollfd for the connection's descriptor, which `guard` keeps open.
            let n = unsafe { libc::poll(&mut fd, 1, left.as_millis().max(1) as libc::c_int) };
            if n > 0 {
                break;
            }
            if n < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                return Err(fail(&std::io::Error::last_os_error()));
            }
        }
        match guard.read() {
            Ok(_) => {}
            Err(wayland_client::backend::WaylandError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(fail(&e)),
        }
    }
    queue.dispatch_pending(state).map_err(|e| fail(&e))?;
    Ok(())
}

/// A display for an output once the compositor has described it.
fn display(o: &Output) -> Option<Display> {
    let (mut width, mut height) = o.mode?;
    if matches!(
        o.transform,
        wl_output::Transform::_90
            | wl_output::Transform::_270
            | wl_output::Transform::Flipped90
            | wl_output::Transform::Flipped270
    ) {
        std::mem::swap(&mut width, &mut height);
    }
    let scale_int = o.integer_scale.max(1);
    let (lx, ly) = o.logical_position.unwrap_or(o.position);
    let (lw, _) = o
        .logical_size
        .unwrap_or((width / scale_int, height / scale_int));
    let scale = width as f32 / lw.max(1) as f32;
    Some(Display {
        id: u64::from(o.global),
        name: o.name.clone(),
        bounds: Rect::new(
            (lx as f32 * scale).round() as i32,
            (ly as f32 * scale).round() as i32,
            width.max(0) as u32,
            height.max(0) as u32,
        ),
        scale,
        primary: false,
        refresh_hz: crate::whole_hz(f64::from(o.refresh_mhz) / 1000.0),
    })
}

impl State {
    fn add_output(
        &mut self,
        registry: &wl_registry::WlRegistry,
        global: u32,
        version: u32,
        qh: &QueueHandle<Self>,
    ) {
        let wl: wl_output::WlOutput = registry.bind(global, version.min(4), qh, global);
        let xdg = self
            .xdg_output_manager
            .as_ref()
            .map(|m| m.get_xdg_output(&wl, qh, global));
        self.outputs.push(Output {
            global,
            wl,
            xdg,
            name: format!("output-{global}"),
            mode: None,
            refresh_mhz: 0,
            position: (0, 0),
            integer_scale: 1,
            transform: wl_output::Transform::Normal,
            logical_position: None,
            logical_size: None,
        });
    }

    fn output(&mut self, global: u32) -> Option<&mut Output> {
        self.outputs.iter_mut().find(|o| o.global == global)
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "wl_output" => state.add_output(registry, name, version, qh),
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(i) = state.outputs.iter().position(|o| o.global == name) {
                    let o = state.outputs.remove(i);
                    if let Some(xdg) = o.xdg {
                        xdg.destroy();
                    }
                    if o.wl.version() >= 3 {
                        o.wl.release();
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.output(*global) else {
            return;
        };
        match event {
            wl_output::Event::Geometry {
                x, y, transform, ..
            } => {
                o.position = (x, y);
                if let WEnum::Value(t) = transform {
                    o.transform = t;
                }
            }
            wl_output::Event::Mode {
                flags,
                width,
                height,
                refresh,
            } => {
                if matches!(flags, WEnum::Value(f) if f.contains(wl_output::Mode::Current)) {
                    o.mode = Some((width, height));
                    o.refresh_mhz = refresh;
                }
            }
            wl_output::Event::Scale { factor } => o.integer_scale = factor,
            wl_output::Event::Name { name } => o.name = name,
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.output(*global) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => o.logical_position = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                o.logical_size = Some((width, height));
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::{Event, Flags};
        let copy = &mut state.copy;
        match event {
            Event::Buffer {
                format,
                width,
                height,
                stride,
            } => copy.shm = Some((format, width, height, stride)),
            Event::BufferDone => copy.buffer_done = true,
            Event::Flags { flags } => {
                copy.y_invert = matches!(flags, WEnum::Value(f) if f.contains(Flags::YInvert));
            }
            Event::Ready { .. } => copy.outcome = Some(true),
            Event::Failed => copy.outcome = Some(false),
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(State: zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);
wayland_client::delegate_noop!(State: zxdg_output_manager_v1::ZxdgOutputManagerV1);

/// A `wl_shm` buffer in a memfd, kept between captures of the same size.
struct ShmBuffer {
    _fd: OwnedFd,
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
    addr: *mut u8,
    len: usize,
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}

// SAFETY: the mapping is private to this buffer, which only the owning `Backend` touches,
// and `Capturer` takes `&mut self` for every call.
unsafe impl Send for ShmBuffer {}

impl ShmBuffer {
    fn new(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<State>,
        format: wl_shm::Format,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<Self> {
        let len = stride as usize * height as usize;
        let len_i32 = i32::try_from(len).map_err(|e| err("wl_shm", e))?;
        // SAFETY: a plain syscall with a static, NUL-terminated name; the result is checked.
        let raw = unsafe { libc::memfd_create(c"snapwright-capture".as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            return Err(err("memfd_create", std::io::Error::last_os_error()));
        }
        // SAFETY: `raw` is a fresh descriptor this function owns from here on.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: `raw` is a valid memfd; the result is checked.
        if unsafe { libc::ftruncate(raw, len as libc::off_t) } < 0 {
            return Err(err("ftruncate", std::io::Error::last_os_error()));
        }
        // SAFETY: maps `len` bytes of the memfd just sized to `len`; the result is checked.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                raw,
                0,
            )
        };
        if addr == libc::MAP_FAILED {
            return Err(err("mmap", std::io::Error::last_os_error()));
        }
        let pool = shm.create_pool(fd.as_fd(), len_i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            qh,
            (),
        );
        Ok(Self {
            _fd: fd,
            pool,
            buffer,
            addr: addr.cast(),
            len,
            format,
            width,
            height,
            stride,
        })
    }

    /// The first `size` bytes of the buffer. Call it only after the compositor's `ready`.
    fn pixels(&self, size: usize) -> &[u8] {
        // SAFETY: `addr` maps `len` bytes for the life of `self`, and the compositor finished
        // writing before it sent `ready`.
        unsafe { std::slice::from_raw_parts(self.addr, size.min(self.len)) }
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
        // SAFETY: `addr` and `len` came from `mmap` in `new` and are unmapped exactly once.
        unsafe { libc::munmap(self.addr.cast(), self.len) };
    }
}

/// Window list from Hyprland's IPC socket.
mod hyprland {
    use super::*;
    use serde_json::Value;

    fn request(command: &str) -> Result<Value> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .ok_or_else(|| err("hyprland", "no XDG_RUNTIME_DIR"))?;
        let sig = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")
            .ok_or_else(|| err("hyprland", "not running under Hyprland"))?;
        let path = std::path::Path::new(&dir)
            .join("hypr")
            .join(sig)
            .join(".socket.sock");
        let mut stream = UnixStream::connect(&path).map_err(|e| err("hyprland socket", e))?;
        let timeout = Some(Duration::from_millis(500));
        stream
            .set_read_timeout(timeout)
            .and_then(|()| stream.set_write_timeout(timeout))
            .and_then(|()| stream.write_all(command.as_bytes()))
            .map_err(|e| err("hyprland socket", e))?;
        let mut reply = Vec::new();
        stream
            .read_to_end(&mut reply)
            .map_err(|e| err("hyprland socket", e))?;
        serde_json::from_slice(&reply).map_err(|e| err("hyprland reply", e))
    }

    pub(super) fn windows() -> Result<Vec<WindowInfo>> {
        let monitors = request("j/monitors")?;
        let clients = request("j/clients")?;
        Ok(parse(&monitors, &clients))
    }

    fn int(v: &Value, key: &str) -> i64 {
        v.get(key).and_then(Value::as_i64).unwrap_or(0)
    }

    fn pair(v: &Value, key: &str) -> Option<(i64, i64)> {
        let a = v.get(key)?.as_array()?;
        Some((a.first()?.as_i64()?, a.get(1)?.as_i64()?))
    }

    /// Windows on the workspaces on screen, front to back, in display pixels (logical
    /// coordinates times the scale of the window's monitor).
    pub(super) fn parse(monitors: &Value, clients: &Value) -> Vec<WindowInfo> {
        let monitors = monitors.as_array().map(Vec::as_slice).unwrap_or_default();
        let visible = |ws: i64| {
            monitors.iter().any(|m| {
                [&m["activeWorkspace"], &m["specialWorkspace"]]
                    .iter()
                    .any(|w| int(w, "id") == ws && ws != 0)
            })
        };
        let mut shown: Vec<&Value> = clients
            .as_array()
            .map(|c| c.iter().collect())
            .unwrap_or_default();
        shown.retain(|c| {
            c["mapped"].as_bool() == Some(true)
                && c["hidden"].as_bool() != Some(true)
                && visible(int(&c["workspace"], "id"))
        });
        // Hyprland draws fullscreen windows over floating ones over tiled ones; within each,
        // the most recently focused is on top.
        shown.sort_by_key(|c| {
            (
                int(c, "fullscreen") == 0,
                c["floating"].as_bool() != Some(true),
                int(c, "focusHistoryID"),
            )
        });
        shown
            .into_iter()
            .filter_map(|c| {
                let (x, y) = pair(c, "at")?;
                let (w, h) = pair(c, "size")?;
                let scale = monitors
                    .iter()
                    .find(|m| int(m, "id") == int(c, "monitor"))
                    .and_then(|m| m["scale"].as_f64())
                    .unwrap_or(1.0);
                let px = |v: i64| (v as f64 * scale).round() as i64;
                let address = c["address"].as_str().unwrap_or_default();
                Some(WindowInfo {
                    id: u64::from_str_radix(address.trim_start_matches("0x"), 16).unwrap_or(0),
                    pid: u32::try_from(int(c, "pid")).unwrap_or(0),
                    app: c["class"].as_str().unwrap_or_default().to_owned(),
                    title: c["title"].as_str().unwrap_or_default().to_owned(),
                    bounds: Rect::new(
                        px(x) as i32,
                        px(y) as i32,
                        px(w).max(0) as u32,
                        px(h).max(0) as u32,
                    ),
                })
            })
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn visible_windows_front_to_back_in_display_pixels() {
            let monitors = serde_json::json!([
                {"id": 0, "scale": 1.25, "activeWorkspace": {"id": 4}, "specialWorkspace": {"id": 0}}
            ]);
            let clients = serde_json::json!([
                {"address": "0x10", "mapped": true, "hidden": false, "at": [8, 8], "size": [800, 600],
                 "workspace": {"id": 4}, "floating": false, "monitor": 0, "class": "kitty",
                 "title": "shell", "pid": 11, "fullscreen": 0, "focusHistoryID": 0},
                {"address": "0x20", "mapped": true, "hidden": false, "at": [100, 100], "size": [200, 100],
                 "workspace": {"id": 4}, "floating": true, "monitor": 0, "class": "pavucontrol",
                 "title": "Volume", "pid": 12, "fullscreen": 0, "focusHistoryID": 3},
                {"address": "0x30", "mapped": true, "hidden": false, "at": [0, 0], "size": [10, 10],
                 "workspace": {"id": 2}, "floating": false, "monitor": 0, "class": "firefox",
                 "title": "elsewhere", "pid": 13, "fullscreen": 0, "focusHistoryID": 1}
            ]);
            let w = parse(&monitors, &clients);
            assert_eq!(w.len(), 2);
            assert_eq!(w[0].app, "pavucontrol");
            assert_eq!(w[0].bounds, Rect::new(125, 125, 250, 125));
            assert_eq!(w[1].id, 0x10);
            assert_eq!(w[1].bounds, Rect::new(10, 10, 1000, 750));
        }
    }
}
