//! Drag and drop. A drop is a paste with explicit sources, mode and target:
//! this module decides what a drop means (pure functions, tested) and wires
//! gpui's drag API to the existing paste engine and the Trash job, so there
//! is no second transfer path.
//!
//! Dragging starts on an entry (a grid tile or a list row); the dragged
//! payload is [`DraggedEntries`]. Files dragged in from other applications
//! arrive as gpui's `ExternalPaths`. Every drop target is built by
//! [`drop_target`]: it highlights only while the pointer is over it *and*
//! the drop would do something, and refuses the rest.

use std::any::Any;
use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bolt::primitives::Icon;
use bolt::tokens::{ChromeColors, Spacing};
use fileflux::core::links::Naming;
use gpui::{
    App, AppContext as _, Context, ExternalDragPayload, ExternalPaths, FileDragPaths,
    InteractiveElement, IntoElement, ParentElement as _, Pixels, Point, Render,
    StatefulInteractiveElement, Styled, WeakEntity, Window, div, px,
};
use gpui_component::ActiveTheme as _;

use super::entries::ENTRIES_PADDING;
use super::icon;
use super::ops::paste::ClipMode;
use super::panes::Side;
use super::rubber::past_threshold;
use super::view::Explorer;

/// Where the dragged paths come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DragOrigin {
    /// Entries dragged inside this window.
    Internal,
    /// Files dragged in from another application: never moved, only copied.
    External,
}

/// What a drop lands on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DropTarget {
    /// A real folder: a tile, a list row, a sidebar row, a breadcrumb or the
    /// shown folder's empty space.
    Folder(PathBuf),
    /// The sidebar's Trash row.
    Trash,
}

/// Where a drop lands, as far as what it means is concerned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DropZone {
    /// A folder or the Trash: today's move, copy, link and trash drops.
    Target(DropTarget),
    /// The "Open in split" strip at the right edge of the files area.
    SplitStrip,
    /// The tab strip of `side`, in the gap `gap` (0 is before the first
    /// tab).
    TabStrip { side: Side, gap: usize },
}

/// What a drop does, once the zone it landed in is known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DropAction {
    /// A drop into a target: [`plan_drop`]'s decision, unchanged.
    Plan(DropPlan),
    /// Show `folder` in the pane that does not have the focus; nothing is
    /// copied or moved.
    OpenInSplit(PathBuf),
    /// Open `folder` in a new tab of `side` at the gap `gap`; nothing is
    /// copied or moved.
    OpenInTab {
        folder: PathBuf,
        side: Side,
        gap: usize,
    },
}

/// Decides a drop in `zone`. `folder` says the (single) dragged path is a
/// folder. The two open zones take exactly one folder dragged inside this
/// window and ignore everything else; a [`DropZone::Target`] is exactly
/// [`plan_drop`].
pub(crate) fn resolve_drop(
    origin: DragOrigin,
    sources: &[PathBuf],
    folder: bool,
    zone: &DropZone,
    ctrl: bool,
    shift: bool,
    same_device: impl FnOnce(&Path) -> bool,
) -> DropAction {
    let dragged_folder = match sources {
        [one] if folder && origin == DragOrigin::Internal => Some(one.clone()),
        _ => None,
    };
    match (zone, dragged_folder) {
        (DropZone::Target(target), _) => {
            DropAction::Plan(plan_drop(origin, sources, target, ctrl, shift, same_device))
        }
        (DropZone::SplitStrip, Some(folder)) => DropAction::OpenInSplit(folder),
        (DropZone::TabStrip { side, gap }, Some(folder)) => DropAction::OpenInTab {
            folder,
            side: *side,
            gap: *gap,
        },
        (DropZone::SplitStrip | DropZone::TabStrip { .. }, None) => {
            DropAction::Plan(DropPlan::Ignore)
        }
    }
}

/// Whether a drop on a target does anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DropCheck {
    /// Valid: highlighted while hovered, and the drop runs.
    Accept,
    /// Entries dragged inside their own folder: nothing to do, no highlight.
    SameFolder,
    /// Invalid: no highlight and the drop is refused.
    Refused,
}

/// Judges a drop from the paths alone (no filesystem access).
pub(crate) fn check_drop(
    origin: DragOrigin,
    sources: &[PathBuf],
    target: &DropTarget,
) -> DropCheck {
    if sources.is_empty() {
        return DropCheck::Refused;
    }
    let dest = match target {
        DropTarget::Trash => {
            return match origin {
                DragOrigin::Internal => DropCheck::Accept,
                DragOrigin::External => DropCheck::Refused,
            };
        }
        DropTarget::Folder(dest) => dest,
    };
    // The item itself or anything inside it (a component-wise prefix, so a
    // sibling that shares a name prefix is fine). The core transfer also
    // refuses these by real location; this keeps them from highlighting.
    if sources.iter().any(|source| dest.starts_with(source)) {
        return DropCheck::Refused;
    }
    let in_place = sources
        .iter()
        .all(|source| source.parent() == Some(dest.as_path()));
    if origin == DragOrigin::Internal && in_place {
        DropCheck::SameFolder
    } else {
        DropCheck::Accept
    }
}

/// What a drop does once accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DropPlan {
    /// Nothing happens.
    Ignore,
    /// A paste of the dragged paths into `dest`.
    Paste { mode: ClipMode, dest: PathBuf },
    /// Symbolic links to the dragged paths in `dest`; nothing is copied or
    /// moved.
    Link { dest: PathBuf },
    /// The existing Trash job.
    Trash,
}

/// Whether the modifiers held at the drop ask for links: Ctrl and Shift
/// together (a copy is Ctrl, a move is Shift). Both origins: links to files
/// dragged in from other applications point at where those files are.
pub(crate) fn links_on_drop(ctrl: bool, shift: bool) -> bool {
    ctrl && shift
}

/// The mode of a drop into a folder: Move on the same device and Copy
/// across devices, Ctrl forces Copy and Shift forces Move (Ctrl wins when
/// both are held here; [`plan_drop`] turns that combination into a link
/// before asking). Files from other
/// applications are always copied.
pub(crate) fn drop_mode(
    origin: DragOrigin,
    same_device: bool,
    ctrl: bool,
    shift: bool,
) -> ClipMode {
    match origin {
        DragOrigin::External => ClipMode::Copy,
        DragOrigin::Internal if ctrl => ClipMode::Copy,
        DragOrigin::Internal if shift => ClipMode::Cut,
        DragOrigin::Internal if same_device => ClipMode::Cut,
        DragOrigin::Internal => ClipMode::Copy,
    }
}

