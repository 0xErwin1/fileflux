//! The details panel's permission and group editor.
//!
//! The editor shows the selected entries' access per class (owner, group,
//! others), "Allow executing" for files and the group, with a mixed state
//! where the entries differ. Choices are a draft; Apply changes only what the
//! user changed, "Apply to enclosed files" walks the selected folders. Both
//! run as an operation off the UI thread ([`Job::Permissions`]) with progress
//! and cancel, answer with one summary notice (failures as its details) and
//! leave an undo that restores the exact previous modes and groups.
//!
//! The editor exists only for entries of a real folder: the Trash, Recent and
//! search views stay read-only. Symbolic links are never changed (their own
//! mode cannot be changed on Linux) and are reported as skipped.

use std::path::PathBuf;
use std::time::Duration;

use bolt::controls::Button;
use bolt::primitives::Icon;
use fileflux::core::Location;
use fileflux::core::perms::{
    self, Access, Applied, Change, Class, Edit, Exec, Group, Merged, Summary,
};
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Styled, Window, div,
    prelude::*, px,
};

use super::ops::{Job, Notice, Report, Severity, UndoAction};
use super::progress::Sink;
use super::view::{Explorer, TabId};

/// How long a selection must rest before its entries are read, so arrowing
/// through a folder does not stat everything on the way.
const LOAD_DEBOUNCE: Duration = Duration::from_millis(100);

/// One permission change, owned by the background executor.
#[derive(Clone, Debug)]
pub(crate) struct PermsJob {
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) change: Change,
    /// Also change everything inside the folders among `paths`.
    pub(crate) enclosed: bool,
}

/// What the entries looked like when read, and what the user may pick.
#[derive(Clone, Debug)]
pub(crate) struct Loaded {
    pub(crate) summary: Summary,
    /// The groups the user belongs to.
    pub(crate) groups: Vec<Group>,
    /// The process's groups could not be read: the group stays read-only.
    pub(crate) groups_available: bool,
    pub(crate) user_names: Vec<(u32, String)>,
    pub(crate) group_names: Vec<(u32, String)>,
    /// Entries that could not be read at all.
    pub(crate) unreadable: usize,
}

/// The editor of one selection: what was read and the user's draft.
#[derive(Clone, Debug)]
pub(crate) struct Editor {
    /// The selected paths (sorted) this editor was read for.
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) loaded: Loaded,
    pub(crate) draft: Edit,
    pub(crate) group_choice: Option<u32>,
    pub(crate) group_list_open: bool,
    /// The enclosed action was pressed once and waits for a second press.
    pub(crate) armed: bool,
}

impl Editor {
    pub(crate) fn new(paths: Vec<PathBuf>, loaded: Loaded) -> Self {
        Self {
            paths,
            loaded,
            draft: Edit::default(),
            group_choice: None,
            group_list_open: false,
            armed: false,
        }
    }

    /// What Apply changes: the classes whose choice differs from what the
    /// entries have, and the group when it differs.
    pub(crate) fn change(&self) -> Change {
        Change {
            edit: self.draft.changed_against(&self.loaded.summary),
            gid: self
                .group_choice
                .filter(|gid| self.loaded.summary.gid != Some(Merged::Same(*gid))),
        }
    }

    /// What "Apply to enclosed files" applies: the folder rule as the
    /// editor shows it, with the group only when the user picked one.
    pub(crate) fn enclosed_change(&self) -> Change {
        Change {
            edit: self.draft.effective(&self.loaded.summary),
            gid: self.group_choice,
        }
    }

    /// Whether "Apply to enclosed files" would change anything: not when
    /// every class is mixed (or custom) and nothing is drafted.
    pub(crate) fn can_apply_enclosed(&self) -> bool {
        let change = self.enclosed_change();
        !change.edit.is_empty() || change.gid.is_some()
    }

    pub(crate) fn dirty(&self) -> bool {
        !self.draft.is_empty() || self.group_choice.is_some()
    }
}

/// What the editor machinery does next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EditorDecision {
    Idle,
    /// Nothing is wanted: drop the editor.
    Clear,
    /// Another read is in flight: let it finish, then decide again.
    Wait,
    Start(Vec<PathBuf>),
}

/// The pure policy: `wanted` is the sorted selection (empty: no editor),
/// `loaded` the paths of the editor now, `in_flight` those being read.
pub(crate) fn editor_decision(
    wanted: &[PathBuf],
    loaded: Option<&[PathBuf]>,
    in_flight: Option<&[PathBuf]>,
) -> EditorDecision {
    if in_flight.is_some_and(|paths| paths != wanted) {
        return EditorDecision::Wait;
    }
    if wanted.is_empty() {
        return if loaded.is_some() {
            EditorDecision::Clear
        } else {
            EditorDecision::Idle
        };
    }
    if loaded == Some(wanted) || in_flight == Some(wanted) {
        EditorDecision::Idle
    } else {
        EditorDecision::Start(wanted.to_vec())
    }
}

