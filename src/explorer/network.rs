//! Connect to Server and the network bookmarks, on the UI side.
//!
//! "Connect to Server…" (the sidebar's Network section and the empty-space
//! menu) takes a server address and mounts it with `gio mount` (see
//! [`fileflux::core::network`]); gio's questions (user, domain, password, an
//! unknown host key) come back as dialogs of this window, and the answers go
//! to gio's standard input. Once mounted, the gvfs folder opens. A network
//! bookmark of the GTK bookmarks file does the same from its sidebar row.
//!
//! The password: it lives in the dialog's masked field until Enter or
//! Continue, moves from there to gio (a [`Secret`], zeroed when dropped) and
//! the field is emptied at once. It is never logged, saved, shown in a
//! notice or put in argv or the environment. Only one attempt runs at a time.
//!
//! Unlocking an encrypted volume ("Unlock and mount" on a Locked device) is
//! the same attempt with another goal: `gio mount -d <device>` asks for the
//! passphrase through the same masked dialog (the same [`Secret`], redaction
//! and cancel). The passphrase is never kept: no keyring, no cache. On
//! success a fresh `lsblk` listing names the unlocked volume's mount point
//! and the tab opens it; on a wrong passphrase the volume stays Locked and
//! the person can try again.

use std::path::{Path, PathBuf};
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use bolt::controls::{InputEvent, InputState};
use fileflux::core::Location;
use fileflux::core::bookmarks::{self, NetworkBookmark};
use fileflux::core::devices::{
    UnlockTarget, devices, list_blocks, unlock_target, wait_for_unlocked_mount,
};
use fileflux::core::network::{
    Answer, MountEvent, MountOutcome, MountSession, Prompt, Secret, ServerUri, mount_target,
    mounted_name, spawn_mount, spawn_unlock, wait_for_mount,
};
use fileflux::core::places::{Volume, gvfs_root_from_env};
use gpui::{AppContext as _, Context, Entity, Pixels, Point, Window};

use super::actions::MenuOn;
use super::devices::MISSING_GIO_UNLOCK;
use super::ops::{
    DialogAction, DialogKind, DialogState, MenuCommand, MenuItemSpec, MenuRow, Severity,
};
use super::state::Transition;
use super::view::Explorer;

/// What a disabled entry says when `gio` is not installed.
pub(crate) const MISSING_GIO: &str = "Install gio (glib) to connect to servers";

/// How many servers the quick picks of the dialog remember.
const RECENT_MAX: usize = 5;
/// How long the gvfs directory is waited for after gio mounted: the FUSE
/// directory can trail gio's exit by a moment.
const LOOKUP_ATTEMPTS: u32 = 20;
const LOOKUP_DELAY: Duration = Duration::from_millis(250);

/// The open "Connect to Server…" dialog: its field, the last refusal and the
/// quick picks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConnectDialog {
    pub(crate) input: Entity<InputState>,
    /// Why the typed address was refused; shown under the field.
    pub(crate) error: Option<String>,
    /// The servers of this session, newest first.
    pub(crate) recent: Vec<String>,
}

/// The open prompt of a running `gio mount`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PromptDialog {
    pub(crate) prompt: Prompt,
    /// The field for a text prompt; a question answers through its buttons.
    pub(crate) input: Option<Entity<InputState>>,
}

/// What an attempt is for.
#[derive(Debug)]
enum Goal {
    /// Mount a server address.
    Connect(ServerUri),
    /// Unlock an encrypted volume of the device listing.
    Unlock(UnlockTarget),
}

/// The attempt in progress: its goal, its label and the running gio.
#[derive(Debug)]
struct Attempt {
    goal: Goal,
    label: String,
    session: MountSession,
}

impl Attempt {
    /// The device row the attempt shows "Unlocking…" on, if it unlocks.
    fn working_key(&self) -> &Path {
        match &self.goal {
            Goal::Unlock(target) => target.path(),
            Goal::Connect(_) => Path::new(""),
        }
    }
}

impl Drop for Attempt {
    /// Closing the window or dropping the attempt never leaves gio waiting
    /// for an answer nobody can give.
    fn drop(&mut self) {
        self.session.control.cancel();
    }
}