/// Decides a drop. `same_device` is asked only when the answer matters. Ctrl+Shift
/// links; a link drop into the folder the items already live in does nothing
/// (Make Link in the context menu links there).
pub(crate) fn plan_drop(
    origin: DragOrigin,
    sources: &[PathBuf],
    target: &DropTarget,
    ctrl: bool,
    shift: bool,
    same_device: impl FnOnce(&Path) -> bool,
) -> DropPlan {
    if check_drop(origin, sources, target) != DropCheck::Accept {
        return DropPlan::Ignore;
    }
    match target {
        DropTarget::Trash => DropPlan::Trash,
        DropTarget::Folder(dest) if links_on_drop(ctrl, shift) => {
            DropPlan::Link { dest: dest.clone() }
        }
        DropTarget::Folder(dest) => {
            // The device only decides a plain internal drop.
            let plain_internal = origin == DragOrigin::Internal && !ctrl && !shift;
            let same = plain_internal && same_device(dest);
            DropPlan::Paste {
                mode: drop_mode(origin, same, ctrl, shift),
                dest: dest.clone(),
            }
        }
    }
}

/// Whether every source lives on the device of `target` (links count as
/// themselves). Anything unreadable says no, which makes the drop a Copy.
pub(crate) fn same_device(sources: &[PathBuf], target: &Path) -> bool {
    let Ok(target) = fs::metadata(target) else {
        return false;
    };
    !sources.is_empty()
        && sources
            .iter()
            .all(|source| fs::symlink_metadata(source).is_ok_and(|meta| meta.dev() == target.dev()))
}

/// The drag preview's text: the item's name, or "N items".
pub(crate) fn preview_text(paths: &[PathBuf]) -> String {
    match paths {
        [one] => one.file_name().map_or_else(
            || one.to_string_lossy().into_owned(),
            |name| name.to_string_lossy().into_owned(),
        ),
        many => format!("{} items", many.len()),
    }
}

/// Where a press on an entry stands on its way to becoming a drag. GPUI
/// starts a drag after 2 px; Fileflux keeps it inert (no preview, no target
/// highlight, no drop, no selection change) until the pointer has moved
/// `rubber::DRAG_THRESHOLD` from the press, the desktop default.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) enum DragGate {
    /// No entry press is being tracked.
    #[default]
    Idle,
    /// An entry was pressed at this window position and the pointer has not
    /// left the threshold yet.
    Pressed((f32, f32)),
    /// The pointer left the threshold: this is a real drag from now on.
    Armed,
}

impl DragGate {
    /// The gate after the pointer was seen at `pointer`: once armed it
    /// stays armed, so moving back to the press never disarms a drag.
    pub(crate) fn observe(self, pointer: (f32, f32)) -> Self {
        match self {
            DragGate::Pressed(press) if past_threshold(press, pointer) => DragGate::Armed,
            other => other,
        }
    }

    /// Whether the drag may show, highlight and drop. An untracked drag
    /// (no press seen) is never held back.
    pub(crate) fn is_armed(self) -> bool {
        !matches!(self, DragGate::Pressed(_))
    }
}

/// Whether releasing the button ends a drag that never left the threshold:
/// that release is a plain click on the pressed entry.
pub(crate) fn release_is_plain_click(gate: DragGate, drag_active: bool) -> bool {
    drag_active && matches!(gate, DragGate::Pressed(_))
}

/// The window's one drag gate, shared by every drag source and target
/// (they run inside GPUI callbacks that cannot reach the explorer).
#[derive(Clone, Default)]
struct GateCell(std::rc::Rc<std::cell::Cell<DragGate>>);

impl gpui::Global for GateCell {}

fn gate_cell(cx: &mut App) -> std::rc::Rc<std::cell::Cell<DragGate>> {
    std::rc::Rc::clone(&cx.default_global::<GateCell>().0)
}

fn point_of(position: Point<Pixels>) -> (f32, f32) {
    (f32::from(position.x), f32::from(position.y))
}

/// Starts tracking a press on an entry at `position`.
pub(crate) fn press_gate(position: Point<Pixels>, cx: &mut App) {
    gate_cell(cx).set(DragGate::Pressed(point_of(position)));
}

/// Stops tracking, returning the gate as of the pointer at `position`.
pub(crate) fn finish_gate(position: Point<Pixels>, cx: &mut App) -> DragGate {
    let cell = gate_cell(cx);
    let gate = cell.get().observe(point_of(position));
    cell.set(DragGate::Idle);
    gate
}

/// Looks at the pointer at `position`, latching the gate when it armed.
pub(crate) fn observe_gate(position: Point<Pixels>, cx: &mut App) -> DragGate {
    let cell = gate_cell(cx);
    let gate = cell.get().observe(point_of(position));
    cell.set(gate);
    gate
}

/// Whether an entry drag may show, highlight and drop right now.
fn drag_armed(window: &Window, cx: &mut App) -> bool {
    observe_gate(window.mouse_position(), cx).is_armed()
}

// --- gpui glue -----------------------------------------------------------------------------

/// The payload of a drag that started on one of this window's entries.
#[derive(Clone, Debug)]
pub(crate) struct DraggedEntries {
    /// The visible selection (or the one unselected entry), shared by every
    /// tile of a frame.
    pub(crate) paths: Arc<Vec<PathBuf>>,
    /// The entry that was grabbed is a folder.
    pub(crate) folder: bool,
}

impl DraggedEntries {
    /// The payload of one folder dragged from a sidebar row: the same
    /// payload an entry's drag carries, so every drop target and zone takes
    /// it the same way.
    pub(crate) fn one_folder(path: PathBuf) -> Self {
        Self {
            paths: Arc::new(vec![path]),
            folder: true,
        }
    }

    /// The folder, when exactly one folder is being dragged: the only drag
    /// the "Open in split" strip and the tab strip take.
    pub(crate) fn sole_folder(&self) -> Option<&Path> {
        match self.paths.as_slice() {
            [one] if self.folder => Some(one.as_path()),
            _ => None,
        }
    }
}

/// The folder a drag payload carries, if it is a drag of exactly one folder.
pub(crate) fn dragged_folder(value: &dyn Any) -> Option<&Path> {
    value
        .downcast_ref::<DraggedEntries>()
        .and_then(DraggedEntries::sole_folder)
}

