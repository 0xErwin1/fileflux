//! The viewer window's view: top bar, the image stage, the info panel, and
//! the keyboard and mouse handling that drive the pure logic in `nav`,
//! `zoom` and `rotation`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use bolt::controls::Button;
use bolt::icon::IconSource;
use bolt::primitives::Icon;
use bolt::tokens::Spacing;
use gpui::{
    AnyElement, Context, Div, FocusHandle, Focusable, ImageSource, ImgResourceLoader,
    InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    NavigationDirection, ObjectFit, ParentElement as _, Pixels, Render, RenderImage, Resource,
    ScrollWheelEvent, Styled as _, StyledImage as _, Window, div, img, px,
};

use super::actions::{
    Close, First, FitToWindow, Last, Next, OpenFolder, Previous, RotateLeft, RotateRight,
    ToggleFullscreen, ToggleInfo, VIEWER_CONTEXT, ZoomActual, ZoomIn, ZoomOut,
};
use super::facts;
use super::header;
use super::nav::{self, Step};
use super::rotation::{Rotation, render_image, rotate_pixels};
use super::zoom::{Pt, ViewState, percent_label};

/// Height of the top bar.
const BAR_HEIGHT: f32 = 40.0;
/// Width of the info panel.
const INFO_WIDTH: f32 = 300.0;
/// Pixels a wheel line counts for, and lines per physical wheel click (X11
/// and Wayland report one click as three lines): one click is one notch.
const WHEEL_LINE: f32 = 40.0;
const WHEEL_NOTCH: f32 = WHEEL_LINE * 3.0;

/// A Lucide icon as a Bolt icon source.
fn icon(name: gpui_kit_assets::IconName) -> IconSource {
    IconSource::Svg(name.path())
}

/// What the current file turned out to be.
enum Shown {
    Loading,
    Ready(Ready),
    TooLarge(u32, u32),
    Failed,
}

#[derive(Clone)]
struct Ready {
    image: Arc<RenderImage>,
    /// The decoded size in pixels.
    width: u32,
    height: u32,
    /// Decoded pixels per logical pixel: GPUI rasterizes vector images at
    /// twice their size, so a 300 px wide SVG decodes to 600 px.
    density: f32,
    frames: usize,
}

impl Ready {
    /// The size the image is meant to be shown at, before zooming.
    fn logical(&self) -> Pt {
        Pt::new(
            self.width as f32 / self.density,
            self.height as f32 / self.density,
        )
    }
}

/// What a header probe allows for one image.
enum Screen {
    /// The header is still being read.
    Pending,
    /// Over the size cap: shown as a notice, never decoded.
    Refused(u32, u32),
    Allowed,
}

/// How many decoded pixels make one logical pixel for the file at `path`.
fn density_of(path: &Path) -> f32 {
    let is_svg = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("svg"));
    if is_svg {
        gpui::SMOOTH_SVG_SCALE_FACTOR
    } else {
        1.0
    }
}

/// A rotated copy of the current image, as pixels GPUI draws directly.
struct Rotated {
    image: Arc<RenderImage>,
    /// The rotated size in logical pixels.
    size: Pt,
}

/// A pan in progress.
struct Drag {
    pointer: Pt,
    offset: Pt,
}

struct FileMeta {
    size: Option<u64>,
    modified: Option<SystemTime>,
}

pub(crate) struct Viewer {
    list: Vec<PathBuf>,
    index: usize,
    focus: FocusHandle,
    view: ViewState,
    rotation: Rotation,
    rotated: Option<Rotated>,
    rotation_job: u64,
    rotating: bool,
    info_open: bool,
    drag: Option<Drag>,
    wheel_travel: f32,
    /// Header-sniffed sizes, so neighbours are screened without decoding.
    probes: HashMap<PathBuf, Option<(u32, u32)>>,
    /// Paths whose header is being read off the UI thread: on a phone (MTP)
    /// or network mount even a header read can take seconds.
    probing: HashSet<PathBuf>,
    /// Paths whose decoded image GPUI's asset cache holds for us.
    requested: HashSet<PathBuf>,
    meta: FileMeta,
    shown: Shown,
    /// The stage (window below the top bar), refreshed every frame.
    area: Pt,
}

