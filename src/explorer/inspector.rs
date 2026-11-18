//! The details panel (Ctrl+I): a read-only inspector on the right of the
//! document island, after the artboard's `.inspector`. Nothing selected
//! describes the current folder, one entry shows a preview with its facts,
//! several show a count and the total size. Permissions are probed off the
//! UI thread (see [`Explorer::sync_permissions`]); everything else comes
//! from the scan.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use bolt::primitives::Icon;
use fileflux::core::places::format_bytes;
use fileflux::core::{Entry, EntryKind, Location};
use gpui::{
    AnyElement, Context, Div, Hsla, InteractiveElement, IntoElement, ParentElement, Stateful,
    Styled, div, prelude::*, px,
};

use super::state::ExplorerState;
use super::view::Explorer;
use super::{entries, icon, util};

/// Width of the details panel.
pub(crate) const INSPECTOR_WIDTH: f32 = 300.0;
/// The entries area keeps at least this much room; below it the panel hides.
pub(crate) const MIN_ENTRIES_WIDTH: f32 = 520.0;
/// Height of the preview box.
const PREVIEW_HEIGHT: f32 = 180.0;
/// Size of the large file glyph in the preview box.
const PREVIEW_GLYPH: f32 = 72.0;
/// Height of the large folder silhouette in the preview box.
const PREVIEW_FOLDER: f32 = 80.0;

/// Whether the panel draws: the user wants it and the entries area keeps at
/// least [`MIN_ENTRIES_WIDTH`] once the panel and the shell around it take
/// their room. `shell_width` is everything in the window that is not the
/// document island (gutters plus the docked sidebar).
pub(crate) fn panel_visible(open: bool, window_width: f32, shell_width: f32) -> bool {
    open && window_width - shell_width - INSPECTOR_WIDTH >= MIN_ENTRIES_WIDTH
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

/// The mode of one path, as last probed off the UI thread. `mode` stays
/// `None` while the probe is in flight or when it failed.
#[derive(Clone, Debug)]
pub(crate) struct PermissionProbe {
    pub(crate) path: PathBuf,
    pub(crate) mode: Option<u32>,
}

impl Explorer {
    /// Starts the permission probe for the single selected entry when the
    /// panel shows and the cached probe is for another path. The one
    /// `metadata` call runs on the background executor; the UI thread never
    /// touches the filesystem for it.
    pub(crate) fn sync_permissions(&mut self, cx: &mut Context<Self>) {
        let wanted = self.inspected_path();
        let current = self.permissions.as_ref().map(|probe| &probe.path);
        if wanted.as_ref() == current {
            return;
        }
        let Some(path) = wanted else {
            self.permissions = None;
            self.permission_task = None;
            return;
        };
        self.permissions = Some(PermissionProbe {
            path: path.clone(),
            mode: None,
        });
        let probed = path.clone();
        let read = cx
            .background_executor()
            .spawn(async move { fileflux::core::fs::read_mode(&probed) });
        self.permission_task = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            if let Err(error) = this.update(cx, |this, cx| match result {
                Ok(mode) => {
                    if let Some(probe) = this.permissions.as_mut().filter(|p| p.path == path) {
                        probe.mode = mode;
                        cx.notify();
                    }
                }
                Err(error) => {
                    log::debug!("could not read permissions of {}: {error}", path.display());
                }
            }) {
                log::debug!("the explorer closed before the permission probe settled: {error}");
            }
        }));
    }

    /// The path the panel inspects: the only selected entry, if one.
    fn inspected_path(&self) -> Option<PathBuf> {
        let selection = self.state.selection();
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
    let selected = selected_entries(&this.state);
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

/// The definition list: muted terms, mono right-aligned wrapping values.
fn facts(rows: Vec<(&'static str, String)>, cx: &Context<Explorer>) -> Div {
    let colors = bolt::colors(cx);
    let mono = bolt::fonts::editor_family(cx);
    div()
        .m(px(16.0))
        .flex()
        .flex_col()
        .gap(px(8.0))
        .text_size(px(12.0))
        .children(rows.into_iter().map(|(term, value)| {
            div()
                .flex()
                .gap(px(16.0))
                .child(div().flex_none().text_color(colors.muted).child(term))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_right()
                        .font_family(mono.clone())
                        .text_color(colors.foreground)
                        .child(value),
                )
        }))
}

fn none_selected(this: &Explorer, cx: &mut Context<Explorer>) -> Vec<AnyElement> {
    let colors = bolt::colors(cx);
    vec![
        preview_box(folder_glyph(cx), cx).into_any_element(),
        title(this.current_label(), cx)
            .mt(px(16.0))
            .into_any_element(),
        subtitle("Folder".to_owned(), cx).into_any_element(),
        facts(
            vec![
                ("Items", this.state.visible_indices().len().to_string()),
                ("Location", wrappable(&location_text(this.state.current()))),
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
    if !entry.is_directory() {
        rows.push((
            "Size",
            entry
                .size
                .map_or_else(|| "\u{2014}".to_owned(), format_bytes),
        ));
    }
    rows.push((
        "Modified",
        util::format_modified(entry.modified, &chrono::Local::now()),
    ));
    if let Some(mode) = this
        .permissions
        .as_ref()
        .filter(|probe| probe.path == entry.path)
        .and_then(|probe| probe.mode)
    {
        rows.push(("Permissions", permission_string(mode)));
    }
    match &entry.origin {
        Some(origin) => rows.push(("Original location", wrappable(&origin.to_string_lossy()))),
        None => rows.push(("Path", wrappable(&entry.path.to_string_lossy()))),
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
        // Window 1440, 16 px gutters + 290 sidebar + 8 gap: 1440-314-300=826.
        assert!(panel_visible(true, 1440.0, 314.0));
        // Exactly the minimum still fits.
        assert!(panel_visible(
            true,
            314.0 + INSPECTOR_WIDTH + MIN_ENTRIES_WIDTH,
            314.0
        ));
        assert!(!panel_visible(
            true,
            314.0 + INSPECTOR_WIDTH + MIN_ENTRIES_WIDTH - 1.0,
            314.0
        ));
        // The user's toggle always wins.
        assert!(!panel_visible(false, 4000.0, 314.0));
        // A narrow window without a docked sidebar: 900-16-300=584.
        assert!(panel_visible(true, 900.0, 16.0));
        assert!(!panel_visible(true, 800.0, 16.0));
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
