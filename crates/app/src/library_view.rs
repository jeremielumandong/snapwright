//! The library window: every capture as a thumbnail grid with search, tags and trash, and a
//! detail pane for the selected capture.
//!
//! Queries run on a background thread against the shared [`Library`]; results land here
//! tagged with a generation number so a slow, stale query never overwrites a newer one.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::{Datelike as _, NaiveDate};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _, DropdownButton};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::{Icon, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Bounds, BoxShadow, ClipboardItem, Context,
    Entity, FocusHandle, Focusable as _, FontWeight, Global, InteractiveElement as _, IntoElement,
    KeyDownEvent, ListAlignment, ListState, ObjectFit, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, Subscription, TitlebarOptions,
    Window, WindowBounds, WindowOptions, div, img, list, point, px, size,
};
use snapwright_library::{Capture, Library, Query, Source};

use crate::session::{self, LibraryService};
use crate::theme::{self, Palette};
use crate::{assets, countdown, editor_view, shortcuts_view, toast, tray};

const SIDEBAR_W: f32 = 216.;
const DETAIL_W: f32 = 340.;
/// Grid padding on each side, and the gaps between cells.
const GRID_PAD: f32 = 20.;
const COL_GAP: f32 = 16.;
const ROW_GAP: f32 = 20.;
/// Narrowest a grid cell gets before a column is dropped.
const MIN_CELL_W: f32 = 180.;
const THUMB_H: f32 = 124.;
const HEADER_H: f32 = 52.;
/// Delay for the capture menu's "Capture in 3 seconds".
const DELAY_SECS: u32 = 3;
/// How long the Copy button reads "Copied".
const COPIED_FOR: std::time::Duration = std::time::Duration::from_millis(1500);

/// Bumped whenever the library changes outside this window (a new capture), so an open
/// library window reloads.
#[derive(Default)]
pub struct LibraryChanged(pub u64);
impl Global for LibraryChanged {}

/// Records that the library changed: an open window reloads, and new or edited captures
/// get their text read.
pub fn notify_changed(cx: &mut App) {
    reload_open(cx);
    crate::ocr::wake(cx);
}

/// Makes an open library window reload.
pub fn reload_open(cx: &mut App) {
    let next = cx.default_global::<LibraryChanged>().0 + 1;
    cx.set_global(LibraryChanged(next));
}

/// The open library window, if any.
#[derive(Default)]
struct LibraryWindow(Option<AnyWindowHandle>);
impl Global for LibraryWindow {}

/// A capture the library window should select once it has loaded.
#[derive(Default)]
struct PendingSelect(Option<i64>);
impl Global for PendingSelect {}

/// Opens the library window, or brings it to the front.
pub fn open(cx: &mut App) {
    open_at(None, cx);
}

/// Opens the library window with capture `id` selected.
pub fn open_at(id: Option<i64>, cx: &mut App) {
    if id.is_some() {
        cx.set_global(PendingSelect(id));
        // An open window reloads on this and picks the selection up.
        notify_changed(cx);
    }
    if let Some(h) = cx.default_global::<LibraryWindow>().0
        && h.update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return;
    }
    let Some(library) = cx.global::<LibraryService>().0.clone() else {
        tracing::error!("the capture library is unavailable");
        return;
    };
    let bounds = Bounds::centered(None, size(px(1200.), px(760.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(860.), px(480.))),
        titlebar: Some(TitlebarOptions {
            title: Some("Snapwright Library".into()),
            ..Default::default()
        }),
        app_id: Some("snapwright".into()),
        ..Default::default()
    };
    match gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| LibraryView::new(library, window, cx))
    }) {
        Ok((handle, _)) => cx.set_global(LibraryWindow(Some(handle))),
        Err(e) => tracing::error!(error = %e, "could not open the library window"),
    }
}

/// Opens the newest capture in the editor.
pub fn edit_latest(cx: &mut App) {
    let Some(library) = cx.global::<LibraryService>().0.clone() else {
        return;
    };
    let find = cx.background_executor().spawn(async move {
        let lib = library.lock().ok()?;
        let newest = lib
            .list(&Query {
                limit: 1,
                ..Query::default()
            })
            .ok()?;
        newest.first().map(|c| c.id)
    });
    cx.spawn(async move |cx| match find.await {
        Some(id) => cx.update(|cx| editor_view::open(id, cx)),
        None => tracing::warn!("no capture to edit"),
    })
    .detach();
}

/// What the grid shows.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Filter {
    All,
    Tag(String),
    Trash,
}

/// A capture with its file paths, resolved off the UI thread.
#[derive(Clone)]
struct Item {
    capture: Capture,
    image: PathBuf,
    thumb: PathBuf,
    /// A recording's video file.
    video: Option<PathBuf>,
}

impl Item {
    /// The file the system opens for this capture: the video for a recording.
    fn file(&self) -> &PathBuf {
        self.video.as_ref().unwrap_or(&self.image)
    }
}

/// Opens a capture: a recording plays in the system player, an image opens in the editor.
fn open_item(item: &Item, cx: &mut App) {
    match &item.video {
        Some(video) => cx.open_with_system(video),
        None => editor_view::open(item.capture.id, cx),
    }
}

struct Loaded {
    items: Vec<Item>,
    tags: Vec<(String, usize)>,
    total: usize,
    trashed: usize,
}

