//! The details panel (Ctrl+I): a read-only inspector on the right of the
//! document island, after the artboard's `.inspector`. Nothing selected
//! describes the current folder, one entry shows a preview with its facts,
//! several show a count and the total size. Permissions are probed off the
//! UI thread (see [`Explorer::sync_permissions`]); everything else comes
//! from the scan.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bolt::primitives::Icon;
use fileflux::core::places::format_bytes;
use fileflux::core::{Entry, EntryKind, Location};
use gpui::{
    AnyElement, Context, Div, Hsla, InteractiveElement, IntoElement, ParentElement, Stateful,
    Styled, div, prelude::*, px,
};

use super::state::ExplorerState;
use super::view::{Explorer, TabId};
use super::{entries, icon, util};

/// Width of the details panel.
pub(crate) const INSPECTOR_WIDTH: f32 = 300.0;
/// The document island's horizontal padding (16 px each side) that the
/// entries area loses on top of the shell and the panel.
pub(crate) const ISLAND_PADDING: f32 = 2.0 * 16.0;
/// How long a selection must rest before its permissions are probed, so
/// arrowing through a folder does not stat every entry on the way.
const PROBE_DEBOUNCE: Duration = Duration::from_millis(100);
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
pub(crate) fn panel_visible(open: bool, window_width: f32, shell_width: f32) -> bool {
    open && window_width - shell_width - ISLAND_PADDING - INSPECTOR_WIDTH >= MIN_ENTRIES_WIDTH
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

/// The selected entries of the committed listing, in listing order.
fn selected_entries(state: &ExplorerState) -> Vec<&Entry> {
    let selection = state.selection();
    state
        .listing()
        .entries
        .iter()
        .filter(|entry| selection.contains(&entry.path))
        .collect()
}

/// The full text of the current location, as the path field seeds it.
fn location_text(location: &Location) -> String {
    match location {
        Location::Directory(path) => path.to_string_lossy().into_owned(),
        Location::Trash => "Trash".to_owned(),
    }
}

/// A path that wraps at its separators even though it has no spaces.
fn wrappable(text: &str) -> String {
    text.replace('/', "/\u{200b}")
}

/// The panel: `.inspector` — 300 px, left line border, scrollable.
pub(crate) fn render(this: &Explorer, cx: &mut Context<Explorer>) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    let selected = selected_entries(&this.tab().state);
    let body = match selected.as_slice() {
        [] => none_selected(this, cx),
        [entry] => one_selected(this, entry, cx),
        many => many_selected(many, cx),
    };
    div()
        .id("explorer-inspector")
        .flex_none()
        .w(px(INSPECTOR_WIDTH))
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

fn title(text: String, cx: &Context<Explorer>) -> Div {
    let colors = bolt::colors(cx);
    div()
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

fn none_selected(this: &Explorer, cx: &mut Context<Explorer>) -> Vec<AnyElement> {
    let colors = bolt::colors(cx);
    // The folder's total; "N of M" while a filter hides some of it.
    let total = this.tab().state.listing().entries.len();
    let shown = this
        .tab()
        .state
        .is_filtering()
        .then(|| this.tab().state.visible_indices().len());
    let items = util::items_label(total, shown);
    vec![
        preview_box(folder_glyph(cx), cx).into_any_element(),
        title(this.current_label(), cx)
            .mt(px(16.0))
            .into_any_element(),
        subtitle("Folder".to_owned(), cx).into_any_element(),
        facts(
            vec![
                Fact::new("Items", items),
                Fact::new(
                    "Location",
                    wrappable(&location_text(this.tab().state.current())),
                ),
            ],
            cx,
        )
        .into_any_element(),
        div()
            .p(px(24.0))
            .text_center()
            .text_size(px(12.0))
            .line_height(px(18.0))
            .text_color(colors.muted)
            .child("Select an item to preview it.")
            .into_any_element(),
    ]
}

fn one_selected(this: &Explorer, entry: &Entry, cx: &mut Context<Explorer>) -> Vec<AnyElement> {
    let colors = bolt::colors(cx);
    let tile: AnyElement = if entry.is_directory() {
        folder_glyph(cx)
    } else if entries::has_thumbnail(&entry.name) {
        let glyph = entries::file_glyph(&entry.name);
        let muted = colors.muted;
        gpui::img(entry.path.clone())
            .size_full()
            .object_fit(gpui::ObjectFit::Contain)
            .with_loading(move || glyph_only(glyph, muted))
            .with_fallback(move || glyph_only(glyph, muted))
            .into_any_element()
    } else {
        glyph_only(entries::file_glyph(&entry.name), colors.muted)
    };

    let mut rows = Vec::new();
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
            "Original location",
            wrappable(&origin.to_string_lossy()),
        )),
        None => rows.push(Fact::new("Path", wrappable(&entry.path.to_string_lossy()))),
    }

    vec![
        preview_box(tile, cx).into_any_element(),
        title(entry.display_name().into_owned(), cx)
            .mt(px(16.0))
            .into_any_element(),
        subtitle(kind_label(entry), cx).into_any_element(),
        facts(rows, cx).into_any_element(),
    ]
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

fn glyph_only(name: gpui_kit_assets::IconName, color: Hsla) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .child(Icon::new(icon(name)).size(px(PREVIEW_GLYPH)).color(color))
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
        assert!(panel_visible(true, 1440.0, 314.0));
        // The island's 2x16 px padding is part of the budget: exactly the
        // minimum still fits, one pixel less does not.
        let exact = 314.0 + ISLAND_PADDING + INSPECTOR_WIDTH + MIN_ENTRIES_WIDTH;
        assert!(panel_visible(true, exact, 314.0));
        assert!(!panel_visible(true, exact - 1.0, 314.0));
        assert_eq!(ISLAND_PADDING, 32.0);
        // Without the padding term this window would have fit 520 px of
        // entries minus 32: it must hide.
        assert!(!panel_visible(
            true,
            314.0 + INSPECTOR_WIDTH + MIN_ENTRIES_WIDTH,
            314.0
        ));
        // The user's toggle always wins.
        assert!(!panel_visible(false, 4000.0, 314.0));
        // A narrow window without a docked sidebar: 900-16-32-300=552.
        assert!(panel_visible(true, 900.0, 16.0));
        assert!(!panel_visible(true, 800.0, 16.0));
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
