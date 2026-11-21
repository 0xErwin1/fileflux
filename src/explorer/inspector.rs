//! The details panel (Ctrl+I): a read-only inspector on the right of the
//! document island, after the artboard's `.inspector`. Nothing selected
//! describes the current folder, one entry shows a preview with its facts,
//! several show a count and the total size. Permissions are probed off the
//! UI thread (see [`Explorer::sync_permissions`]); everything else comes
//! from the scan.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bolt::primitives::Icon;
use fileflux::core::du::{self, DiskUsage, Progress, WalkEnd};
use fileflux::core::places::format_bytes;
#[cfg(test)]
use fileflux::core::settings::DEFAULT_DETAILS_WIDTH;
use fileflux::core::settings::DETAILS_WIDTH_RANGE;
use fileflux::core::{Entry, EntryKind, Location};
use gpui::{
    AnyElement, Context, Div, Hsla, InteractiveElement, IntoElement, ParentElement, Stateful,
    Styled, div, prelude::*, px,
};

use super::view::{Explorer, TabId};
use super::{entries, icon, util};

/// Default width of the details panel (see [`DEFAULT_DETAILS_WIDTH`]).
#[cfg(test)]
pub(crate) const INSPECTOR_WIDTH: f32 = DEFAULT_DETAILS_WIDTH as f32;
/// The document island's horizontal padding (16 px each side) that the
/// entries area loses on top of the shell and the panel.
pub(crate) const ISLAND_PADDING: f32 = 2.0 * 16.0;
/// How long a selection must rest before its permissions are probed, so
/// arrowing through a folder does not stat every entry on the way.
const PROBE_DEBOUNCE: Duration = Duration::from_millis(100);
/// How often a running folder-size walk publishes its partial total.
const DU_TICK: Duration = Duration::from_millis(100);
/// The entries area keeps at least this much room; below it the panel hides.
pub(crate) const MIN_ENTRIES_WIDTH: f32 = 520.0;
/// Height of the preview box.
const PREVIEW_HEIGHT: f32 = 180.0;
/// Size of the large file glyph in the preview box.
const PREVIEW_GLYPH: f32 = 72.0;
/// Height of the large folder silhouette in the preview box.
const PREVIEW_FOLDER: f32 = 80.0;

/// Whether the panel draws: the user wants it and the entries area keeps at
/// least [`MIN_ENTRIES_WIDTH`] once the panel, the island's own padding and
/// the shell around it take their room. `shell_width` is everything in the
/// window that is not the document island (gutters plus the docked sidebar).
pub(crate) fn panel_visible(
    open: bool,
    window_width: f32,
    shell_width: f32,
    panel_width: f32,
) -> bool {
    open && window_width - shell_width - ISLAND_PADDING - panel_width >= MIN_ENTRIES_WIDTH
}

/// The widest the panel may be dragged in this window: past it the entries
/// area would drop below [`MIN_ENTRIES_WIDTH`] and the panel would hide under
/// the pointer. Never below the panel's own minimum.
pub(crate) fn max_panel_width(window_width: f32, shell_width: f32) -> f32 {
    (window_width - shell_width - ISLAND_PADDING - MIN_ENTRIES_WIDTH)
        .clamp(DETAILS_WIDTH_RANGE.0 as f32, DETAILS_WIDTH_RANGE.1 as f32)
}

/// The kind line: `Folder`, `Image · JPG`, `Document · PDF`. The category
/// follows the extension-only glyph table the grid uses, so the label and
/// the icon never disagree; no content is read.
pub(crate) fn kind_label(entry: &Entry) -> String {
    use gpui_kit_assets::IconName;
    match entry.kind {
        EntryKind::Directory => return "Folder".to_owned(),
        EntryKind::Symlink => return "Broken link".to_owned(),
        EntryKind::Other => return "Special file".to_owned(),
        EntryKind::File => {}
    }
    let category = match entries::file_glyph(&entry.name) {
        IconName::FileImage => "Image",
        IconName::FileVideoCamera => "Video",
        IconName::FileMusic => "Audio",
        IconName::FileArchive => "Archive",
        IconName::FileSpreadsheet => "Spreadsheet",
        IconName::FileCode => "Code",
        IconName::FileText => "Document",
        _ => "File",
    };
    match Path::new(&entry.name)
        .extension()
        .map(OsStr::to_string_lossy)
    {
        Some(extension) => format!("{category} \u{b7} {}", extension.to_uppercase()),
        None => category.to_owned(),
    }
}

/// `rw-r--r--` from unix mode bits; setuid, setgid and sticky show as
/// `s`/`S`, `s`/`S` and `t`/`T` in the execute slot, like `ls -l`.
pub(crate) fn permission_string(mode: u32) -> String {
    /// One `rwx` slot: its bit, its letter, and the special bit that
    /// replaces the execute slot with a lower/upper-case marker.
    struct Slot {
        bit: u32,
        letter: char,
        special: Option<(u32, char, char)>,
    }
    const fn slot(bit: u32, letter: char) -> Slot {
        Slot {
            bit,
            letter,
            special: None,
        }
    }
    const fn exec(bit: u32, flag: u32, on: char, off: char) -> Slot {
        Slot {
            bit,
            letter: 'x',
            special: Some((flag, on, off)),
        }
    }
    const SLOTS: [Slot; 9] = [
        slot(0o400, 'r'),
        slot(0o200, 'w'),
        exec(0o100, 0o4000, 's', 'S'),
        slot(0o040, 'r'),
        slot(0o020, 'w'),
        exec(0o010, 0o2000, 's', 'S'),
        slot(0o004, 'r'),
        slot(0o002, 'w'),
        exec(0o001, 0o1000, 't', 'T'),
    ];
    SLOTS
        .iter()
        .map(|slot| {
            let set = mode & slot.bit != 0;
            match slot.special {
                Some((flag, on, off)) if mode & flag != 0 => {
                    if set {
                        on
                    } else {
                        off
                    }
                }
                _ if set => slot.letter,
                _ => '-',
            }
        })
        .collect()
}

