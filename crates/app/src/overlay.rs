//! The selection overlay: one borderless window per display showing that display's frozen
//! frame. Hover highlights a window, a click takes it, a drag selects a region; see
//! [`crate::selection`] for the whole interaction. A loupe beside the pointer shows the pixels
//! under it and their colour; `C` copies that colour as hex, `T` the text in the selection (or
//! the hovered window). Esc cancels. In record mode the selection is the area to record, and
//! for a video `A` and `M` switch the system's sound and the microphone on or off.

use std::sync::Arc;
use std::time::Instant;

use gpui_kit::{
    ClipboardItem, Context, CursorStyle, Div, FocusHandle, FontWeight, Hsla, ImageSource,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, Point, Render, RenderImage, Rgba,
    Size, Styled as _, Window, div, hsla, img, px, size,
};
use snapwright_capture::{Frame, Rect};

use gpui_kit::prelude::FluentBuilder as _;

use crate::selection::{Arrow, Outcome, Selector};
use crate::session::{self, Mode, SnapTarget};
use crate::theme::{self, hud};

/// Loupe grid: cells per side (odd, so one sits under the pointer) and their size.
const LOUPE_CELLS: i32 = 11;
const LOUPE_CELL: f32 = 12.0;
/// Height of the size label above the selection.
const LABEL_H: f32 = 24.;
/// Height of the key hint bar and its distance from the display edge.
const HINTS_H: f32 = 40.;
const HINTS_MARGIN: f32 = 24.;

pub struct Overlay {
    display_id: u64,
    /// The frozen frame. Its pixels live in the image (GPUI takes BGRA, so no conversion and
    /// no copy); crops and the loupe read them back from there.
    image: Arc<RenderImage>,
    width: u32,
    height: u32,
    focus: FocusHandle,
    selector: Selector,
    targets: Vec<SnapTarget>,
    mode: Mode,
    viewport: Size<Pixels>,
    pressed_at: Instant,
    shown: bool,
}

impl Overlay {
    /// `targets` are windows in this frame's pixels, front to back.
    pub fn new(
        frame: Frame,
        targets: Vec<SnapTarget>,
        mode: Mode,
        pressed_at: Instant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let Frame {
            display_id,
            width,
            height,
            data,
        } = frame;
        let buffer = image::RgbaImage::from_raw(width, height, data)
            .unwrap_or_else(|| image::RgbaImage::new(width, height));
        let image = Arc::new(RenderImage::new([image::Frame::new(buffer)]));
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self {
            display_id,
            image,
            width,
            height,
            focus,
            selector: Selector::new(width, height, targets.iter().map(|t| t.rect).collect()),
            targets,
            mode,
            viewport: size(px(1.), px(1.)),
            pressed_at,
            shown: false,
        }
    }

    /// Frame pixels per logical pixel, per axis.
    fn scale(&self) -> (f32, f32) {
        (
            self.width as f32 / f32::from(self.viewport.width).max(1.0),
            self.height as f32 / f32::from(self.viewport.height).max(1.0),
        )
    }

    fn to_frame(&self, p: Point<Pixels>) -> (i32, i32) {
        let (sx, sy) = self.scale();
        (
            (f32::from(p.x) * sx).floor() as i32,
            (f32::from(p.y) * sy).floor() as i32,
        )
    }

    /// A frame rectangle in logical pixels: (x, y, width, height).
    fn to_logical(&self, r: Rect) -> (Pixels, Pixels, Pixels, Pixels) {
        let (sx, sy) = self.scale();
        (
            px(r.x as f32 / sx),
            px(r.y as f32 / sy),
            px(r.width as f32 / sx),
            px(r.height as f32 / sy),
        )
    }