pub struct LibraryView {
    library: Arc<Mutex<Library>>,
    search: Entity<InputState>,
    new_tag: Entity<InputState>,
    note: Entity<TextareaState>,
    filter: Filter,
    items: Vec<Item>,
    tags: Vec<(String, usize)>,
    total: usize,
    trashed: usize,
    selected: Option<i64>,
    /// The note field shows another capture's note; refreshed on the next render.
    note_stale: bool,
    /// The "Add tag" chip has turned into a text field.
    adding_tag: bool,
    generation: u64,
    grid_focus: FocusHandle,
    /// The grid as day headers and rows of cells, for `columns` per row.
    rows: Vec<Row>,
    columns: usize,
    cell_w: f32,
    list: ListState,
    /// Item to scroll into view on the next render.
    scroll_to: Option<usize>,
    /// Clear the search box on the next render (which then reloads).
    search_reset: bool,
    /// The capture just put on the clipboard, shown as "Copied" on its Copy button for a moment.
    copied: Option<i64>,
    _subs: Vec<Subscription>,
}

impl LibraryView {
    fn new(library: Arc<Mutex<Library>>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search apps, titles, tags, notes, text in images")
        });
        let new_tag = cx.new(|cx| InputState::new(window, cx).placeholder("Tag name"));
        let note = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 8)
                .placeholder("Add a note")
        });
        let subs = vec![
            cx.subscribe_in(&search, window, |this, _, ev: &InputEvent, _, cx| {
                if let InputEvent::Change = ev {
                    this.reload(cx);
                }
            }),
            cx.subscribe_in(
                &new_tag,
                window,
                |this, input, ev: &InputEvent, window, cx| match ev {
                    InputEvent::PressEnter { .. } => {
                        let name = input.read(cx).value().trim().to_string();
                        input.update(cx, |s, cx| s.set_value("", window, cx));
                        if !name.is_empty() {
                            this.edit(cx, move |lib, id| lib.tag(id, &name));
                        }
                    }
                    InputEvent::Blur => {
                        this.adding_tag = false;
                        cx.notify();
                    }
                    _ => {}
                },
            ),
            // The note saves when it loses focus.
            cx.subscribe_in(&note, window, |this, input, ev: &InputEvent, _, cx| {
                if let InputEvent::Blur = ev {
                    let text = input.read(cx).value().to_string();
                    let unchanged = this.selected_item().is_some_and(|i| i.capture.note == text);
                    if !unchanged {
                        this.edit(cx, move |lib, id| lib.set_note(id, &text));
                    }
                }
            }),
            cx.observe_global_in::<LibraryChanged>(window, |this, _, cx| this.reload(cx)),
            cx.observe_window_appearance(window, |_, window, cx| theme::sync(window, cx)),
            cx.observe_window_activation(window, |_, window, cx| theme::activated(window, cx)),
        ];
        let grid_focus = cx.focus_handle();
        window.focus(&grid_focus, cx);
        let mut view = Self {
            library,
            search,
            new_tag,
            note,
            filter: Filter::All,
            items: Vec::new(),
            tags: Vec::new(),
            total: 0,
            trashed: 0,
            selected: None,
            note_stale: false,
            adding_tag: false,
            generation: 0,
            grid_focus,
            rows: Vec::new(),
            columns: 0,
            cell_w: MIN_CELL_W,
            list: ListState::new(0, ListAlignment::Top, px(400.)),
            scroll_to: None,
            search_reset: false,
            copied: None,
            _subs: subs,
        };
        view.reload(cx);
        view
    }

    fn query(&self, cx: &App) -> Query {
        let text = self.search.read(cx).value().trim().to_owned();
        Query {
            text: (!text.is_empty()).then_some(text),
            tag: match &self.filter {
                Filter::Tag(t) => Some(t.clone()),
                _ => None,
            },
            trash: self.filter == Filter::Trash,
            ..Query::default()
        }
    }

    /// Re-runs the current query off the UI thread.
    fn reload(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        let query = self.query(cx);
        let library = self.library.clone();
        let load = cx.background_executor().spawn(async move {
            let lib = library
                .lock()
                .map_err(|_| anyhow::anyhow!("library poisoned by an earlier panic"))?;
            let items = lib
                .list(&query)?
                .into_iter()
                .map(|c| Item {
                    image: lib.image_path_of(&c),
                    thumb: lib.thumb_path_of(&c),
                    video: lib.video_path_of(&c),
                    capture: c,
                })
                .collect();
            let total = lib.count(&Query::default())?;
            let trashed = lib.count(&Query {
                trash: true,
                ..Query::default()
            })?;
            anyhow::Ok(Loaded {
                items,
                tags: lib.tags()?,
                total,
                trashed,
            })
        });
        cx.spawn(async move |this, cx| {
            let loaded = load.await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match loaded {
                    Ok(l) => {
                        if let Some(id) = cx.default_global::<PendingSelect>().0.take() {
                            match l.items.iter().position(|i| i.capture.id == id) {
                                Some(ix) => {
                                    this.selected = Some(id);
                                    this.note_stale = true;
                                    this.scroll_to = Some(ix);
                                }
                                // Not under this filter or search: show everything and look
                                // again.
                                None if this.filter != Filter::All
                                    || !this.search.read(cx).value().is_empty() =>
                                {
                                    this.filter = Filter::All;
                                    this.search_reset = true;
                                    cx.set_global(PendingSelect(Some(id)));
                                    cx.notify();
                                    return;
                                }
                                None => {}
                            }
                        }
                        this.items = l.items;
                        // Lay the grid out again on the next render.
                        this.columns = 0;
                        this.tags = l.tags;
                        this.total = l.total;
                        this.trashed = l.trashed;
                        if this
                            .selected
                            .is_some_and(|id| !this.items.iter().any(|i| i.capture.id == id))
                        {
                            this.selected = None;
                        }
                    }
                    Err(e) => tracing::error!(error = %e, "loading the library failed"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Applies a change to the selected capture off the UI thread, then reloads.
    fn edit(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut Library, i64) -> snapwright_library::Result<()> + Send + 'static,
    ) {
        let Some(id) = self.selected else {
            return;
        };
        let library = self.library.clone();
        let task = cx.background_executor().spawn(async move {
            let mut lib = library
                .lock()
                .map_err(|_| anyhow::anyhow!("library poisoned by an earlier panic"))?;
            change(&mut lib, id)?;
            anyhow::Ok(())
        });
        cx.spawn(async move |this, cx| {
            if let Err(e) = task.await {
                tracing::error!(error = %e, "changing the capture failed");
            }
            let _ = this.update(cx, |this, cx| this.reload(cx));
        })
        .detach();
    }

    fn empty_trash(&mut self, cx: &mut Context<Self>) {
        let library = self.library.clone();
        let task = cx.background_executor().spawn(async move {
            let mut lib = library
                .lock()
                .map_err(|_| anyhow::anyhow!("library poisoned by an earlier panic"))?;
            anyhow::Ok(lib.purge_trash()?)
        });
        cx.spawn(async move |this, cx| {
            match task.await {
                Ok(n) => tracing::info!(removed = n, "emptied the trash"),
                Err(e) => tracing::error!(error = %e, "emptying the trash failed"),
            }
            let _ = this.update(cx, |this, cx| this.reload(cx));
        })
        .detach();
    }

    fn selected_item(&self) -> Option<&Item> {
        let id = self.selected?;
        self.items.iter().find(|i| i.capture.id == id)
    }

    fn select(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(id);
        self.adding_tag = false;
        let note = self
            .selected_item()
            .map(|i| i.capture.note.clone())
            .unwrap_or_default();
        self.note.update(cx, |s, cx| s.set_value(note, window, cx));
        cx.notify();
    }

    fn set_filter(&mut self, filter: Filter, cx: &mut Context<Self>) {
        self.filter = filter;
        self.selected = None;
        self.reload(cx);
    }

    fn copy_selected(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.selected_item() else {
            return;
        };
        let id = item.capture.id;
        let copy = session::copy_file_to_clipboard(item.image.clone(), cx);
        cx.spawn(async move |this, cx| {
            if let Err(e) = copy.await {
                tracing::error!(error = %e, "copying the capture failed");
                return;
            }
            tracing::info!(id, "capture copied");
            let _ = this.update(cx, |this, cx| {
                this.copied = Some(id);
                cx.notify();
            });
            cx.background_executor().timer(COPIED_FOR).await;
            let _ = this.update(cx, |this, cx| {
                if this.copied == Some(id) {
                    this.copied = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn trash_or_restore(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.selected_item() else {
            return;
        };
        if item.capture.trashed {
            self.edit(cx, |lib, id| lib.restore(id));
        } else {
            self.edit(cx, |lib, id| lib.trash(id));
        }
    }

    fn on_key_down(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let mods = e.keystroke.modifiers;
        let key = e.keystroke.key.as_str();
        if key == "f" && mods.secondary() {
            let focus = self.search.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
            cx.stop_propagation();
            return;
        }
        // Keys typed into the search field, tag field or note are theirs.
        if !self.grid_focus.is_focused(window) {
            if key == "escape" {
                window.focus(&self.grid_focus, cx);
            }
            return;
        }
        match key {
            "delete" | "backspace" => self.trash_or_restore(cx),
            "c" if mods.secondary() => self.copy_selected(cx),
            "enter" | "e" if !mods.secondary() => {
                if let Some(item) = self.selected_item().cloned() {
                    open_item(&item, cx);
                }
            }
            key @ ("left" | "right" | "up" | "down") => {
                let at = self
                    .selected
                    .and_then(|id| self.items.iter().position(|i| i.capture.id == id));
                let next = match (at, key) {
                    (None, _) => Some(0),
                    (Some(i), "left") => Some(i.saturating_sub(1)),
                    (Some(i), "right") => Some((i + 1).min(self.items.len().saturating_sub(1))),
                    (Some(i), "up") => step_vertical(&self.rows, i, true),
                    (Some(i), _) => step_vertical(&self.rows, i, false),
                };
                if let Some(next) = next
                    && let Some(id) = self.items.get(next).map(|i| i.capture.id)
                {
                    self.select(id, window, cx);
                    self.scroll_to = Some(next);
                }
            }
            _ => {}
        }
    }

    /// Thumbnails per grid row at the current window width.
    /// Thumbnails per grid row and their width at the current window width.
    fn columns_for(window: &Window) -> (usize, f32) {
        let width = f32::from(window.viewport_size().width) - SIDEBAR_W - DETAIL_W - GRID_PAD * 2.;
        let columns = ((width + COL_GAP) / (MIN_CELL_W + COL_GAP)).floor().max(1.);
        let cell_w = (width - COL_GAP * (columns - 1.)) / columns;
        (columns as usize, cell_w.max(MIN_CELL_W))
    }

    /// Lays the grid out again when the items or the column count changed.
    fn layout_grid(&mut self, window: &Window) {
        let (columns, cell_w) = Self::columns_for(window);
        self.cell_w = cell_w;
        if columns == self.columns {
            return;
        }
        self.columns = columns;
        let days: Vec<NaiveDate> = self
            .items
            .iter()
            .map(|i| local_day(i.capture.created_ms))
            .collect();
        self.rows = layout_rows(&days, columns, local_today());
        self.list.reset(self.rows.len());
    }

    /// Starts a capture from the library window, keeping the window out of it where the OS
    /// would not.
    fn capture(how: CaptureKind, window: &mut Window, cx: &mut App) {
        let hide = !snapwright_capture::EXCLUDES_OWN_WINDOWS;
        if hide {
            window.minimize_window();
        }
        cx.spawn(async move |cx| {
            if hide {
                cx.background_executor().timer(toast::SETTLE).await;
            }
            cx.update(|cx| match how {
                CaptureKind::Region => session::start(Instant::now(), cx),
                CaptureKind::Delayed => countdown::start(DELAY_SECS, cx),
                CaptureKind::Repeat => session::repeat_last(Instant::now(), cx),
            });
        })
        .detach();
    }

    fn render_sidebar(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let row = |id: SharedString, icon: IconName, label: String, count: usize, active: bool| {
            div()
                .id(id)
                .h(px(30.))
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(8.))
                .rounded(px(6.))
                .cursor_pointer()
                .when(active, |d| d.bg(p.sel).font_weight(FontWeight::MEDIUM))
                .when(!active, |d| d.text_color(p.fg2).hover(|d| d.bg(p.hover)))
                .child(Icon::new(icon).size(px(15.)))
                .child(div().flex_1().truncate().child(label))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(p.fg3)
                        .font_weight(FontWeight::NORMAL)
                        .child(count.to_string()),
                )
        };
        let brand = div()
            .h(px(HEADER_H))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(16.))
            .child(img(assets::BRAND).size(px(22.)).rounded(px(5.)))
            .child(
                div()
                    .text_size(px(14.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Snapwright"),
            );
        let nav = div()
            .flex()
            .flex_col()
            .gap(px(1.))
            .px(px(8.))
            .py(px(4.))
            .child(
                row(
                    "all".into(),
                    IconName::Images,
                    "All captures".into(),
                    self.total,
                    self.filter == Filter::All,
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_filter(Filter::All, cx))),
            )
            .child(
                row(
                    "trash".into(),
                    IconName::Trash,
                    "Trash".into(),
                    self.trashed,
                    self.filter == Filter::Trash,
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_filter(Filter::Trash, cx))),
            );
        let mut tags = div().flex().flex_col().gap(px(1.)).px(px(8.));
        for (i, (tag, n)) in self.tags.iter().enumerate() {
            let active = self.filter == Filter::Tag(tag.clone());
            let name = tag.clone();
            tags =
                tags.child(
                    row(
                        format!("tag-{i}").into(),
                        IconName::Hash,
                        tag.clone(),
                        *n,
                        active,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_filter(Filter::Tag(name.clone()), cx)
                    })),
                );
        }
        div()
            .w(px(SIDEBAR_W))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(p.panel)
            .border_r_1()
            .border_color(p.line)
            .child(brand)
            .child(nav)
            .when(!self.tags.is_empty(), |d| {
                d.child(
                    theme::section_label("Tags", p)
                        .pt(px(18.))
                        .pb(px(6.))
                        .px(px(16.)),
                )
                .child(tags)
            })
            .child(div().flex_1())
            .child(self.render_hotkey_card(p, cx))
    }

    /// How to take a capture: the hotkey as keycaps (a Hyprland bind counts), or the
    /// compositor binding on other Wayland compositors.
    fn render_hotkey_card(&self, p: &Palette, cx: &App) -> impl IntoElement {
        let label = cx
            .try_global::<tray::Labels>()
            .and_then(|l| l.capture.clone());
        let body = match label {
            Some(label) => div()
                .flex()
                .gap(px(4.))
                .children(hotkey_keys(&label).into_iter().map(|k| theme::keycap(k, p)))
                .into_any_element(),
            None => div()
                .text_size(px(11.5))
                .text_color(p.fg3)
                .child("Bind snapwright-app --capture-now to a key in your compositor")
                .into_any_element(),
        };
        div()
            .m(px(12.))
            .p(px(12.))
            .rounded(px(8.))
            .border_1()
            .border_color(p.line)
            .bg(p.bg)
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.))
                            .font_weight(FontWeight::MEDIUM)
                            .child("Capture a region"),
                    )
                    .child(
                        Button::new("shortcuts")
                            .ghost()
                            .xsmall()
                            .label("Change")
                            .tooltip("Keyboard shortcuts")
                            .on_click(|_, _, cx| shortcuts_view::open(cx)),
                    ),
            )
            .child(body)
    }

    fn render_header(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let search_key = if cfg!(target_os = "macos") {
            "⌘F".to_owned()
        } else {
            format!("{} F", theme::key_label("secondary"))
        };
        let search = div()
            .flex_1()
            .h(px(32.))
            .flex()
            .items_center()
            .px(px(10.))
            .rounded(px(8.))
            .bg(p.hover)
            .child(
                Input::new(&self.search)
                    .appearance(false)
                    .cleanable(true)
                    .prefix(Icon::new(IconName::Search).size(px(15.)).text_color(p.fg3))
                    .suffix(theme::key_hint(search_key, p)),
            );
        let capture = DropdownButton::new("capture")
            .primary()
            .button(
                Button::new("capture-region")
                    .icon(Icon::new(IconName::Scan))
                    .label("Capture")
                    .on_click(|_, window, cx| Self::capture(CaptureKind::Region, window, cx)),
            )
            .dropdown_menu(|menu, _, _| {
                menu.item(
                    PopupMenuItem::new("Capture region")
                        .icon(Icon::new(IconName::Scan))
                        .on_click(|_, window, cx| Self::capture(CaptureKind::Region, window, cx)),
                )
                .item(
                    PopupMenuItem::new(format!("Capture in {DELAY_SECS} seconds"))
                        .icon(Icon::new(IconName::Timer))
                        .on_click(|_, window, cx| Self::capture(CaptureKind::Delayed, window, cx)),
                )
                .item(
                    PopupMenuItem::new("Repeat last region")
                        .icon(Icon::new(IconName::Repeat2))
                        .on_click(|_, window, cx| Self::capture(CaptureKind::Repeat, window, cx)),
                )
            });
        div()
            .h(px(HEADER_H))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(16.))
            .border_b_1()
            .border_color(p.line)
            .child(search)
            .when(
                self.filter == Filter::Trash && !self.items.is_empty(),
                |d| {
                    d.child(
                        Button::new("empty-trash")
                            .danger()
                            .label("Empty Trash")
                            .on_click(cx.listener(|this, _, _, cx| this.empty_trash(cx))),
                    )
                },
            )
            .child(capture)
    }

    fn render_cell(&self, item: &Item, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let c = &item.capture;
        let selected = self.selected == Some(c.id);
        let id = c.id;
        let (w, h) = c.shown_size();
        div()
            .id(("capture", c.id as u64))
            .group("cell")
            .w(px(self.cell_w))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(8.))
            .cursor_pointer()
            .on_click(
                cx.listener(move |this, e: &gpui_kit::ClickEvent, window, cx| {
                    window.focus(&this.grid_focus, cx);
                    this.select(id, window, cx);
                    if e.click_count() >= 2
                        && let Some(item) = this.selected_item()
                    {
                        cx.open_with_system(item.file());
                    }
                }),
            )
            .child(
                div()
                    .relative()
                    .h(px(THUMB_H))
                    .w_full()
                    .p(px(10.))
                    .overflow_hidden()
                    .rounded(px(10.))
                    .bg(p.thumb)
                    .border_1()
                    .border_color(p.line)
                    .when(selected, |d| d.shadow(ring(p)))
                    .when(!selected, |d| {
                        d.group_hover("cell", |d| d.border_color(p.line2))
                    })
                    .child(fitted(item.thumb.clone()))
                    .when_some(c.video.as_ref(), |d, v| {
                        d.child(
                            div()
                                .absolute()
                                .right(px(8.))
                                .bottom(px(8.))
                                .h(px(20.))
                                .flex()
                                .items_center()
                                .gap(px(4.))
                                .px(px(7.))
                                .rounded(px(5.))
                                .bg(gpui_kit::hsla(240. / 360., 0.09, 0.1, 0.78))
                                .text_size(px(11.))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(gpui_kit::white())
                                .child(Icon::new(IconName::Play).size(px(10.)))
                                .child(format!(
                                    "{} {}",
                                    v.ext.to_uppercase(),
                                    crate::toast::clock(v.duration_ms)
                                )),
                        )
                    })
                    .when(c.edit.is_some(), |d| {
                        d.child(
                            div()
                                .absolute()
                                .left(px(8.))
                                .top(px(8.))
                                .h(px(20.))
                                .flex()
                                .items_center()
                                .gap(px(4.))
                                .px(px(7.))
                                .rounded(px(5.))
                                .bg(gpui_kit::hsla(240. / 360., 0.09, 0.1, 0.78))
                                .text_size(px(11.))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(gpui_kit::white())
                                .child(Icon::new(IconName::Pencil).size(px(11.)))
                                .child("Edited"),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .px(px(2.))
                    .min_w_0()
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .truncate()
                            .text_color(if selected { p.accent } else { p.fg })
                            .child(title(c)),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .text_size(px(12.))
                            .text_color(p.fg3)
                            .child(div().flex_1().min_w_0().truncate().child(cell_meta(c)))
                            .child(format!("{w}×{h}")),
                    ),
            )
    }

    fn render_row(&self, ix: usize, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        match self.rows.get(ix) {
            Some(Row::Day { label, count }) => div()
                .flex()
                .items_baseline()
                .gap(px(8.))
                .px(px(GRID_PAD))
                .pt(px(14.))
                .pb(px(12.))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(label.clone()))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(p.fg3)
                        .child(captures(*count)),
                )
                .into_any_element(),
            Some(Row::Cells(range)) => {
                let cells: Vec<AnyElement> = self.items[range.clone()]
                    .iter()
                    .map(|item| self.render_cell(item, p, cx).into_any_element())
                    .collect();
                div()
                    .flex()
                    .gap(px(COL_GAP))
                    .px(px(GRID_PAD))
                    .pb(px(ROW_GAP))
                    .children(cells)
                    .into_any_element()
            }
            None => div().into_any_element(),
        }
    }

    fn render_grid(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        if self.items.is_empty() {
            let message = match (&self.filter, self.search.read(cx).value().is_empty()) {
                (Filter::Trash, _) => "The trash is empty.".to_owned(),
                (_, false) => "No captures match.".to_owned(),
                _ => "No captures yet. Take one with the Capture button or the hotkey.".to_owned(),
            };
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(p.fg3)
                .child(message)
                .into_any_element();
        }
        let view = cx.entity();
        let p = *p;
        list(self.list.clone(), move |ix, _, cx| {
            view.update(cx, |this, cx| this.render_row(ix, &p, cx))
        })
        .flex_1()
        .pt(px(4.))
        .into_any_element()
    }

    fn render_detail(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = div()
            .w(px(DETAIL_W))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(p.line)
            .bg(p.bg);
        let Some(item) = self.selected_item() else {
            return pane
                .items_center()
                .justify_center()
                .text_color(p.fg3)
                .child("Select a capture to see it here.")
                .into_any_element();
        };
        let c = &item.capture;
        let line = |label: &'static str, value: AnyElement| {
            div()
                .flex()
                .items_center()
                .child(div().w(px(84.)).flex_none().text_color(p.fg3).child(label))
                .child(div().flex_1().min_w_0().truncate().child(value))
        };
        let text = |s: String| div().child(s).into_any_element();
        let (source_icon, source) = match c.meta.source {
            Source::Region => (IconName::SquareDashed, "Region"),
            Source::Window => (IconName::AppWindow, "Window"),
            Source::Display => (IconName::Monitor, "Whole display"),
            Source::Import => (IconName::FileImage, "Imported"),
        };
        let (w, h) = c.shown_size();
        let meta = div()
            .flex()
            .flex_col()
            .gap(px(9.))
            .text_size(px(12.5))
            .child(line("Captured", text(when(c.created_ms))))
            .child(line("Dimensions", text(format!("{w} × {h} px"))))
            .when_some(c.video.as_ref(), |d, v| {
                d.child(line("Length", text(crate::toast::clock(v.duration_ms))))
            })
            .child(line(
                "File size",
                text(human_bytes(
                    c.video.as_ref().map_or(c.shown_bytes(), |v| v.bytes),
                )),
            ))
            .child(line(
                "Source",
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(Icon::new(source_icon).size(px(13.)).text_color(p.fg2))
                    .child(source)
                    .into_any_element(),
            ))
            .when(c.edit.is_some(), |d| {
                d.child(line(
                    "Edits",
                    div()
                        .flex()
                        .gap(px(8.))
                        .child("Annotated")
                        .child(
                            div()
                                .id("revert")
                                .cursor_pointer()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(p.accent)
                                .hover(|d| d.text_color(p.fg))
                                .child("Revert")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.edit(cx, |lib, id| lib.revert_edit(id));
                                })),
                        )
                        .into_any_element(),
                ))
            })
            .when(!c.meta.url.is_empty(), |d| {
                d.child(line("URL", text(c.meta.url.clone())))
            });

        let mut chips = div().flex().flex_wrap().gap(px(6.));
        for (i, tag) in c.tags.iter().enumerate() {
            let name = tag.clone();
            chips = chips.child(
                div()
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .pl(px(9.))
                    .pr(px(4.))
                    .rounded(px(6.))
                    .bg(p.hover)
                    .text_size(px(12.))
                    .child(tag.clone())
                    .child(
                        div()
                            .id(("untag", i))
                            .size(px(16.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.))
                            .cursor_pointer()
                            .text_color(p.fg3)
                            .hover(|d| d.text_color(p.fg).bg(p.sel))
                            .child(Icon::new(IconName::X).size(px(11.)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let name = name.clone();
                                this.edit(cx, move |lib, id| lib.untag(id, &name));
                            })),
                    ),
            );
        }
        chips = if self.adding_tag {
            chips.child(div().w(px(140.)).child(Input::new(&self.new_tag).small()))
        } else {
            chips.child(
                div()
                    .id("add-tag")
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .px(px(9.))
                    .rounded(px(6.))
                    .border_1()
                    .border_dashed()
                    .border_color(p.line2)
                    .text_size(px(12.))
                    .text_color(p.fg3)
                    .cursor_pointer()
                    .hover(|d| d.text_color(p.fg))
                    .child(Icon::new(IconName::Plus).size(px(11.)))
                    .child("Add tag")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.adding_tag = true;
                        let focus = this.new_tag.read(cx).focus_handle(cx);
                        window.focus(&focus, cx);
                        cx.notify();
                    })),
            )
        };

        let icon_button = |id: &'static str, icon: IconName, tip: &'static str| {
            Button::new(id).outline().icon(Icon::new(icon)).tooltip(tip)
        };
        let actions = div()
            .flex()
            .gap(px(6.))
            .px(px(16.))
            .py(px(12.))
            .border_b_1()
            .border_color(p.line)
            .child(
                Button::new("copy")
                    .primary()
                    .flex_1()
                    .map(|b| {
                        if self.copied == Some(c.id) {
                            b.icon(Icon::new(IconName::Check)).label("Copied")
                        } else {
                            b.icon(Icon::new(IconName::Copy)).label("Copy")
                        }
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.copy_selected(cx))),
            )
            .child(if c.video.is_some() {
                Button::new("play")
                    .outline()
                    .flex_1()
                    .icon(Icon::new(IconName::Play))
                    .label("Play")
                    .tooltip("Play in the system player (Enter)")
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(item) = this.selected_item().cloned() {
                            open_item(&item, cx);
                        }
                    }))
            } else {
                Button::new("edit")
                    .outline()
                    .flex_1()
                    .icon(Icon::new(IconName::Pencil))
                    .label("Edit")
                    .tooltip("Annotate, redact or crop (E)")
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(id) = this.selected {
                            editor_view::open(id, cx);
                        }
                    }))
            })
            .child(
                icon_button("open", IconName::ExternalLink, "Open").on_click(cx.listener(
                    |this, _, _, cx| {
                        if let Some(item) = this.selected_item() {
                            cx.open_with_system(item.file());
                        }
                    },
                )),
            )
            .child(
                icon_button("reveal", IconName::FolderOpen, "Show file").on_click(cx.listener(
                    |this, _, _, cx| {
                        if let Some(item) = this.selected_item() {
                            cx.reveal_path(item.file());
                        }
                    },
                )),
            );

        let footer = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(16.))
            .py(px(10.))
            .border_t_1()
            .border_color(p.line)
            .child(
                div()
                    .id("trash-restore")
                    .h(px(28.))
                    .ml(px(-8.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(8.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .hover(|d| d.bg(p.hover))
                    .text_size(px(12.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if c.trashed { p.fg } else { p.danger })
                    .child(Icon::new(IconName::Trash).size(px(14.)))
                    .child(if c.trashed {
                        "Restore"
                    } else {
                        "Move to Trash"
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.trash_or_restore(cx))),
            )
            .child(div().flex_1())
            .child(theme::key_hint(theme::key_label("backspace").to_owned(), p));

        pane.child(
            div()
                .h(px(212.))
                .flex_none()
                .p(px(20.))
                .bg(p.canvas)
                .border_b_1()
                .border_color(p.line)
                .child(fitted(item.image.clone())),
        )
        .child(actions)
        .child(
            div()
                .id("detail-body")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(px(20.))
                .p(px(16.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(3.))
                        .child(
                            div()
                                .text_size(px(15.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(title(c)),
                        )
                        .when(!c.meta.app.is_empty(), |d| {
                            d.child(div().text_color(p.fg2).child(c.meta.app.clone()))
                        }),
                )
                .child(meta)
                .when(!c.text.is_empty(), |d| d.child(self.render_text(c, p, cx)))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .child(theme::section_label("Tags", p))
                        .child(chips),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .child(theme::section_label("Note", p))
                        .child(Textarea::new(&self.note)),
                ),
        )
        .child(footer)
        .into_any_element()
    }
}

impl LibraryView {
    /// The text read from the capture, the first lines of it, with a link to copy it all.
    fn render_text(&self, c: &Capture, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let (lines, more) = excerpt(&c.text, TEXT_LINES);
        let all = c.text.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(theme::section_label("Text", p))
                    .child(
                        div()
                            .id("copy-text")
                            .cursor_pointer()
                            .text_size(px(12.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(p.accent)
                            .hover(|d| d.text_color(p.fg))
                            .child("Copy text")
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(all.clone()));
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .text_size(px(12.))
                    .text_color(p.fg2)
                    .children(
                        lines
                            .into_iter()
                            .map(|l| div().min_w_0().truncate().child(l.to_owned())),
                    )
                    .when(more > 0, |d| {
                        d.child(
                            div()
                                .text_color(p.fg3)
                                .child(format!("and {more} more {}", plural(more, "line"))),
                        )
                    }),
            )
    }
}

impl Render for LibraryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.note_stale) {
            let note = self
                .selected_item()
                .map(|i| i.capture.note.clone())
                .unwrap_or_default();
            self.note.update(cx, |s, cx| s.set_value(note, window, cx));
        }
        if std::mem::take(&mut self.search_reset) {
            // Clearing emits a change, which reloads; reload anyway in case it was empty.
            self.search.update(cx, |s, cx| s.set_value("", window, cx));
            self.reload(cx);
        }
        self.layout_grid(window);
        if let Some(ix) = self.scroll_to.take()
            && let Some(row) = row_of(&self.rows, ix)
        {
            self.list.scroll_to_reveal_item(row);
        }
        let p = theme::palette(cx);
        div()
            .size_full()
            .flex()
            .bg(p.bg)
            .text_color(p.fg)
            .text_size(px(13.))
            .child(self.render_sidebar(&p, cx))
            .child(
                div()
                    .id("grid-area")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .track_focus(&self.grid_focus)
                    .on_key_down(cx.listener(Self::on_key_down))
                    .child(self.render_header(&p, cx))
                    .child(self.render_grid(&p, cx)),
            )
            .child(self.render_detail(&p, cx))
    }
}

/// What the capture button and its menu start.
#[derive(Debug, Clone, Copy)]
enum CaptureKind {
    Region,
    Delayed,
    Repeat,
}

/// A row of the grid: a day's header, or the cells (item indexes) of one row.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Row {
    Day { label: String, count: usize },
    Cells(Range<usize>),
}