/// Network state of the window.
#[derive(Debug, Default)]
pub(crate) struct NetworkHub {
    /// The network entries of the GTK bookmarks file.
    pub(crate) bookmarks: Vec<NetworkBookmark>,
    /// The servers of this session, newest first (addresses, never
    /// passwords); not saved.
    pub(crate) recent: Vec<String>,
    attempt: Option<Attempt>,
}

impl NetworkHub {
    /// The label of the server being connected to.
    pub(crate) fn connecting(&self) -> Option<&str> {
        self.attempt.as_ref().map(|attempt| attempt.label.as_str())
    }

    /// Whether the attempt in progress unlocks an encrypted volume.
    pub(crate) fn unlocking(&self) -> bool {
        self.attempt
            .as_ref()
            .is_some_and(|attempt| matches!(attempt.goal, Goal::Unlock(_)))
    }

    /// What the sidebar's cancel row says: "Connecting to X…" or
    /// "Unlocking X…".
    pub(crate) fn status(&self) -> Option<String> {
        let attempt = self.attempt.as_ref()?;
        Some(match attempt.goal {
            Goal::Connect(_) => format!("Connecting to {}\u{2026}", attempt.label),
            Goal::Unlock(_) => format!("Unlocking {}\u{2026}", attempt.label),
        })
    }
}

/// Puts `uri` first in `list`, once, keeping at most [`RECENT_MAX`].
pub(crate) fn remember(list: &mut Vec<String>, uri: &ServerUri) {
    let text = uri.to_string();
    list.retain(|known| *known != text);
    list.insert(0, text);
    list.truncate(RECENT_MAX);
}