/// The groups to offer, and whether they could be read at all.
fn groups_or_unavailable(read: std::io::Result<Vec<Group>>) -> (Vec<Group>, bool) {
    match read {
        Ok(groups) => (groups, true),
        Err(error) => {
            log::warn!("could not read the user's groups: {error}");
            (Vec::new(), false)
        }
    }
}

/// Reads the entries (blocking: the background executor).
pub(crate) fn load(paths: &[PathBuf]) -> Loaded {
    let states: Vec<_> = paths
        .iter()
        .filter_map(|path| perms::read_state(path).ok())
        .collect();
    let read =
        |file: &str| perms::parse_id_file(&std::fs::read_to_string(file).unwrap_or_default());
    let (groups, groups_available) = groups_or_unavailable(perms::current_groups());
    Loaded {
        summary: perms::summarize(&states),
        groups,
        groups_available,
        user_names: read("/etc/passwd"),
        group_names: read("/etc/group"),
        unreadable: paths.len() - states.len(),
    }
}

// --- the job --------------------------------------------------------------------

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// The one summary notice of a finished change (or undo), the failures as
/// its details, and the undo to record. `what` completes "Changed …".
pub(crate) fn report_from(applied: Applied, what: &str, verb: &str) -> Report {
    let mut parts = Vec::new();
    if applied.changed > 0 {
        parts.push(format!(
            "{verb} {what} of {}",
            plural(applied.changed, "item", "items")
        ));
    } else if applied.failures.is_empty() {
        parts.push(format!("Nothing to change in {what}"));
    }
    if !applied.failures.is_empty() {
        parts.push(format!("{} failed", applied.failures.len()));
    }
    if applied.cancelled {
        parts.push("cancelled".to_owned());
    }
    let mut text = parts.join("; ");
    if applied.symlinks > 0 {
        text.push_str(&format!(
            ". Skipped {}: their own permissions cannot be changed",
            plural(applied.symlinks, "symbolic link", "symbolic links")
        ));
    }
    if applied.other_filesystems > 0 {
        text.push_str(&format!(
            ". Skipped {} on another filesystem",
            plural(applied.other_filesystems, "folder", "folders")
        ));
    }
    if applied.undo_dropped {
        text.push_str(". Too many items to undo");
    }
    let severity = if !applied.failures.is_empty() {
        if applied.changed == 0 {
            Severity::Error
        } else {
            Severity::Warning
        }
    } else if applied.cancelled {
        Severity::Warning
    } else if applied.changed == 0 {
        Severity::Info
    } else {
        Severity::Success
    };
    Report {
        notice: Some(Notice { severity, text }),
        undo: (!applied.previous.is_empty() && !applied.undo_dropped).then_some(
            UndoAction::Permissions {
                items: applied.previous,
            },
        ),
        details: applied
            .failures
            .iter()
            .map(|failure| format!("{}: {}", failure.path.display(), failure.message))
            .collect(),
        cancelled: applied.cancelled,
        permissions_changed: true,
        ..Report::default()
    }
}

/// What a change is called in the notice.
pub(crate) fn what_changed(change: &Change) -> &'static str {
    match (change.edit.is_empty(), change.gid.is_some()) {
        (false, true) => "permissions and group",
        (true, true) => "group",
        _ => "permissions",
    }
}

/// Performs a change (blocking: the background executor).
pub(crate) fn run(job: PermsJob, sink: &Sink) -> Report {
    if !job.enclosed {
        sink.set_total(job.paths.len() as u64);
    }
    let applied = perms::apply(
        &job.paths,
        &job.change,
        job.enclosed,
        sink.cancel_flag(),
        &mut |path, done| {
            sink.advance(
                done,
                &path
                    .file_name()
                    .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
            );
        },
    );
    report_from(applied, what_changed(&job.change), "Changed")
}

/// Restores what a change recorded (blocking).
pub(crate) fn undo(items: &[perms::Previous], sink: &Sink) -> Report {
    sink.set_total(items.len() as u64);
    let applied = perms::restore(items, sink.cancel_flag(), &mut |path, done| {
        sink.advance(
            done,
            &path
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
        );
    });
    let mut report = report_from(applied, "permissions", "Restored");
    // An undo is not itself undoable.
    report.undo = None;
    report
}

// --- the Explorer side ----------------------------------------------------------