impl Viewer {
    pub(crate) fn new(
        list: Vec<PathBuf>,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut viewer = Self {
            list,
            index,
            focus: cx.focus_handle(),
            view: ViewState::default(),
            rotation: Rotation::default(),
            rotated: None,
            rotation_job: 0,
            rotating: false,
            info_open: false,
            drag: None,
            wheel_travel: 0.0,
            probes: HashMap::new(),
            probing: HashSet::new(),
            requested: HashSet::new(),
            meta: FileMeta {
                size: None,
                modified: None,
            },
            shown: Shown::Loading,
            area: Pt::new(1.0, 1.0),
        };
        viewer.arrive(window, cx);
        viewer
    }

    fn current(&self) -> Option<&Path> {
        self.list.get(self.index).map(PathBuf::as_path)
    }

    fn current_name(&self) -> String {
        self.current()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Resets per-image state for the image now at `index`.
    fn arrive(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.view.fit();
        self.rotation = Rotation::default();
        self.rotation_job += 1;
        self.rotating = false;
        self.drag = None;
        self.drop_rotated(cx);
        self.meta = FileMeta {
            size: None,
            modified: None,
        };
        if let Some(path) = self.current().map(Path::to_path_buf) {
            // A stat on a phone or network mount can block for seconds.
            cx.spawn(async move |this, cx| {
                let read = cx
                    .background_executor()
                    .spawn({
                        let path = path.clone();
                        async move { std::fs::metadata(&path).ok() }
                    })
                    .await;
                if let Err(error) = this.update(cx, |this, cx| {
                    if this.current() != Some(path.as_path()) {
                        return;
                    }
                    if let Some(meta) = read {
                        this.meta = FileMeta {
                            size: Some(meta.len()),
                            modified: meta.modified().ok(),
                        };
                        cx.notify();
                    }
                }) {
                    log::debug!("the viewer closed before the file's details arrived: {error}");
                }
            })
            .detach();
        }
        window.set_window_title(&format!("{} — Fileflux", self.current_name()));
        cx.notify();
    }

    fn drop_rotated(&mut self, cx: &mut Context<Self>) {
        if let Some(rotated) = self.rotated.take() {
            ImageSource::from(rotated.image).remove_asset(cx);
        }
    }

    fn go(&mut self, step: Step, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = nav::target(self.index, self.list.len(), step) {
            self.index = index;
            self.shown = Shown::Loading;
            self.arrive(window, cx);
        }
    }

    // ---- image state ------------------------------------------------

    /// Whether `path` may be decoded: its header size is read once, off the
    /// UI thread, and an image over the cap is refused without decoding.
    fn screen(&mut self, path: &Path, cx: &mut Context<Self>) -> Screen {
        match self.probes.get(path) {
            Some(Some((width, height))) if header::exceeds_cap(*width, *height) => {
                Screen::Refused(*width, *height)
            }
            Some(_) => Screen::Allowed,
            None => {
                if self.probing.insert(path.to_path_buf()) {
                    let owned = path.to_path_buf();
                    cx.spawn(async move |this, cx| {
                        let found = cx
                            .background_executor()
                            .spawn({
                                let path = owned.clone();
                                async move { header::file_dimensions(&path).ok().flatten() }
                            })
                            .await;
                        if let Err(error) = this.update(cx, |this, cx| {
                            this.probing.remove(&owned);
                            this.probes.insert(owned, found);
                            cx.notify();
                        }) {
                            log::debug!("the viewer closed before a header was read: {error}");
                        }
                    })
                    .detach();
                }
                Screen::Pending
            }
        }
    }

    /// Loads the current image through GPUI's asset cache, preloads the
    /// neighbours and releases everything else.
    fn refresh_shown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.current().map(Path::to_path_buf) else {
            return;
        };
        let mut keep = HashSet::new();
        let was_failed = matches!(self.shown, Shown::Failed);
        self.shown = match self.screen(&path, cx) {
            Screen::Pending => Shown::Loading,
            Screen::Refused(width, height) => Shown::TooLarge(width, height),
            Screen::Allowed => {
                keep.insert(path.clone());
                match window.use_asset::<ImgResourceLoader>(&Resource::from(path.clone()), cx) {
                    None => Shown::Loading,
                    Some(Err(error)) => {
                        // Logged once, not on every frame the failure is drawn.
                        if !was_failed {
                            log::warn!("cannot open {}: {error}", path.display());
                        }
                        Shown::Failed
                    }
                    Some(Ok(image)) => {
                        let size = image.size(0);
                        Shown::Ready(Ready {
                            width: u32::try_from(i32::from(size.width)).unwrap_or(0),
                            height: u32::try_from(i32::from(size.height)).unwrap_or(0),
                            density: density_of(&path),
                            frames: image.frame_count(),
                            image,
                        })
                    }
                }
            }
        };
        for index in nav::neighbors(self.index, self.list.len()) {
            let Some(neighbour) = self.list.get(index).cloned() else {
                continue;
            };
            if matches!(self.screen(&neighbour, cx), Screen::Allowed) {
                // Starts decoding; the result waits in the asset cache.
                let _ =
                    window.get_asset::<ImgResourceLoader>(&Resource::from(neighbour.clone()), cx);
                keep.insert(neighbour);
            }
        }
        for stale in self.requested.difference(&keep) {
            cx.remove_asset::<ImgResourceLoader>(&Resource::from(stale.clone()));
        }
        self.requested = keep;
    }