/// What the probe of one path found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProbeOutcome {
    /// The probe has not settled yet.
    Pending,
    /// The unix mode bits.
    Mode(u32),
    /// The probe failed (or the platform has no mode bits).
    Unavailable,
}

/// The mode of one path, as last probed off the UI thread.
#[derive(Clone, Debug)]
pub(crate) struct PermissionProbe {
    pub(crate) path: PathBuf,
    pub(crate) outcome: ProbeOutcome,
}

/// What the probe machinery does next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProbeDecision {
    /// Nothing to do: the wanted path is already probed or running, or
    /// nothing is wanted and nothing is cached.
    Idle,
    /// Nothing is inspected: drop the cached probe.
    Clear,
    /// A probe is in flight for another path: let it finish, then decide
    /// again, so only the latest selection is probed and never two at once.
    Wait,
    /// Probe this path (after the debounce).
    Start(PathBuf),
}

/// The pure probe policy. `wanted` is the single inspected path, `cached`
/// the path of the cached probe, `in_flight` the path being probed now.
pub(crate) fn probe_decision(
    wanted: Option<&Path>,
    cached: Option<&Path>,
    in_flight: Option<&Path>,
) -> ProbeDecision {
    if in_flight.is_some() && in_flight != wanted {
        return ProbeDecision::Wait;
    }
    match wanted {
        None if cached.is_some() => ProbeDecision::Clear,
        None => ProbeDecision::Idle,
        Some(path) if cached == Some(path) || in_flight == Some(path) => ProbeDecision::Idle,
        Some(path) => ProbeDecision::Start(path.to_path_buf()),
    }
}

/// Whether a finished probe of `result_for` may be shown: only while that
/// path is still the one inspected; a stale result is dropped.
pub(crate) fn accepts_probe_result(result_for: &Path, wanted: Option<&Path>) -> bool {
    wanted == Some(result_for)
}

/// The outcome of one finished `read_mode`.
pub(crate) fn probe_outcome(result: io::Result<Option<u32>>) -> ProbeOutcome {
    match result {
        Ok(Some(mode)) => ProbeOutcome::Mode(mode),
        Ok(None) | Err(_) => ProbeOutcome::Unavailable,
    }
}

/// The Permissions row of an outcome: its text and whether it is muted.
/// `None` while the probe is pending (no row yet).
pub(crate) fn probe_row(outcome: &ProbeOutcome) -> Option<(String, bool)> {
    match outcome {
        ProbeOutcome::Pending => None,
        ProbeOutcome::Mode(mode) => Some((permission_string(*mode), false)),
        ProbeOutcome::Unavailable => Some(("unavailable".to_owned(), true)),
    }
}

/// What the folder-size walk of a tab shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DuState {
    /// Still walking: the totals so far.
    Walking(DiskUsage),
    Done(DiskUsage),
    /// The folder itself could not be inspected.
    Failed,
}

/// The folder-size result (or partial total) of one path.
#[derive(Clone, Debug)]
pub(crate) struct DuView {
    pub(crate) path: PathBuf,
    pub(crate) state: DuState,
}

/// The walk in flight: its folder and the flag that stops it.
#[derive(Clone, Debug)]
pub(crate) struct DuRun {
    pub(crate) path: PathBuf,
    pub(crate) cancel: Arc<AtomicBool>,
}

/// What the folder-size machinery does next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DuAction {
    /// The wanted folder is already measured or being measured.
    Idle,
    /// No folder is wanted: drop the cached result.
    Clear,
    /// Walk this folder (after the debounce).
    Start(PathBuf),
}

/// The pure folder-size policy: a decision plus whether the walk in flight
/// must be cancelled first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DuPlan {
    /// A walk for another folder is running: raise its cancel flag, so a
    /// newer selection never waits for an older walk (and at most one walk
    /// runs per tab).
    pub(crate) cancel_in_flight: bool,
    pub(crate) action: DuAction,
}

/// `wanted` is the folder to measure, `cached` the folder of the cached
/// result, `in_flight` the folder being walked now.
pub(crate) fn du_plan(
    wanted: Option<&Path>,
    cached: Option<&Path>,
    in_flight: Option<&Path>,
) -> DuPlan {
    let cancel_in_flight = in_flight.is_some() && in_flight != wanted;
    let running = if cancel_in_flight { None } else { in_flight };
    let action = match wanted {
        None if cached.is_some() => DuAction::Clear,
        None => DuAction::Idle,
        Some(path) if cached == Some(path) || running == Some(path) => DuAction::Idle,
        Some(path) => DuAction::Start(path.to_path_buf()),
    };
    DuPlan {
        cancel_in_flight,
        action,
    }
}

/// Whether a finished walk of `result_for` may be shown: only while that
/// folder is still the one measured; a stale result is dropped.
pub(crate) fn accepts_du_result(result_for: &Path, wanted: Option<&Path>) -> bool {
    wanted == Some(result_for)
}

/// Whether `path` is walked without being asked: not under the gvfs mount
/// directory (phones, network shares), where a walk is slow and may wake a
/// sleeping device, unless the user pressed Calculate for this very folder.
pub(crate) fn du_auto_allowed(
    path: &Path,
    gvfs_root: Option<&Path>,
    calculated: Option<&Path>,
) -> bool {
    calculated == Some(path) || !gvfs_root.is_some_and(|root| path.starts_with(root))
}