/// The grid for items taken on `days` (newest first), `columns` to a row, under a header
/// for each day.
fn layout_rows(days: &[NaiveDate], columns: usize, today: NaiveDate) -> Vec<Row> {
    let columns = columns.max(1);
    let mut rows = Vec::new();
    let mut start = 0;
    while start < days.len() {
        let day = days[start];
        let end = days[start..]
            .iter()
            .position(|d| *d != day)
            .map_or(days.len(), |n| start + n);
        rows.push(Row::Day {
            label: day_label(day, today),
            count: end - start,
        });
        let mut at = start;
        while at < end {
            let next = (at + columns).min(end);
            rows.push(Row::Cells(at..next));
            at = next;
        }
        start = end;
    }
    rows
}

/// The grid row holding item `ix`.
fn row_of(rows: &[Row], ix: usize) -> Option<usize> {
    rows.iter()
        .position(|r| matches!(r, Row::Cells(range) if range.contains(&ix)))
}

/// The item above or below item `ix`, in the same column where the next row is long enough
/// (day headers are skipped).
fn step_vertical(rows: &[Row], ix: usize, up: bool) -> Option<usize> {
    let at = row_of(rows, ix)?;
    let Row::Cells(here) = &rows[at] else {
        return None;
    };
    let column = ix - here.start;
    let mut others: Box<dyn Iterator<Item = &Row>> = if up {
        Box::new(rows[..at].iter().rev())
    } else {
        Box::new(rows[at + 1..].iter())
    };
    others.find_map(|r| match r {
        Row::Cells(range) => Some((range.start + column).min(range.end - 1)),
        Row::Day { .. } => None,
    })
}