impl Explorer {
    /// The sorted paths the editor is for in tab `id`: its visible selected
    /// entries, only in a real folder with the panel showing.
    pub(crate) fn permission_wanted(&self, id: TabId) -> Vec<PathBuf> {
        if !self.inspector_shown {
            return Vec::new();
        }
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == id) else {
            return Vec::new();
        };
        if !matches!(tab.state.current(), Location::Directory(_)) {
            return Vec::new();
        }
        let mut paths: Vec<PathBuf> = tab
            .state
            .visible_selected()
            .into_iter()
            .map(|entry| entry.path.clone())
            .collect();
        paths.sort();
        paths
    }

    /// Reads the selected entries for the editor, at most one read at a
    /// time per tab; called from every render like the probe.
    pub(crate) fn sync_permission_editor(&mut self, cx: &mut Context<Self>) {
        let id = self.active_id();
        let wanted = self.permission_wanted(id);
        let tab = self.tab_mut();
        let decision = editor_decision(
            &wanted,
            tab.perms.as_ref().map(|editor| editor.paths.as_slice()),
            tab.perms_in_flight.as_deref(),
        );
        match decision {
            EditorDecision::Idle | EditorDecision::Wait => {}
            EditorDecision::Clear => tab.perms = None,
            EditorDecision::Start(paths) => self.start_permission_load(id, paths, cx),
        }
    }

    fn start_permission_load(&mut self, id: TabId, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.by_id_mut(id) else {
            return;
        };
        tab.perms = None;
        tab.perms_in_flight = Some(paths.clone());
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LOAD_DEBOUNCE).await;
            let still_wanted = this.update(cx, |this, _| this.permission_wanted(id) == paths);
            let loaded = match still_wanted {
                Ok(true) => {
                    let read = paths.clone();
                    Some(
                        cx.background_executor()
                            .spawn(async move { load(&read) })
                            .await,
                    )
                }
                Ok(false) => None,
                Err(error) => {
                    log::debug!("the explorer closed before the permissions were read: {error}");
                    return;
                }
            };
            if let Err(error) = this.update(cx, |this, cx| {
                let still_wanted = this.permission_wanted(id) == paths;
                let Some(tab) = this.tabs.by_id_mut(id) else {
                    return;
                };
                tab.perms_in_flight = None;
                tab.perms = loaded
                    .filter(|_| still_wanted)
                    .map(|loaded| Editor::new(paths, loaded));
                cx.notify();
            }) {
                log::debug!("the explorer closed before the permissions settled: {error}");
            }
        });
        if let Some(tab) = self.tabs.by_id_mut(id) {
            tab.perms_task = Some(task);
        }
    }

    /// Forgets what every tab read, after a change: the next render reads
    /// the entries again.
    pub(crate) fn forget_permission_editors(&mut self) {
        for tab in self.tabs.iter_mut() {
            tab.perms = None;
            tab.perms_task = None;
            tab.perms_in_flight = None;
            // The single-entry mode row reads again too.
            tab.permissions = None;
        }
    }

    fn editor_mut(&mut self) -> Option<&mut Editor> {
        self.tab_mut().perms.as_mut()
    }

    /// Runs Apply or Apply to enclosed files for the editor's entries.
    fn apply_permissions(&mut self, enclosed: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.tab().perms.as_ref() else {
            return;
        };
        let change = if enclosed {
            editor.enclosed_change()
        } else {
            editor.change()
        };
        if change.edit.is_empty() && change.gid.is_none() {
            return;
        }
        let job = Job::Permissions(PermsJob {
            paths: editor.paths.clone(),
            change,
            enclosed,
        });
        let title = if enclosed {
            "Changing permissions inside"
        } else {
            "Changing permissions"
        };
        self.start_job(title, job, true, window, cx);
    }
}

// --- the panel ------------------------------------------------------------------

/// The value-line caption of a class row.
fn class_note(shown: Option<Merged<Access>>, draft: Option<Access>) -> Option<&'static str> {
    match (draft, shown) {
        (Some(_), _) => None,
        (None, Some(Merged::Mixed)) => Some("mixed"),
        (None, Some(Merged::Same(Access::WriteOnly))) => Some("write only"),
        (None, Some(Merged::Same(Access::Custom))) => Some("custom"),
        _ => None,
    }
}

/// Whether "Allow executing" shows as on, off, or mixed (`None`).
fn exec_shown(summary: &Summary, draft: Option<bool>) -> Option<bool> {
    draft.or(match summary.exec {
        Some(Merged::Same(Exec::On)) => Some(true),
        Some(Merged::Same(Exec::Off)) => Some(false),
        _ => None,
    })
}

fn label(text: String, cx: &Context<Explorer>) -> gpui::Div {
    let colors = bolt::colors(cx);
    div()
        .text_size(px(12.0))
        .text_color(colors.muted)
        .child(text)
}

/// One choice chip.
/// The width of one choice cell in the access grid; the header uses the same
/// number so the columns line up.
const CELL_WIDTH: f32 = 74.0;