/// The Size on disk value and whether it is muted.
pub(crate) fn du_text(state: &DuState) -> (String, bool) {
    match state {
        DuState::Walking(usage) if usage.bytes == 0 && usage.files == 0 => {
            ("Calculating\u{2026}".to_owned(), true)
        }
        DuState::Walking(usage) => (
            format!("{} so far\u{2026}", format_bytes(usage.bytes)),
            false,
        ),
        DuState::Done(usage) if usage.incomplete => {
            (format!("{} (incomplete)", format_bytes(usage.bytes)), false)
        }
        DuState::Done(usage) => (format_bytes(usage.bytes), false),
        DuState::Failed => ("unavailable".to_owned(), true),
    }
}

/// The count line of a finished walk: `1204 files \u{b7} 36 folders`.
pub(crate) fn du_counts_text(usage: &DiskUsage) -> String {
    let plural = |count: u64, one: &str, many: &str| {
        if count == 1 {
            format!("1 {one}")
        } else {
            format!("{count} {many}")
        }
    };
    format!(
        "{} \u{b7} {}",
        plural(usage.files, "file", "files"),
        plural(usage.folders, "folder", "folders")
    )
}

impl Explorer {
    /// Drives the permission probe for the active tab's single selected
    /// entry while the panel shows. At most one probe is in flight per tab;
    /// a selection change while one runs waits for it and then probes only
    /// the latest selection, after a short debounce. The one `metadata` call
    /// runs on the background executor; the UI thread never touches the
    /// filesystem for it. Called from every render, so each settled probe
    /// re-decides.
    pub(crate) fn sync_permissions(&mut self, cx: &mut Context<Self>) {
        let id = self.active_id();
        let wanted = self.inspected_path(id);
        let tab = self.tab_mut();
        let decision = probe_decision(
            wanted.as_deref(),
            tab.permissions.as_ref().map(|probe| probe.path.as_path()),
            tab.permission_in_flight.as_deref(),
        );
        match decision {
            ProbeDecision::Idle | ProbeDecision::Wait => {}
            ProbeDecision::Clear => tab.permissions = None,
            ProbeDecision::Start(path) => self.start_probe(id, path, cx),
        }
    }