fn local_day(ms: i64) -> NaiveDate {
    use chrono::TimeZone as _;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map_or(NaiveDate::MIN, |t| t.date_naive())
}

fn local_today() -> NaiveDate {
    chrono::Local::now().date_naive()
}

/// "Today", "Yesterday", the weekday within a week, then the date.
fn day_label(day: NaiveDate, today: NaiveDate) -> String {
    match (today - day).num_days() {
        0 => "Today".to_owned(),
        1 => "Yesterday".to_owned(),
        2..=6 => day.format("%A").to_string(),
        _ if day.year() == today.year() => day.format("%-d %B").to_string(),
        _ => day.format("%-d %B %Y").to_string(),
    }
}

fn captures(n: usize) -> String {
    if n == 1 {
        "1 capture".to_owned()
    } else {
        format!("{n} captures")
    }
}

/// An image scaled to fit inside its parent's padding, centred.
fn fitted(path: PathBuf) -> impl IntoElement {
    div().size_full().relative().child(
        img(path)
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .object_fit(ObjectFit::Contain),
    )
}

/// The selection ring around a grid thumbnail: the accent, then a soft halo.
fn ring(p: &Palette) -> Vec<BoxShadow> {
    let spread = |color, width: f32| BoxShadow {
        color,
        offset: point(px(0.), px(0.)),
        blur_radius: px(0.),
        spread_radius: px(width),
        inset: false,
    };
    vec![spread(p.accent_soft, 5.), spread(p.accent, 2.)]
}