/// The folder of `uri`'s mount among the mounted `volumes` (the gvfs ones
/// the sidebar already lists), when it is mounted.
pub(crate) fn mounted_path(
    volumes: &[Volume],
    gvfs_root: Option<&Path>,
    uri: &ServerUri,
) -> Option<PathBuf> {
    let root = gvfs_root?;
    // Only real gvfs mounts: a disk labelled like one is not a server.
    let volumes: Vec<&Volume> = volumes
        .iter()
        .filter(|volume| volume.kind.is_gvfs() && volume.path.parent() == Some(root))
        .collect();
    let names: Vec<String> = volumes
        .iter()
        .filter_map(|volume| volume.path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    let name = mounted_name(uri, &names)?;
    Some(mount_target(root, &name, uri))
}

/// The menu of a network bookmark row.
pub(crate) fn bookmark_menu_rows(gio: bool) -> Vec<MenuRow> {
    vec![
        MenuRow::Item(MenuItemSpec {
            enabled: gio,
            ..MenuItemSpec::new(MenuCommand::ConnectBookmark, "Connect")
        }),
        MenuRow::Separator,
        MenuRow::Item(MenuItemSpec::new(
            MenuCommand::RemoveBookmark,
            "Remove bookmark",
        )),
    ]
}

/// The "Connect to Server…" rows of the empty-space menu.
pub(crate) fn add_connect_row(rows: &mut Vec<MenuRow>, gio: bool) {
    if !rows.is_empty() {
        rows.push(MenuRow::Separator);
    }
    rows.push(MenuRow::Item(MenuItemSpec {
        enabled: gio,
        ..MenuItemSpec::new(
            MenuCommand::ConnectToServer,
            if gio {
                "Connect to Server\u{2026}"
            } else {
                "Connect to Server\u{2026} (needs gio)"
            },
        )
    }));
}

impl Explorer {
    fn gio(&self) -> Option<PathBuf> {
        self.devices.gio.clone()
    }

    /// A one-line input whose Enter confirms the open dialog of `live`.
    fn network_input(
        &mut self,
        live: fn(&DialogAction, &Entity<InputState>) -> bool,
        setup: impl FnOnce(&mut InputState, &mut Window, &mut Context<InputState>),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        let input = cx.new(|cx| {
            let mut input = InputState::new(window, cx);
            setup(&mut input, window, cx);
            input
        });
        cx.subscribe_in(
            &input,
            window,
            move |this, input, event: &InputEvent, window, cx| {
                let current = this
                    .dialogs
                    .current()
                    .is_some_and(|dialog| live(&dialog.action, input));
                if current && matches!(event, InputEvent::PressEnter { .. }) {
                    this.confirm_dialog(window, cx);
                }
            },
        )
        .detach();
        input
    }

    /// "Connect to Server…": asks for an address.
    pub(crate) fn connect_to_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.gio().is_none() {
            return self.say(Severity::Info, MISSING_GIO, window, cx);
        }
        if let Some(label) = self.network.connecting() {
            let text = format!("Already connecting to {label}");
            return self.say(Severity::Info, text, window, cx);
        }
        let input = self.network_input(
            |action, input| matches!(action, DialogAction::Connect(state) if state.input == *input),
            |input, window, cx| input.set_placeholder("sftp://user@host/path", window, cx),
            window,
            cx,
        );
        let dialog = ConnectDialog {
            input: input.clone(),
            error: None,
            recent: self.network.recent.clone(),
        };
        self.offer_dialog(
            DialogState::new(DialogKind::Connect, &[], DialogAction::Connect(dialog)),
            window,
            cx,
        );
        cx.defer_in(window, move |_this, window, cx| {
            input.update(cx, |input, cx| input.focus(window, cx));
        });
    }

    /// A quick pick: puts the remembered address in the field.
    pub(crate) fn pick_recent_server(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(DialogAction::Connect(dialog)) =
            self.dialogs.current().map(|dialog| dialog.action.clone())
        else {
            return;
        };
        let Some(text) = dialog.recent.get(index).cloned() else {
            return;
        };
        dialog.input.update(cx, |input, cx| {
            input.set_value(text, window, cx);
            input.focus(window, cx);
        });
    }

    /// Connect (or Enter): a refused address keeps the dialog open with the
    /// reason; a good one closes it and connects.
    pub(crate) fn confirm_connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(DialogAction::Connect(dialog)) =
            self.dialogs.current().map(|dialog| dialog.action.clone())
        else {
            return;
        };
        let text = dialog.input.read(cx).value().to_string();
        match ServerUri::parse(&text) {
            Err(error) => {
                if let Some(DialogState {
                    action: DialogAction::Connect(state),
                    ..
                }) = self.dialogs.current_mut()
                {
                    state.error = Some(error.to_string());
                }
                cx.notify();
            }
            Ok(uri) => {
                self.dialogs.close();
                self.dialog_closed(window, cx);
                self.connect_to(&uri, None, window, cx);
            }
        }
    }

    /// Opens `uri`: its folder when it is mounted already, else mounts it
    /// (answering gio's questions in dialogs) and opens it. `title` is the
    /// bookmark's label, when it came from one.
    pub(crate) fn connect_to(
        &mut self,
        uri: &ServerUri,
        title: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(gio) = self.gio() else {
            return self.say(Severity::Info, MISSING_GIO, window, cx);
        };
        if let Some(label) = self.network.connecting() {
            let text = format!("Already connecting to {label}");
            return self.say(Severity::Info, text, window, cx);
        }
        remember(&mut self.network.recent, uri);
        if let Some(path) = self
            .places
            .as_ref()
            .and_then(|places| mounted_path(&places.volumes, gvfs_root_from_env().as_deref(), uri))
        {
            self.navigate(Transition::Visit(Location::Directory(path)), window, cx);
            return;
        }
        let label = uri.label(title);
        match spawn_mount(&gio, uri) {
            Ok(session) => {
                self.network.attempt = Some(Attempt {
                    goal: Goal::Connect(uri.clone()),
                    label: label.clone(),
                    session,
                });
                self.say(
                    Severity::Info,
                    format!("Connecting to {label}\u{2026}"),
                    window,
                    cx,
                );
                cx.notify();
            }
            Err(error) => self.say(
                Severity::Error,
                format!("Could not start gio to connect to {label}: {error}"),
                window,
                cx,
            ),
        }
    }

    /// Takes what the running attempt reported since the last tick.
    pub(crate) fn pump_network(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        loop {
            let Some(attempt) = self.network.attempt.as_ref() else {
                return;
            };
            match attempt.session.events.try_recv() {
                Ok(MountEvent::Prompt(prompt)) => self.open_mount_prompt(prompt, window, cx),
                Ok(MountEvent::Done(outcome)) => return self.finish_mount(outcome, window, cx),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    let lost = MountOutcome::Failed("gio stopped unexpectedly".to_owned());
                    return self.finish_mount(lost, window, cx);
                }
            }
        }
    }

    /// Shows gio's question; the answer is sent when it is given.
    fn open_mount_prompt(&mut self, prompt: Prompt, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.network.connecting().map(str::to_owned) else {
            return;
        };
        let (label, message, secret, question) = match &prompt {
            Prompt::Text {
                label,
                message,
                kind,
                ..
            } => (
                label.clone(),
                message.clone(),
                *kind == fileflux::core::network::PromptKind::Password,
                false,
            ),
            Prompt::Question { message, .. } => {
                ("Question".to_owned(), message.clone(), false, true)
            }
        };
        let input = (!question).then(|| {
            let default = match &prompt {
                Prompt::Text { default, .. } => default.clone().unwrap_or_default(),
                Prompt::Question { .. } => String::new(),
            };
            let placeholder = label.clone();
            self.network_input(
                |action, input| {
                    matches!(action, DialogAction::MountPrompt(state)
                        if state.input.as_ref() == Some(input))
                },
                move |input, window, cx| {
                    if secret {
                        // A password is masked while typed.
                        input.set_masked(true, window, cx);
                    }
                    input.set_placeholder(placeholder, window, cx);
                    input.set_value(default, window, cx);
                },
                window,
                cx,
            )
        });
        let field = input.clone();
        let unlock = self.network.unlocking();
        let kind = DialogKind::MountPrompt {
            host,
            unlock,
            label,
            secret,
            question,
        };
        self.offer_dialog(
            DialogState::new(
                kind,
                &message,
                DialogAction::MountPrompt(PromptDialog { prompt, input }),
            ),
            window,
            cx,
        );
        if let Some(field) = field {
            cx.defer_in(window, move |_this, window, cx| {
                field.update(cx, |input, cx| {
                    input.focus(window, cx);
                    input.select_all(window, cx);
                });
            });
        }
    }

    /// The open prompt's text answer (Enter or Continue): the field's text
    /// goes to gio and the field is emptied.
    pub(crate) fn confirm_mount_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(DialogAction::MountPrompt(dialog)) =
            self.dialogs.current().map(|dialog| dialog.action.clone())
        else {
            return;
        };
        // A question is answered by its own buttons.
        let Some(input) = dialog.input else {
            return;
        };
        let answer = Secret::from_string(input.read(cx).value().to_string());
        input.update(cx, |input, cx| input.set_value("", window, cx));
        self.dialogs.close();
        self.dialog_closed(window, cx);
        if let Some(attempt) = &self.network.attempt {
            attempt.session.control.answer(Answer::Line(answer));
        }
    }

    /// A button of a question: the choice goes to gio.
    pub(crate) fn answer_mount_choice(
        &mut self,
        number: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(
            self.dialogs.current().map(|dialog| &dialog.action),
            Some(DialogAction::MountPrompt(_))
        ) {
            return;
        }
        self.dialogs.close();
        self.dialog_closed(window, cx);
        if let Some(attempt) = &self.network.attempt {
            attempt.session.control.answer(Answer::Choice(number));
        }
    }

    /// Cancel on a prompt, or the sidebar's "Connecting…" row: kills gio.
    /// `dialog` is the prompt that was closed, when the cancel came from it:
    /// its field is emptied.
    pub(crate) fn cancel_mount(
        &mut self,
        dialog: Option<PromptDialog>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(input) = dialog.and_then(|dialog| dialog.input) {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        let Some(attempt) = self.network.attempt.take() else {
            return;
        };
        attempt.session.control.cancel();
        self.devices.working.remove(attempt.working_key());
        if self.dialogs.discard_mount_prompts(Instant::now()) {
            self.dialog_closed(window, cx);
        }
        let verb = match attempt.goal {
            Goal::Connect(_) => "connecting to",
            Goal::Unlock(_) => "unlocking",
        };
        self.say(
            Severity::Info,
            format!("Cancelled {verb} {}", attempt.label),
            window,
            cx,
        );
        cx.notify();
    }

    /// gio ended: report the failure, or open the mounted folder.
    fn finish_mount(&mut self, outcome: MountOutcome, window: &mut Window, cx: &mut Context<Self>) {
        let Some(attempt) = self.network.attempt.take() else {
            return;
        };
        if self.dialogs.discard_mount_prompts(Instant::now()) {
            self.dialog_closed(window, cx);
        }
        self.devices.working.remove(attempt.working_key());
        cx.notify();
        let Attempt { goal, label, .. } = &attempt;
        match (outcome, goal) {
            (MountOutcome::Cancelled, _) => {}
            (MountOutcome::Failed(message), Goal::Connect(_)) => self.say(
                Severity::Error,
                format!("Could not connect to {label}: {message}"),
                window,
                cx,
            ),
            (MountOutcome::Failed(message), Goal::Unlock(_)) => {
                // Still locked: the row stays, and a new try asks again.
                self.devices.last_poll = None;
                self.say(
                    Severity::Error,
                    format!("Could not unlock {label}: {message}"),
                    window,
                    cx,
                );
            }
            (MountOutcome::Mounted, Goal::Connect(uri)) => {
                self.open_mounted(uri, label, window, cx);
            }
            (MountOutcome::Mounted, Goal::Unlock(target)) => {
                self.open_unlocked(target, label, window, cx);
            }
        }
    }

    /// "Unlock and mount": validates `path` against the device listing,
    /// starts `gio mount -d` and answers its passphrase prompt in the masked
    /// dialog.
    pub(crate) fn unlock_device(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(gio) = self.gio() else {
            return self.say(Severity::Info, MISSING_GIO_UNLOCK, window, cx);
        };
        if let Some(label) = self.network.connecting() {
            let text = format!("Already working on {label}");
            return self.say(Severity::Info, text, window, cx);
        }
        let target = match unlock_target(&self.devices.nodes, &path) {
            Ok(target) => target,
            Err(error) => return self.say(Severity::Info, error.to_string(), window, cx),
        };
        if let Some(text) = self
            .devices
            .find(target.path())
            .and_then(|device| self.devices.refusal_for(device))
        {
            return self.say(Severity::Info, text, window, cx);
        }
        let label = target.label().to_owned();
        match spawn_unlock(&gio, &target) {
            Ok(session) => {
                self.devices
                    .working
                    .insert(target.path().to_path_buf(), "Unlocking\u{2026}");
                self.network.attempt = Some(Attempt {
                    goal: Goal::Unlock(target),
                    label: label.clone(),
                    session,
                });
                self.say(
                    Severity::Info,
                    format!("Unlocking {label}\u{2026}"),
                    window,
                    cx,
                );
                cx.notify();
            }
            Err(error) => self.say(
                Severity::Error,
                format!("Could not start gio to unlock {label}: {error}"),
                window,
                cx,
            ),
        }
    }

    /// Reads fresh listings off the UI thread until the unlocked volume's
    /// mount point shows, then opens it.
    fn open_unlocked(
        &mut self,
        target: &UnlockTarget,
        label: &str,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.devices.last_poll = None;
        let Some(lsblk) = self.devices.lsblk.clone() else {
            self.tab_mut()
                .state
                .set_notice(format!("Unlocked {label}, but lsblk is missing"));
            return;
        };
        let (container, label) = (target.path().to_path_buf(), label.to_owned());
        let lookup = cx.background_executor().spawn(async move {
            wait_for_unlocked_mount(
                || list_blocks(&lsblk),
                &container,
                LOOKUP_ATTEMPTS,
                LOOKUP_DELAY,
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            let found = lookup.await;
            if let Err(closed) = this.update_in(cx, |this, window, cx| match found {
                Some((nodes, mount)) => {
                    this.devices.list = devices(&nodes);
                    this.devices.nodes = nodes;
                    this.devices.last_poll = None;
                    this.navigate(Transition::Visit(Location::Directory(mount)), window, cx);
                    cx.notify();
                }
                None => this.say(
                    Severity::Warning,
                    format!("Unlocked {label}, but its folder did not appear"),
                    window,
                    cx,
                ),
            }) {
                log::debug!("the explorer closed before the volume was opened: {closed}");
            }
        })
        .detach();
    }

    /// Looks for the gvfs folder of a mounted address off the UI thread and
    /// opens it.
    fn open_mounted(
        &mut self,
        uri: &ServerUri,
        label: &str,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = gvfs_root_from_env() else {
            self.tab_mut().state.set_notice(format!(
                "Connected to {label}, but the gvfs folder is unknown"
            ));
            return;
        };
        let (uri, label) = (uri.clone(), label.to_owned());
        let lookup = cx
            .background_executor()
            .spawn(async move { wait_for_mount(&root, &uri, LOOKUP_ATTEMPTS, LOOKUP_DELAY) });
        cx.spawn_in(window, async move |this, cx| {
            let found = lookup.await;
            if let Err(closed) = this.update_in(cx, |this, window, cx| match found {
                Some(path) => {
                    this.navigate(Transition::Visit(Location::Directory(path)), window, cx);
                }
                None => this.say(
                    Severity::Warning,
                    format!("Connected to {label}, but its folder did not appear"),
                    window,
                    cx,
                ),
            }) {
                log::debug!("the explorer closed before the mount was opened: {closed}");
            }
        })
        .detach();
    }

    /// Opens the menu of a network bookmark row.
    pub(crate) fn open_network_menu(
        &mut self,
        position: Point<Pixels>,
        row: String,
        bookmark: NetworkBookmark,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_rename(window, cx);
        let name = bookmark.display_name();
        let rows = bookmark_menu_rows(self.gio().is_some());
        self.show_menu(
            position,
            rows,
            (MenuOn::Network { row, bookmark }, Some(name)),
            window,
            cx,
        );
    }

    /// Removes the bookmark from the GTK bookmarks file off the UI thread;
    /// the file watcher (and the reload that follows) updates the sidebar.
    pub(crate) fn remove_network_bookmark(
        &mut self,
        uri: ServerUri,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.network.bookmarks.retain(|known| known.uri != uri);
        cx.notify();
        let Some(file) = self.bookmarks.path().map(std::path::Path::to_path_buf) else {
            return;
        };
        let write = cx
            .background_executor()
            .spawn(async move { bookmarks::remove_network(&file, &uri) });
        cx.spawn_in(window, async move |this, cx| {
            let written = write.await;
            if let Err(closed) = this.update_in(cx, |this, window, cx| {
                if let Err(error) = written {
                    log::warn!("the GTK bookmarks were not written: {error}");
                    this.say(
                        Severity::Error,
                        format!("Could not remove the bookmark ({error})"),
                        window,
                        cx,
                    );
                }
                this.attach_bookmark_watch();
                this.reload_favorites(cx);
            }) {
                log::debug!("the explorer closed before the bookmark was removed: {closed}");
            }
        })
        .detach();
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use std::path::Path;

    use fileflux::core::places::VolumeKind;

    use super::*;

    fn uri(text: &str) -> ServerUri {
        ServerUri::parse(text).expect("the URI is valid")
    }

    fn volume(name: &str) -> Volume {
        Volume {
            label: name.to_owned(),
            path: Path::new("/run/user/1000/gvfs").join(name),
            removable: false,
            usage: None,
            kind: VolumeKind::Remote,
        }
    }

    #[test]
    fn recent_servers_are_newest_first_without_repeats_and_capped() {
        let mut list = Vec::new();
        for host in ["a", "b", "c", "a", "d", "e", "f"] {
            remember(&mut list, &uri(&format!("sftp://{host}/")));
        }
        assert_eq!(
            list,
            [
                "sftp://f/",
                "sftp://e/",
                "sftp://d/",
                "sftp://a/",
                "sftp://c/"
            ]
        );
    }

    #[test]
    fn a_disk_named_like_a_gvfs_mount_is_never_opened() {
        let mut disk = volume("sftp:host=nas");
        disk.path = Path::new("/run/media/me").join("sftp:host=nas");
        disk.kind = VolumeKind::Disk;
        let root = Path::new("/run/user/1000/gvfs");
        assert_eq!(
            mounted_path(&[disk.clone()], Some(root), &uri("sftp://nas/")),
            None
        );
        let mut elsewhere = volume("sftp:host=nas");
        elsewhere.path = Path::new("/mnt").join("sftp:host=nas");
        assert_eq!(
            mounted_path(&[elsewhere], Some(root), &uri("sftp://nas/")),
            None
        );
        assert_eq!(
            mounted_path(&[volume("sftp:host=nas")], None, &uri("sftp://nas/")),
            None,
            "without a gvfs root nothing is a gvfs mount"
        );
    }

    #[test]
    fn an_already_mounted_bookmark_opens_its_gvfs_path() {
        let volumes = [
            volume("mtp:host=Pixel"),
            volume("sftp:host=example.com,user=bob"),
        ];
        assert_eq!(
            mounted_path(
                &volumes,
                Some(Path::new("/run/user/1000/gvfs")),
                &uri("sftp://bob@example.com/srv/app")
            ),
            Some(PathBuf::from(
                "/run/user/1000/gvfs/sftp:host=example.com,user=bob/srv/app"
            ))
        );
        assert_eq!(
            mounted_path(
                &volumes,
                Some(Path::new("/run/user/1000/gvfs")),
                &uri("sftp://other.example.com/")
            ),
            None
        );
        assert_eq!(
            mounted_path(
                &[],
                Some(Path::new("/run/user/1000/gvfs")),
                &uri("sftp://example.com/")
            ),
            None
        );
    }

    #[test]
    fn the_dialogs_name_the_server_and_never_a_password() {
        use super::super::ops::dialog_text;
        let connect = dialog_text(&DialogKind::Connect, &[]);
        assert_eq!(connect.title, "Connect to Server");
        assert_eq!(connect.confirm, "Connect");
        assert!(connect.body.contains("password"));
        let kind = DialogKind::MountPrompt {
            host: "nas/media".to_owned(),
            unlock: false,
            label: "Password".to_owned(),
            secret: true,
            question: false,
        };
        let lines = ["Authentication Required".to_owned()];
        let prompt = dialog_text(&kind, &lines);
        assert_eq!(prompt.title, "Connect to nas/media");
        assert!(prompt.body.starts_with("Password is needed"));
        assert_eq!(prompt.names, lines, "gio's own lines are shown as they are");
    }

    #[test]
    fn the_unlock_dialog_names_the_volume_and_never_a_passphrase() {
        use super::super::ops::dialog_text;
        let kind = DialogKind::MountPrompt {
            host: "Vault".to_owned(),
            unlock: true,
            label: "Password".to_owned(),
            secret: true,
            question: false,
        };
        let lines = ["Enter a passphrase to unlock the volume".to_owned()];
        let prompt = dialog_text(&kind, &lines);
        assert_eq!(prompt.title, "Unlock Vault");
        assert_eq!(
            prompt.body,
            "Password is needed to unlock it. Cancel stops unlocking."
        );
        assert_eq!(prompt.confirm, "Continue");
        assert_eq!(prompt.names, lines);
    }

    #[test]
    fn the_bookmark_menu_connects_and_removes() {
        let commands = |gio| -> Vec<(MenuCommand, bool)> {
            bookmark_menu_rows(gio)
                .into_iter()
                .filter_map(|row| match row {
                    MenuRow::Item(spec) => Some((spec.command, spec.enabled)),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            commands(true),
            [
                (MenuCommand::ConnectBookmark, true),
                (MenuCommand::RemoveBookmark, true)
            ]
        );
        assert_eq!(
            commands(false),
            [
                (MenuCommand::ConnectBookmark, false),
                (MenuCommand::RemoveBookmark, true)
            ],
            "without gio only Connect is disabled: the bookmark can still go"
        );
    }

    #[test]
    fn the_connect_row_follows_a_separator_and_needs_gio() {
        let mut rows = vec![MenuRow::Item(MenuItemSpec::new(
            MenuCommand::NewFolder,
            "New folder",
        ))];
        add_connect_row(&mut rows, false);
        assert!(matches!(rows.get(1), Some(MenuRow::Separator)));
        let Some(MenuRow::Item(spec)) = rows.get(2) else {
            panic!("the connect row is last");
        };
        assert_eq!(spec.command, MenuCommand::ConnectToServer);
        assert!(!spec.enabled);
        assert!(spec.label.contains("needs gio"));
        let mut alone = Vec::new();
        add_connect_row(&mut alone, true);
        assert_eq!(alone.len(), 1, "no separator on an empty menu");
    }
}