    fn start_probe(&mut self, id: TabId, path: PathBuf, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.by_id_mut(id) else {
            return;
        };
        tab.permissions = Some(PermissionProbe {
            path: path.clone(),
            outcome: ProbeOutcome::Pending,
        });
        tab.permission_in_flight = Some(path.clone());
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PROBE_DEBOUNCE).await;
            // Selections made during the debounce have settled: probe the
            // latest one, or none when the wish moved away from this path.
            let still_wanted = this.update(cx, |this, _| {
                accepts_probe_result(&path, this.inspected_path(id).as_deref())
            });
            let outcome = match still_wanted {
                Ok(true) => {
                    let probed = path.clone();
                    let read = cx
                        .background_executor()
                        .spawn(async move { fileflux::core::fs::read_mode(&probed) });
                    let result = read.await;
                    if let Err(error) = &result {
                        log::debug!("could not read permissions of {}: {error}", path.display());
                    }
                    Some(probe_outcome(result))
                }
                Ok(false) => None,
                Err(error) => {
                    log::debug!("the explorer closed before the permission probe ran: {error}");
                    return;
                }
            };
            if let Err(error) = this.update(cx, |this, cx| {
                let wanted = this.inspected_path(id);
                let Some(tab) = this.tabs.by_id_mut(id) else {
                    return;
                };
                tab.permission_in_flight = None;
                match outcome {
                    Some(outcome) if accepts_probe_result(&path, wanted.as_deref()) => {
                        if let Some(probe) =
                            tab.permissions.as_mut().filter(|probe| probe.path == path)
                        {
                            probe.outcome = outcome;
                        }
                    }
                    _ => {
                        // Superseded: drop the cache so the next render
                        // probes the latest selection.
                        if tab
                            .permissions
                            .as_ref()
                            .is_some_and(|probe| probe.path == path)
                        {
                            tab.permissions = None;
                        }
                    }
                }
                cx.notify();
            }) {
                log::debug!("the explorer closed before the permission probe settled: {error}");
            }
        });
        if let Some(tab) = self.tabs.by_id_mut(id) {
            tab.permission_task = Some(task);
        }
    }

    /// The folder the panel measures for tab `id`, if any: the one selected
    /// folder (a symbolic link is not a folder to measure), or the current
    /// folder when nothing is selected.
    pub(crate) fn du_target(&self, id: TabId) -> Option<PathBuf> {
        if !self.inspector_shown {
            return None;
        }
        let tab = self.tabs.iter().find(|tab| tab.id == id)?;
        match tab.state.selected_entries().as_slice() {
            [] => match tab.state.current() {
                Location::Directory(path) => Some(path.clone()),
                _ => None,
            },
            [entry] => (entry.is_directory() && !entry.is_symlink).then(|| entry.path.clone()),
            _ => None,
        }
    }

    /// Whether `path` waits for the Calculate button.
    fn du_needs_calculate(&self, id: TabId, path: &Path) -> bool {
        let calculated = self
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.du_calculated.as_deref());
        let root = fileflux::core::places::gvfs_root_from_env();
        !du_auto_allowed(path, root.as_deref(), calculated)
    }

    /// The folder to walk now: the target unless it waits for Calculate.
    fn du_wanted(&self, id: TabId) -> Option<PathBuf> {
        self.du_target(id)
            .filter(|path| !self.du_needs_calculate(id, path))
    }

    /// Drives the folder-size walk of the active tab: one walk in flight, a
    /// newer target cancels the older walk, the result of a superseded walk
    /// is dropped. Called from every render, like [`Self::sync_permissions`].
    pub(crate) fn sync_du(&mut self, cx: &mut Context<Self>) {
        let id = self.active_id();
        let wanted = self.du_wanted(id);
        let tab = self.tab_mut();
        let plan = du_plan(
            wanted.as_deref(),
            tab.du.as_ref().map(|view| view.path.as_path()),
            tab.du_run.as_ref().map(|run| run.path.as_path()),
        );
        if plan.cancel_in_flight {
            if let Some(run) = tab.du_run.take() {
                run.cancel.store(true, Ordering::Relaxed);
            }
            tab.du_task = None;
        }
        match plan.action {
            DuAction::Idle => {}
            DuAction::Clear => tab.du = None,
            DuAction::Start(path) => self.start_du(id, path, cx),
        }
    }

    fn start_du(&mut self, id: TabId, path: PathBuf, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.by_id_mut(id) else {
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        tab.du = Some(DuView {
            path: path.clone(),
            state: DuState::Walking(DiskUsage::default()),
        });
        tab.du_run = Some(DuRun {
            path: path.clone(),
            cancel: Arc::clone(&cancel),
        });
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PROBE_DEBOUNCE).await;
            let current = |this: &Self| {
                this.tabs
                    .iter()
                    .find(|tab| tab.id == id)
                    .and_then(|tab| tab.du_run.as_ref())
                    .is_some_and(|run| Arc::ptr_eq(&run.cancel, &cancel))
            };
            let go = this.update(cx, |this, _| {
                current(this) && accepts_du_result(&path, this.du_wanted(id).as_deref())
            });
            match go {
                Ok(true) => {}
                Ok(false) => {
                    // Superseded during the debounce: the next render
                    // decides again.
                    if let Err(error) = this.update(cx, |this, cx| {
                        if current(this)
                            && let Some(tab) = this.tabs.by_id_mut(id)
                        {
                            tab.du_run = None;
                            tab.du = None;
                        }
                        cx.notify();
                    }) {
                        log::debug!("the explorer closed before the size walk ran: {error}");
                    }
                    return;
                }
                Err(error) => {
                    log::debug!("the explorer closed before the size walk ran: {error}");
                    return;
                }
            }
            let progress = Arc::new(Progress::default());
            let walker = {
                let (path, cancel, progress) =
                    (path.clone(), Arc::clone(&cancel), Arc::clone(&progress));
                cx.background_executor()
                    .spawn(async move { du::walk(&path, &cancel, &progress) })
            };
            while !progress.is_finished() {
                cx.background_executor().timer(DU_TICK).await;
                let usage = progress.snapshot();
                let alive = this.update(cx, |this, cx| {
                    if current(this)
                        && let Some(tab) = this.tabs.by_id_mut(id)
                    {
                        tab.du = Some(DuView {
                            path: path.clone(),
                            state: DuState::Walking(usage),
                        });
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
            let result = walker.await;
            if let Err(error) = &result {
                log::debug!("could not measure {}: {error}", path.display());
            }
            if let Err(error) = this.update(cx, |this, cx| {
                if !current(this) {
                    return; // superseded: its cancel flag was raised
                }
                let wanted = this.du_wanted(id);
                let Some(tab) = this.tabs.by_id_mut(id) else {
                    return;
                };
                tab.du_run = None;
                let state = match result {
                    Ok(WalkEnd::Done(usage)) => Some(DuState::Done(usage)),
                    Ok(WalkEnd::Cancelled) => None,
                    Err(_) => Some(DuState::Failed),
                };
                tab.du = match state {
                    Some(state) if accepts_du_result(&path, wanted.as_deref()) => {
                        Some(DuView { path, state })
                    }
                    _ => None,
                };
                cx.notify();
            }) {
                log::debug!("the explorer closed before the size walk settled: {error}");
            }
        });
        if let Some(tab) = self.tabs.by_id_mut(id) {
            tab.du_task = Some(task);
        }
    }

    /// The path the panel inspects for tab `id`: its only selected entry,
    /// if one, while the panel shows.
    fn inspected_path(&self, id: TabId) -> Option<PathBuf> {
        let tab = self.tabs.iter().find(|tab| tab.id == id)?;
        let selection = tab.state.selection();
        if selection.len() != 1 || !self.inspector_shown {
            return None;
        }
        selection.iter().next().cloned()
    }
}

/// The full text of the current location, as the path field seeds it.
fn location_text(location: &Location) -> String {
    match location {
        Location::Directory(path) => path.to_string_lossy().into_owned(),
        Location::Trash => "Trash".to_owned(),
        Location::Recent => "Recent".to_owned(),
        Location::Search { root, query } => {
            format!("Search “{query}” in {}", root.to_string_lossy())
        }
    }
}

/// A path that wraps at its separators even though it has no spaces.
fn wrappable(text: &str) -> String {
    text.replace('/', "/\u{200b}")
}

/// The panel: `.inspector` — resizable, left line border, scrollable.
pub(crate) fn render(this: &Explorer, cx: &mut Context<Explorer>) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    let selected = this.tab().state.selected_entries();
    let body = match selected.as_slice() {
        [] => none_selected(this, cx),
        [entry] => one_selected(this, entry, cx),
        many => many_selected(many, cx),
    };
    div()
        .id("explorer-inspector")
        .flex_none()
        .w(px(this.details_width))
        .h_full()
        .border_l_1()
        .border_color(colors.line)
        .overflow_y_scroll()
        .child(div().flex().flex_col().w_full().pb(px(16.0)).children(body))
}