    /// The pixel size of what is on screen, rotation included.
    fn image_size(&self) -> Option<Pt> {
        let Shown::Ready(ready) = &self.shown else {
            return None;
        };
        Some(
            self.rotated
                .as_ref()
                .map_or_else(|| ready.logical(), |rotated| rotated.size),
        )
    }

    fn center(&self) -> Pt {
        Pt::new(self.area.x / 2.0, self.area.y / 2.0)
    }

    // ---- actions ----------------------------------------------------

    fn zoom_steps(&mut self, steps: f32, pointer: Pt, cx: &mut Context<Self>) {
        if let Some(image) = self.image_size() {
            self.view.zoom_at(steps, pointer, image, self.area);
            cx.notify();
        }
    }

    fn zoom_actual(&mut self, cx: &mut Context<Self>) {
        if let Some(image) = self.image_size() {
            self.view.set_scale_at(1.0, self.center(), image, self.area);
            cx.notify();
        }
    }

    fn zoom_fit(&mut self, cx: &mut Context<Self>) {
        self.view.fit();
        cx.notify();
    }

    fn can_rotate(&self) -> bool {
        matches!(&self.shown, Shown::Ready(ready) if ready.frames == 1)
    }

    fn rotate(&mut self, clockwise: bool, cx: &mut Context<Self>) {
        let Shown::Ready(ready) = &self.shown else {
            return;
        };
        if ready.frames != 1 {
            return;
        }
        let image = ready.image.clone();
        let density = ready.density;
        self.rotation = if clockwise {
            self.rotation.clockwise()
        } else {
            self.rotation.counter_clockwise()
        };
        self.view.fit();
        self.rotation_job += 1;
        let (job, rotation) = (self.rotation_job, self.rotation);
        if rotation.is_identity() {
            self.rotating = false;
            self.drop_rotated(cx);
            cx.notify();
            return;
        }
        self.rotating = true;
        cx.spawn(async move |this, cx| {
            let produced = cx
                .background_executor()
                .spawn(async move { rotated_image(&image, rotation) })
                .await;
            if let Err(error) = this.update(cx, |this, cx| {
                if this.rotation_job != job {
                    return;
                }
                this.rotating = false;
                match produced {
                    Some((image, width, height)) => {
                        this.drop_rotated(cx);
                        this.rotated = Some(Rotated {
                            image,
                            size: Pt::new(width as f32 / density, height as f32 / density),
                        });
                    }
                    None => log::warn!("could not rotate {}", this.current_name()),
                }
                cx.notify();
            }) {
                log::debug!("the viewer closed before the rotation finished: {error}");
            }
        })
        .detach();
        cx.notify();
    }