/// A hotkey label like `Ctrl+Shift+2` as keycaps.
pub(crate) fn hotkey_keys(label: &str) -> Vec<String> {
    label
        .split('+')
        .map(|k| match k {
            "Cmd" | "Command" | "Super" if cfg!(target_os = "macos") => "⌘".to_owned(),
            "Alt" | "Option" if cfg!(target_os = "macos") => "⌥".to_owned(),
            "Ctrl" if cfg!(target_os = "macos") => "⌃".to_owned(),
            "Shift" => "⇧".to_owned(),
            other => other.to_owned(),
        })
        .collect()
}

/// What a capture shows: its window title, else its app, else what kind of capture it was.
pub(crate) fn title(c: &Capture) -> String {
    if !c.meta.window_title.is_empty() {
        c.meta.window_title.clone()
    } else if !c.meta.app.is_empty() {
        c.meta.app.clone()
    } else {
        match c.meta.source {
            Source::Display => "Whole display".to_owned(),
            Source::Window => "Window".to_owned(),
            Source::Import => "Imported image".to_owned(),
            Source::Region => "Region".to_owned(),
        }
    }
}

/// The line under a grid thumbnail's title: the app (when the title is not already the app)
/// and the time.
fn cell_meta(c: &Capture) -> String {
    use chrono::TimeZone as _;
    let time = chrono::Local
        .timestamp_millis_opt(c.created_ms)
        .single()
        .map(|t| t.format("%H:%M").to_string())
        .unwrap_or_default();
    if c.meta.app.is_empty() || c.meta.window_title.is_empty() {
        time
    } else {
        format!("{} · {time}", c.meta.app)
    }
}