/// The preview box: 180 px, background token, 1 px line border.
fn preview_box(content: AnyElement, cx: &Context<Explorer>) -> Div {
    let colors = bolt::colors(cx);
    div()
        .mx(px(16.0))
        .mt(px(16.0))
        .h(px(PREVIEW_HEIGHT))
        .flex()
        .items_center()
        .justify_center()
        .overflow_hidden()
        .bg(colors.background)
        .border_1()
        .border_color(colors.line)
        .child(content)
}

fn folder_glyph(cx: &Context<Explorer>) -> AnyElement {
    let colors = bolt::colors(cx);
    entries::folder_silhouette(colors.info, colors.strong, PREVIEW_FOLDER).into_any_element()
}

fn title(text: String, cx: &Context<Explorer>) -> gpui::Stateful<Div> {
    let colors = bolt::colors(cx);
    div()
        .id("inspector-title")
        .role(gpui::accesskit::Role::Label)
        .aria_label(text.clone())
        .mx(px(16.0))
        .mt(px(0.0))
        .text_size(px(14.0))
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(colors.strong)
        .child(text)
}

fn subtitle(text: String, cx: &Context<Explorer>) -> Div {
    let colors = bolt::colors(cx);
    div()
        .mx(px(16.0))
        .mt(px(4.0))
        .text_size(px(12.0))
        .text_color(colors.muted)
        .child(text)
}

/// One row of the definition list.
struct Fact {
    term: &'static str,
    value: String,
    /// Drawn in the muted color (an unavailable value).
    muted: bool,
}

impl Fact {
    fn new(term: &'static str, value: String) -> Self {
        Self {
            term,
            value,
            muted: false,
        }
    }
}

/// The definition list: muted terms, mono right-aligned wrapping values.
fn facts(rows: Vec<Fact>, cx: &Context<Explorer>) -> Div {
    let colors = bolt::colors(cx);
    let mono = bolt::fonts::editor_family(cx);
    div()
        .m(px(16.0))
        .flex()
        .flex_col()
        .gap(px(8.0))
        .text_size(px(12.0))
        .children(rows.into_iter().map(|fact| {
            div()
                .flex()
                .gap(px(16.0))
                .child(div().flex_none().text_color(colors.muted).child(fact.term))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_right()
                        .font_family(mono.clone())
                        .text_color(if fact.muted {
                            colors.muted
                        } else {
                            colors.foreground
                        })
                        .child(fact.value),
                )
        }))
}

/// The Size on disk rows of `path` (a folder), and the Calculate button
/// when the folder waits for it.
fn du_rows(
    this: &Explorer,
    path: &Path,
    cx: &mut Context<Explorer>,
) -> (Vec<Fact>, Option<AnyElement>) {
    let id = this.active_id();
    if this.du_needs_calculate(id, path) {
        let colors = bolt::colors(cx);
        let target = path.to_path_buf();
        let button = div()
            .id("inspector-calculate-size")
            .mx(px(16.0))
            .text_size(px(12.0))
            .text_color(colors.info)
            .cursor_pointer()
            .child("Calculate size")
            .on_click(cx.listener(move |this, _, _, cx| {
                this.tab_mut().du_calculated = Some(target.clone());
                cx.notify();
            }))
            .into_any_element();
        let row = Fact {
            term: "Size on disk",
            value: "not calculated".to_owned(),
            muted: true,
        };
        return (vec![row], Some(button));
    }
    let Some(view) = this.tab().du.as_ref().filter(|view| view.path == path) else {
        return (Vec::new(), None);
    };
    let (value, muted) = du_text(&view.state);
    let mut rows = vec![Fact {
        term: "Size on disk",
        value,
        muted,
    }];
    if let DuState::Done(usage) = &view.state {
        rows.push(Fact::new("In total", du_counts_text(usage)));
    }
    (rows, None)
}

fn none_selected(this: &Explorer, cx: &mut Context<Explorer>) -> Vec<AnyElement> {
    let colors = bolt::colors(cx);
    // The folder's total; "N of M" while a filter hides some of it.
    let total = this.tab().state.listing().entries.len();
    let shown = this
        .tab()
        .state
        .shows_fewer_than_listed()
        .then(|| this.tab().state.visible_shared().len());
    let items = util::items_label(total, shown);
    let mut rows = vec![Fact::new("Items", items)];
    let mut calculate = None;
    if let Location::Directory(path) = this.tab().state.current() {
        let (size, button) = du_rows(this, path, cx);
        rows.extend(size);
        calculate = button;
    }
    rows.push(Fact::new(
        "Location",
        wrappable(&location_text(this.tab().state.current())),
    ));
    let mut body = vec![
        preview_box(folder_glyph(cx), cx).into_any_element(),
        title(this.current_label(), cx)
            .mt(px(16.0))
            .into_any_element(),
        subtitle("Folder".to_owned(), cx).into_any_element(),
        facts(rows, cx).into_any_element(),
    ];
    body.extend(calculate);
    body.push(
        div()
            .p(px(24.0))
            .text_center()
            .text_size(px(12.0))
            .line_height(px(18.0))
            .text_color(colors.muted)
            .child("Select an item to preview it.")
            .into_any_element(),
    );
    body
}

