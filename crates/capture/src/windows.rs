//! Windows backend: DXGI Desktop Duplication.
//!
//! Each monitor keeps its D3D11 device, duplication and staging texture between captures, so
//! a repeat capture is one GPU copy and one map. When the desktop has not changed since the
//! last capture, the duplication has no new frame and the last one is returned as is.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME, DXGI_OUTPUT_DESC, IDXGIAdapter1, IDXGIFactory1,
    IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
    SetProcessDpiAwarenessContext,
};
use windows::core::Interface;

use crate::cursor::{Cursor, CursorKind};
use crate::{CaptureError, Display, Frame, Rect, Result, WindowInfo};

fn err(api: &'static str, e: windows::core::Error) -> CaptureError {
    if e.code() == windows::Win32::Foundation::E_ACCESSDENIED {
        return CaptureError::PermissionDenied(format!("{api}: {e}"));
    }
    CaptureError::Platform {
        api,
        message: e.to_string(),
    }
}

// SAFETY: the D3D11 devices are free-threaded and the immediate contexts, duplications and
// staging textures are owned by this backend alone; `Capturer` takes `&mut self` for every
// call, so they are only ever used from one thread at a time.
unsafe impl Send for Backend {}

pub(crate) struct Backend {
    outputs: Vec<Output>,
}

struct Output {
    id: u64,
    output: IDXGIOutput1,
    desc: DXGI_OUTPUT_DESC,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: Option<IDXGIOutputDuplication>,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    /// The pointer as the duplication last reported it: shown, and where. Desktop
    /// Duplication leaves the pointer out of the desktop image.
    pointer: Option<(i32, i32)>,
    /// The last pointer shape; the duplication sends one only when it changes.
    shape: Option<Cursor>,
}

impl Backend {
    pub(crate) fn new() -> Result<Self> {
        // Desktop Duplication and physical-pixel coordinates both need per-monitor DPI
        // awareness. The call fails harmlessly when the app manifest already set it.
        // SAFETY: a process-wide setting with no pointers involved.
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        Ok(Self {
            outputs: enumerate()?,
        })
    }

    pub(crate) fn displays(&mut self) -> Result<Vec<Display>> {
        Ok(self.outputs.iter().map(Output::display).collect())
    }

    pub(crate) fn windows(&mut self) -> Result<Vec<WindowInfo>> {
        list_windows()
    }

    pub(crate) fn capture(&mut self, display: &Display, cursor: bool) -> Result<Frame> {
        let Some(output) = self.outputs.iter_mut().find(|o| o.id == display.id) else {
            return Err(CaptureError::NoSuchDisplay(display.id));
        };
        let mut frame = match output.capture() {
            // The duplication was lost (mode change, secure desktop) and dropped: retry once
            // with a new one.
            Err(e) if output.duplication.is_none() => {
                tracing::debug!(error = %e, "duplication lost; recreating");
                output.capture()
            }
            other => other,
        }?;
        if cursor && let (Some((x, y)), Some(shape)) = (output.pointer, &output.shape) {
            frame.draw_cursor(&Cursor {
                x,
                y,
                ..shape.clone()
            });
        }
        Ok(frame)
    }
}

fn enumerate() -> Result<Vec<Output>> {
    // SAFETY: COM factory creation; the result is checked.
    let factory: IDXGIFactory1 =
        unsafe { CreateDXGIFactory1() }.map_err(|e| err("CreateDXGIFactory1", e))?;
    let mut outputs = Vec::new();
    for a in 0u32.. {
        // SAFETY: index enumeration ends with DXGI_ERROR_NOT_FOUND.
        let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(a) } {
            Ok(adapter) => adapter,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(err("EnumAdapters1", e)),
        };
        let mut device_for_adapter: Option<(ID3D11Device, ID3D11DeviceContext)> = None;
        for o in 0u32.. {
            // SAFETY: as above.
            let output = match unsafe { adapter.EnumOutputs(o) } {
                Ok(output) => output,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(err("EnumOutputs", e)),
            };
            let output: IDXGIOutput1 = output.cast().map_err(|e| err("IDXGIOutput1", e))?;
            // SAFETY: GetDesc fills a plain struct.
            let desc = unsafe { output.GetDesc() }.map_err(|e| err("GetDesc", e))?;
            if !desc.AttachedToDesktop.as_bool() {
                continue;
            }
            let (device, context) = match &device_for_adapter {
                Some(pair) => pair.clone(),
                None => {
                    let pair = create_device(&adapter)?;
                    device_for_adapter = Some(pair.clone());
                    pair
                }
            };
            outputs.push(Output {
                id: (u64::from(a) << 16) | u64::from(o),
                output,
                desc,
                device,
                context,
                duplication: None,
                staging: None,
                pointer: None,
                shape: None,
            });
        }
    }
    if outputs.is_empty() {
        return Err(CaptureError::Unsupported(
            "no display is attached to the desktop".into(),
        ));
    }
    Ok(outputs)
}