    /// The BGRA pixel at frame coordinates, if inside the frame.
    fn pixel(&self, x: i32, y: i32) -> Option<[u8; 4]> {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return None;
        }
        let i = (y as usize * self.width as usize + x as usize) * 4;
        let bytes = self.image.as_bytes(0)?;
        bytes.get(i..i + 4)?.try_into().ok()
    }

    fn apply(&mut self, outcome: Outcome, window: &mut Window, cx: &mut Context<Self>) {
        match outcome {
            Outcome::Unchanged => {}
            Outcome::Changed => cx.notify(),
            Outcome::Deliver(r) => self.deliver(r, window, cx),
        }
    }

    /// The pixels of `r`, clipped to the frame, and the clipped rectangle.
    fn crop(&self, r: Rect) -> Option<(Rect, Frame)> {
        let bytes = self.image.as_bytes(0)?;
        // Crop straight from the image's pixels: one copy of the selected rows only.
        let r = r.intersect(&Rect::new(0, 0, self.width, self.height))?;
        let stride = self.width as usize * 4;
        let start = r.y as usize * stride + r.x as usize * 4;
        let rows = bytes.get(start..)?;
        let frame = Frame::from_bgrx_rows(self.display_id, r.width, r.height, stride, rows);
        Some((r, frame))
    }

    fn deliver(&mut self, r: Rect, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.mode, Mode::Record(_)) {
            let Some(r) = r.intersect(&Rect::new(0, 0, self.width, self.height)) else {
                return;
            };
            let meta =
                session::describe(r, self.width, self.height, &self.targets, self.display_id);
            session::deliver_recording(self.display_id, r, meta, window, cx);
            return;
        }
        let Some((r, frame)) = self.crop(r) else {
            return;
        };
        let meta = session::describe(r, self.width, self.height, &self.targets, self.display_id);
        session::deliver(frame, r, meta, self.pressed_at, window, cx);
    }

    /// Copies the text in the selection (or the hovered window) instead of its pixels.
    fn copy_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, frame)) = self.crop(self.selector.region()) else {
            return;
        };
        session::finish(window, cx);
        crate::ocr::copy_text(frame, cx);
    }

    fn on_mouse_down(&mut self, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let outcome = self.selector.pointer_down(self.to_frame(e.position));
        self.apply(outcome, window, cx);
    }

    fn on_mouse_move(&mut self, e: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        let outcome = self.selector.pointer_move(self.to_frame(e.position));
        self.apply(outcome, window, cx);
    }

    fn on_mouse_up(&mut self, e: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        let outcome = self
            .selector
            .pointer_up(self.to_frame(e.position), e.modifiers.shift);
        self.apply(outcome, window, cx);
    }

    fn on_key_down(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = e.keystroke.key.as_str();
        let mods = e.keystroke.modifiers;
        if let Some(arrow) = Arrow::from_key(key) {
            let fast = mods.control || mods.platform;
            let outcome = self.selector.arrow(arrow, mods.shift, fast);
            self.apply(outcome, window, cx);
            return;
        }
        match key {
            "escape" => session::finish(window, cx),
            "enter" => {
                let outcome = self.selector.confirm();
                self.apply(outcome, window, cx);
            }
            "c" if self.mode == Mode::Capture => {
                if let Some(hex) = self
                    .selector
                    .cursor()
                    .and_then(|(x, y)| self.pixel(x, y))
                    .map(hex)
                {
                    tracing::info!(colour = %hex, "colour copied");
                    cx.write_to_clipboard(ClipboardItem::new_string(hex));
                    session::finish(window, cx);
                }
            }
            "t" if !mods.control && !mods.platform && !matches!(self.mode, Mode::Record(_)) => {
                self.copy_text(window, cx)
            }
            "a" | "m"
                if !mods.control
                    && !mods.platform
                    && self.mode == Mode::Record(snapwright_record::Format::Mp4) =>
            {
                let choice = if key == "a" {
                    crate::tray::RecordingChoice::SystemAudio
                } else {
                    crate::tray::RecordingChoice::Microphone
                };
                crate::recording::choose(choice, cx);
                cx.notify();
            }
            _ => {}
        }
    }

    /// Magnified pixels around the pointer with their colour and position, placed beside the
    /// pointer and flipped away from the display edges.
    fn loupe(&self, at: (i32, i32), pointer: Point<Pixels>) -> Div {
        let half = LOUPE_CELLS / 2;
        let side = LOUPE_CELLS as f32 * LOUPE_CELL;
        let mut grid = div()
            .relative()
            .flex()
            .flex_col()
            .rounded(px(6.))
            .overflow_hidden();
        for dy in -half..=half {
            let mut row = div().flex().flex_row();
            for dx in -half..=half {
                let colour = self
                    .pixel(at.0 + dx, at.1 + dy)
                    .map_or(hsla(0., 0., 0., 1.), colour);
                row = row.child(div().w(px(LOUPE_CELL)).h(px(LOUPE_CELL)).bg(colour));
            }
            grid = grid.child(row);
        }
        let centre = half as f32 * LOUPE_CELL;
        grid = grid.child(
            div()
                .absolute()
                .left(px(centre - 1.))
                .top(px(centre - 1.))
                .size(px(LOUPE_CELL + 2.))
                .border_1()
                .border_color(hsla(0., 0., 0., 0.5))
                .child(div().size_full().border(px(1.5)).border_color(hud::fg())),
        );
        let here = self.pixel(at.0, at.1);
        let mut label = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(3.))
            .pb(px(2.))
            .text_size(px(11.))
            .text_color(hud::fg());
        if let Some(p) = here {
            label = label
                .child(
                    div()
                        .size(px(10.))
                        .rounded(px(3.))
                        .border_1()
                        .border_color(hsla(0., 0., 1., 0.3))
                        .bg(colour(p)),
                )
                .child(div().flex_1().font_weight(FontWeight::MEDIUM).child(hex(p)));
        } else {
            label = label.child(div().flex_1());
        }
        label = label.child(
            div()
                .text_color(hud::fg3())
                .child(format!("{}, {}", at.0, at.1)),
        );
        let gap = px(18.);
        let width = px(side + 10.);
        let height = px(side + 33.);
        let x = if pointer.x + gap + width > self.viewport.width {
            pointer.x - gap - width
        } else {
            pointer.x + gap
        };
        let y = if pointer.y + gap + height > self.viewport.height {
            pointer.y - gap - height
        } else {
            pointer.y + gap
        };
        div()
            .absolute()
            .left(x)
            .top(y)
            .w(width)
            .p(px(5.))
            .rounded(px(10.))
            .bg(hud::bg())
            .border_1()
            .border_color(hud::border())
            .shadow(vec![shadow()])
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(grid)
            .child(label)
    }

    /// The selection's size, as a pill above it (below it at the top of the display).
    fn size_label(&self, region: Rect, x0: Pixels, y0: Pixels, y1: Pixels) -> Div {
        let vh = self.viewport.height;
        let y = if y0 > px(LABEL_H + 10.) {
            y0 - px(LABEL_H + 8.)
        } else {
            y1 + px(8.)
        };
        let y = y.min(vh - px(LABEL_H + 2.)).max(px(2.));
        div()
            .absolute()
            .left(x0.max(px(2.)))
            .top(y)
            .h(px(LABEL_H))
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(8.))
            .rounded(px(6.))
            .bg(hsla(240. / 360., 0.09, 0.1, 0.92))
            .border_1()
            .border_color(hud::border())
            .text_size(px(12.))
            .font_weight(FontWeight::MEDIUM)
            .text_color(hud::fg())
            .child(region.width.to_string())
            .child(div().text_color(hud::fg3()).child("×"))
            .child(
                div().flex().child(region.height.to_string()).child(
                    div()
                        .text_color(hud::fg3())
                        .font_weight(FontWeight::NORMAL)
                        .child("px"),
                ),
            )
    }

    /// Keys for the current step, centred at the bottom of the display (at the top when a
    /// selection reaches down there).
    fn hints(&self, y1: Pixels, prefs: crate::recording::Prefs) -> Div {
        let bar = div()
            .h(px(HINTS_H))
            .flex()
            .items_center()
            .gap(px(16.))
            .px(px(14.))
            .rounded(px(12.))
            .bg(hud::bg())
            .border_1()
            .border_color(hud::border())
            .shadow(vec![shadow()])
            .text_size(px(12.))
            .text_color(hud::fg2());
        let recording = matches!(self.mode, Mode::Record(_));
        let bar = if self.selector.is_adjusting() {
            bar.child(theme::hud_hint(&["left", "right"], "Move"))
                .child(theme::hud_hint(&["shift", "left"], "Resize"))
                .child(theme::hud_hint(
                    &["enter"],
                    if recording { "Record" } else { "Copy" },
                ))
                .when(!recording, |bar| {
                    bar.child(theme::hud_hint(&["T"], "Copy text"))
                })
                .child(theme::hud_hint(&["escape"], "Cancel"))
        } else if recording {
            let what = match self.mode {
                Mode::Record(snapwright_record::Format::Gif) => "a GIF",
                _ => "a video",
            };
            bar.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(div().size(px(8.)).rounded_full().bg(hud::record()))
                    .child(format!(
                        "Click a window or display, or drag a region, for {what}"
                    )),
            )
            .child(theme::hud_hint(&["shift"], "Adjust"))
            .when(
                self.mode == Mode::Record(snapwright_record::Format::Mp4),
                |bar| {
                    let state =
                        |on: bool, yes: &'static str, no: &'static str| if on { yes } else { no };
                    bar.child(theme::hud_hint(
                        &["A"],
                        state(prefs.audio.system, "System audio on", "System audio off"),
                    ))
                    .child(theme::hud_hint(
                        &["M"],
                        state(prefs.audio.microphone, "Mic on", "Mic off"),
                    ))
                },
            )
            .child(theme::hud_hint(&["escape"], "Cancel"))
        } else {
            bar.child("Click a window or drag a region")
                .child(theme::hud_hint(&["shift"], "Adjust"))
                .child(theme::hud_hint(&["C"], "Copy colour"))
                .child(theme::hud_hint(&["T"], "Copy text"))
                .child(theme::hud_hint(&["escape"], "Cancel"))
        };
        let row = div().absolute().left_0().w_full().flex().justify_center();
        // Out of the way of a selection that reaches the bottom; a hovered window or display
        // is not a selection yet.
        let low = y1 > self.viewport.height - px(HINTS_H + HINTS_MARGIN * 2.);
        let top = if low && !self.selector.is_hovering() {
            px(HINTS_MARGIN)
        } else {
            self.viewport.height - px(HINTS_H + HINTS_MARGIN)
        };
        row.top(top).child(bar)
    }
}