/// Whether a folder drag is on screen and past its threshold: the strips
/// that open a folder show only then.
pub(crate) fn folder_drag_live(this: &Explorer, cx: &App) -> bool {
    this.folder_drag.is_some()
        && cx.has_active_drag()
        && cx
            .try_global::<GateCell>()
            .is_none_or(|gate| gate.0.get().is_armed())
}

/// The element id the automation bridge reads the drag card by.
pub(crate) const DRAG_PREVIEW_ID: &str = "explorer-drag-preview";

/// How far the drag card's top-left sits from the pointer, right and down,
/// so the card never covers the pointer. pcmanfm paints the grabbed cell with
/// the pointer inside it at the same relative spot; a small card reads better
/// at a fixed offset than a snapshot would, since the card is not the cell.
pub(crate) const PREVIEW_OFFSET: f32 = 12.0;

/// The margin that puts the drag card `PREVIEW_OFFSET` right and down of the
/// pointer.
///
/// GPUI prepaints a drag view at `pointer - grab`, where `grab` is the press
/// point it hands to `on_drag` (`gpui-pre/src/window.rs`, the `active_drag`
/// branch of the frame). Cancelling that term needs `grab + PREVIEW_OFFSET`,
/// and it has to sit on a child: the layout engine places children by their
/// margins and ignores the margin of the root element it lays out, which is
/// why the card used to follow the grabbed tile's origin instead of the
/// pointer.
pub(crate) fn preview_margin(grab: Point<Pixels>) -> (Pixels, Pixels) {
    (grab.x + px(PREVIEW_OFFSET), grab.y + px(PREVIEW_OFFSET))
}

/// The view that follows the pointer while dragging: the item's icon and
/// name, or "N items".
pub(crate) struct DragPreview {
    text: String,
    glyph: gpui_kit_assets::IconName,
    folder: bool,
    /// Where inside the dragged element the pointer grabbed it, in pixels
    /// from that element's origin.
    grab: Point<Pixels>,
}

impl Render for DragPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Inert until the pointer left the drag threshold.
        if !drag_armed(window, cx) {
            return div().id(DRAG_PREVIEW_ID);
        }
        let colors = bolt::colors(cx);
        let tint = ChromeColors::tint(cx.theme());
        let (margin_x, margin_y) = preview_margin(self.grab);
        // Named so the automation bridge can read the card's bounds and check
        // where it sits against the pointer, which a screenshot cannot.
        div()
            .id(DRAG_PREVIEW_ID)
            .role(gpui::accesskit::Role::Tooltip)
            .aria_label(self.text.clone())
            .flex()
            .child(
                div()
                    .ml(margin_x)
                    .mt(margin_y)
                    .flex()
                    .items_center()
                    .gap(Spacing::SM)
                    .px(px(10.0))
                    .py(px(6.0))
                    .max_w(px(280.0))
                    .bg(colors.raised)
                    .border_1()
                    .border_color(tint)
                    .text_size(px(13.0))
                    .text_color(colors.strong)
                    .child(
                        Icon::new(icon(self.glyph))
                            .size(px(16.0))
                            .color(if self.folder {
                                colors.info
                            } else {
                                colors.muted
                            }),
                    )
                    .child(div().min_w_0().truncate().child(self.text.clone())),
            )
    }
}

/// Makes `element` start a drag of the selection when it is selected, else
/// of the entry alone. The drag stays inert until the pointer left the drag
/// threshold (see [`DragGate`]); only then is an unselected entry selected
/// alone, like a click would. `selection` is the visible selection of this
/// frame.
pub(crate) fn drag_source<E: StatefulInteractiveElement>(
    element: E,
    path: PathBuf,
    glyph: gpui_kit_assets::IconName,
    folder: bool,
    selected: bool,
    selection: &Arc<Vec<PathBuf>>,
    weak: &WeakEntity<Explorer>,
) -> E {
    let paths = if selected {
        Arc::clone(selection)
    } else {
        Arc::new(vec![path.clone()])
    };
    attach_drag(element, DraggedEntries { paths, folder }, path, glyph, weak)
}

/// Makes a sidebar row the drag source of the one folder it stands for. The
/// payload is an entry drag's, so drops on folders, the split rail and the
/// tab strip treat it the same; a click that does not move still navigates.
pub(crate) fn place_drag_source<E: StatefulInteractiveElement>(
    element: E,
    path: PathBuf,
    weak: &WeakEntity<Explorer>,
) -> E {
    let dragged = DraggedEntries::one_folder(path.clone());
    attach_drag(
        element,
        dragged,
        path,
        gpui_kit_assets::IconName::Folder,
        weak,
    )
}

/// Starts `dragged` from `element`: the preview card, the folder-drag mark
/// the strips wait for, and the file list handed to other applications.
fn attach_drag<E: StatefulInteractiveElement>(
    element: E,
    dragged: DraggedEntries,
    path: PathBuf,
    glyph: gpui_kit_assets::IconName,
    weak: &WeakEntity<Explorer>,
) -> E {
    let folder = dragged.folder;
    let payload_explorer = weak.clone();
    let drag_explorer = weak.clone();
    let dragged_path = path;
    element
        .on_drag(dragged, move |dragged, grab, _window, cx| {
            if dragged.sole_folder().is_some()
                && let Err(error) = drag_explorer.update(cx, |this, cx| {
                    // The press focused the pane the folder lives in.
                    this.folder_drag = Some(this.tabs.focus());
                    cx.notify();
                })
            {
                log::debug!("a folder drag began in a closed explorer: {error}");
            }
            let one = dragged.paths.len() == 1;
            let preview = DragPreview {
                text: preview_text(&dragged.paths),
                glyph: if one {
                    glyph
                } else {
                    gpui_kit_assets::IconName::Files
                },
                folder: one && folder,
                grab,
            };
            cx.new(|_| preview)
        })
        // Leaving the window hands the drag to the desktop as a file list, so
        // the entries can be dropped on other applications.
        .external_drag_payload(move |dragged: &DraggedEntries, _window, cx| {
            external_payload(dragged, &dragged_path, folder, &payload_explorer, cx)
        })
}

/// Pairs each dragged path with whether it is a folder: the grabbed row
/// knows its own kind, the rest of the selection is looked up in
/// `directories`.
fn file_kinds(
    paths: &[PathBuf],
    dragged_path: &Path,
    dragged_is_dir: bool,
    directories: &std::collections::HashSet<&Path>,
) -> Vec<(PathBuf, bool)> {
    paths
        .iter()
        .map(|path| {
            let is_dir = if path == dragged_path {
                dragged_is_dir
            } else {
                directories.contains(path.as_path())
            };
            (path.clone(), is_dir)
        })
        .collect()
}