fn chip(
    id: impl Into<gpui::ElementId>,
    text: impl Into<gpui::SharedString>,
    on: bool,
    cx: &Context<Explorer>,
) -> gpui::Stateful<gpui::Div> {
    let colors = bolt::colors(cx);
    div()
        .id(id)
        .w(px(CELL_WIDTH))
        .h(px(22.0))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(11.0))
        .border_1()
        .cursor_pointer()
        .border_color(if on { colors.info } else { colors.line })
        .bg(if on {
            colors.info.opacity(0.14)
        } else {
            colors.background
        })
        .text_color(if on { colors.info } else { colors.foreground })
        .child(text.into())
}

fn section(children: Vec<AnyElement>) -> AnyElement {
    div()
        .mx(px(16.0))
        .mb(px(12.0))
        .flex()
        .flex_col()
        .gap(px(6.0))
        .children(children)
        .into_any_element()
}

fn name_or_id(names: &[(u32, String)], id: Option<Merged<u32>>) -> String {
    match id {
        Some(Merged::Same(id)) => {
            perms::name_of(names, id).map_or_else(|| id.to_string(), str::to_owned)
        }
        Some(Merged::Mixed) => "several".to_owned(),
        None => "\u{2014}".to_owned(),
    }
}

/// The editor's rows for the panel body (a selection of one or more
/// entries), empty where the panel is read-only.
pub(crate) fn render(this: &Explorer, cx: &mut Context<Explorer>) -> Vec<AnyElement> {
    let colors = bolt::colors(cx);
    let wanted = this.permission_wanted(this.active_id());
    if wanted.is_empty() {
        return Vec::new();
    }
    let heading = div()
        .mx(px(16.0))
        .mt(px(4.0))
        .mb(px(8.0))
        .text_size(px(12.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(colors.strong)
        .child("Permissions")
        .into_any_element();
    let Some(editor) = this.tab().perms.as_ref().filter(|e| e.paths == wanted) else {
        return vec![
            heading,
            section(vec![
                label("Reading permissions\u{2026}".to_owned(), cx).into_any_element(),
            ]),
        ];
    };
    let summary = &editor.loaded.summary;
    let mut rows = vec![heading];
    if summary.files + summary.folders == 0 {
        let text = if editor.loaded.unreadable > 0 {
            "These entries could not be read."
        } else {
            "A symbolic link has no permissions of its own to change."
        };
        rows.push(section(vec![label(text.to_owned(), cx).into_any_element()]));
        return rows;
    }

    // Owner: shown, never changed (that needs root).
    rows.push(section(vec![
        label(
            format!(
                "Owner: {}",
                name_or_id(&editor.loaded.user_names, summary.uid)
            ),
            cx,
        )
        .into_any_element(),
    ]));

    // One column per choice, labelled once: the same three buttons repeated
    // per class read as a wall of chips.
    rows.push(
        div()
            .mx(px(16.0))
            .mb(px(4.0))
            .flex()
            .items_center()
            .gap(px(4.0))
            .text_size(px(10.0))
            .text_color(colors.muted)
            .child(div().flex_1().child("Who"))
            .child(div().w(px(CELL_WIDTH)).text_center().child("None"))
            .child(div().w(px(CELL_WIDTH)).text_center().child("Read"))
            .child(div().w(px(CELL_WIDTH)).text_center().child("Read + write"))
            .into_any_element(),
    );
    for class in Class::ALL {
        let shown = summary.access(class);
        let draft = editor.draft.get(class);
        let current = draft.or(match shown {
            Some(Merged::Same(access)) => Some(access),
            _ => None,
        });
        let mut title = format!("{} access", class.label());
        if let Some(note) = class_note(shown, draft) {
            title.push_str(&format!(" ({note})"));
        }
        let chips = Access::CHOICES
            .iter()
            .map(|&access| {
                chip(
                    (
                        match class {
                            Class::Owner => "perm-owner",
                            Class::Group => "perm-group",
                            Class::Others => "perm-others",
                        },
                        access as usize,
                    ),
                    access.label(),
                    current == Some(access),
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(editor) = this.editor_mut() {
                        editor.draft.set(class, Some(access));
                        editor.armed = false;
                    }
                    cx.notify();
                }))
                .into_any_element()
            })
            .collect::<Vec<_>>();
        rows.push(
            div()
                .mx(px(16.0))
                .mb(px(2.0))
                .flex()
                .items_center()
                .gap(px(4.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.0))
                        .text_color(colors.foreground)
                        .child(title),
                )
                .children(chips)
                .into_any_element(),
        );
    }

    if summary.files > 0 {
        let shown = exec_shown(summary, editor.draft.exec);
        let text = if shown.is_none() {
            "Allow executing (mixed)"
        } else {
            "Allow executing"
        };
        rows.push(section(vec![
            chip("perm-exec", text, shown == Some(true), cx)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(editor) = this.editor_mut() {
                        let now = exec_shown(&editor.loaded.summary, editor.draft.exec);
                        editor.draft.exec = Some(!now.unwrap_or(false));
                        editor.armed = false;
                    }
                    cx.notify();
                }))
                .into_any_element(),
        ]));
    }

    rows.push(group_section(editor, cx));
    rows.push(actions_section(editor, cx));
    if summary.symlinks > 0 {
        rows.push(section(vec![
            label(
                format!(
                    "{} skipped: a symbolic link's own permissions cannot be changed.",
                    plural(summary.symlinks, "symbolic link", "symbolic links")
                ),
                cx,
            )
            .into_any_element(),
        ]));
    }
    rows
}