fn create_device(adapter: &IDXGIAdapter1) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    // SAFETY: out-pointers are valid locals; the adapter outlives the call.
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(|e| err("D3D11CreateDevice", e))?;
    match (device, context) {
        (Some(d), Some(c)) => Ok((d, c)),
        _ => Err(CaptureError::Platform {
            api: "D3D11CreateDevice",
            message: "returned no device".into(),
        }),
    }
}

/// The current refresh rate of the monitor named `device` (`\\.\DISPLAY1`), from its display
/// mode; 0 and 1 there mean "the hardware default", which says nothing.
fn refresh_hz(device: &[u16; 32]) -> Option<u32> {
    use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW};
    let mut mode = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: `device` is the NUL-terminated name DXGI reported; `mode` is a valid DEVMODEW
    // with its size set.
    let ok = unsafe {
        EnumDisplaySettingsW(
            windows::core::PCWSTR(device.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
        )
    };
    (ok.as_bool() && mode.dmDisplayFrequency > 1).then_some(mode.dmDisplayFrequency)
}

impl Output {
    fn display(&self) -> Display {
        let r = self.desc.DesktopCoordinates;
        let name_len = self
            .desc
            .DeviceName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(32);
        let mut dpi_x = 96u32;
        let mut dpi_y = 96u32;
        // SAFETY: the monitor handle comes from DXGI; out-pointers are valid locals.
        let _ = unsafe {
            GetDpiForMonitor(self.desc.Monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y)
        };
        Display {
            id: self.id,
            name: String::from_utf16_lossy(&self.desc.DeviceName[..name_len]),
            bounds: Rect::new(
                r.left,
                r.top,
                (r.right - r.left).max(0) as u32,
                (r.bottom - r.top).max(0) as u32,
            ),
            scale: dpi_x as f32 / 96.0,
            primary: r.left == 0 && r.top == 0,
            refresh_hz: refresh_hz(&self.desc.DeviceName),
        }
    }