/// The file list offered to other applications: each path with its kind,
/// taken from the listing in memory (never a stat on the UI thread, which
/// can block on a phone or network mount).
fn external_payload(
    dragged: &DraggedEntries,
    dragged_path: &Path,
    dragged_is_dir: bool,
    explorer: &WeakEntity<Explorer>,
    cx: &App,
) -> Option<ExternalDragPayload> {
    let explorer = explorer.upgrade()?;
    let state = &explorer.read(cx).tab().state;
    let directories: std::collections::HashSet<&Path> = state
        .selected_entries()
        .into_iter()
        .filter(|entry| entry.kind == fileflux::core::EntryKind::Directory)
        .map(|entry| entry.path.as_path())
        .collect();
    let files = file_kinds(&dragged.paths, dragged_path, dragged_is_dir, &directories);
    Some(ExternalDragPayload::Files(FileDragPaths::new(files)))
}

/// The width of the "Open in split" rail while nothing hovers it: the
/// padding the grid keeps free at the right of the files area, so the rail
/// never covers a tile (the list's rows reach the edge; the rail hides only
/// their last pixels).
pub(crate) const SPLIT_RAIL_WIDTH: f32 = ENTRIES_PADDING;
/// The width the rail widens to, with its label, while the drag hovers it.
const SPLIT_RAIL_OPEN_WIDTH: f32 = 132.0;

/// The "Open in split" rail: a narrow column at the very right edge of the
/// files area that takes a dragged folder (see [`DropZone::SplitStrip`]). At
/// rest it sits in the padding beside the tiles, so every entry stays a
/// target for the move and copy drops; it widens and shows its label only
/// while the drag hovers it. It hides what is under it from the pointer, so a
/// folder dropped on it is never also dropped on an entry. The caller draws
/// it only while [`folder_drag_live`].
pub(crate) fn split_strip(cx: &mut Context<Explorer>) -> gpui::Stateful<gpui::Div> {
    let colors = bolt::colors(cx);
    let tint = ChromeColors::tint(cx.theme());
    let wash = cx.theme().list_active;
    const GROUP: &str = "explorer-split-rail-group";
    div()
        .id("explorer-split-strip")
        .group(GROUP)
        .role(gpui::accesskit::Role::Group)
        .aria_label("Open in split")
        .absolute()
        .top_0()
        .bottom_0()
        .right_0()
        .w(px(SPLIT_RAIL_WIDTH))
        .overflow_hidden()
        .block_mouse_except_scroll()
        .flex()
        .items_center()
        .justify_center()
        .gap(Spacing::SM)
        .bg(colors.raised.opacity(0.92))
        .border_l_2()
        .border_color(tint.opacity(0.5))
        .text_size(px(13.0))
        .text_color(colors.strong)
        .child(
            Icon::new(icon(gpui_kit_assets::IconName::PanelRight))
                .size(px(12.0))
                .color(tint),
        )
        .child(
            div()
                .id("explorer-split-label")
                .hidden()
                .whitespace_nowrap()
                .group_drag_over::<DraggedEntries>(GROUP, |style| style.flex())
                .child("Open in split"),
        )
        .can_drop(|value, _, _| dragged_folder(value).is_some())
        .drag_over::<DraggedEntries>(move |style, _, _, _| {
            style
                .w(px(SPLIT_RAIL_OPEN_WIDTH))
                .bg(wash)
                .border_color(tint)
        })
        .on_drop::<DraggedEntries>(cx.listener(|this, dragged: &DraggedEntries, window, cx| {
            this.drop_dragged(dragged, &DropZone::SplitStrip, window, cx);
        }))
}

/// While something is dragged, an entry that is not a target hides what is
/// behind it, so the pointer over a file or an invalid folder never counts
/// as being over the shown folder's empty space.
pub(crate) fn block_while_dragging<E: InteractiveElement>(element: E, cx: &App) -> E {
    if cx.has_active_drag() {
        element.block_mouse_except_scroll()
    } else {
        element
    }
}

/// The paths of a drag payload, with where they come from.
fn dragged_paths(value: &dyn Any) -> Option<(DragOrigin, &[PathBuf])> {
    if let Some(entries) = value.downcast_ref::<DraggedEntries>() {
        return Some((DragOrigin::Internal, entries.paths.as_slice()));
    }
    value
        .downcast_ref::<ExternalPaths>()
        .map(|external| (DragOrigin::External, external.paths()))
}

/// Makes `element` a drop target: tint-washed and bordered while a drag that
/// would do something hovers it, refusing every other drag.
pub(crate) fn drop_target<E>(
    element: E,
    target: DropTarget,
    weak: &WeakEntity<Explorer>,
    cx: &App,
) -> E
where
    E: InteractiveElement + Styled,
{
    let theme = cx.theme();
    let tint = ChromeColors::tint(theme);
    let wash = theme.list_active;
    let accepts = {
        let target = target.clone();
        move |value: &dyn Any, window: &mut Window, cx: &mut App| {
            dragged_paths(value).is_some_and(|(origin, paths)| {
                (origin == DragOrigin::External || drag_armed(window, cx))
                    && check_drop(origin, paths, &target) == DropCheck::Accept
            })
        }
    };
    let on_internal = {
        let (weak, target) = (weak.clone(), target.clone());
        move |dragged: &DraggedEntries, window: &mut Window, cx: &mut App| {
            if !drag_armed(window, cx) {
                return;
            }
            let paths = dragged.paths.to_vec();
            drop_on(
                &weak,
                DragOrigin::Internal,
                paths,
                target.clone(),
                window,
                cx,
            );
        }
    };
    let on_external = {
        let weak = weak.clone();
        move |dropped: &ExternalPaths, window: &mut Window, cx: &mut App| {
            let paths = dropped.paths().to_vec();
            drop_on(
                &weak,
                DragOrigin::External,
                paths,
                target.clone(),
                window,
                cx,
            );
        }
    };
    element
        .can_drop(accepts)
        .drag_over::<DraggedEntries>(move |style, _, window, cx| {
            if drag_armed(window, cx) {
                style.bg(wash).border_1().border_color(tint)
            } else {
                style
            }
        })
        .drag_over::<ExternalPaths>(move |style, _, _, _| {
            style.bg(wash).border_1().border_color(tint)
        })
        .on_drop::<DraggedEntries>(on_internal)
        .on_drop::<ExternalPaths>(on_external)
}