fn shadow() -> gpui_kit::BoxShadow {
    gpui_kit::BoxShadow {
        color: hud::shadow(),
        offset: gpui_kit::point(px(0.), px(10.)),
        blur_radius: px(30.),
        spread_radius: px(0.),
        inset: false,
    }
}

/// A BGRA pixel as a GPUI colour.
fn colour([b, g, r, _]: [u8; 4]) -> Hsla {
    Rgba {
        r: f32::from(r) / 255.0,
        g: f32::from(g) / 255.0,
        b: f32::from(b) / 255.0,
        a: 1.0,
    }
    .into()
}

/// `#RRGGBB` for a BGRA pixel.
fn hex([b, g, r, _]: [u8; 4]) -> String {
    format!("#{r:02X}{g:02X}{b:02X}")
}

fn shade() -> Hsla {
    hud::shade()
}

impl Render for Overlay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.viewport = window.viewport_size();
        if !self.shown {
            self.shown = true;
            let pressed_at = self.pressed_at;
            let display_id = self.display_id;
            window.on_next_frame(move |_, _| {
                tracing::info!(
                    display_id,
                    ms = pressed_at.elapsed().as_secs_f64() * 1000.0,
                    "overlay visible (from hotkey)"
                );
            });
        }

        let vw = self.viewport.width;
        let vh = self.viewport.height;
        let mut root = div()
            .id("overlay")
            .size_full()
            .relative()
            .track_focus(&self.focus)
            .cursor(CursorStyle::Crosshair)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_key_down(cx.listener(Self::on_key_down))
            .child(
                img(ImageSource::Render(self.image.clone()))
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
            );

        let region = self.selector.region();
        let (x0, y0, w, h) = self.to_logical(region);
        let x1 = x0 + w;
        let y1 = y0 + h;
        let band = |x: Pixels, y: Pixels, w: Pixels, h: Pixels| {
            div().absolute().left(x).top(y).w(w).h(h).bg(shade())
        };
        let border = if self.selector.is_hovering() {
            hud::accent()
        } else {
            hud::fg()
        };
        // Hovering the bare desktop targets the whole display; dim it all so the frozen frame
        // never passes for the live screen.
        if self.selector.is_hovering() && region == Rect::new(0, 0, self.width, self.height) {
            root = root.child(band(px(0.), px(0.), vw, vh));
        }
        root = root
            .child(band(px(0.), px(0.), vw, y0))
            .child(band(px(0.), y1, vw, vh - y1))
            .child(band(px(0.), y0, x0, h))
            .child(band(x1, y0, vw - x1, h))
            // A dark hairline outside the white edge keeps it visible on light content.
            .child(
                div()
                    .absolute()
                    .left(x0 - px(1.))
                    .top(y0 - px(1.))
                    .w(w + px(2.))
                    .h(h + px(2.))
                    .border_1()
                    .border_color(hsla(0., 0., 0., 0.35))
                    .child(div().size_full().border(px(1.5)).border_color(border)),
            )
            .child(self.size_label(region, x0, y0, y1))
            .child(self.hints(y1, crate::recording::prefs(cx)));
        if let Some(at) = self.selector.cursor()
            && window.is_window_hovered()
        {
            root = root.child(self.loupe(at, window.mouse_position()));
        }
        root
    }
}