    fn capture(&mut self) -> Result<Frame> {
        let first = self.duplication.is_none();
        if first {
            // SAFETY: the device belongs to this output's adapter.
            let dup = unsafe { self.output.DuplicateOutput(&self.device) }
                .map_err(|e| err("DuplicateOutput", e))?;
            self.duplication = Some(dup);
        }
        let Some(dup) = self.duplication.clone() else {
            return Err(CaptureError::Platform {
                api: "DuplicateOutput",
                message: "no duplication".into(),
            });
        };
        // A fresh duplication delivers the whole desktop as its first image; later calls only
        // return when something changed, so they do not wait.
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            let timeout = if first || self.staging.is_none() {
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .try_into()
                    .unwrap_or(u32::MAX)
            } else {
                0
            };
            // SAFETY: out-pointers are valid locals; the frame is released below on every path.
            match unsafe { dup.AcquireNextFrame(timeout, &mut info, &mut resource) } {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT && self.staging.is_some() => {
                    return self.read_staging();
                }
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    self.duplication = None;
                    self.staging = None;
                    return Err(err("AcquireNextFrame", e));
                }
                Err(e) => return Err(err("AcquireNextFrame", e)),
            }
            self.update_pointer(&dup, &info);
            // A frame with no present time only moved the pointer, and its texture does not
            // hold the desktop yet on a fresh duplication: copying it gave a black first
            // capture. Keep the last image, or wait for the first real one.
            let has_image = info.LastPresentTime != 0;
            let copied = if has_image {
                resource
                    .ok_or_else(|| CaptureError::Platform {
                        api: "AcquireNextFrame",
                        message: "no frame resource".into(),
                    })
                    .and_then(|r| {
                        r.cast::<ID3D11Texture2D>()
                            .map_err(|e| err("ID3D11Texture2D", e))
                    })
                    .and_then(|texture| self.copy_to_staging(&texture))
            } else {
                Ok(())
            };
            // SAFETY: a frame was acquired above.
            let released = unsafe { dup.ReleaseFrame() }.map_err(|e| err("ReleaseFrame", e));
            copied?;
            released?;
            if has_image || self.staging.is_some() {
                return self.read_staging();
            }
        }
    }

    /// Tracks the pointer from a frame's info: its position when the mouse moved, its shape
    /// when that changed. Failures only cost the pointer in recordings.
    fn update_pointer(&mut self, dup: &IDXGIOutputDuplication, info: &DXGI_OUTDUPL_FRAME_INFO) {
        if info.LastMouseUpdateTime != 0 {
            let p = info.PointerPosition;
            self.pointer = p.Visible.as_bool().then_some((p.Position.x, p.Position.y));
        }
        if info.PointerShapeBufferSize == 0 {
            return;
        }
        let mut data = vec![0u8; info.PointerShapeBufferSize as usize];
        let mut used = 0u32;
        let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        // SAFETY: the buffer is a local of the size passed; the frame is still acquired.
        let got = unsafe {
            dup.GetFramePointerShape(
                data.len() as u32,
                data.as_mut_ptr().cast(),
                &mut used,
                &mut shape,
            )
        };
        if let Err(e) = got {
            tracing::debug!(error = %e, "no pointer shape");
            return;
        }
        let kind = match shape.Type as i32 {
            t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 => CursorKind::Straight,
            t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 => CursorKind::MaskedColor,
            t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 => CursorKind::Monochrome,
            _ => return,
        };
        data.truncate(used as usize);
        self.shape = Some(Cursor {
            kind,
            x: 0,
            y: 0,
            width: shape.Width,
            height: shape.Height,
            pitch: shape.Pitch as usize,
            data,
        });
    }

    fn copy_to_staging(&mut self, texture: &ID3D11Texture2D) -> Result<()> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: fills a plain struct.
        unsafe { texture.GetDesc(&mut desc) };
        let reuse =
            matches!(&self.staging, Some((_, w, h)) if *w == desc.Width && *h == desc.Height);
        if !reuse {
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            desc.MiscFlags = 0;
            desc.MipLevels = 1;
            desc.ArraySize = 1;
            let mut staging = None;
            // SAFETY: the descriptor is valid for a CPU-readable copy of the frame.
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut staging)) }
                .map_err(|e| err("CreateTexture2D", e))?;
            let staging = staging.ok_or_else(|| CaptureError::Platform {
                api: "CreateTexture2D",
                message: "returned no texture".into(),
            })?;
            self.staging = Some((staging, desc.Width, desc.Height));
        }
        if let Some((staging, _, _)) = &self.staging {
            // SAFETY: both textures share size and format and live on this device.
            unsafe { self.context.CopyResource(staging, texture) };
        }
        Ok(())
    }

    fn read_staging(&self) -> Result<Frame> {
        let Some((staging, width, height)) = &self.staging else {
            return Err(CaptureError::Platform {
                api: "Map",
                message: "nothing captured yet".into(),
            });
        };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: the staging texture is CPU-readable and unmapped below.
        unsafe {
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .map_err(|e| err("Map", e))?;
        let stride = mapped.RowPitch as usize;
        // SAFETY: a mapped texture exposes `RowPitch * height` readable bytes at `pData`.
        let bytes = unsafe {
            std::slice::from_raw_parts(mapped.pData as *const u8, stride * *height as usize)
        };
        let frame = Frame::from_bgrx_rows(self.id, *width, *height, stride, bytes);
        // SAFETY: mapped above.
        unsafe { self.context.Unmap(staging, 0) };
        Ok(frame)
    }
}