fn drop_on(
    weak: &WeakEntity<Explorer>,
    origin: DragOrigin,
    paths: Vec<PathBuf>,
    target: DropTarget,
    window: &mut Window,
    cx: &mut App,
) {
    if let Err(error) = weak.update(cx, |this, cx| {
        this.drop_entries(origin, paths, &target, window, cx);
    }) {
        log::debug!("a drop reached a closed explorer: {error}");
    }
}

impl Explorer {
    /// A drop landed on a target: asks [`resolve_drop`] what it means (Ctrl
    /// and Shift are read at the moment of the drop) and hands it to the
    /// paste engine or the Trash job. Nothing here moves a byte by itself.
    pub(crate) fn drop_entries(
        &mut self,
        origin: DragOrigin,
        sources: Vec<PathBuf>,
        target: &DropTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drop_in_zone(
            origin,
            sources,
            // Only the open zones read "the dragged path is a folder"; a
            // target drop plans from the paths alone.
            false,
            &DropZone::Target(target.clone()),
            window,
            cx,
        );
    }

    /// Entries of this window dropped on the "Open in split" strip or the
    /// tab strip (see [`DropZone`]).
    pub(crate) fn drop_dragged(
        &mut self,
        dragged: &DraggedEntries,
        zone: &DropZone,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drop_in_zone(
            DragOrigin::Internal,
            dragged.paths.to_vec(),
            dragged.folder,
            zone,
            window,
            cx,
        );
    }