/// Local date and time of a capture, like `12 Oct 2026, 14:05`.
fn when(ms: i64) -> String {
    use chrono::TimeZone as _;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%-d %b %Y, %H:%M").to_string())
        .unwrap_or_default()
}

/// Lines of recognised text the inspector shows.
const TEXT_LINES: usize = 8;

/// The first `max` non-empty lines of `text`, and how many more there are.
fn excerpt(text: &str, max: usize) -> (Vec<&str>, usize) {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let shown: Vec<&str> = lines.by_ref().take(max).collect();
    (shown, lines.count())
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        word.to_owned()
    } else {
        format!("{word}s")
    }
}

fn human_bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / f64::from(1 << 20)),
        n if n >= 1 << 10 => format!("{:.0} KB", n as f64 / 1024.0),
        n => format!("{n} bytes"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snapwright_library::Metadata;

    fn capture(app: &str, title: &str, source: Source) -> Capture {
        Capture {
            id: 1,
            hash: String::new(),
            created_ms: 0,
            width: 1,
            height: 1,
            bytes: 0,
            meta: Metadata {
                source,
                app: app.into(),
                window_title: title.into(),
                ..Metadata::default()
            },
            note: String::new(),
            tags: Vec::new(),
            text: String::new(),
            trashed: false,
            edit: None,
            video: None,
        }
    }

    #[test]
    fn titles_lead_with_the_window_title() {
        assert_eq!(title(&capture("Figma", "Login", Source::Window)), "Login");
        assert_eq!(title(&capture("Figma", "", Source::Region)), "Figma");
        assert_eq!(title(&capture("", "", Source::Display)), "Whole display");
        assert!(cell_meta(&capture("Figma", "Login", Source::Window)).starts_with("Figma · "));
        assert!(!cell_meta(&capture("Figma", "", Source::Region)).contains("Figma"));
    }

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, d).unwrap()
    }

    #[test]
    fn grid_rows_group_by_day() {
        let days = [day(12), day(12), day(12), day(11), day(1)];
        let rows = layout_rows(&days, 2, day(12));
        assert_eq!(
            rows,
            vec![
                Row::Day {
                    label: "Today".into(),
                    count: 3
                },
                Row::Cells(0..2),
                Row::Cells(2..3),
                Row::Day {
                    label: "Yesterday".into(),
                    count: 1
                },
                Row::Cells(3..4),
                Row::Day {
                    label: "1 October".into(),
                    count: 1
                },
                Row::Cells(4..5),
            ]
        );
        assert_eq!(row_of(&rows, 2), Some(2));
        assert_eq!(day_label(day(8), day(12)), "Thursday");
        assert_eq!(
            day_label(NaiveDate::from_ymd_opt(2025, 3, 4).unwrap(), day(12)),
            "4 March 2025"
        );
    }

    #[test]
    fn arrows_move_between_rows_across_days() {
        let days = [day(12), day(12), day(12), day(11), day(11)];
        let rows = layout_rows(&days, 2, day(12));
        // Down from the second column of a full row into a one-item row: the last item.
        assert_eq!(step_vertical(&rows, 1, false), Some(2));
        // Down across the day header keeps the column.
        assert_eq!(step_vertical(&rows, 2, false), Some(3));
        assert_eq!(step_vertical(&rows, 4, true), Some(2));
        assert_eq!(step_vertical(&rows, 0, true), None);
        assert_eq!(step_vertical(&rows, 4, false), None);
    }

    #[test]
    fn hotkeys_read_as_keycaps() {
        assert_eq!(
            hotkey_keys("Ctrl+Shift+2").last().map(String::as_str),
            Some("2")
        );
        assert!(hotkey_keys("Ctrl+Shift+2").contains(&"⇧".to_owned()));
    }

    #[test]
    fn text_excerpts_skip_blank_lines_and_count_the_rest() {
        let (shown, more) = excerpt("one\n\n  two \nthree\nfour", 2);
        assert_eq!((shown, more), (vec!["one", "two"], 2));
        assert_eq!(excerpt("", 8), (vec![], 0));
        assert_eq!(plural(1, "line"), "line");
        assert_eq!(plural(3, "line"), "lines");
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(human_bytes(512), "512 bytes");
        assert_eq!(human_bytes(20 * 1024), "20 KB");
        assert_eq!(human_bytes(3 * 1024 * 1024 + 300 * 1024), "3.3 MB");
    }
}