/// Visible top-level windows, front to back (`EnumWindows` walks the z-order from the top).
fn list_windows() -> Result<Vec<WindowInfo>> {
    use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
    use windows::Win32::Graphics::Dwm::{
        DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GWL_EXSTYLE, GetWindowLongW, GetWindowTextW, GetWindowThreadProcessId,
        IsIconic, IsWindowVisible, WS_EX_TOOLWINDOW,
    };
    use windows::core::{BOOL, PWSTR};

    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the `&mut Vec<HWND>` passed to `EnumWindows` below, which outlives
        // the enumeration and is not otherwise borrowed during it.
        let found = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
        found.push(hwnd);
        BOOL(1)
    }

    let mut handles: Vec<HWND> = Vec::new();
    // SAFETY: the callback only pushes into the vector behind `lparam`.
    unsafe {
        EnumWindows(
            Some(collect),
            LPARAM(&mut handles as *mut Vec<HWND> as isize),
        )
    }
    .map_err(|e| err("EnumWindows", e))?;

    let mut names: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
    let mut out = Vec::new();
    for hwnd in handles {
        // SAFETY: plain queries on a window handle from `EnumWindows`; a window that closed
        // since then makes them fail or return zero, which skips it.
        let visible = unsafe { IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool() };
        if !visible {
            continue;
        }
        // SAFETY: as above.
        let ex_style = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32;
        if ex_style & WS_EX_TOOLWINDOW.0 != 0 {
            continue;
        }
        let mut cloaked = 0u32;
        // SAFETY: the out-pointer is a local of the size passed.
        let _ = unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                (&mut cloaked as *mut u32).cast(),
                size_of::<u32>() as u32,
            )
        };
        if cloaked != 0 {
            continue;
        }
        let mut r = RECT::default();
        // SAFETY: the out-pointer is a local RECT of the size passed. The extended frame bounds
        // exclude the invisible resize borders that `GetWindowRect` includes.
        if unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                (&mut r as *mut RECT).cast(),
                size_of::<RECT>() as u32,
            )
        }
        .is_err()
        {
            continue;
        }
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if w < 2 || h < 2 {
            continue;
        }
        let mut title = [0u16; 256];
        // SAFETY: the buffer is a local array; the call writes at most its length.
        let len = unsafe { GetWindowTextW(hwnd, &mut title) }.max(0) as usize;
        let mut pid = 0u32;
        // SAFETY: the out-pointer is a local.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        let app = names
            .entry(pid)
            .or_insert_with(|| {
                // SAFETY: the process handle is checked, used once and closed; the name buffer
                // is a local array whose length is passed in and updated by the call.
                unsafe {
                    let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
                    else {
                        return String::new();
                    };
                    let mut buf = [0u16; 512];
                    let mut n = buf.len() as u32;
                    let ok = QueryFullProcessImageNameW(
                        process,
                        PROCESS_NAME_WIN32,
                        PWSTR(buf.as_mut_ptr()),
                        &mut n,
                    )
                    .is_ok();
                    let _ = CloseHandle(process);
                    if !ok {
                        return String::new();
                    }
                    let path = String::from_utf16_lossy(&buf[..n as usize]);
                    std::path::Path::new(&path)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default()
                }
            })
            .clone();
        out.push(WindowInfo {
            id: hwnd.0 as usize as u64,
            pid,
            app,
            title: String::from_utf16_lossy(&title[..len]),
            bounds: Rect::new(r.left, r.top, w as u32, h as u32),
        });
    }
    Ok(out)
}

pub(crate) fn exclude_window(hwnd: isize) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE,
    };
    // SAFETY: SetWindowDisplayAffinity validates the handle and fails on one that is not a
    // window of this process; nothing is dereferenced here.
    unsafe {
        SetWindowDisplayAffinity(HWND(hwnd as *mut core::ffi::c_void), WDA_EXCLUDEFROMCAPTURE)
    }
    .is_ok()
}