    fn open_folder(&self) {
        let Some(dir) = self.current().and_then(Path::parent) else {
            return;
        };
        let spawned = std::env::current_exe()
            .and_then(|exe| std::process::Command::new(exe).arg(dir).spawn());
        match spawned {
            // Reap the child when it exits so it does not linger as a zombie.
            Ok(mut child) => {
                std::thread::spawn(move || {
                    if let Err(error) = child.wait() {
                        log::debug!("waiting for the opened Fileflux failed: {error}");
                    }
                });
            }
            Err(error) => log::warn!("cannot open {} in Fileflux: {error}", dir.display()),
        }
    }

    // ---- pointer ----------------------------------------------------

    /// Window coordinates to stage coordinates.
    fn stage_point(position: gpui::Point<Pixels>) -> Pt {
        Pt::new(f32::from(position.x), f32::from(position.y) - BAR_HEIGHT)
    }

    fn on_wheel(&mut self, event: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        let delta = f32::from(event.delta.pixel_delta(px(WHEEL_LINE)).y);
        if event.modifiers.control {
            self.zoom_steps(delta / WHEEL_NOTCH, Self::stage_point(event.position), cx);
            return;
        }
        // A direction change starts a fresh notch.
        if self.wheel_travel * delta < 0.0 {
            self.wheel_travel = 0.0;
        }
        self.wheel_travel += delta;
        if self.wheel_travel.abs() >= WHEEL_NOTCH {
            let step = if self.wheel_travel < 0.0 {
                Step::Next
            } else {
                Step::Previous
            };
            self.wheel_travel = 0.0;
            self.go(step, window, cx);
        }
    }

    fn on_press(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let pointer = Self::stage_point(event.position);
        let Some(image) = self.image_size() else {
            return;
        };
        if event.click_count == 2 {
            self.view.toggle_at(pointer, image, self.area);
            self.drag = None;
        } else {
            self.drag = Some(Drag {
                pointer,
                offset: self.view.offset(image, self.area),
            });
        }
        cx.notify();
    }