fn group_section(editor: &Editor, cx: &mut Context<Explorer>) -> AnyElement {
    let colors = bolt::colors(cx);
    let summary = &editor.loaded.summary;
    let current = editor.group_choice.map(Merged::Same).or(summary.gid);
    let shown = name_or_id(&editor.loaded.group_names, current);
    if !editor.loaded.groups_available {
        return section(vec![
            label(format!("Group: {shown} (groups unavailable)"), cx).into_any_element(),
        ]);
    }
    // A select trigger, then the groups as full-width rows: a wrapping row of
    // chips hides most of them and reads like a filter, not a choice.
    let mut children = vec![
        label("Group".to_owned(), cx).into_any_element(),
        div()
            .id("perm-group-current")
            .h(px(30.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .border_1()
            .border_color(if editor.group_list_open {
                colors.line_2
            } else {
                colors.line
            })
            .bg(colors.background)
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                if let Some(editor) = this.editor_mut() {
                    editor.group_list_open = !editor.group_list_open;
                }
                cx.notify();
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.0))
                    .text_color(colors.strong)
                    .child(shown),
            )
            .child(
                Icon::new(super::icon(if editor.group_list_open {
                    gpui_kit_assets::IconName::ChevronUp
                } else {
                    gpui_kit_assets::IconName::ChevronDown
                }))
                .size(px(12.0))
                .color(colors.muted),
            )
            .into_any_element(),
    ];
    if editor.group_list_open {
        let choices = editor
            .loaded
            .groups
            .iter()
            .map(|group| {
                let gid = group.gid;
                let chosen = current == Some(Merged::Same(gid));
                let mut row = div()
                    .id(("perm-group-choice", u64::from(gid)))
                    .h(px(28.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .text_size(px(12.0))
                    .text_color(if chosen {
                        colors.strong
                    } else {
                        colors.foreground
                    })
                    .child(div().flex_1().min_w_0().truncate().child(group.label()));
                if chosen {
                    row = row.bg(colors.info.opacity(0.14));
                }
                row.on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(editor) = this.editor_mut() {
                        editor.group_choice = Some(gid);
                        editor.group_list_open = false;
                        editor.armed = false;
                    }
                    cx.notify();
                }))
                .into_any_element()
            })
            .collect::<Vec<_>>();
        children.push(
            div()
                .mt(px(4.0))
                .max_h(px(198.0))
                .overflow_hidden()
                .border_1()
                .border_color(colors.line)
                .children(choices)
                .into_any_element(),
        );
    }
    section(children)
}