/// What the preview box shows for a file: the image itself when the app
/// decodes it, else the thumbnail an installed freedesktop thumbnailer made
/// (a video's frame, a PDF's page, an office document's first page) once the
/// background job has it. `None` keeps the type's icon — the installed
/// theme's when it has one, else the type glyph.
fn preview_source(this: &Explorer, entry: &Entry) -> Option<PathBuf> {
    if fileflux::core::thumbnails::is_decodable_image(&entry.path) {
        return Some(entry.path.clone());
    }
    this.tab()
        .thumbs
        .get(&entry.path)
        .and_then(|thumb| thumb.found.clone())
}

/// The theme icon of a plain file entry: symlinks and broken links keep
/// their Lucide glyphs, so they never get one.
fn file_theme_icon(entry: &Entry) -> Option<PathBuf> {
    (!entry.is_symlink)
        .then(|| fileflux::core::icons::for_file(&entry.name))
        .flatten()
}

fn one_selected(this: &Explorer, entry: &Entry, cx: &mut Context<Explorer>) -> Vec<AnyElement> {
    let colors = bolt::colors(cx);
    let theme = file_theme_icon(entry);
    let tile: AnyElement = if entry.is_directory() {
        folder_glyph(cx)
    } else if entries::has_thumbnail(&entry.name) {
        let glyph = entries::file_glyph(&entry.name);
        let muted = colors.muted;
        match preview_source(this, entry) {
            Some(source) => {
                let loading = theme.clone();
                gpui::img(source)
                    .size_full()
                    .object_fit(gpui::ObjectFit::Contain)
                    .with_loading(move || glyph_only(loading.clone(), glyph, muted))
                    .with_fallback(move || glyph_only(theme.clone(), glyph, muted))
                    .into_any_element()
            }
            None => glyph_only(theme, glyph, muted),
        }
    } else {
        glyph_only(theme, entries::file_glyph(&entry.name), colors.muted)
    };

    let mut rows = Vec::new();
    let mut calculate = None;
    if entry.is_directory() {
        if let Some(count) = this
            .tab()
            .state
            .child_counts()
            .get(&entry.path)
            .copied()
            .flatten()
        {
            rows.push(Fact::new("Contains", util::format_item_count(count)));
        }
        if !entry.is_symlink {
            let (size, button) = du_rows(this, &entry.path, cx);
            rows.extend(size);
            calculate = button;
        }
    } else {
        rows.push(Fact::new(
            "Size",
            entry
                .size
                .map_or_else(|| "\u{2014}".to_owned(), format_bytes),
        ));
    }
    rows.push(Fact::new(
        "Modified",
        util::format_modified(entry.modified, &chrono::Local::now()),
    ));
    if let Some((text, muted)) = this
        .tab()
        .permissions
        .as_ref()
        .filter(|probe| probe.path == entry.path)
        .and_then(|probe| probe_row(&probe.outcome))
    {
        rows.push(Fact {
            term: "Permissions",
            value: text,
            muted,
        });
    }
    match &entry.origin {
        Some(origin) => rows.push(Fact::new(
            if this.tab().state.current().lists_files_from_elsewhere() {
                "Folder"
            } else {
                "Original location"
            },
            wrappable(&origin.to_string_lossy()),
        )),
        None => rows.push(Fact::new("Path", wrappable(&entry.path.to_string_lossy()))),
    }

    let mut body = vec![
        preview_box(tile, cx).into_any_element(),
        title(entry.display_name().into_owned(), cx)
            .mt(px(16.0))
            .into_any_element(),
        subtitle(kind_label(entry), cx).into_any_element(),
        facts(rows, cx).into_any_element(),
    ];
    body.extend(calculate);
    body
}

fn many_selected(selected: &[&Entry], cx: &mut Context<Explorer>) -> Vec<AnyElement> {
    let colors = bolt::colors(cx);
    let total: u64 = selected.iter().filter_map(|entry| entry.size).sum();
    let count = div()
        .text_size(px(28.0))
        .font_family(bolt::fonts::editor_family(cx))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(colors.strong)
        .child(selected.len().to_string())
        .into_any_element();
    vec![
        preview_box(count, cx).into_any_element(),
        title(format!("{} items selected", selected.len()), cx)
            .mt(px(16.0))
            .into_any_element(),
        subtitle(format!("{} in files", format_bytes(total)), cx).into_any_element(),
    ]
}