    fn on_drag(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(drag) = &self.drag else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
            return;
        }
        let Some(image) = self.image_size() else {
            return;
        };
        let pointer = Self::stage_point(event.position);
        let delta = Pt::new(pointer.x - drag.pointer.x, pointer.y - drag.pointer.y);
        let start = drag.offset;
        self.view.pan_from(start, delta, image, self.area);
        cx.notify();
    }

    // ---- rendering --------------------------------------------------

    fn top_bar(&self, cx: &mut Context<Self>) -> Div {
        let colors = bolt::colors(cx);
        let mono = bolt::fonts::editor_family(cx);
        let scale = self
            .image_size()
            .map(|image| self.view.scale(image, self.area));
        let rotate_hint = |label: &'static str| {
            if self.can_rotate() {
                label
            } else {
                "Rotation is not available for animated or unloaded images"
            }
        };

        div()
            .flex_none()
            .h(px(BAR_HEIGHT))
            .flex()
            .items_center()
            .gap(Spacing::SM)
            .px(Spacing::MD)
            .bg(colors.background)
            .border_b_1()
            .border_color(colors.line)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(13.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(colors.strong)
                    .child(self.current_name()),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .gap(Spacing::MD)
                    .font_family(mono)
                    .text_size(px(12.0))
                    .text_color(colors.muted)
                    .child(nav::position_label(self.index, self.list.len()))
                    .children(scale.map(percent_label)),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(Spacing::XS)
                    .child(
                        Button::new("viewer-fit", "Fit")
                            .ghost()
                            .icon_only()
                            .icon(icon(gpui_kit_assets::IconName::Maximize))
                            .tooltip("Fit to window (0)")
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_fit(cx))),
                    )
                    .child(
                        Button::new("viewer-actual", "100%")
                            .ghost()
                            .tooltip("Actual size (1)")
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_actual(cx))),
                    )
                    .child(
                        Button::new("viewer-rotate-left", "Rotate left")
                            .ghost()
                            .icon_only()
                            .icon(icon(gpui_kit_assets::IconName::RotateCcw))
                            .tooltip(rotate_hint("Rotate left (Shift+R)"))
                            .disabled(!self.can_rotate())
                            .on_click(cx.listener(|this, _, _, cx| this.rotate(false, cx))),
                    )
                    .child(
                        Button::new("viewer-rotate-right", "Rotate right")
                            .ghost()
                            .icon_only()
                            .icon(icon(gpui_kit_assets::IconName::RotateCw))
                            .tooltip(rotate_hint("Rotate right (R)"))
                            .disabled(!self.can_rotate())
                            .on_click(cx.listener(|this, _, _, cx| this.rotate(true, cx))),
                    )
                    .child(
                        Button::new("viewer-info", "Info")
                            .ghost()
                            .icon_only()
                            .icon(icon(gpui_kit_assets::IconName::Info))
                            .tooltip("Image information (I)")
                            .selected(self.info_open)
                            .toggled(self.info_open)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.info_open = !this.info_open;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("viewer-folder", "Open containing folder")
                            .ghost()
                            .icon_only()
                            .icon(icon(gpui_kit_assets::IconName::FolderOpen))
                            .tooltip("Open the containing folder in Fileflux")
                            .on_click(cx.listener(|this, _, _, _| this.open_folder())),
                    ),
            )
    }

    /// A centered message for loading, errors and refused images.
    fn message(&self, title: &str, detail: &str, danger: bool, cx: &Context<Self>) -> AnyElement {
        let colors = bolt::colors(cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(Spacing::SM)
            .px(Spacing::LG)
            .child(
                Icon::new(icon(if danger {
                    gpui_kit_assets::IconName::TriangleAlert
                } else {
                    gpui_kit_assets::IconName::Image
                }))
                .size(px(28.0))
                .color(if danger { colors.danger } else { colors.muted }),
            )
            .child(
                div()
                    .text_size(px(14.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(colors.strong)
                    .child(title.to_owned()),
            )
            .child(
                div()
                    .max_w(px(560.0))
                    .text_size(px(12.0))
                    .text_color(colors.muted)
                    .text_center()
                    .child(detail.to_owned()),
            )
            .into_any_element()
    }

    fn picture(&self, ready: &Ready) -> AnyElement {
        let Some(image) = self.image_size() else {
            return div().into_any_element();
        };
        let scale = self.view.scale(image, self.area);
        let shown = Pt::new(image.x * scale, image.y * scale);
        let corner = super::zoom::top_left(self.view.offset(image, self.area), shown, self.area);
        let source: ImageSource = match &self.rotated {
            Some(rotated) => rotated.image.clone().into(),
            None => ready.image.clone().into(),
        };
        // The id gives the element state GPUI needs to step through the
        // frames of an animated GIF or WebP; it changes per image so each
        // one starts at its first frame.
        img(source)
            .id(("viewer-image", self.index))
            .absolute()
            .left(px(corner.x))
            .top(px(corner.y))
            .w(px(shown.x))
            .h(px(shown.y))
            .object_fit(ObjectFit::Fill)
            .into_any_element()
    }

    fn info_panel(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let path = self.current()?;
        let colors = bolt::colors(cx);
        let mono = bolt::fonts::editor_family(cx);
        let dimensions = match &self.shown {
            Shown::Ready(ready) => {
                let logical = ready.logical();
                Some((logical.x.round() as u32, logical.y.round() as u32))
            }
            Shown::TooLarge(width, height) => Some((*width, *height)),
            _ => self.probes.get(path).copied().flatten(),
        };
        let frames = match &self.shown {
            Shown::Ready(ready) if ready.frames > 1 => format!(" · {} frames", ready.frames),
            _ => String::new(),
        };
        let unavailable = || "—".to_owned();
        let rows = [
            ("Name", self.current_name()),
            (
                "Dimensions",
                dimensions.map_or_else(unavailable, |(w, h)| facts::dimensions_label(w, h)),
            ),
            (
                "File size",
                self.meta.size.map_or_else(unavailable, facts::human_size),
            ),
            (
                "Modified",
                self.meta
                    .modified
                    .map_or_else(unavailable, facts::modified_label),
            ),
            (
                "Folder",
                path.parent()
                    .map_or_else(unavailable, |dir| dir.display().to_string()),
            ),
            ("Format", format!("{}{frames}", facts::format_name(path))),
        ];
        Some(
            div()
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .w(px(INFO_WIDTH))
                .bg(colors.panel)
                .border_l_1()
                .border_color(colors.line)
                .p(Spacing::LG)
                .flex()
                .flex_col()
                .gap(Spacing::MD)
                .text_size(px(12.0))
                .children(rows.into_iter().map(|(term, value)| {
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(div().text_color(colors.muted).child(term))
                        .child(
                            div()
                                .font_family(mono.clone())
                                .text_color(colors.foreground)
                                .child(value),
                        )
                }))
                .into_any_element(),
        )
    }
}

/// Rotated copy of frame 0 as a GPUI image with its size.
fn rotated_image(image: &RenderImage, rotation: Rotation) -> Option<(Arc<RenderImage>, u32, u32)> {
    let size = image.size(0);
    let width = u32::try_from(i32::from(size.width)).ok()?;
    let height = u32::try_from(i32::from(size.height)).ok()?;
    let turned = rotate_pixels(image.as_bytes(0)?, width, height, rotation)?;
    let (turned_width, turned_height) = turned.dimensions();
    Some((render_image(turned), turned_width, turned_height))
}

impl Focusable for Viewer {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Viewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = bolt::colors(cx);
        let viewport = window.viewport_size();
        self.area = Pt::new(
            f32::from(viewport.width),
            (f32::from(viewport.height) - BAR_HEIGHT).max(1.0),
        );
        self.refresh_shown(window, cx);

        let name = self.current_name();
        let content = match &self.shown {
            Shown::Loading => self.message("Loading…", &name, false, cx),
            Shown::TooLarge(width, height) => {
                self.message(&name, &facts::too_large_message(*width, *height), true, cx)
            }
            Shown::Failed => self.message(
                &name,
                "This file could not be opened as an image: it is damaged or not in a supported format.",
                true,
                cx,
            ),
            Shown::Ready(ready) => self.picture(ready),
        };

        let stage = div()
            .id("viewer-stage")
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .bg(colors.desk)
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                this.on_wheel(event, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| this.on_press(event, cx)),
            )
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.go(Step::Previous, window, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Forward),
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.go(Step::Next, window, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                this.on_drag(event, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.drag = None),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.drag = None),
            )
            .child(content)
            .children(if self.info_open {
                self.info_panel(cx)
            } else {
                None
            });

        div()
            .key_context(VIEWER_CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.desk)
            .text_color(colors.foreground)
            .on_action(cx.listener(|_, _: &Close, window, _| window.remove_window()))
            .on_action(cx.listener(|this, _: &Previous, window, cx| {
                this.go(Step::Previous, window, cx);
            }))
            .on_action(cx.listener(|this, _: &Next, window, cx| this.go(Step::Next, window, cx)))
            .on_action(cx.listener(|this, _: &First, window, cx| this.go(Step::First, window, cx)))
            .on_action(cx.listener(|this, _: &Last, window, cx| this.go(Step::Last, window, cx)))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| {
                let center = this.center();
                this.zoom_steps(1.0, center, cx);
            }))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| {
                let center = this.center();
                this.zoom_steps(-1.0, center, cx);
            }))
            .on_action(cx.listener(|this, _: &FitToWindow, _, cx| this.zoom_fit(cx)))
            .on_action(cx.listener(|this, _: &ZoomActual, _, cx| this.zoom_actual(cx)))
            .on_action(cx.listener(|this, _: &RotateRight, _, cx| this.rotate(true, cx)))
            .on_action(cx.listener(|this, _: &RotateLeft, _, cx| this.rotate(false, cx)))
            .on_action(cx.listener(|this, _: &ToggleInfo, _, cx| {
                this.info_open = !this.info_open;
                cx.notify();
            }))
            .on_action(cx.listener(|_, _: &ToggleFullscreen, window, _| window.toggle_fullscreen()))
            .on_action(cx.listener(|this, _: &OpenFolder, _, _| this.open_folder()))
            .child(self.top_bar(cx))
            .child(stage)
    }
}