    fn drop_in_zone(
        &mut self,
        origin: DragOrigin,
        sources: Vec<PathBuf>,
        folder: bool,
        zone: &DropZone,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let modifiers = window.modifiers();
        let action = resolve_drop(
            origin,
            &sources,
            folder,
            zone,
            modifiers.control,
            modifiers.shift,
            |dest| same_device(&sources, dest),
        );
        let from = self.folder_drag.take();
        match action {
            DropAction::Plan(DropPlan::Ignore) => {}
            DropAction::Plan(DropPlan::Paste { mode, dest }) => {
                self.start_paste(sources, dest, mode, None, window, cx)
            }
            DropAction::Plan(DropPlan::Link { dest }) => {
                self.start_links(sources, Some(dest), Naming::Source, window, cx)
            }
            DropAction::Plan(DropPlan::Trash) => self.trash_paths(sources, window, cx),
            DropAction::OpenInSplit(folder) => {
                // The pane the drop landed in took the focus first; "the
                // other pane" is the other one of the pane the drag began in.
                if let Some(from) = from {
                    self.tabs.focus_side(from);
                }
                self.open_in_split(folder, window, cx);
            }
            DropAction::OpenInTab { folder, side, gap } => {
                self.open_folder_tab_at(folder, side, gap, window, cx);
            }
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::point;

    #[test]
    fn the_drag_card_keeps_a_fixed_offset_from_the_pointer() {
        // The user's report: the card followed the grabbed tile's origin, so
        // it drifted further from the pointer the deeper inside the tile the
        // press landed. GPUI prepaints the card at `pointer - grab`; the
        // margin adds `grab` back plus the fixed offset, whatever was grabbed.
        for grab in [(0.0, 0.0), (56.0, 30.0), (111.0, 117.0)] {
            let grab = point(px(grab.0), px(grab.1));
            let pointer = point(px(400.0), px(320.0));
            let (margin_x, margin_y) = preview_margin(grab);
            let painted = point(pointer.x - grab.x + margin_x, pointer.y - grab.y + margin_y);
            assert_eq!(painted, point(px(412.0), px(332.0)), "grab {grab:?}");
        }
    }

    #[test]
    fn a_press_stays_inert_until_the_pointer_leaves_the_threshold() {
        let press = (200.0, 300.0);
        let gate = DragGate::Pressed(press);
        assert!(!gate.is_armed());
        // GPUI itself would start the drag at 2 px; the gate does not.
        assert!(!gate.observe((203.0, 300.0)).is_armed());
        assert!(!gate.observe((207.0, 300.0)).is_armed());
        assert_eq!(gate.observe((203.0, 300.0)), gate, "nothing latched yet");
        assert_eq!(gate.observe((208.0, 300.0)), DragGate::Armed);
        assert!(gate.observe((200.0, 308.0)).is_armed());
    }

    #[test]
    fn an_armed_drag_stays_armed_when_the_pointer_comes_back() {
        let armed = DragGate::Pressed((0.0, 0.0)).observe((50.0, 0.0));
        assert_eq!(armed.observe((1.0, 0.0)), DragGate::Armed);
    }

    #[test]
    fn a_drag_nobody_pressed_for_is_not_held_back() {
        assert!(DragGate::Idle.is_armed());
        assert_eq!(DragGate::Idle.observe((9.0, 9.0)), DragGate::Idle);
        assert!(DragGate::Armed.is_armed());
    }

    #[test]
    fn only_a_release_before_the_threshold_is_a_plain_click() {
        let pressed = DragGate::Pressed((1.0, 1.0));
        assert!(release_is_plain_click(pressed, true));
        assert!(
            !release_is_plain_click(pressed, false),
            "no drag started: GPUI's own click handles it"
        );
        assert!(!release_is_plain_click(DragGate::Armed, true));
        assert!(!release_is_plain_click(DragGate::Idle, true));
    }

    fn p(path: &str) -> PathBuf {
        PathBuf::from(path)
    }
    fn folder(path: &str) -> DropTarget {
        DropTarget::Folder(p(path))
    }

    #[test]
    fn file_kinds_marks_the_grabbed_row_and_selected_folders() {
        let paths = vec![p("/w/photo.jpg"), p("/w/docs"), p("/w/notes.txt")];
        let directories = std::collections::HashSet::from([Path::new("/w/docs")]);
        assert_eq!(
            file_kinds(&paths, Path::new("/w/photo.jpg"), false, &directories),
            vec![
                (p("/w/photo.jpg"), false),
                (p("/w/docs"), true),
                (p("/w/notes.txt"), false),
            ]
        );
        // The grabbed row's own kind wins even before the listing knows it.
        assert_eq!(
            file_kinds(&[p("/w/new")], Path::new("/w/new"), true, &directories),
            vec![(p("/w/new"), true)]
        );
    }
    fn check(sources: &[&str], target: &DropTarget) -> DropCheck {
        let sources: Vec<PathBuf> = sources.iter().map(|s| p(s)).collect();
        check_drop(DragOrigin::Internal, &sources, target)
    }

    #[test]
    fn a_folder_elsewhere_accepts() {
        assert_eq!(check(&["/h/a.txt"], &folder("/h/docs")), DropCheck::Accept);
        assert_eq!(
            check(&["/h/a.txt", "/h/b"], &folder("/other")),
            DropCheck::Accept
        );
    }

    #[test]
    fn the_item_itself_and_its_descendants_are_refused() {
        assert_eq!(check(&["/h/docs"], &folder("/h/docs")), DropCheck::Refused);
        assert_eq!(
            check(&["/h/docs"], &folder("/h/docs/deep/er")),
            DropCheck::Refused
        );
        // One bad source refuses the whole drop.
        assert_eq!(
            check(&["/h/a.txt", "/h/docs"], &folder("/h/docs")),
            DropCheck::Refused
        );
    }

    #[test]
    fn a_sibling_with_a_common_name_prefix_is_not_a_descendant() {
        assert_eq!(
            check(&["/h/docs"], &folder("/h/docs-old")),
            DropCheck::Accept
        );
    }

    #[test]
    fn dropping_in_the_folder_the_items_already_live_in_does_nothing() {
        assert_eq!(
            check(&["/h/a", "/h/b"], &folder("/h")),
            DropCheck::SameFolder
        );
        // Mixed parents: some move, so it is a real drop.
        assert_eq!(check(&["/h/a", "/x/b"], &folder("/h")), DropCheck::Accept);
    }

    #[test]
    fn nothing_dragged_is_refused() {
        assert_eq!(check(&[], &folder("/h")), DropCheck::Refused);
        assert_eq!(check(&[], &DropTarget::Trash), DropCheck::Refused);
    }

    #[test]
    fn the_trash_row_takes_internal_drags_only() {
        assert_eq!(check(&["/h/a"], &DropTarget::Trash), DropCheck::Accept);
        assert_eq!(
            check_drop(DragOrigin::External, &[p("/h/a")], &DropTarget::Trash),
            DropCheck::Refused
        );
    }

    #[test]
    fn external_files_may_land_in_their_own_folder_as_a_copy() {
        assert_eq!(
            check_drop(DragOrigin::External, &[p("/h/a")], &folder("/h")),
            DropCheck::Accept
        );
        assert_eq!(
            check_drop(DragOrigin::External, &[p("/h/docs")], &folder("/h/docs")),
            DropCheck::Refused
        );
    }

    #[test]
    fn the_default_mode_follows_the_device() {
        let internal = DragOrigin::Internal;
        assert_eq!(drop_mode(internal, true, false, false), ClipMode::Cut);
        assert_eq!(drop_mode(internal, false, false, false), ClipMode::Copy);
    }

    #[test]
    fn ctrl_copies_and_shift_moves_whatever_the_device() {
        let internal = DragOrigin::Internal;
        assert_eq!(drop_mode(internal, true, true, false), ClipMode::Copy);
        assert_eq!(drop_mode(internal, false, false, true), ClipMode::Cut);
        assert_eq!(drop_mode(internal, true, false, true), ClipMode::Cut);
        assert_eq!(drop_mode(internal, false, true, false), ClipMode::Copy);
    }

    #[test]
    fn ctrl_wins_over_shift() {
        for same in [true, false] {
            assert_eq!(
                drop_mode(DragOrigin::Internal, same, true, true),
                ClipMode::Copy
            );
        }
    }

    #[test]
    fn external_drops_are_always_copies() {
        for (same, ctrl, shift) in [
            (true, false, false),
            (true, false, true),
            (false, false, true),
            (true, true, true),
        ] {
            assert_eq!(
                drop_mode(DragOrigin::External, same, ctrl, shift),
                ClipMode::Copy
            );
        }
    }

    fn plan(
        origin: DragOrigin,
        sources: &[&str],
        target: &DropTarget,
        ctrl: bool,
        shift: bool,
        same: bool,
    ) -> DropPlan {
        let sources: Vec<PathBuf> = sources.iter().map(|s| p(s)).collect();
        plan_drop(origin, &sources, target, ctrl, shift, |_| same)
    }

    #[test]
    fn a_plain_drop_on_a_folder_moves_on_the_same_device() {
        assert_eq!(
            plan(
                DragOrigin::Internal,
                &["/h/a"],
                &folder("/h/d"),
                false,
                false,
                true
            ),
            DropPlan::Paste {
                mode: ClipMode::Cut,
                dest: p("/h/d")
            }
        );
        assert_eq!(
            plan(
                DragOrigin::Internal,
                &["/h/a"],
                &folder("/m/d"),
                false,
                false,
                false
            ),
            DropPlan::Paste {
                mode: ClipMode::Copy,
                dest: p("/m/d")
            }
        );
    }

    #[test]
    fn a_drop_the_check_refuses_or_skips_is_ignored() {
        let origin = DragOrigin::Internal;
        assert_eq!(
            plan(origin, &["/h/d"], &folder("/h/d/x"), false, true, true),
            DropPlan::Ignore
        );
        assert_eq!(
            plan(origin, &["/h/a"], &folder("/h"), true, false, true),
            DropPlan::Ignore
        );
        assert_eq!(
            plan(
                DragOrigin::External,
                &["/h/a"],
                &DropTarget::Trash,
                false,
                false,
                true
            ),
            DropPlan::Ignore
        );
    }

    #[test]
    fn ctrl_and_shift_together_link_instead_of_copying_or_moving() {
        for origin in [DragOrigin::Internal, DragOrigin::External] {
            assert_eq!(
                plan(origin, &["/h/a"], &folder("/h/d"), true, true, true),
                DropPlan::Link { dest: p("/h/d") }
            );
        }
        assert!(links_on_drop(true, true));
        assert!(!links_on_drop(true, false));
        assert!(!links_on_drop(false, true));
        assert!(!links_on_drop(false, false));
    }

    #[test]
    fn one_modifier_alone_still_copies_or_moves() {
        let copy =
            |origin, ctrl, shift| plan(origin, &["/h/a"], &folder("/m/d"), ctrl, shift, true);
        assert_eq!(
            copy(DragOrigin::Internal, true, false),
            DropPlan::Paste {
                mode: ClipMode::Copy,
                dest: p("/m/d")
            }
        );
        assert_eq!(
            copy(DragOrigin::Internal, false, true),
            DropPlan::Paste {
                mode: ClipMode::Cut,
                dest: p("/m/d")
            }
        );
    }

    #[test]
    fn a_link_drop_the_check_refuses_or_skips_is_ignored() {
        let internal = DragOrigin::Internal;
        assert_eq!(
            plan(internal, &["/h/d"], &folder("/h/d/x"), true, true, true),
            DropPlan::Ignore
        );
        assert_eq!(
            plan(internal, &["/h/a"], &folder("/h"), true, true, true),
            DropPlan::Ignore
        );
    }

    #[test]
    fn a_link_drop_never_asks_for_the_device() {
        let sources = [p("/h/a")];
        let plan = plan_drop(
            DragOrigin::Internal,
            &sources,
            &folder("/h/d"),
            true,
            true,
            |_| unreachable!("a link does not depend on the device"),
        );
        assert_eq!(plan, DropPlan::Link { dest: p("/h/d") });
    }

    #[test]
    fn the_trash_row_still_trashes_with_ctrl_and_shift() {
        assert_eq!(
            plan(
                DragOrigin::Internal,
                &["/h/a"],
                &DropTarget::Trash,
                true,
                true,
                false
            ),
            DropPlan::Trash
        );
    }

    #[test]
    fn the_trash_row_trashes_whatever_the_modifiers() {
        for (ctrl, shift) in [(false, false), (true, false), (false, true)] {
            assert_eq!(
                plan(
                    DragOrigin::Internal,
                    &["/h/a"],
                    &DropTarget::Trash,
                    ctrl,
                    shift,
                    false
                ),
                DropPlan::Trash
            );
        }
    }

    #[test]
    fn the_device_is_not_asked_unless_a_folder_drop_needs_it() {
        let sources = [p("/h/a")];
        let asked = std::cell::Cell::new(false);
        let ask = |_: &Path| {
            asked.set(true);
            false
        };
        let cases = [
            (DragOrigin::Internal, DropTarget::Trash, false, false),
            (DragOrigin::External, folder("/x"), false, false),
            (DragOrigin::Internal, folder("/x"), true, false),
            (DragOrigin::Internal, folder("/x"), false, true),
        ];
        for (origin, target, ctrl, shift) in cases {
            plan_drop(origin, &sources, &target, ctrl, shift, ask);
        }
        assert!(!asked.get(), "the device was asked for a decided drop");
        plan_drop(
            DragOrigin::Internal,
            &sources,
            &folder("/x"),
            false,
            false,
            ask,
        );
        assert!(asked.get(), "a plain internal drop depends on the device");
    }

    #[test]
    fn the_preview_names_one_item_and_counts_several() {
        assert_eq!(preview_text(&[p("/h/photo.png")]), "photo.png");
        assert_eq!(preview_text(&[p("/h/a"), p("/h/b"), p("/h/c")]), "3 items");
    }

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// A scratch directory under the system temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> std::io::Result<Scratch> {
            let tick = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            let dir =
                std::env::temp_dir().join(format!("fileflux-dnd-{}-{tick}", std::process::id()));
            fs::create_dir_all(&dir)?;
            Ok(Scratch(dir))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.0) {
                eprintln!("could not remove {}: {error}", self.0.display());
            }
        }
    }

    #[test]
    fn same_device_reads_the_filesystem_and_fails_toward_copy() -> TestResult {
        let scratch = Scratch::new()?;
        let dir = scratch.0.join("dir");
        fs::create_dir(&dir)?;
        let file = scratch.0.join("f");
        fs::write(&file, b"x")?;
        let link = scratch.0.join("link");
        std::os::unix::fs::symlink("/", &link)?;
        assert!(same_device(std::slice::from_ref(&file), &dir));
        assert!(same_device(&[file.clone(), link], &dir), "a link is itself");
        assert!(!same_device(&[scratch.0.join("missing")], &dir));
        assert!(!same_device(&[file], &scratch.0.join("missing")));
        assert!(!same_device(&[], &dir), "nothing to move: not a move");
        Ok(())
    }

    #[test]
    fn entry_drop_targets_ignore_tab_drags() {
        use crate::explorer::panes::Side;
        use crate::explorer::tabs::DraggedTab;
        let tab = DraggedTab {
            id: 1,
            side: Side::Left,
        };
        assert!(dragged_paths(&tab).is_none());
        let entries = DraggedEntries {
            paths: Arc::new(vec![PathBuf::from("/a")]),
            folder: false,
        };
        assert!(dragged_paths(&entries).is_some());
    }

    fn resolve(
        origin: DragOrigin,
        sources: &[&str],
        folder: bool,
        zone: &DropZone,
        ctrl: bool,
        shift: bool,
    ) -> DropAction {
        let sources: Vec<PathBuf> = sources.iter().map(|s| p(s)).collect();
        resolve_drop(origin, &sources, folder, zone, ctrl, shift, |_| true)
    }

    #[test]
    fn an_existing_move_drop_still_resolves_to_a_move() {
        let zone = DropZone::Target(folder("/h/docs"));
        assert_eq!(
            resolve(
                DragOrigin::Internal,
                &["/h/a.txt"],
                false,
                &zone,
                false,
                false
            ),
            DropAction::Plan(DropPlan::Paste {
                mode: ClipMode::Cut,
                dest: p("/h/docs"),
            })
        );
        // A dragged folder dropped on a folder is still a move, not an open.
        assert_eq!(
            resolve(
                DragOrigin::Internal,
                &["/h/music"],
                true,
                &zone,
                false,
                false
            ),
            DropAction::Plan(DropPlan::Paste {
                mode: ClipMode::Cut,
                dest: p("/h/docs"),
            })
        );
        // The decision is the one plan_drop makes.
        for (ctrl, shift) in [(false, false), (true, false), (false, true), (true, true)] {
            let sources = vec![p("/h/a.txt")];
            let target = folder("/h/docs");
            assert_eq!(
                resolve_drop(
                    DragOrigin::Internal,
                    &sources,
                    false,
                    &DropZone::Target(target.clone()),
                    ctrl,
                    shift,
                    |_| false
                ),
                DropAction::Plan(plan_drop(
                    DragOrigin::Internal,
                    &sources,
                    &target,
                    ctrl,
                    shift,
                    |_| false
                ))
            );
        }
        assert_eq!(
            resolve(
                DragOrigin::Internal,
                &["/h/a.txt"],
                false,
                &DropZone::Target(DropTarget::Trash),
                false,
                false
            ),
            DropAction::Plan(DropPlan::Trash)
        );
    }

    #[test]
    fn a_folder_dragged_from_the_sidebar_drops_like_one_dragged_from_the_entries() {
        let dragged = DraggedEntries::one_folder(p("/h/music"));
        assert_eq!(dragged.sole_folder(), Some(Path::new("/h/music")));
        assert_eq!(dragged_folder(&dragged), Some(Path::new("/h/music")));
        let entries = DraggedEntries {
            paths: Arc::new(vec![p("/h/music")]),
            folder: true,
        };
        let tabs = DropZone::TabStrip {
            side: Side::Right,
            gap: 1,
        };
        for zone in [
            DropZone::Target(folder("/h/docs")),
            DropZone::Target(DropTarget::Trash),
            DropZone::SplitStrip,
            tabs,
        ] {
            let resolved = |payload: &DraggedEntries, ctrl, shift| {
                resolve_drop(
                    DragOrigin::Internal,
                    &payload.paths,
                    payload.folder,
                    &zone,
                    ctrl,
                    shift,
                    |_| true,
                )
            };
            for (ctrl, shift) in [(false, false), (true, false), (false, true), (true, true)] {
                assert_eq!(
                    resolved(&dragged, ctrl, shift),
                    resolved(&entries, ctrl, shift),
                    "{zone:?} ctrl={ctrl} shift={shift}"
                );
            }
        }
        assert_eq!(
            resolve_drop(
                DragOrigin::Internal,
                &dragged.paths,
                dragged.folder,
                &DropZone::SplitStrip,
                false,
                false,
                |_| true
            ),
            DropAction::OpenInSplit(p("/h/music"))
        );
    }

    #[test]
    fn the_split_rail_never_covers_a_tile_of_the_rightmost_column() {
        use crate::explorer::entries::grid_row_width;
        use crate::explorer::panes::MIN_PANE_WIDTH;
        use fileflux::core::zoom::GridZoom;
        // The grid lays its tiles out inside the area's padding (the width
        // `grid_row_width` is the production layout's), so the rail (the
        // width of that padding) at the area's right edge never overlaps the
        // rightmost column, at every area width a pane gets and every zoom:
        // a point over that tile is not in the split zone and resolves to the
        // folder target.
        for zoom in GridZoom::ALL {
            for width in (MIN_PANE_WIDTH as u32)..=2400 {
                let area = width as f32;
                let inner = area - 2.0 * ENTRIES_PADDING;
                let last_right = ENTRIES_PADDING + grid_row_width(inner, zoom);
                assert!(
                    last_right <= area - SPLIT_RAIL_WIDTH,
                    "{zoom:?} at {area}: the rightmost tile ends at {last_right}"
                );
            }
        }
    }

    #[test]
    fn a_grid_row_too_narrow_for_one_tile_is_one_tile_wide() {
        use crate::explorer::entries::grid_row_width;
        use crate::explorer::zoom::GridZoomExt as _;
        use fileflux::core::zoom::GridZoom;
        // The forced single column used to underflow `columns - 1`.
        for zoom in GridZoom::ALL {
            let tile = zoom.metrics().tile_width;
            for inner in [0.0, 1.0, tile - 1.0, tile] {
                assert_eq!(grid_row_width(inner, zoom), tile, "{zoom:?} at {inner}");
            }
        }
    }

    #[test]
    fn a_drop_on_the_rightmost_tile_is_the_folder_targets_plan() {
        let tile = DropZone::Target(folder("/h/docs"));
        assert!(matches!(
            resolve(DragOrigin::Internal, &["/h/a"], true, &tile, false, false),
            DropAction::Plan(DropPlan::Paste { .. })
        ));
    }

    #[test]
    fn one_dragged_folder_on_the_split_strip_opens_in_the_split() {
        assert_eq!(
            resolve(
                DragOrigin::Internal,
                &["/h/music"],
                true,
                &DropZone::SplitStrip,
                false,
                false
            ),
            DropAction::OpenInSplit(p("/h/music"))
        );
        // Modifiers do not turn the open into a copy, move or link.
        assert_eq!(
            resolve(
                DragOrigin::Internal,
                &["/h/music"],
                true,
                &DropZone::SplitStrip,
                true,
                true
            ),
            DropAction::OpenInSplit(p("/h/music"))
        );
    }

    #[test]
    fn the_strip_and_the_tab_strip_take_exactly_one_internal_folder() {
        let tabs = DropZone::TabStrip {
            side: Side::Right,
            gap: 2,
        };
        for zone in [DropZone::SplitStrip, tabs.clone()] {
            let ignore = DropAction::Plan(DropPlan::Ignore);
            // A file, several items, nothing, or files from another app.
            assert_eq!(
                resolve(
                    DragOrigin::Internal,
                    &["/h/a.txt"],
                    false,
                    &zone,
                    false,
                    false
                ),
                ignore
            );
            assert_eq!(
                resolve(
                    DragOrigin::Internal,
                    &["/h/a", "/h/b"],
                    true,
                    &zone,
                    false,
                    false
                ),
                ignore
            );
            assert_eq!(
                resolve(DragOrigin::Internal, &[], true, &zone, false, false),
                ignore
            );
            assert_eq!(
                resolve(
                    DragOrigin::External,
                    &["/h/music"],
                    true,
                    &zone,
                    false,
                    false
                ),
                ignore
            );
        }
        assert_eq!(
            resolve(
                DragOrigin::Internal,
                &["/h/music"],
                true,
                &tabs,
                false,
                false
            ),
            DropAction::OpenInTab {
                folder: p("/h/music"),
                side: Side::Right,
                gap: 2,
            }
        );
    }

    #[test]
    fn only_a_sole_folder_payload_is_a_folder_drag() {
        let one = |paths: &[&str], folder| DraggedEntries {
            paths: Arc::new(paths.iter().map(|s| p(s)).collect()),
            folder,
        };
        assert_eq!(one(&["/a"], true).sole_folder(), Some(Path::new("/a")));
        assert_eq!(one(&["/a"], false).sole_folder(), None);
        assert_eq!(one(&["/a", "/b"], true).sole_folder(), None);
        assert_eq!(dragged_folder(&one(&["/a"], true)), Some(Path::new("/a")));
        let tab = crate::explorer::tabs::DraggedTab {
            id: 1,
            side: Side::Left,
        };
        assert_eq!(dragged_folder(&tab), None);
    }
}