fn actions_section(editor: &Editor, cx: &mut Context<Explorer>) -> AnyElement {
    let change = editor.change();
    let can_apply = !(change.edit.is_empty() && change.gid.is_none());
    let mut buttons = vec![
        Button::new("perm-apply", "Apply")
            .ghost()
            .disabled(!can_apply)
            .on_click(cx.listener(|this, _, window, cx| {
                this.apply_permissions(false, window, cx);
            }))
            .into_any_element(),
        Button::new("perm-reset", "Reset")
            .ghost()
            .disabled(!editor.dirty())
            .on_click(cx.listener(|this, _, _, cx| {
                if let Some(editor) = this.editor_mut() {
                    editor.draft = Edit::default();
                    editor.group_choice = None;
                    editor.armed = false;
                }
                cx.notify();
            }))
            .into_any_element(),
    ];
    let mut children = Vec::new();
    if editor.loaded.summary.folders > 0 {
        let can_enclosed = editor.can_apply_enclosed();
        let text = if editor.armed && can_enclosed {
            "Press again to apply to everything inside"
        } else {
            "Apply to enclosed files\u{2026}"
        };
        buttons.push(
            Button::new("perm-enclosed", text)
                .ghost()
                .disabled(!can_enclosed)
                .on_click(cx.listener(|this, _, window, cx| {
                    let Some(editor) = this.editor_mut() else {
                        return;
                    };
                    if editor.armed {
                        editor.armed = false;
                        this.apply_permissions(true, window, cx);
                    } else {
                        editor.armed = true;
                    }
                    cx.notify();
                }))
                .into_any_element(),
        );
        if editor.armed && can_enclosed {
            children.push(
                label(
                    "Folders get the shown access (r-x or rwx); files inside get the same read and write access. Symbolic links are skipped and other filesystems are not entered."
                        .to_owned(),
                    cx,
                )
                .into_any_element(),
            );
        }
    }
    children.insert(
        0,
        div()
            .flex()
            .flex_wrap()
            .gap(px(4.0))
            .children(buttons)
            .into_any_element(),
    );
    section(children)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    use std::path::Path;

    use fileflux::core::perms::{EntryState, Failure, Previous, summarize};

    use super::super::devices::job_paths;
    use super::super::ops::{JobCtx, run_job};
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn p(path: &str) -> PathBuf {
        PathBuf::from(path)
    }

    fn state(mode: u32, is_dir: bool, gid: u32) -> EntryState {
        EntryState {
            path: p("/x"),
            is_dir,
            is_symlink: false,
            mode,
            uid: 1000,
            gid,
        }
    }

    fn editor(states: &[EntryState]) -> Editor {
        Editor::new(
            vec![p("/x")],
            Loaded {
                summary: summarize(states),
                groups: Vec::new(),
                groups_available: true,
                user_names: Vec::new(),
                group_names: Vec::new(),
                unreadable: 0,
            },
        )
    }

    #[test]
    fn the_editor_reads_the_wanted_selection_once_and_waits_for_a_busy_read() {
        let a = vec![p("/a")];
        let b = vec![p("/b")];
        let none: Vec<PathBuf> = Vec::new();
        assert_eq!(
            editor_decision(&a, None, None),
            EditorDecision::Start(a.clone())
        );
        assert_eq!(editor_decision(&a, Some(&a), None), EditorDecision::Idle);
        assert_eq!(editor_decision(&a, None, Some(&a)), EditorDecision::Idle);
        assert_eq!(
            editor_decision(&b, Some(&a), None),
            EditorDecision::Start(b.clone())
        );
        assert_eq!(editor_decision(&b, None, Some(&a)), EditorDecision::Wait);
        assert_eq!(
            editor_decision(&none, Some(&a), None),
            EditorDecision::Clear
        );
        assert_eq!(editor_decision(&none, None, None), EditorDecision::Idle);
        assert_eq!(editor_decision(&none, None, Some(&a)), EditorDecision::Wait);
    }

    #[test]
    fn apply_changes_only_what_differs_and_the_enclosed_rule_adds_the_shown_values() {
        let mut editor = editor(&[state(0o100644, false, 10), state(0o100664, false, 10)]);
        assert_eq!(editor.change(), Change::default());
        assert!(!editor.dirty());
        editor.draft.set(Class::Owner, Some(Access::ReadWrite)); // as it is
        editor.draft.set(Class::Others, Some(Access::None));
        editor.group_choice = Some(10); // the group they already have
        assert!(editor.dirty());
        assert_eq!(
            editor.change(),
            Change {
                edit: Edit {
                    others: Some(Access::None),
                    ..Edit::default()
                },
                gid: None,
            }
        );
        // The enclosed action applies the shown values and the picked group.
        assert_eq!(
            editor.enclosed_change(),
            Change {
                edit: Edit {
                    owner: Some(Access::ReadWrite),
                    group: None,
                    others: Some(Access::None),
                    exec: None,
                },
                gid: Some(10),
            }
        );
        editor.group_choice = Some(11);
        assert_eq!(editor.change().gid, Some(11));
    }

    #[test]
    fn the_execute_toggle_and_class_notes_show_mixed_states() {
        let mixed = summarize(&[state(0o100755, false, 1), state(0o100644, false, 1)]);
        assert_eq!(exec_shown(&mixed, None), None);
        assert_eq!(exec_shown(&mixed, Some(true)), Some(true));
        let on = summarize(&[state(0o100755, false, 1)]);
        assert_eq!(exec_shown(&on, None), Some(true));
        let groups = summarize(&[state(0o100664, false, 1), state(0o100644, false, 1)]);
        assert_eq!(class_note(groups.group, None), Some("mixed"));
        assert_eq!(class_note(groups.group, Some(Access::Read)), None);
        assert_eq!(class_note(groups.owner, None), None);
        assert_eq!(
            class_note(summarize(&[state(0o100200, false, 1)]).owner, None),
            Some("write only")
        );
    }

    fn applied(changed: usize) -> Applied {
        Applied {
            changed,
            previous: (0..changed)
                .map(|n| Previous {
                    path: p(&format!("/x/{n}")),
                    dev: 1,
                    ino: n as u64,
                    born: None,
                    mode: 0o644,
                    gid: 1,
                })
                .collect(),
            ..Applied::default()
        }
    }

    #[test]
    fn one_summary_notice_with_the_failures_as_details_and_an_undo() {
        let mut result = applied(2);
        result.failures = vec![Failure {
            path: p("/x/locked"),
            message: "Permission denied (os error 13)".to_owned(),
        }];
        result.symlinks = 1;
        let report = report_from(result, "permissions", "Changed");
        let notice = report.notice.as_ref();
        assert_eq!(notice.map(|n| n.severity), Some(Severity::Warning));
        assert_eq!(
            notice.map(|n| n.text.as_str()),
            Some(
                "Changed permissions of 2 items; 1 failed. Skipped 1 symbolic link: their own permissions cannot be changed"
            )
        );
        assert_eq!(
            report.details,
            ["/x/locked: Permission denied (os error 13)"]
        );
        assert!(
            matches!(report.undo, Some(UndoAction::Permissions { ref items }) if items.len() == 2)
        );
        assert!(report.permissions_changed);

        let ok = report_from(applied(1), "group", "Changed");
        assert_eq!(
            ok.notice.as_ref().map(|n| n.severity),
            Some(Severity::Success)
        );
        assert_eq!(
            ok.notice.as_ref().map(|n| n.text.as_str()),
            Some("Changed group of 1 item")
        );
    }

    #[test]
    fn nothing_changed_failed_cancelled_and_too_large_report_honestly() {
        let nothing = report_from(Applied::default(), "permissions", "Changed");
        assert_eq!(
            nothing.notice.as_ref().map(|n| n.severity),
            Some(Severity::Info)
        );
        assert_eq!(
            nothing.notice.as_ref().map(|n| n.text.as_str()),
            Some("Nothing to change in permissions")
        );
        assert!(nothing.undo.is_none());

        let failed = Applied {
            failures: vec![Failure {
                path: p("/x"),
                message: "boom".to_owned(),
            }],
            ..Applied::default()
        };
        let failed = report_from(failed, "permissions", "Changed");
        assert_eq!(
            failed.notice.as_ref().map(|n| n.severity),
            Some(Severity::Error)
        );
        assert_eq!(
            failed.notice.as_ref().map(|n| n.text.as_str()),
            Some("1 failed")
        );

        let cancelled = Applied {
            cancelled: true,
            ..applied(1)
        };
        let cancelled = report_from(cancelled, "permissions", "Changed");
        assert_eq!(
            cancelled.notice.as_ref().map(|n| n.text.as_str()),
            Some("Changed permissions of 1 item; cancelled")
        );
        assert!(
            cancelled.cancelled && cancelled.undo.is_some(),
            "what was done stays undoable"
        );

        let big = Applied {
            undo_dropped: true,
            other_filesystems: 2,
            ..applied(1)
        };
        let big = report_from(big, "permissions", "Changed");
        assert!(big.undo.is_none(), "a partial undo is never offered");
        assert_eq!(
            big.notice.as_ref().map(|n| n.text.as_str()),
            Some(
                "Changed permissions of 1 item. Skipped 2 folders on another filesystem. Too many items to undo"
            )
        );
    }

    #[test]
    fn the_notice_names_what_was_changed() {
        let edit = Edit {
            owner: Some(Access::None),
            ..Edit::default()
        };
        assert_eq!(what_changed(&Change { edit, gid: None }), "permissions");
        assert_eq!(
            what_changed(&Change { edit, gid: Some(1) }),
            "permissions and group"
        );
        assert_eq!(
            what_changed(&Change {
                edit: Edit::default(),
                gid: Some(1)
            }),
            "group"
        );
    }

    fn ctx() -> JobCtx {
        JobCtx::from_env(PathBuf::from("/nonexistent-trash"), Vec::new())
    }

    fn scratch_dir(tag: &str) -> Result<PathBuf, std::io::Error> {
        let root = std::env::temp_dir().join(format!(
            "fileflux-perms-job-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&root)?;
        Ok(root)
    }

    fn mode_of(path: &Path) -> Result<u32, std::io::Error> {
        Ok(fs::symlink_metadata(path)?.mode() & 0o7777)
    }

    #[test]
    fn a_permission_job_changes_the_modes_and_its_undo_puts_them_back() -> TestResult {
        let root = scratch_dir("job")?;
        let file = root.join("f");
        fs::write(&file, "x")?;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644))?;
        let job = Job::Permissions(PermsJob {
            paths: vec![file.clone()],
            change: Change {
                edit: Edit {
                    others: Some(Access::None),
                    exec: Some(true),
                    ..Edit::default()
                },
                gid: None,
            },
            enclosed: false,
        });
        assert_eq!(
            job_paths(&job, Path::new("/trash")),
            std::slice::from_ref(&file)
        );
        let sink = Sink::new(super::super::progress::Unit::Items);
        let mut report = run_job(job, &ctx(), &sink);
        assert_eq!(mode_of(&file)?, 0o750);
        assert_eq!(sink.snapshot().done, 1);
        assert!(report.permissions_changed);
        let undo = report.undo.take();
        let Some(action) = undo else {
            return Err("a change must be undoable".into());
        };
        let undone = run_job(Job::Undo(action), &ctx(), &sink);
        assert_eq!(mode_of(&file)?, 0o644);
        assert!(undone.undo.is_none());
        assert_eq!(
            undone.notice.as_ref().map(|n| n.text.as_str()),
            Some("Restored permissions of 1 item")
        );
        fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[test]
    fn a_cancelled_job_changes_nothing() -> TestResult {
        let root = scratch_dir("cancel")?;
        let file = root.join("f");
        fs::write(&file, "x")?;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644))?;
        let sink = Sink::new(super::super::progress::Unit::Items);
        sink.request_cancel();
        let report = run(
            PermsJob {
                paths: vec![file.clone()],
                change: Change {
                    edit: Edit {
                        owner: Some(Access::None),
                        ..Edit::default()
                    },
                    gid: None,
                },
                enclosed: false,
            },
            &sink,
        );
        assert!(report.cancelled);
        assert_eq!(mode_of(&file)?, 0o644);
        fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[test]
    fn load_reads_the_entries_of_a_selection() -> TestResult {
        let root = scratch_dir("load")?;
        let file = root.join("f");
        fs::write(&file, "x")?;
        let loaded = load(&[file.clone(), root.join("gone")]);
        assert_eq!(loaded.unreadable, 1);
        assert_eq!(loaded.summary.files, 1);
        assert_eq!(
            loaded.summary.gid,
            Some(Merged::Same(fs::metadata(&file)?.gid()))
        );
        fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[test]
    fn a_folder_without_x_can_be_fixed_with_a_plain_apply() {
        let mut editor = editor(&[state(0o040740, true, 10)]);
        assert_eq!(editor.change(), Change::default());
        editor.draft.set(Class::Group, Some(Access::Read));
        let change = editor.change();
        assert_eq!(change.edit.group, Some(Access::Read), "Apply is enabled");
        assert_eq!(
            perms::apply_mode(0o040740, true, &change.edit) & 0o777,
            0o750
        );
        assert_eq!(
            class_note(editor.loaded.summary.group, None),
            Some("custom")
        );
    }

    #[test]
    fn enclosed_stays_disabled_until_something_would_change() {
        let mut editor = editor(&[
            state(0o040700, true, 10),
            state(0o040070, true, 10),
            state(0o100007, false, 10),
        ]);
        assert!(!editor.can_apply_enclosed());
        editor.draft.set(Class::Others, Some(Access::Read));
        assert!(editor.can_apply_enclosed());
        editor.draft = Edit::default();
        editor.group_choice = Some(10);
        assert!(editor.can_apply_enclosed());
        editor.group_choice = None;
        editor.draft.exec = Some(true);
        assert!(editor.can_apply_enclosed());
    }

    #[test]
    fn unreadable_process_groups_are_reported_not_silently_empty() {
        let (groups, available) = groups_or_unavailable(Err(std::io::Error::other("no /proc")));
        assert!(groups.is_empty() && !available);
        let one = Group { gid: 7, name: None };
        let (groups, available) = groups_or_unavailable(Ok(vec![one.clone()]));
        assert_eq!(groups, [one]);
        assert!(available);
    }

    #[test]
    fn undoing_advances_the_progress_per_restored_entry() -> TestResult {
        let root = scratch_dir("undoprogress")?;
        let a = root.join("a");
        let b = root.join("b");
        for file in [&a, &b] {
            fs::write(file, "x")?;
            fs::set_permissions(file, fs::Permissions::from_mode(0o644))?;
        }
        let sink = Sink::new(super::super::progress::Unit::Items);
        let mut report = run(
            PermsJob {
                paths: vec![a.clone(), b.clone()],
                change: Change {
                    edit: Edit {
                        others: Some(Access::None),
                        ..Edit::default()
                    },
                    gid: None,
                },
                enclosed: false,
            },
            &sink,
        );
        let Some(UndoAction::Permissions { items }) = report.undo.take() else {
            return Err("a change must be undoable".into());
        };
        let undo_sink = Sink::new(super::super::progress::Unit::Items);
        let undone = undo(&items, &undo_sink);
        let snapshot = undo_sink.snapshot();
        assert_eq!((snapshot.done, snapshot.total), (2, 2));
        assert_eq!(mode_of(&a)?, 0o644);
        assert!(undone.undo.is_none());
        fs::remove_dir_all(&root)?;
        Ok(())
    }
}