/// The preview box's fallback art for a file, centered: the installed
/// theme's icon for its type when it has one — rasterized in the theme's
/// own colours, like the grid's tiles — else the large Lucide glyph.
fn glyph_only(theme: Option<PathBuf>, name: gpui_kit_assets::IconName, color: Hsla) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .child(
            entries::theme_icon(theme, PREVIEW_GLYPH).unwrap_or_else(|| {
                Icon::new(icon(name))
                    .size(px(PREVIEW_GLYPH))
                    .color(color)
                    .into_any_element()
            }),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use fileflux::core::{Entry, EntryKind};

    use super::*;

    fn entry(name: &str, kind: EntryKind) -> Entry {
        Entry {
            name: OsString::from(name),
            path: PathBuf::from(format!("/fixture/{name}")),
            kind,
            is_symlink: false,
            size: None,
            modified: None,
            origin: None,
        }
    }

    #[test]
    fn panel_hides_when_the_entries_area_would_drop_below_its_minimum() {
        // Window 1440, 16 px gutters + 290 sidebar + 8 gap: 1440-314-32-300.
        assert!(panel_visible(true, 1440.0, 314.0, INSPECTOR_WIDTH));
        // The island's 2x16 px padding is part of the budget: exactly the
        // minimum still fits, one pixel less does not.
        let exact = 314.0 + ISLAND_PADDING + INSPECTOR_WIDTH + MIN_ENTRIES_WIDTH;
        assert!(panel_visible(true, exact, 314.0, INSPECTOR_WIDTH));
        assert!(!panel_visible(true, exact - 1.0, 314.0, INSPECTOR_WIDTH));
        assert_eq!(ISLAND_PADDING, 32.0);
        // Without the padding term this window would have fit 520 px of
        // entries minus 32: it must hide.
        assert!(!panel_visible(
            true,
            314.0 + INSPECTOR_WIDTH + MIN_ENTRIES_WIDTH,
            314.0,
            INSPECTOR_WIDTH
        ));
        // The user's toggle always wins.
        assert!(!panel_visible(false, 4000.0, 314.0, INSPECTOR_WIDTH));
        // A narrow window without a docked sidebar: 900-16-32-300=552.
        assert!(panel_visible(true, 900.0, 16.0, INSPECTOR_WIDTH));
        assert!(!panel_visible(true, 800.0, 16.0, INSPECTOR_WIDTH));
    }

    /// The drag cap leaves the entries area its minimum and stays in range.
    #[test]
    fn the_drag_cap_keeps_the_entries_area_alive() {
        assert_eq!(max_panel_width(4000.0, 314.0), 600.0);
        assert_eq!(
            max_panel_width(1400.0, 314.0),
            1400.0 - 314.0 - ISLAND_PADDING - MIN_ENTRIES_WIDTH
        );
        assert_eq!(
            max_panel_width(600.0, 314.0),
            240.0,
            "never below the minimum"
        );
    }

    /// The auto-hide threshold follows the panel's current width: a wider
    /// panel hides at a larger window, a narrower one keeps showing.
    #[test]
    fn the_auto_hide_threshold_uses_the_current_panel_width() {
        let exact = |panel: f32| 314.0 + ISLAND_PADDING + panel + MIN_ENTRIES_WIDTH;
        assert!(panel_visible(true, exact(500.0), 314.0, 500.0));
        assert!(!panel_visible(true, exact(500.0) - 1.0, 314.0, 500.0));
        // 1000 px fits a 240 px panel but not a 500 px one.
        assert!(panel_visible(true, exact(240.0), 314.0, 240.0));
        assert!(!panel_visible(true, exact(240.0), 314.0, 500.0));
    }

    fn p(path: &str) -> PathBuf {
        PathBuf::from(path)
    }

    #[test]
    fn probe_decision_starts_the_wanted_path_when_nothing_is_cached_or_running() {
        let a = p("/a");
        assert_eq!(
            probe_decision(Some(&a), None, None),
            ProbeDecision::Start(p("/a"))
        );
        assert_eq!(probe_decision(None, None, None), ProbeDecision::Idle);
    }

    #[test]
    fn probe_decision_is_idle_for_an_already_probed_path_and_clears_without_a_wish() {
        let a = p("/a");
        assert_eq!(
            probe_decision(Some(&a), Some(&a), None),
            ProbeDecision::Idle
        );
        assert_eq!(
            probe_decision(Some(&a), Some(&a), Some(&a)),
            ProbeDecision::Idle,
            "the running probe is the wanted one"
        );
        assert_eq!(
            probe_decision(Some(&a), None, Some(&a)),
            ProbeDecision::Idle,
            "a reload that dropped the cache never starts a second probe"
        );
        assert_eq!(
            probe_decision(None, Some(&a), None),
            ProbeDecision::Clear,
            "a stale cache goes away when nothing is inspected"
        );
    }

    #[test]
    fn probe_decision_waits_while_one_probe_is_in_flight_then_probes_only_the_latest() {
        let (a, b, c) = (p("/a"), p("/b"), p("/c"));
        // /a is running; the selection moved to /b, then /c: wait, never start.
        assert_eq!(
            probe_decision(Some(&b), Some(&a), Some(&a)),
            ProbeDecision::Wait
        );
        assert_eq!(
            probe_decision(Some(&c), Some(&a), Some(&a)),
            ProbeDecision::Wait
        );
        // /a settled: the next probe is for the latest wish, /c, never /b.
        assert_eq!(
            probe_decision(Some(&c), Some(&a), None),
            ProbeDecision::Start(p("/c"))
        );
        // Nothing wanted while one runs: wait for it, then clear.
        assert_eq!(
            probe_decision(None, Some(&a), Some(&a)),
            ProbeDecision::Wait
        );
    }

    #[test]
    fn a_probe_result_is_applied_only_for_the_path_still_wanted() {
        let (a, b) = (p("/a"), p("/b"));
        assert!(accepts_probe_result(&a, Some(&a)));
        assert!(!accepts_probe_result(&a, Some(&b)), "stale result ignored");
        assert!(!accepts_probe_result(&a, None), "nothing inspected");
    }

    #[test]
    fn a_failed_probe_reads_unavailable_not_absent() {
        assert_eq!(
            probe_row(&ProbeOutcome::Mode(0o644)),
            Some(("rw-r--r--".to_owned(), false))
        );
        assert_eq!(
            probe_row(&ProbeOutcome::Unavailable),
            Some(("unavailable".to_owned(), true)),
            "muted text instead of a vanished row"
        );
        assert_eq!(probe_row(&ProbeOutcome::Pending), None);
        // A platform without mode bits: the probe ran but has nothing to show.
        assert_eq!(probe_outcome(Ok(None)), ProbeOutcome::Unavailable);
        assert_eq!(probe_outcome(Ok(Some(0o600))), ProbeOutcome::Mode(0o600));
        let failure = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert_eq!(probe_outcome(Err(failure)), ProbeOutcome::Unavailable);
    }

    #[test]
    fn du_plan_starts_the_wanted_folder_when_nothing_runs() {
        let a = p("/a");
        assert_eq!(
            du_plan(Some(&a), None, None),
            DuPlan {
                cancel_in_flight: false,
                action: DuAction::Start(p("/a"))
            }
        );
        assert_eq!(
            du_plan(None, None, None),
            DuPlan {
                cancel_in_flight: false,
                action: DuAction::Idle
            }
        );
    }

    #[test]
    fn du_plan_never_restarts_a_measured_or_running_folder() {
        let a = p("/a");
        for (cached, running) in [(Some(&a), None), (Some(&a), Some(&a)), (None, Some(&a))] {
            assert_eq!(
                du_plan(
                    Some(&a),
                    cached.map(PathBuf::as_path),
                    running.map(PathBuf::as_path)
                ),
                DuPlan {
                    cancel_in_flight: false,
                    action: DuAction::Idle
                }
            );
        }
    }

    #[test]
    fn a_newer_selection_cancels_the_older_walk_and_starts_the_latest() {
        let (a, b) = (p("/a"), p("/b"));
        assert_eq!(
            du_plan(Some(&b), Some(&a), Some(&a)),
            DuPlan {
                cancel_in_flight: true,
                action: DuAction::Start(p("/b"))
            },
            "one walk in flight: the old one is cancelled, not awaited"
        );
        assert_eq!(
            du_plan(None, Some(&a), Some(&a)),
            DuPlan {
                cancel_in_flight: true,
                action: DuAction::Clear
            }
        );
    }

    #[test]
    fn a_stale_walk_result_is_dropped() {
        let (a, b) = (p("/a"), p("/b"));
        assert!(accepts_du_result(&a, Some(&a)));
        assert!(!accepts_du_result(&a, Some(&b)));
        assert!(!accepts_du_result(&a, None));
    }

    #[test]
    fn gvfs_folders_wait_for_calculate_and_everything_else_walks() {
        let root = p("/run/user/1000/gvfs");
        let phone = p("/run/user/1000/gvfs/mtp:host=x/DCIM");
        let home = p("/home/me");
        assert!(!du_auto_allowed(&phone, Some(&root), None));
        assert!(du_auto_allowed(&phone, Some(&root), Some(&phone)));
        assert!(
            !du_auto_allowed(&phone, Some(&root), Some(&home)),
            "Calculate applies to its own folder only"
        );
        assert!(du_auto_allowed(&home, Some(&root), None));
        assert!(du_auto_allowed(&phone, None, None), "no gvfs, no gate");
    }

    #[test]
    fn size_on_disk_streams_then_settles_and_flags_incomplete() {
        let usage = |bytes, files, incomplete| DiskUsage {
            bytes,
            files,
            folders: 1,
            incomplete,
        };
        assert_eq!(
            du_text(&DuState::Walking(DiskUsage::default())),
            ("Calculating\u{2026}".to_owned(), true)
        );
        assert_eq!(
            du_text(&DuState::Walking(usage(12_400_000_000, 5, false))),
            ("12.4 GB so far\u{2026}".to_owned(), false)
        );
        assert_eq!(
            du_text(&DuState::Done(usage(12_400_000_000, 5, false))),
            ("12.4 GB".to_owned(), false)
        );
        assert_eq!(
            du_text(&DuState::Done(usage(1_500_000, 5, true))),
            ("1.5 MB (incomplete)".to_owned(), false)
        );
        assert_eq!(du_text(&DuState::Failed), ("unavailable".to_owned(), true));
        assert_eq!(
            du_counts_text(&DiskUsage {
                bytes: 0,
                files: 1,
                folders: 2,
                incomplete: false
            }),
            "1 file \u{b7} 2 folders"
        );
    }

    #[test]
    fn permission_string_renders_rwx_triplets() {
        assert_eq!(permission_string(0o100644), "rw-r--r--");
        assert_eq!(permission_string(0o755), "rwxr-xr-x");
        assert_eq!(permission_string(0o000), "---------");
        assert_eq!(permission_string(0o777), "rwxrwxrwx");
    }

    #[test]
    fn permission_string_marks_special_bits() {
        assert_eq!(permission_string(0o4755), "rwsr-xr-x");
        assert_eq!(permission_string(0o4644), "rwSr--r--");
        assert_eq!(permission_string(0o2755), "rwxr-sr-x");
        assert_eq!(permission_string(0o1777), "rwxrwxrwt");
        assert_eq!(permission_string(0o1666), "rw-rw-rwT");
    }

    #[test]
    fn kind_label_names_category_and_extension() {
        assert_eq!(kind_label(&entry("a.jpg", EntryKind::File)), "Image · JPG");
        assert_eq!(
            kind_label(&entry("A.PDF", EntryKind::File)),
            "Document · PDF"
        );
        assert_eq!(kind_label(&entry("x.rs", EntryKind::File)), "Code · RS");
        assert_eq!(
            kind_label(&entry("x.tar", EntryKind::File)),
            "Archive · TAR"
        );
        assert_eq!(kind_label(&entry("x.zzz", EntryKind::File)), "File · ZZZ");
        assert_eq!(kind_label(&entry("Makefile", EntryKind::File)), "File");
        assert_eq!(kind_label(&entry(".bashrc", EntryKind::File)), "File");
        assert_eq!(kind_label(&entry("pics", EntryKind::Directory)), "Folder");
        assert_eq!(
            kind_label(&entry("dead", EntryKind::Symlink)),
            "Broken link"
        );
        assert_eq!(kind_label(&entry("sock", EntryKind::Other)), "Special file");
    }
}
